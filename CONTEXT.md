# Waku Domain Context

## Workspaces

- A `worktree` is a Git-managed checkout with its own filesystem path and
  `HEAD` state.
- An `existing worktree` is a valid worktree already registered with Git and
  available for a task to select.
- A `task workspace` is the filesystem target used by a task. It may be the
  local checkout, a planned new worktree, or an existing worktree.
- A `shared path` is a relationship in which multiple tasks reference the
  same task workspace. It is not an intrinsic property of a Git worktree, and
  Waku does not infer ownership or display task counts.
