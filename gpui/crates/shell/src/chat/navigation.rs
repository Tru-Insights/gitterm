//! Read-only chat navigation is separate from roster reveal and preview mutations.
use gitterm_core::tasks::{Delegation, DelegationChild};
use gitterm_gpui_chat::model::roster::RosterOutcome;
use std::path::Path;
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Navigation<'a> {
    Worker(&'a str),
    File(&'a Path, bool, Option<usize>),
}
pub(crate) fn navigation(outcome: &RosterOutcome) -> Option<Navigation<'_>> {
    match outcome {
        RosterOutcome::OpenWorker { id } => Some(Navigation::Worker(id)),
        RosterOutcome::OpenFile { path, review, line } => {
            Some(Navigation::File(path, *review, *line))
        }
        RosterOutcome::Open { .. } | RosterOutcome::Dismiss { .. } => None,
    }
}
pub(crate) fn worker_tab_target<T>(
    delegation: &Delegation,
    find: impl FnOnce(&str) -> Option<T>,
) -> Result<T, String> {
    match &delegation.child {
        DelegationChild::TaskSession {
            task_session_id: Some(session),
            ..
        } => find(session).ok_or_else(|| {
            "The worker's tab is closed. Open its task from the Tasks rail to resume it.".into()
        }),
        DelegationChild::TaskSession { .. } => {
            Err("The worker has not started yet (it is queued behind other tasks).".into())
        }
        _ => Err(format!(
            "delegation {} has no worker tab",
            delegation.delegation_id
        )),
    }
}
use crate::{FileOpen, Shell, pane};
use gpui::{Context, Entity, Window};
impl Shell {
    pub(crate) fn open_chat_checkout(
        &mut self,
        requested: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(focused) = self.focused_chat(window, cx) else {
            return;
        };
        let source = self
            .workspaces
            .iter()
            .flat_map(|w| w.panes.values())
            .find_map(|pane| {
                if pane.entity_id() != focused {
                    return None;
                }
                match &pane.read(cx).content {
                    pane::PaneContent::PreviewChat(view) => Some(view.clone()),
                    _ => None,
                }
            });
        let Some(source) = source else {
            return;
        };
        let current = source.read(cx).cwd();
        if let Some(path) = gitterm_core::editor::known_checkout_path(
            &[],
            current,
            requested.unwrap_or(current),
            self.local,
        ) {
            self.open_editor(
                crate::content::actions::OpenEditor::configured(path, None),
                window,
                cx,
            );
        } else {
            source.update(cx, |source, cx| {
                source.show_notice(
                    "That checkout is not available in this workspace.".into(),
                    cx,
                )
            });
        }
    }

    pub(crate) fn open_chat_file(
        &mut self,
        source: &Entity<super::ChatPane>,
        path: &std::path::Path,
        (review, line): (bool, Option<usize>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.local {
            return;
        }
        let Some(workspace) = self.workspaces.iter().find(|workspace| workspace.panes.values().any(|pane| matches!(&pane.read(cx).content, pane::PaneContent::PreviewChat(chat) if chat == source))) else { return; };
        let workspace_id = workspace.workspace.id;
        let root = source.read(cx).cwd().to_path_buf();
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let mut event = FileOpen {
            workspace: workspace_id,
            path: path.clone(),
            changed: None,
            from_files: !review,
        };
        if review {
            event.changed = self
                .owner
                .read(cx)
                .model
                .status_view(workspace_id)
                .files
                .into_iter()
                .find(|file| root.join(&file.path) == path);
            if event.changed.is_none() {
                source.update(cx, |pane, cx| {
                    pane.show_notice(
                        format!("{} has no changes in this checkout.", path.display()),
                        cx,
                    )
                });
                return;
            }
        }
        self.open_file_viewer_at_line(&event, line, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn h24_roster_reveal_and_hide_never_switch_the_host_session() {
        assert_eq!(
            navigation(&RosterOutcome::Open {
                id: "dlg:worker".into()
            }),
            None
        );
        assert_eq!(
            navigation(&RosterOutcome::Dismiss {
                id: "dlg:worker".into()
            }),
            None
        );
        assert_eq!(
            navigation(&RosterOutcome::OpenWorker {
                id: "worker".into()
            }),
            Some(Navigation::Worker("worker"))
        );
    }
    #[test]
    fn h09_h13_review_and_finding_choose_the_correct_pane_without_effects() {
        let file = RosterOutcome::OpenFile {
            path: "src/lib.rs".into(),
            review: false,
            line: None,
        };
        let review = RosterOutcome::OpenFile {
            path: "src/lib.rs".into(),
            review: true,
            line: None,
        };
        assert_eq!(
            navigation(&file),
            Some(Navigation::File(Path::new("src/lib.rs"), false, None))
        );
        assert_eq!(
            navigation(&review),
            Some(Navigation::File(Path::new("src/lib.rs"), true, None))
        );
    }
    #[test]
    fn h13a_open_worker_focuses_only_an_existing_session_and_reports_closed_or_queued() {
        let mut worker:Delegation=serde_json::from_value(serde_json::json!({
            "delegation_id":"worker","kind":"implement","parent":{"session_uid":"parent","workspace":"sample","cwd":"/private/tmp/sample"},
            "child":{"runner":"task_session","task_id":"task","task_session_id":"session"},"brief":"work","status":{"state":"running"},"created_at":"2026-11-20T00:00:00Z","updated_at":"2026-11-20T00:00:00Z"
        })).unwrap();
        assert_eq!(
            worker_tab_target(&worker, |id| {
                assert_eq!(id, "session");
                Some(7)
            }),
            Ok(7)
        );
        assert_eq!(
            worker_tab_target::<()>(&worker, |_| None).unwrap_err(),
            "The worker's tab is closed. Open its task from the Tasks rail to resume it."
        );
        if let DelegationChild::TaskSession {
            task_session_id, ..
        } = &mut worker.child
        {
            *task_session_id = None;
        }
        assert_eq!(
            worker_tab_target::<()>(&worker, |_| panic!(
                "queued worker has no session to look up"
            ))
            .unwrap_err(),
            "The worker has not started yet (it is queued behind other tasks)."
        );
    }
}
