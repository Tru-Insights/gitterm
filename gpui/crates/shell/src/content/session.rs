//! Viewer cover/return bridge. Session entities and selection survive the visit.
use crate::{FileOpen, Pane, PaneKind, Shell, WorkspaceId, data::SessionId, pane};
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    dock::PanelId,
};
use std::collections::HashMap;

pub(super) const DIFF_BACK_LABEL: &str = "Back to Terminal";
pub(super) const DIFF_HINT: &str = "j/k: navigate Esc: back";

#[derive(Clone)]
pub(super) struct ViewerVisit {
    pub(super) pane: String,
    session: Option<SessionId>,
}

impl Shell {
    pub(crate) fn open_work_review(
        &mut self,
        checkout: std::path::PathBuf,
        vs_base: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.local {
            return;
        }
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let state = self
            .workspaces
            .iter_mut()
            .find(|state| state.workspace.id == workspace)
            .unwrap();
        let session = self
            .content
            .read(cx)
            .visits
            .get(&workspace)
            .map(|visit| visit.session)
            .unwrap_or(state.active_session);
        // Keep branch review separate from the workspace's status-driven review.
        let id = format!(
            "{}-branch-review",
            crate::content_pane_id(workspace, &checkout)
        );
        let pane = state
            .panes
            .entry(id.clone())
            .or_insert_with(|| {
                let review = cx
                    .new(|cx| gitterm_gpui_review::ReviewView::new(Default::default(), window, cx));
                let pane = cx.new(|cx| {
                    Pane::new(
                        id.clone(),
                        "Review".into(),
                        PaneKind::Review(review),
                        workspace,
                        self.data.clone(),
                        window,
                        cx,
                    )
                });
                self.registry
                    .borrow_mut()
                    .insert(id.clone(), pane.downgrade());
                pane
            })
            .clone();
        let pane::PaneContent::Review(review) = &pane.read(cx).content else {
            unreachable!()
        };
        review.clone().update(cx, |view, cx| {
            view.open_branch_review(
                checkout,
                if vs_base {
                    gitterm_core::review::model::ReviewSource::VsBase
                } else {
                    gitterm_core::review::model::ReviewSource::Uncommitted
                },
                cx,
            )
        });
        self.selected_files.remove(&workspace);
        self.content.update(cx, |content, cx| {
            content.visits.insert(
                workspace,
                ViewerVisit {
                    pane: id.clone(),
                    session,
                },
            );
            cx.notify();
        });
        self.show_workspace_pane(workspace, &id, window, cx);
    }

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
        if !super::window::supported(&event.path) {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some(format!(
                    "Cannot preview {}: unsupported file type",
                    event.path.display()
                ));
                cx.notify();
            });
            return;
        }
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
        let review = if let pane::PaneContent::Review(view) = &pane.read(cx).content {
            Some(view.clone())
        } else {
            None
        };
        let is_review = review.is_some();
        let local_source = self.local && self.data.terminal_cwd(workspace, cx).is_some();
        let source = visit.and_then(|visit| self.content.read(cx).source(&visit.pane));
        let rendered = source
            .as_ref()
            .is_some_and(|(_, path)| super::actions::is_rendered(path));
        let title = if is_review {
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
            // Content panes expose a focus handle but their retained body has no
            // focus scope. Keep Escape routed through the visible viewer wrapper.
            .when(
                matches!(pane.read(cx).content, pane::PaneContent::Content(_)),
                |view| view.track_focus(&pane.focus_handle(cx)),
            )
            .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if is_review
                    && !event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.platform
                    && !event.keystroke.modifiers.alt
                {
                    let key = event.keystroke.key.as_str();
                    if let Some(review) = &review {
                        match key {
                            "j" | "k" => {
                                review.update(cx, |view, cx| view.navigate(key == "k", window, cx))
                            }
                            "g" | "G" => review.update(cx, |view, cx| {
                                view.navigate_to(
                                    key == "G" || event.keystroke.modifiers.shift,
                                    window,
                                    cx,
                                )
                            }),
                            "e" => this.editor_preview_notice(cx),
                            _ => {
                                if key != "escape" {
                                    return;
                                }
                            }
                        }
                        if matches!(key, "j" | "k" | "g" | "G" | "e") {
                            cx.stop_propagation();
                            return;
                        }
                    }
                }
                if event.keystroke.key == "escape" {
                    this.selected_files.remove(&workspace);
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
                    .when(is_review, |header| header.child(DIFF_HINT))
                    .when_some(source, |header, (root, relative)| {
                        let browser_path = root.join(&relative);
                        header
                            .child(
                                Button::new("viewer-open-editor")
                                    .debug_selector(|| "viewer-open-editor".into())
                                    .ghost()
                                    .label("Open in editor")
                                    .disabled(!local_source)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.editor_preview_notice(cx)
                                        }),
                                    ),
                            )
                            .when(!rendered, |header| {
                                header.child(
                                    Button::new("viewer-copy-all")
                                        .debug_selector(|| "viewer-copy-all".into())
                                        .ghost()
                                        .label("Copy All")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.copy_viewer_file(
                                                root.clone(),
                                                relative.clone(),
                                                cx,
                                            );
                                        })),
                                )
                            })
                            .child(
                                Button::new("viewer-open-browser")
                                    .debug_selector(|| "viewer-open-browser".into())
                                    .ghost()
                                    .label(if rendered {
                                        "View in Browser"
                                    } else {
                                        "Open in Browser"
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Err(error) =
                                            super::actions::open_file(&browser_path, cx)
                                        {
                                            this.viewer_error(error, cx);
                                        }
                                    })),
                            )
                    })
                    .child(
                        Button::new("close-file-viewer")
                            .debug_selector(|| "close-file-viewer".into())
                            .ghost()
                            .label(if is_review { DIFF_BACK_LABEL } else { "Close" })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.selected_files.remove(&workspace);
                                this.close_file_viewer(workspace, window, cx)
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().child(pane.clone()))
    }
}

pub(super) type ViewerVisits = HashMap<WorkspaceId, ViewerVisit>;
