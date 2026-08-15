//! Git branch discovery and checkout operations for workspace selectors.
//!
//! Git inspection and mutation functions in this module perform process I/O;
//! callers must run them from the background executor. Render paths consume
//! only cached snapshots and pure picker models.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt as _;

use anyhow::{Context as _, anyhow, bail};

const MAX_UNTRACKED_FILES: usize = 2_048;
const MAX_UNTRACKED_FILE_BYTES: u64 = 8 * 1_024 * 1_024;
const MAX_UNTRACKED_TOTAL_BYTES: u64 = 32 * 1_024 * 1_024;
const BINARY_PROBE_BYTES: usize = 8_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchEntry {
    pub name: String,
    /// Git refuses to check out one branch in two worktrees. Keep such rows
    /// visible so the list remains truthful, but let the picker draw them
    /// disabled.
    pub checked_out_elsewhere: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorktreeHead {
    Branch(String),
    Detached { commit: String },
}

impl WorktreeHead {
    pub fn branch(&self) -> Option<&str> {
        match self {
            Self::Branch(branch) => Some(branch),
            Self::Detached { .. } => None,
        }
    }

    pub fn commit(&self) -> Option<&str> {
        match self {
            Self::Branch(_) => None,
            Self::Detached { commit } => Some(commit),
        }
    }

    pub fn display_commit(&self) -> Option<String> {
        self.commit().map(short_commit)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeEntry {
    /// The Git worktree root, canonicalized while discovery runs.
    pub root: PathBuf,
    pub head: WorktreeHead,
    /// A lock is metadata only. Locked but valid worktrees remain selectable.
    pub locked: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeSnapshot {
    pub repository: PathBuf,
    /// The canonical project path in the active checkout. It is always
    /// excluded from existing-worktree rows, even when the caller originally stored a
    /// symlinked or non-canonical project path.
    pub project_path: PathBuf,
    /// The project path relative to the repository root. An empty path means
    /// the project itself is the repository root.
    pub project_relative: PathBuf,
    pub worktrees: Vec<WorktreeEntry>,
}

impl WorktreeSnapshot {
    pub fn project_path(&self, worktree: &WorktreeEntry) -> PathBuf {
        worktree.root.join(&self.project_relative)
    }

    pub fn existing_worktrees(&self, excluded_paths: &[&Path]) -> Vec<ExistingWorktree> {
        self.worktrees
            .iter()
            .filter_map(|worktree| {
                let path = self.project_path(worktree);
                if path == self.project_path
                    || excluded_paths.iter().any(|excluded| *excluded == path)
                {
                    return None;
                }
                Some(ExistingWorktree {
                    path,
                    name: worktree_name(&worktree.root),
                    head: worktree.head.clone(),
                })
            })
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExistingWorktree {
    /// The project-relative path a task should use, not just the repository
    /// root. This is what gets persisted in the session workspace.
    pub path: PathBuf,
    /// The linked worktree folder, used for detached picker labels.
    pub name: String,
    pub head: WorktreeHead,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceRef {
    Branch {
        name: String,
        checked_out_elsewhere: bool,
    },
    Worktree(ExistingWorktree),
}

impl WorkspaceRef {
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Self::Branch { name, .. } => Some(name),
            Self::Worktree(worktree) => worktree.head.branch(),
        }
    }

    pub fn display_name(&self) -> &str {
        match self {
            Self::Branch { name, .. } => name,
            Self::Worktree(worktree) => worktree.head.branch().unwrap_or(worktree.name.as_str()),
        }
    }

    pub fn secondary_text(&self) -> Option<String> {
        match self {
            Self::Branch { .. } => None,
            Self::Worktree(worktree) => worktree
                .head
                .display_commit()
                .map(|commit| format!("detached at {commit}")),
        }
    }

    pub fn is_worktree(&self) -> bool {
        matches!(self, Self::Worktree(_))
    }

    pub fn is_disabled(&self) -> bool {
        matches!(
            self,
            Self::Branch {
                checked_out_elsewhere: true,
                ..
            }
        )
    }

    pub fn matches_query(&self, normalized_query: &str) -> bool {
        let search_text = self.search_text();
        normalized_query
            .split_whitespace()
            .all(|token| search_text.contains(token))
    }

    fn search_text(&self) -> String {
        match self {
            Self::Branch { name, .. } => name.to_ascii_lowercase(),
            Self::Worktree(worktree) => {
                let mut text = format!(
                    "{} {}",
                    worktree.name.to_ascii_lowercase(),
                    worktree.path.to_string_lossy().to_ascii_lowercase()
                );
                if let Some(branch) = worktree.head.branch() {
                    text.push(' ');
                    text.push_str(&branch.to_ascii_lowercase());
                }
                if let Some(commit) = worktree.head.commit() {
                    text.push(' ');
                    text.push_str(&commit.to_ascii_lowercase());
                }
                text
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchSnapshot {
    pub repository: PathBuf,
    pub current: Option<String>,
    pub detached_head: Option<String>,
    pub default_branch: Option<String>,
    pub branches: Vec<BranchEntry>,
    pub remote_branches: Vec<String>,
    /// Working-tree changes against `HEAD`, including untracked text files,
    /// cached with the branch snapshot so environment UI never shells out from
    /// a render path.
    pub additions: u64,
    pub deletions: u64,
}

impl BranchSnapshot {
    pub fn display_branch(&self) -> Option<&str> {
        self.current.as_deref().or(self.detached_head.as_deref())
    }
}

/// Inspect local branches and which worktree, if any, currently owns each.
/// `Ok(None)` means `cwd` is not inside a Git repository.
pub fn inspect(cwd: &Path) -> anyhow::Result<Option<BranchSnapshot>> {
    let repository_output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .context("failed to execute git")?;
    if !repository_output.status.success() {
        return Ok(None);
    }
    let repository = PathBuf::from(
        String::from_utf8_lossy(&repository_output.stdout)
            .trim()
            .to_owned(),
    );
    let repository = fs::canonicalize(&repository).unwrap_or(repository);

    let current =
        optional_stdout(cwd, &["branch", "--show-current"])?.filter(|branch| !branch.is_empty());
    let detached_head = if current.is_none() {
        Some(git_stdout(cwd, &["rev-parse", "--short", "HEAD"])?).filter(|head| !head.is_empty())
    } else {
        None
    };

    // `%(worktreepath)` is empty for an available branch and points at the
    // owning checkout otherwise. A NUL separates it from the branch name so
    // paths containing spaces need no shell-style decoding.
    let refs = git_stdout(
        cwd,
        &[
            "for-each-ref",
            "--format=%(refname:short)%00%(worktreepath)",
            "refs/heads",
        ],
    )?;
    let mut branches = refs
        .lines()
        .filter_map(|line| {
            let (name, worktree_path) = line.split_once('\0')?;
            if name.is_empty() {
                return None;
            }
            let checked_out_elsewhere = if worktree_path.is_empty() {
                false
            } else {
                let worktree_path = PathBuf::from(worktree_path);
                let worktree_path = fs::canonicalize(&worktree_path).unwrap_or(worktree_path);
                worktree_path != repository
            };
            Some(BranchEntry {
                name: name.to_owned(),
                checked_out_elsewhere,
            })
        })
        .collect::<Vec<_>>();

    branches.sort_by(|left, right| {
        let left_current = current.as_deref() == Some(left.name.as_str());
        let right_current = current.as_deref() == Some(right.name.as_str());
        right_current
            .cmp(&left_current)
            .then_with(|| left.name.cmp(&right.name))
    });

    let remote_default = optional_stdout(
        cwd,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )?;
    let default_branch = remote_default
        .as_deref()
        .and_then(|branch| branch.strip_prefix("origin/"))
        .filter(|branch| branches.iter().any(|entry| entry.name == *branch))
        .map(str::to_owned)
        .or_else(|| current.clone());
    let mut remote_branches = git_stdout(
        cwd,
        &["for-each-ref", "--format=%(refname:short)", "refs/remotes"],
    )?
    .lines()
    .filter(|branch| !branch.is_empty() && !branch.ends_with("/HEAD"))
    .map(str::to_owned)
    .collect::<Vec<_>>();
    remote_branches.sort();
    let (additions, deletions) = worktree_line_counts(&repository);

    Ok(Some(BranchSnapshot {
        repository,
        current,
        detached_head,
        default_branch,
        branches,
        remote_branches,
        additions,
        deletions,
    }))
}

/// Enumerate every valid worktree registered for the repository containing
/// `project_path`. The subprocess and path walks belong on a background
/// executor; this function intentionally does not inspect dirty state.
pub fn discover_worktrees(project_path: &Path) -> anyhow::Result<Option<WorktreeSnapshot>> {
    let repository_output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(project_path)
        .output()
        .context("failed to execute git")?;
    if !repository_output.status.success() {
        return Ok(None);
    }
    let repository = canonicalize_or_original(PathBuf::from(
        String::from_utf8_lossy(&repository_output.stdout)
            .trim()
            .to_owned(),
    ));
    let project_path = fs::canonicalize(project_path)
        .with_context(|| format!("could not resolve project {}", project_path.display()))?;
    let project_relative = project_path
        .strip_prefix(&repository)
        .context("project is outside its Git repository root")?
        .to_owned();
    let output = git_stdout(project_path.as_path(), &["worktree", "list", "--porcelain"])?;
    let mut worktrees = parse_worktree_list(&output)
        .into_iter()
        .filter(|record| !record.prunable)
        .filter_map(|record| {
            let root = fs::canonicalize(record.root).ok()?;
            if !root.is_dir() {
                return None;
            }
            if !root.join(&project_relative).is_dir() {
                return None;
            }
            let head = match (record.branch, record.head) {
                (Some(branch), _) if !branch.is_empty() => WorktreeHead::Branch(branch),
                (_, Some(commit)) if !commit.is_empty() => WorktreeHead::Detached { commit },
                _ => return None,
            };
            Some(WorktreeEntry {
                root,
                head,
                locked: record.locked,
            })
        })
        .collect::<Vec<_>>();
    worktrees.sort_by(|left, right| left.root.cmp(&right.root));
    worktrees.dedup_by(|left, right| left.root == right.root);
    Ok(Some(WorktreeSnapshot {
        repository,
        project_path,
        project_relative,
        worktrees,
    }))
}

/// Re-read worktrees immediately before an existing-worktree selection is applied. The
/// returned metadata is fresh, so a branch that moved to detached HEAD does
/// not leave stale display data in the draft.
pub fn validate_existing_worktree(
    project_path: &Path,
    selected_path: &Path,
) -> anyhow::Result<Option<ExistingWorktree>> {
    let Some(snapshot) = discover_worktrees(project_path)? else {
        return Ok(None);
    };
    let selected_path = canonicalize_or_original(selected_path.to_owned());
    Ok(snapshot
        .worktrees
        .iter()
        .find(|worktree| snapshot.project_path(worktree) == selected_path)
        .filter(|worktree| snapshot.project_path(worktree).is_dir())
        .map(|worktree| ExistingWorktree {
            path: snapshot.project_path(worktree),
            name: worktree_name(&worktree.root),
            head: worktree.head.clone(),
        }))
}

pub fn short_commit(commit: &str) -> String {
    commit.chars().take(7).collect()
}

fn canonicalize_or_original(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn worktree_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

#[derive(Default)]
struct WorktreeRecord {
    root: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    locked: bool,
    prunable: bool,
}

fn parse_worktree_list(output: &str) -> Vec<WorktreeRecord> {
    let mut records = Vec::new();
    let mut current = WorktreeRecord::default();
    let mut has_record = false;
    let finish =
        |records: &mut Vec<WorktreeRecord>, current: &mut WorktreeRecord, has_record: &mut bool| {
            if *has_record && !current.root.as_os_str().is_empty() {
                records.push(std::mem::take(current));
            }
            *has_record = false;
        };
    for line in output.lines() {
        if line.is_empty() {
            finish(&mut records, &mut current, &mut has_record);
            continue;
        }
        if let Some(root) = line.strip_prefix("worktree ") {
            finish(&mut records, &mut current, &mut has_record);
            current.root = PathBuf::from(root);
            has_record = true;
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            current.head = Some(head.to_owned());
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            current.branch = Some(branch.to_owned());
        } else if line == "locked" || line.starts_with("locked ") {
            current.locked = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            current.prunable = true;
        }
    }
    finish(&mut records, &mut current, &mut has_record);
    records
}

/// Build the deterministic ref-picker model from public Git metadata.
pub fn workspace_ref_entries(
    snapshot: &BranchSnapshot,
    worktrees: Option<&WorktreeSnapshot>,
    excluded_worktree_paths: &[&Path],
    normalized_query: &str,
) -> Vec<WorkspaceRef> {
    let existing = worktrees
        .map(|worktrees| worktrees.existing_worktrees(excluded_worktree_paths))
        .unwrap_or_default();
    let worktree_by_branch = existing
        .iter()
        .filter_map(|worktree| {
            worktree
                .head
                .branch()
                .map(|branch| (branch, worktree.clone()))
        })
        .collect::<HashMap<_, _>>();
    let worktree_branch_names = worktree_by_branch
        .keys()
        .map(|branch| (*branch).to_owned())
        .collect::<HashSet<_>>();
    let current = snapshot.current.as_deref();
    let default = snapshot
        .default_branch
        .as_deref()
        .filter(|branch| Some(*branch) != current);
    let mut refs = Vec::new();

    if let Some(current) = current {
        refs.push(pinned_ref(current, &worktree_by_branch));
    } else if let Some(detached_head) = snapshot.detached_head.as_deref() {
        refs.push(WorkspaceRef::Branch {
            name: short_commit(detached_head),
            checked_out_elsewhere: false,
        });
    }
    if let Some(default) = default {
        refs.push(pinned_ref(default, &worktree_by_branch));
    }

    let pinned_names = refs
        .iter()
        .filter_map(WorkspaceRef::branch_name)
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    let mut worktree_refs = existing
        .into_iter()
        .filter(|worktree| {
            worktree
                .head
                .branch()
                .is_none_or(|branch| !pinned_names.contains(branch))
        })
        .map(WorkspaceRef::Worktree)
        .collect::<Vec<_>>();
    worktree_refs.sort_by(|left, right| {
        left.display_name()
            .cmp(right.display_name())
            .then_with(|| left.secondary_text().cmp(&right.secondary_text()))
    });
    refs.extend(worktree_refs);

    let mut local = snapshot
        .branches
        .iter()
        .filter(|branch| worktrees.is_none() || !branch.checked_out_elsewhere)
        .filter(|branch| !pinned_names.contains(branch.name.as_str()))
        .filter(|branch| !worktree_branch_names.contains(branch.name.as_str()))
        .map(|branch| WorkspaceRef::Branch {
            name: branch.name.clone(),
            checked_out_elsewhere: branch.checked_out_elsewhere,
        })
        .collect::<Vec<_>>();
    local.sort_by(|left, right| left.display_name().cmp(right.display_name()));
    refs.extend(local);

    let mut remote = snapshot
        .remote_branches
        .iter()
        .map(|name| WorkspaceRef::Branch {
            name: name.clone(),
            checked_out_elsewhere: false,
        })
        .collect::<Vec<_>>();
    remote.sort_by(|left, right| left.display_name().cmp(right.display_name()));
    refs.extend(remote);

    refs.into_iter()
        .filter(|entry| entry.matches_query(normalized_query))
        .collect()
}

fn pinned_ref(branch: &str, worktree_by_branch: &HashMap<&str, ExistingWorktree>) -> WorkspaceRef {
    worktree_by_branch
        .get(branch)
        .map(|worktree| WorkspaceRef::Worktree(worktree.clone()))
        .unwrap_or_else(|| WorkspaceRef::Branch {
            name: branch.to_owned(),
            checked_out_elsewhere: false,
        })
}

fn worktree_line_counts(cwd: &Path) -> (u64, u64) {
    let tracked = Command::new("git")
        .args(["diff", "--numstat", "HEAD", "--"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or((0, 0), |output| numstat_line_counts(&output.stdout));
    (
        tracked.0.saturating_add(untracked_line_additions(cwd)),
        tracked.1,
    )
}

fn numstat_line_counts(output: &[u8]) -> (u64, u64) {
    String::from_utf8_lossy(output)
        .lines()
        .fold((0, 0), |(additions, deletions), line| {
            let mut fields = line.splitn(3, '\t');
            let added = fields.next().and_then(|value| value.parse::<u64>().ok());
            let deleted = fields.next().and_then(|value| value.parse::<u64>().ok());
            match (added, deleted) {
                (Some(added), Some(deleted)) => (
                    additions.saturating_add(added),
                    deletions.saturating_add(deleted),
                ),
                // Binary files are reported as `-\t-` and have no line count.
                _ => (additions, deletions),
            }
        })
}

/// `git diff --numstat` deliberately omits untracked files. Count their text
/// lines as additions so the compact status agrees with the Uncommitted review
/// source that opens when it is clicked. Bounds keep generated trees from
/// turning a background metadata refresh into unbounded work.
fn untracked_line_additions(repository: &Path) -> u64 {
    let Ok(output) = Command::new("git")
        .args([
            "ls-files",
            "--others",
            "--exclude-standard",
            "--full-name",
            "-z",
            "--",
        ])
        .current_dir(repository)
        .output()
    else {
        return 0;
    };
    if !output.status.success() {
        return 0;
    }

    let mut remaining_bytes = MAX_UNTRACKED_TOTAL_BYTES;
    let mut additions = 0_u64;
    for path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .take(MAX_UNTRACKED_FILES)
    {
        if remaining_bytes == 0 {
            break;
        }
        let path = repository.join(path_from_git_bytes(path));
        let (lines, bytes_read) =
            untracked_text_line_count(&path, remaining_bytes.min(MAX_UNTRACKED_FILE_BYTES));
        additions = additions.saturating_add(lines);
        remaining_bytes = remaining_bytes.saturating_sub(bytes_read);
    }
    additions
}

fn untracked_text_line_count(path: &Path, maximum_bytes: u64) -> (u64, u64) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if metadata.file_type().is_symlink() {
        return (1, 0);
    }
    if !metadata.is_file() || metadata.len() > maximum_bytes {
        return (0, 0);
    }

    let Ok(file) = fs::File::open(path) else {
        return (0, 0);
    };
    let mut content = Vec::with_capacity(metadata.len() as usize);
    if file
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut content)
        .is_err()
    {
        return (0, 0);
    }
    let bytes_read = content.len() as u64;
    if bytes_read > maximum_bytes || content[..content.len().min(BINARY_PROBE_BYTES)].contains(&0) {
        return (0, bytes_read);
    }
    if content.is_empty() {
        return (0, 0);
    }
    let newline_count = content.iter().filter(|byte| **byte == b'\n').count() as u64;
    let trailing_line = u64::from(content.last() != Some(&b'\n'));
    (newline_count.saturating_add(trailing_line), bytes_read)
}

#[cfg(unix)]
fn path_from_git_bytes(path: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(path.to_vec()))
}

#[cfg(not(unix))]
fn path_from_git_bytes(path: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(path).into_owned())
}

pub fn checkout(cwd: &Path, branch: &str) -> anyhow::Result<BranchSnapshot> {
    let output = Command::new("git")
        .args(["switch", "--"])
        .arg(branch)
        .current_dir(cwd)
        .output()
        .context("failed to execute git switch")?;
    if !output.status.success() {
        bail!("{}", command_error(&output));
    }
    inspect(cwd)?.ok_or_else(|| anyhow!("the workspace is no longer a Git repository"))
}

pub fn create_and_checkout(cwd: &Path, branch: &str) -> anyhow::Result<BranchSnapshot> {
    let branch = branch.trim();
    if branch.is_empty() {
        bail!("enter a branch name");
    }
    let validation = Command::new("git")
        .args(["check-ref-format", "--branch"])
        .arg(branch)
        .current_dir(cwd)
        .output()
        .context("failed to validate the branch name")?;
    if !validation.status.success() {
        bail!("{}", command_error(&validation));
    }
    let output = Command::new("git")
        .args(["switch", "-c"])
        .arg(branch)
        .current_dir(cwd)
        .output()
        .context("failed to execute git switch")?;
    if !output.status.success() {
        bail!("{}", command_error(&output));
    }
    inspect(cwd)?.ok_or_else(|| anyhow!("the workspace is no longer a Git repository"))
}

fn git_stdout(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("failed to execute git")?;
    if !output.status.success() {
        bail!("{}", command_error(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn optional_stdout(cwd: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("failed to execute git")?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    bail!("{}", command_error(&output))
}

fn command_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        stderr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", command_error(&output));
    }

    fn repository() -> PathBuf {
        let root = std::env::temp_dir().join(format!("waku-branch-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        run_git(&root, &["init", "-b", "main"]);
        fs::write(root.join("README.md"), "main\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(
            &root,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "-m",
                "initial",
            ],
        );
        run_git(&root, &["branch", "feature"]);
        root
    }

    #[test]
    fn discovers_current_and_worktree_owned_branches() {
        let repository = repository();
        let other = repository.with_extension("other-worktree");
        run_git(
            &repository,
            &["worktree", "add", "-b", "occupied", other.to_str().unwrap()],
        );

        let snapshot = inspect(&repository).unwrap().unwrap();
        assert_eq!(snapshot.current.as_deref(), Some("main"));
        assert_eq!(snapshot.branches[0].name, "main");
        assert!(
            snapshot
                .branches
                .iter()
                .find(|branch| branch.name == "occupied")
                .unwrap()
                .checked_out_elsewhere
        );
        assert!(
            !snapshot
                .branches
                .iter()
                .find(|branch| branch.name == "feature")
                .unwrap()
                .checked_out_elsewhere
        );
    }

    #[test]
    fn switches_and_creates_branches() {
        let repository = repository();
        let switched = checkout(&repository, "feature").unwrap();
        assert_eq!(switched.current.as_deref(), Some("feature"));

        let created = create_and_checkout(&repository, "topic/new-picker").unwrap();
        assert_eq!(created.current.as_deref(), Some("topic/new-picker"));
        assert!(
            created
                .branches
                .iter()
                .any(|branch| branch.name == "topic/new-picker")
        );
    }

    #[test]
    fn counts_tracked_worktree_changes() {
        let repository = repository();
        fs::write(repository.join("README.md"), "main\nsecond\n").unwrap();

        let snapshot = inspect(&repository).unwrap().unwrap();
        assert_eq!((snapshot.additions, snapshot.deletions), (1, 0));
    }

    #[test]
    fn counts_tracked_and_untracked_changes_in_a_linked_worktree() {
        let repository = repository();
        let worktree = repository.with_extension("linked-worktree");
        run_git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "linked-worktree",
                worktree.to_str().unwrap(),
            ],
        );
        fs::write(worktree.join("README.md"), "main\nsecond\n").unwrap();
        fs::write(worktree.join("new file.txt"), "first\nsecond").unwrap();
        fs::write(worktree.join("binary.dat"), b"binary\0content\n").unwrap();

        let snapshot = inspect(&worktree).unwrap().unwrap();
        assert_eq!((snapshot.additions, snapshot.deletions), (3, 0));
    }

    #[test]
    fn discovers_branch_detached_locked_and_skips_stale_worktrees() {
        let repository = repository();
        let branch_path = repository.with_extension("branch-worktree");
        let detached_path = repository.with_extension("detached-worktree");
        let stale_path = repository.with_extension("stale-worktree");
        run_git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "linked",
                branch_path.to_str().unwrap(),
            ],
        );
        run_git(
            &repository,
            &[
                "worktree",
                "add",
                "--detach",
                detached_path.to_str().unwrap(),
            ],
        );
        run_git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "stale",
                stale_path.to_str().unwrap(),
            ],
        );
        run_git(
            &repository,
            &[
                "worktree",
                "lock",
                "--reason",
                "test lock",
                branch_path.to_str().unwrap(),
            ],
        );
        fs::remove_dir_all(&stale_path).unwrap();

        let snapshot = discover_worktrees(&repository).unwrap().unwrap();
        let branch = snapshot
            .worktrees
            .iter()
            .find(|worktree| worktree.root == fs::canonicalize(&branch_path).unwrap())
            .unwrap();
        assert_eq!(branch.head, WorktreeHead::Branch("linked".into()));
        assert!(branch.locked);

        let detached = snapshot
            .worktrees
            .iter()
            .find(|worktree| worktree.root == fs::canonicalize(&detached_path).unwrap())
            .unwrap();
        let WorktreeHead::Detached { commit } = &detached.head else {
            panic!("expected detached worktree");
        };
        assert_eq!(commit.len(), 40);
        assert!(
            snapshot
                .worktrees
                .iter()
                .all(|worktree| worktree.root != stale_path)
        );
    }

    #[test]
    fn maps_a_nested_project_to_the_same_subdirectory_in_each_worktree() {
        let repository = repository();
        let project = repository.join("packages/app");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();
        run_git(&repository, &["add", "."]);
        run_git(
            &repository,
            &[
                "-c",
                "user.name=Waku Tests",
                "-c",
                "user.email=waku@example.com",
                "commit",
                "-m",
                "nested project",
            ],
        );
        let linked_root = repository.with_extension("nested-worktree");
        run_git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "nested",
                linked_root.to_str().unwrap(),
            ],
        );

        let snapshot = discover_worktrees(&project).unwrap().unwrap();
        assert_eq!(snapshot.project_relative, PathBuf::from("packages/app"));
        let linked = snapshot
            .worktrees
            .iter()
            .find(|worktree| worktree.head == WorktreeHead::Branch("nested".into()))
            .unwrap();
        assert_eq!(
            snapshot.project_path(linked),
            fs::canonicalize(linked_root).unwrap().join("packages/app")
        );
    }

    #[test]
    fn workspace_refs_are_grouped_sorted_and_search_detached_metadata() {
        let snapshot = BranchSnapshot {
            repository: PathBuf::from("/repo"),
            current: Some("feature".into()),
            detached_head: None,
            default_branch: Some("main".into()),
            branches: vec![
                BranchEntry {
                    name: "zulu".into(),
                    checked_out_elsewhere: false,
                },
                BranchEntry {
                    name: "apple".into(),
                    checked_out_elsewhere: false,
                },
                BranchEntry {
                    name: "stale".into(),
                    checked_out_elsewhere: true,
                },
                BranchEntry {
                    name: "feature".into(),
                    checked_out_elsewhere: false,
                },
                BranchEntry {
                    name: "main".into(),
                    checked_out_elsewhere: false,
                },
            ],
            remote_branches: vec!["origin/zulu".into(), "origin/main".into()],
            additions: 0,
            deletions: 0,
        };
        let worktrees = WorktreeSnapshot {
            repository: PathBuf::from("/repo"),
            project_path: PathBuf::from("/repo"),
            project_relative: PathBuf::new(),
            worktrees: vec![
                WorktreeEntry {
                    root: PathBuf::from("/repo"),
                    head: WorktreeHead::Branch("feature".into()),
                    locked: false,
                },
                WorktreeEntry {
                    root: PathBuf::from("/worktrees/zulu"),
                    head: WorktreeHead::Branch("shared/zulu".into()),
                    locked: true,
                },
                WorktreeEntry {
                    root: PathBuf::from("/worktrees/apple"),
                    head: WorktreeHead::Detached {
                        commit: "deadbeef1234567890abcdef1234567890abcdef".into(),
                    },
                    locked: false,
                },
            ],
        };

        let refs = workspace_ref_entries(&snapshot, Some(&worktrees), &[Path::new("/repo")], "");
        assert_eq!(
            refs.iter()
                .map(WorkspaceRef::display_name)
                .collect::<Vec<_>>(),
            vec![
                "feature",
                "main",
                "apple",
                "shared/zulu",
                "apple",
                "zulu",
                "origin/main",
                "origin/zulu"
            ]
        );
        assert!(refs.iter().any(|entry| {
            matches!(
                entry,
                WorkspaceRef::Worktree(worktree)
                    if worktree.name == "apple"
                        && worktree.head.commit() == Some("deadbeef1234567890abcdef1234567890abcdef")
            )
        }));
        let by_commit = workspace_ref_entries(
            &snapshot,
            Some(&worktrees),
            &[Path::new("/repo")],
            "deadbeef",
        );
        assert_eq!(by_commit.len(), 1);
        assert!(by_commit[0].is_worktree());
    }
}
