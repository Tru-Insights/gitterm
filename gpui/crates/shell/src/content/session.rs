//! Viewer cover/return bridge. Session entities and selection survive the visit.
use crate::{FileOpen, Pane, PaneKind, Shell, WorkspaceId, data::SessionId, pane};
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    dock::PanelId,
};
use std::collections::HashMap;

#[derive(Clone)]
pub(super) struct ViewerVisit {
    pub(super) pane: String,
    session: Option<SessionId>,
}

impl Shell {
    pub fn open_file_viewer(
        &mut self,
        event: &FileOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.local {
            return;
        }
        let Some(state) = self
            .workspaces
            .iter()
            .find(|w| w.workspace.id == event.workspace)
        else {
            return;
        };
        let session = self
            .content
            .read(cx)
            .visits
            .get(&event.workspace)
            .map(|visit| visit.session)
            .unwrap_or(state.active_session);
        if event.from_files {
            self.open_viewer_file(event, window, cx);
        } else {
            self.open_review(event, window, cx);
        }
        let state = self
            .workspaces
            .iter()
            .find(|w| w.workspace.id == event.workspace)
            .unwrap();
        let id = if event.from_files {
            crate::content_pane_id(event.workspace, &event.path)
        } else {
            format!("workspace-{}-review", state.workspace.layout_key)
        };
        if state.panes.contains_key(&id) {
            self.content.update(cx, |content, cx| {
                content
                    .visits
                    .insert(event.workspace, ViewerVisit { pane: id, session });
                cx.notify();
            });
            cx.notify();
        }
    }

    fn open_viewer_file(&mut self, event: &FileOpen, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self
            .workspaces
            .iter_mut()
            .find(|w| w.workspace.id == event.workspace)
        else {
            return;
        };
        // Work roots may be outside the home checkout; validate against the
        // selected root, then let core enforce canonical containment/symlinks.
        let root = self
            .owner
            .read(cx)
            .iced_workspace(event.workspace)
            .and_then(|saved| {
                saved
                    .repos
                    .iter()
                    .map(std::path::PathBuf::from)
                    .chain(std::iter::once(
                        std::path::PathBuf::from(&saved.dir).join("docs"),
                    ))
                    .filter(|root| event.path.starts_with(root))
                    .max_by_key(|root| root.components().count())
            })
            .or_else(|| self.data.terminal_cwd(event.workspace, cx));
        let Some(root) = root else {
            return;
        };
        let Ok(relative) = event.path.strip_prefix(&root) else {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some(format!(
                    "Cannot preview {}: outside workspace root",
                    event.path.display()
                ));
                cx.notify();
            });
            return;
        };
        let id = crate::content_pane_id(event.workspace, &event.path);
        if !state.panes.contains_key(&id) {
            let pane = cx.new(|cx| {
                Pane::new(
                    id.clone(),
                    event
                        .path
                        .file_name()
                        .unwrap_or(event.path.as_os_str())
                        .to_string_lossy()
                        .into_owned(),
                    PaneKind::Content(self.content.clone()),
                    event.workspace,
                    self.data.clone(),
                    window,
                    cx,
                )
            });
            let dark = gpui_component::Theme::global(cx).mode.is_dark();
            self.content.update(cx, |content, cx| {
                content.open(
                    id.clone(),
                    super::window::ContentRequest {
                        root: root.clone(),
                        relative: relative.to_owned(),
                        workspace: event.workspace.0,
                        pane: pane.entity_id().as_u64(),
                        dark,
                    },
                    window,
                    cx,
                )
            });
            self.registry
                .borrow_mut()
                .insert(id.clone(), pane.downgrade());
            state.panes.insert(id.clone(), pane);
        }
        self.content
            .update(cx, |content, _| content.select(Some(id.clone())));
        self.show_workspace_pane(event.workspace, &id, window, cx);
    }

    pub fn close_file_viewer(
        &mut self,
        workspace: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visit = self
            .content
            .update(cx, |content, _| content.visits.remove(&workspace));
        let Some(visit) = visit else {
            return;
        };
        let Some(state) = self
            .workspaces
            .iter_mut()
            .find(|w| w.workspace.id == workspace)
        else {
            return;
        };
        if let Some(pane) = state.panes.get(&visit.pane).cloned() {
            state
                .dock
                .update(cx, |dock, cx| dock.remove_panel(pane, window, cx));
        }
        state.active_session = visit.session;
        if let Some(pane) = visit
            .session
            .and_then(|id| state.panes.get(&crate::session_pane_id(id)))
        {
            let panel = PanelId::from(pane.entity_id());
            state
                .dock
                .update(cx, |dock, cx| dock.select_panel(panel, window, cx));
            pane.focus_handle(cx).focus(window, cx);
        }
        self.content.update(cx, |content, cx| {
            content.select(None);
            cx.notify();
        });
        cx.notify();
    }

    pub fn file_viewer_content(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let state = self.active(cx);
        let visit = self.content.read(cx).visits.get(&state.workspace.id);
        let Some(pane) = visit.and_then(|visit| state.panes.get(&visit.pane)) else {
            return self.session_content(cx);
        };
        let workspace = state.workspace.id;
        let title = if matches!(pane.read(cx).content, pane::PaneContent::Review(_)) {
            "Review".to_owned()
        } else {
            pane.read(cx).title.clone()
        };
        div()
            .id("session-content")
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.close_file_viewer(workspace, window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .h(px(40.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px_2()
                    .child(div().flex_1().child(title))
                    .child(
                        Button::new("close-file-viewer")
                            .ghost()
                            .label("Close")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_file_viewer(workspace, window, cx)
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().child(pane.clone()))
    }
}

pub(super) type ViewerVisits = HashMap<WorkspaceId, ViewerVisit>;
