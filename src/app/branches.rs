use super::*;

enum BranchOperation {
    Checkout(String),
    Create(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceSelectionPlan {
    BindShared(crate::git_branch::SharedWorktree),
    SetNewWorktreeBase(String),
    CheckoutInCurrent(String),
}

pub(super) fn plan_workspace_selection(
    workspace: &SessionWorkspace,
    selection: &WorkspaceRef,
) -> WorkspaceSelectionPlan {
    match selection {
        WorkspaceRef::Shared(worktree) => WorkspaceSelectionPlan::BindShared(worktree.clone()),
        WorkspaceRef::Branch { name, .. } => {
            if matches!(workspace, SessionWorkspace::NewWorktree { .. }) {
                WorkspaceSelectionPlan::SetNewWorktreeBase(name.clone())
            } else {
                WorkspaceSelectionPlan::CheckoutInCurrent(name.clone())
            }
        }
    }
}

impl Waku {
    pub(super) fn sync_branch_picker_rows(&self, rows: &[WorkspaceRef]) {
        let mut cached = self.branch_picker_row_cache.borrow_mut();
        if cached.as_slice() == rows {
            return;
        }
        *cached = rows.to_vec();
        self.branch_picker_list_state
            .reset_with_uniform_height(rows.len(), px(BRANCH_PICKER_ROW_HEIGHT));
    }

    /// Discover all repository worktrees off the render path. A project path
    /// is the cache key because a nested project must map to the same relative
    /// directory in every linked worktree.
    pub(super) fn worktree_snapshot_for_project(
        &mut self,
        project_path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Option<WorktreeSnapshot> {
        let project_path = project_path.to_path_buf();
        match self.worktree_snapshots.read(&project_path) {
            Query::Ready(result) => match result.as_ref() {
                Ok(Some(snapshot)) => Some(snapshot.clone()),
                Ok(None) | Err(_) => None,
            },
            Query::Pending => None,
            Query::Missing(token) => {
                let fetch_path = project_path.clone();
                cx.spawn(async move |waku, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            crate::git_branch::discover_worktrees(&fetch_path)
                                .map_err(|error| error.to_string())
                        })
                        .await;
                    let _ = waku.update(cx, |waku, cx| {
                        if waku.worktree_snapshots.fulfill(token, result) {
                            cx.notify();
                        }
                    });
                })
                .detach();
                None
            }
        }
    }

    /// Cache whether a pinned workspace still exists. Render only reads this
    /// result and never probes the filesystem directly.
    pub(super) fn workspace_is_available(
        &mut self,
        workspace_path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        let workspace_path = workspace_path.to_path_buf();
        match self.workspace_availability.read(&workspace_path) {
            Query::Ready(available) => Some(*available),
            Query::Pending => None,
            Query::Missing(token) => {
                let fetch_path = workspace_path.clone();
                cx.spawn(async move |waku, cx| {
                    let available = cx
                        .background_executor()
                        .spawn(async move { fetch_path.is_dir() })
                        .await;
                    let _ = waku.update(cx, |waku, cx| {
                        if waku.workspace_availability.fulfill(token, available) {
                            cx.notify();
                        }
                    });
                })
                .detach();
                None
            }
        }
    }

    /// Read the selected workspace's cached Git branches, starting one
    /// background fetch on a miss. The previous selected-path snapshot remains
    /// drawable while an invalidation is being refreshed.
    pub(super) fn branch_snapshot_for_workspace(
        &mut self,
        workspace_path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Option<BranchSnapshot> {
        let workspace_path = workspace_path.to_path_buf();
        let fallback = self
            .visible_branch_snapshot
            .as_ref()
            .filter(|(path, _)| path == &workspace_path)
            .map(|(_, snapshot)| snapshot.clone());

        match self.branch_snapshots.read(&workspace_path) {
            Query::Ready(result) => match result.as_ref() {
                Ok(Some(snapshot)) => {
                    let snapshot = snapshot.clone();
                    self.visible_branch_snapshot = Some((workspace_path, snapshot.clone()));
                    Some(snapshot)
                }
                Ok(None) => {
                    if self
                        .visible_branch_snapshot
                        .as_ref()
                        .is_some_and(|(path, _)| path == &workspace_path)
                    {
                        self.visible_branch_snapshot = None;
                    }
                    None
                }
                Err(_) => fallback,
            },
            Query::Pending => fallback,
            Query::Missing(token) => {
                let fetch_path = workspace_path.clone();
                cx.spawn(async move |waku, cx| {
                    let result = cx
                        .background_executor()
                        .spawn({
                            let fetch_path = fetch_path.clone();
                            async move {
                                crate::git_branch::inspect(&fetch_path)
                                    .map_err(|error| error.to_string())
                            }
                        })
                        .await;
                    let _ = waku.update(cx, |waku, cx| {
                        if !waku.branch_snapshots.fulfill(token, result.clone()) {
                            return;
                        }
                        let selected = waku
                            .selected_workspace_path()
                            .is_some_and(|path| path == fetch_path);
                        if selected {
                            match result {
                                Ok(Some(snapshot)) => {
                                    let mut persisted_branch_changed = false;
                                    if let Some(session) = waku.selected_session_mut()
                                        && let SessionWorkspace::Worktree {
                                            branch,
                                            detached_head,
                                            ..
                                        } = &mut session.workspace
                                    {
                                        let next_branch = snapshot.current.clone();
                                        let next_detached_head = snapshot.detached_head.clone();
                                        if *branch != next_branch
                                            || *detached_head != next_detached_head
                                        {
                                            *branch = next_branch;
                                            *detached_head = next_detached_head;
                                            persisted_branch_changed = true;
                                        }
                                    }
                                    waku.visible_branch_snapshot = Some((fetch_path, snapshot));
                                    if persisted_branch_changed {
                                        waku.save();
                                    }
                                }
                                Ok(None) => waku.visible_branch_snapshot = None,
                                Err(_) => {}
                            }
                            cx.notify();
                        }
                    });
                })
                .detach();
                fallback
            }
        }
    }

    pub(super) fn refresh_selected_branch_snapshot(&mut self, cx: &mut Context<Self>) {
        let project_path = self.selected_project().map(|project| project.path.clone());
        let Some(path) = self
            .selected_workspace_path()
            .map(std::path::Path::to_path_buf)
        else {
            self.visible_branch_snapshot = None;
            if let Some(project_path) = project_path {
                self.worktree_snapshots.invalidate(&project_path);
            }
            return;
        };
        self.branch_snapshots.invalidate(&path);
        self.workspace_availability.invalidate(&path);
        if let Some(project_path) = project_path {
            self.worktree_snapshots.invalidate(&project_path);
        }
        cx.notify();
    }

    /// Apply a picker selection. Existing worktrees are validated again on
    /// the click path, while ordinary branch operations retain their existing
    /// background Git mutation.
    pub(super) fn choose_workspace_ref(
        &mut self,
        selection: WorkspaceRef,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.selected_session() else {
            return false;
        };
        if session.has_started() || session.is_busy() || self.branch_operation_pending {
            return false;
        }
        let plan = plan_workspace_selection(&session.workspace, &selection);
        match plan {
            WorkspaceSelectionPlan::BindShared(shared) => {
                let Some(project_path) =
                    self.selected_project().map(|project| project.path.clone())
                else {
                    return false;
                };
                let validated =
                    crate::git_branch::validate_shared_worktree(&project_path, &shared.path)
                        .ok()
                        .flatten();
                let Some(validated) = validated else {
                    self.refresh_selected_branch_snapshot(cx);
                    self.show_toast(tr!("errors.stale_worktree_selection"));
                    cx.notify();
                    return true;
                };
                let workspace = SessionWorkspace::Worktree {
                    path: validated.path,
                    branch: validated.head.branch().map(str::to_owned),
                    detached_head: validated.head.commit().map(str::to_owned),
                };
                self.select_workspace(workspace, cx);
                true
            }
            WorkspaceSelectionPlan::SetNewWorktreeBase(branch) => {
                let changed = self.selected_session_mut().is_some_and(|session| {
                    let SessionWorkspace::NewWorktree { base_branch } = &mut session.workspace
                    else {
                        return false;
                    };
                    if base_branch.as_deref() == Some(branch.as_str()) {
                        return false;
                    }
                    *base_branch = Some(branch);
                    true
                });
                if changed {
                    self.save();
                    cx.notify();
                }
                true
            }
            WorkspaceSelectionPlan::CheckoutInCurrent(branch) => {
                let selected_path = self
                    .selected_workspace_path()
                    .map(std::path::Path::to_path_buf);
                let already_current = selected_path.as_ref().is_some_and(|path| {
                    self.visible_branch_snapshot
                        .as_ref()
                        .filter(|(snapshot_path, _)| snapshot_path == path)
                        .is_some_and(|(_, snapshot)| {
                            snapshot.current.as_deref() == Some(branch.as_str())
                                || snapshot.detached_head.as_deref().is_some_and(|head| {
                                    crate::git_branch::short_commit(head) == branch
                                })
                        })
                });
                if already_current {
                    return true;
                }
                let Some(project_path) =
                    self.selected_project().map(|project| project.path.clone())
                else {
                    return false;
                };
                if !session.workspace.is_local() {
                    self.select_workspace(SessionWorkspace::Local, cx);
                }
                self.start_branch_operation(project_path, BranchOperation::Checkout(branch), cx);
                true
            }
        }
    }

    pub(super) fn begin_branch_creation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.branch_operation_pending
            || self.selected_session().is_none_or(|session| {
                session.has_started()
                    || session.is_busy()
                    || matches!(session.workspace, SessionWorkspace::NewWorktree { .. })
            })
        {
            return;
        }
        self.branch_picker_mode = BranchPickerMode::Create;
        self.branch_picker_highlight = None;
        self.branch_create_input
            .update(cx, |input, cx| input.clear(cx));
        let focus = self.branch_create_input.read(cx).focus_handle(cx);
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| window.focus(&focus, cx));
        });
        cx.notify();
    }

    pub(super) fn confirm_branch_creation(&mut self, cx: &mut Context<Self>) -> bool {
        if self.branch_picker_mode != BranchPickerMode::Create || self.branch_operation_pending {
            return false;
        }
        let branch = self
            .branch_create_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        if branch.is_empty() {
            return false;
        }
        let Some(path) = self
            .selected_workspace_path()
            .map(std::path::Path::to_path_buf)
        else {
            return false;
        };
        self.start_branch_operation(path, BranchOperation::Create(branch), cx);
        true
    }

    pub(super) fn move_branch_picker_highlight(
        &mut self,
        key: &str,
        actions: &[BranchPickerAction],
        cx: &mut Context<Self>,
    ) {
        if self.branch_picker_mode != BranchPickerMode::Browse || actions.is_empty() {
            return;
        }
        let current = self
            .branch_picker_highlight
            .filter(|index| *index < actions.len());
        let next = match (key, current) {
            ("up", Some(0)) => actions.len() - 1,
            ("up", Some(index)) => index - 1,
            ("up", None) => actions.len() - 1,
            (_, Some(index)) => (index + 1) % actions.len(),
            (_, None) => 0,
        };
        self.branch_picker_highlight = Some(next);
        if let Some(BranchPickerAction::Select(selection)) = actions.get(next)
            && let Some(row) = self
                .branch_picker_row_cache
                .borrow()
                .iter()
                .position(|entry| entry == selection)
        {
            self.branch_picker_list_state.scroll_to_reveal_item(row);
        }
        cx.notify();
    }

    /// Apply the keyboard-selected action, returning whether the caller should
    /// dismiss the picker after releasing its `Waku` update lease.
    pub(super) fn confirm_branch_picker_action(
        &mut self,
        actions: &[BranchPickerAction],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.branch_picker_mode == BranchPickerMode::Create {
            return self.confirm_branch_creation(cx);
        }
        let Some(action) = actions.get(self.branch_picker_highlight.unwrap_or(0)) else {
            return false;
        };
        match action {
            BranchPickerAction::Select(selection) => {
                self.choose_workspace_ref(selection.clone(), cx)
            }
            BranchPickerAction::Create => {
                self.begin_branch_creation(window, cx);
                false
            }
        }
    }

    fn start_branch_operation(
        &mut self,
        path: PathBuf,
        operation: BranchOperation,
        cx: &mut Context<Self>,
    ) {
        if self.branch_operation_pending {
            return;
        }
        self.branch_operation_pending = true;
        cx.notify();
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
                        match operation {
                            BranchOperation::Checkout(branch) => {
                                crate::git_branch::checkout(&path, &branch)
                            }
                            BranchOperation::Create(branch) => {
                                crate::git_branch::create_and_checkout(&path, &branch)
                            }
                        }
                    }
                })
                .await;
            let _ = waku.update(cx, |waku, cx| {
                waku.branch_operation_pending = false;
                match result {
                    Ok(snapshot) => {
                        let current = snapshot.current.clone();
                        waku.visible_branch_snapshot = Some((path.clone(), snapshot));
                        waku.branch_snapshots.invalidate(&path);
                        let selected_path = waku
                            .selected_workspace_path()
                            .map(std::path::Path::to_path_buf);
                        if selected_path.as_ref() == Some(&path) {
                            if let Some(current) = current
                                && let Some(session) = waku.selected_session_mut()
                                && let SessionWorkspace::Worktree {
                                    branch,
                                    detached_head,
                                    ..
                                } = &mut session.workspace
                            {
                                *branch = Some(current);
                                *detached_head = None;
                            }
                            waku.invalidate_workspace_queries(cx);
                            waku.reload_clean_right_panel_file_editors(cx);
                            waku.save();
                        }
                    }
                    Err(error) => {
                        waku.show_toast(tr!("errors.change_branch", error = error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
