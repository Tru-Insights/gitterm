//! Viewer cover/return bridge. Session entities and selection survive the visit.
use crate::{FileOpen, Pane, PaneKind, Shell, WorkspaceId, data::SessionId, pane};
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    dock::PanelId,
};
use std::{collections::HashMap, sync::Arc};

pub(super) const DIFF_BACK_LABEL: &str = "Back to Terminal";
pub(super) const DIFF_HINT: &str = "j/k: navigate Esc: back";

#[derive(Clone)]
pub(super) struct ViewerVisit {
    pub(super) pane: String,
    session: Option<SessionId>,
    line: Option<usize>,
    review: Option<Entity<gitterm_gpui_review::ReviewView>>,
    file: Option<FileOpen>,
}

fn page_pane_id(workspace: WorkspaceId) -> String {
    format!("workspace-{}-page", workspace.0)
}

enum AggregateReview {
    Branch {
        checkout: std::path::PathBuf,
        vs_base: bool,
    },
    Edit(gitterm_core::review::model::EditReview),
}

impl Shell {
    pub(crate) fn workspace_mut_for_context(
        &mut self,
        workspace: WorkspaceId,
    ) -> &mut crate::WorkspaceDock {
        self.workspaces
            .iter_mut()
            .find(|state| state.workspace.id == workspace)
            .unwrap()
    }

    /// Iced launch_session_tab reads current_dir, including Files browsing,
    /// without following an agent into its derived panel root.
    pub(crate) fn session_working_directory(
        &self,
        workspace: WorkspaceId,
        cx: &App,
    ) -> Option<std::path::PathBuf> {
        let state = self
            .workspaces
            .iter()
            .find(|state| state.workspace.id == workspace)?;
        let id = state.active_session?;
        let pane = state.panes.get(&crate::session_pane_id(id))?.read(cx);
        let terminal_dir = pane
            .terminal_saved_dir
            .clone()
            .or_else(|| pane.task_folder(cx));
        if let Some(tab) = self.owner.read(cx).model.session_contexts.get(&id) {
            return terminal_dir
                .filter(|dir| Some(dir) != tab.terminal_dir.as_ref())
                .or_else(|| Some(tab.cwd.clone()));
        }
        terminal_dir.or_else(|| pane.imported_tab.as_ref().map(|tab| tab.dir.clone().into()))
    }

    /// Iced TabSelect + refresh_panels_for_context: switch only the tab's
    /// navigation state; retain workspace geometry and global preferences.
    pub(crate) fn activate_session_context(
        &mut self,
        workspace: WorkspaceId,
        id: SessionId,
        cx: &mut Context<Self>,
    ) {
        let previous = self.workspace(workspace).active_session;
        if let Some(previous) = previous
            && let Some(pane) = self
                .workspace(workspace)
                .panes
                .get(&crate::session_pane_id(previous))
                .cloned()
        {
            let reported = {
                let pane = pane.read(cx);
                pane.terminal_saved_dir
                    .clone()
                    .or_else(|| pane.task_folder(cx))
            };
            let cwd = self.owner.update(cx, |owner, _| {
                let tab = owner.model.session_contexts.get_mut(&previous)?;
                if let Some(reported) = reported
                    && Some(&reported) != tab.terminal_dir.as_ref()
                {
                    tab.cwd = reported;
                }
                tab.terminal_dir = Some(tab.cwd.clone());
                Some(tab.cwd.clone())
            });
            if let Some(cwd) = cwd {
                pane.update(cx, |pane, _| pane.terminal_saved_dir = Some(cwd));
            }
        }
        let state = self.workspace_mut_for_context(workspace);
        if let Some(previous) = previous {
            state
                .store
                .regions
                .session_modes
                .insert(previous, state.store.regions.sidebar_mode);
        }
        state.store.regions.sidebar_mode = state
            .store
            .regions
            .session_modes
            .get(&id)
            .copied()
            .unwrap_or_default();
        let pane = state.panes.get(&crate::session_pane_id(id)).cloned();
        let saved_tab = pane
            .as_ref()
            .and_then(|pane| pane.read(cx).imported_tab.clone())
            .or_else(|| {
                let owner = self.owner.read(cx);
                let saved = owner.iced_workspace(workspace)?;
                owner
                    .iced_sessions(saved)
                    .iter()
                    .position(|session| session.id == id)
                    .and_then(|i| saved.tabs.get(i))
                    .cloned()
            });
        let cwd = pane
            .as_ref()
            .and_then(|pane| {
                let pane = pane.read(cx);
                pane.terminal_saved_dir
                    .clone()
                    .or_else(|| pane.task_folder(cx))
            })
            .or_else(|| saved_tab.as_ref().map(|tab| tab.dir.clone().into()))
            .or_else(|| self.data.terminal_cwd(workspace, cx));
        let task = self.session_task(id, cx);
        let task_root = task
            .as_ref()
            .and_then(|id| {
                self.task_records(cx)
                    .into_iter()
                    .find(|task| &task.task_id == id)
            })
            .and_then(|task| task.worktree.path)
            .filter(|path| path.is_dir());
        if let Some(cwd) = cwd {
            let repo = saved_tab
                .as_ref()
                .and_then(|tab| tab.repo_dir.clone())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| cwd.clone());
            self.owner.update(cx, |owner, cx| {
                let tab = owner.model.session_contexts.entry(id).or_insert_with(|| {
                    crate::data::SessionContext {
                        terminal_dir: Some(cwd.clone()),
                        cwd,
                        repo,
                        task_root: None,
                        task_context: false,
                        panel_repo: None,
                        follow: Default::default(),
                        observed_events: 0,
                        follow_listing: false,
                        follow_pending: Vec::new(),
                    }
                });
                tab.task_root = task_root;
                tab.task_context = task.is_some();
                let root = tab.panel_root();
                owner.dispatch(
                    crate::workspace_status::Intent::SelectSession {
                        workspace,
                        session: id,
                        root,
                    },
                    std::time::Instant::now(),
                    cx,
                );
                cx.notify();
            });
        }
        self.selected_files.remove(&workspace);
        self.content.update(cx, |content, cx| {
            content.visits.activate(workspace, Some(id));
            cx.notify();
        });
        if let Some(file) = self
            .content
            .read(cx)
            .visits
            .get(&workspace)
            .and_then(|visit| visit.file.clone())
        {
            self.selected_files.insert(workspace, file);
        }
        if let Some(visit) = self.content.read(cx).visits.get(&workspace).cloned()
            && let Some(review) = visit.review
            && let Some(pane) = self.workspace(workspace).panes.get(&visit.pane)
        {
            pane.update(cx, |pane, _| {
                pane.content = pane::PaneContent::Review(review.clone())
            });
            self.owner.update(cx, |owner, cx| {
                owner.reviews.insert(workspace, review);
                owner.reload_review(workspace, cx);
            });
        }
    }

    fn tab_viewer_pane_id(&self, workspace: WorkspaceId, base: String, cx: &App) -> String {
        let session = self.workspace(workspace).active_session;
        let suffix = format!("{base}-session-{}", session.map(|id| id.0).unwrap_or(0));
        let visits = &self.content.read(cx).visits;
        if visits
            .get(&workspace)
            .is_some_and(|visit| visit.pane == suffix)
            || visits
                .values()
                .any(|visit| visit.session != session && visit.pane == base)
        {
            suffix
        } else {
            base
        }
    }

    pub(crate) fn session_viewer_pane(&self, workspace: WorkspaceId, cx: &App) -> Option<String> {
        self.content
            .read(cx)
            .visits
            .get(&workspace)
            .map(|visit| visit.pane.clone())
    }

    pub(crate) fn session_worktree_follow(
        &self,
        cx: &App,
    ) -> Option<crate::session_strip::WorktreeFollow> {
        let workspace = self.active_workspace(cx)?;
        let id = self
            .workspaces
            .iter()
            .find(|state| state.workspace.id == workspace)?
            .active_session?;
        let follow = &self.owner.read(cx).model.session_contexts.get(&id)?.follow;
        let active = follow.active.as_ref()?;
        Some(crate::session_strip::WorktreeFollow {
            following: follow.follow,
            path: active.path.clone(),
            label: follow.label.clone(),
            branch: active.branch.clone(),
        })
    }

    /// Iced `open_page_in_store`: show a PR or issue in the workspace's
    /// embedded Page, in GPUI's own copy of the selected login store.
    pub(crate) fn open_page(
        &mut self,
        url: String,
        store: gitterm_core::page_store::PageStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = gitterm_core::page_store::browser_url(&url) {
            eprintln!("[page] open refused: {error}");
            return;
        }
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let session = self
            .content
            .read(cx)
            .visits
            .get(&workspace)
            .map(|visit| visit.session);
        let id = self.tab_viewer_pane_id(workspace, page_pane_id(workspace), cx);
        let (content, data) = (self.content.clone(), self.data.clone());
        let state = self
            .workspaces
            .iter_mut()
            .find(|state| state.workspace.id == workspace)
            .unwrap();
        let session = session.unwrap_or(state.active_session);
        let pane = state
            .panes
            .entry(id.clone())
            .or_insert_with(|| {
                let pane = cx.new(|cx| {
                    Pane::new(
                        id.clone(),
                        "Page".into(),
                        PaneKind::Content(content),
                        workspace,
                        data,
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
        let origin = super::Origin {
            workspace: workspace.0,
            pane: pane.entity_id().as_u64(),
            source: url.clone().into(),
        };
        let routes = self
            .work_sidebars
            .get(&workspace)
            .map(|sidebar| sidebar.read(cx).page_routes())
            .unwrap_or_default();
        self.selected_files.remove(&workspace);
        self.content.update(cx, |content, cx| {
            content.open_page(id.clone(), url, store, routes, origin, window, cx);
            content.select(Some(id.clone()));
            content.visits.insert(
                workspace,
                ViewerVisit {
                    pane: id.clone(),
                    session,
                    line: None,
                    review: None,
                    file: None,
                },
            );
            cx.notify();
        });
        self.show_workspace_pane(workspace, &id, window, cx);
    }

    /// Iced `view_page_header`: URL, login store, `Open in browser`, `×`.
    fn page_header(
        &self,
        url: String,
        store: String,
        workspace: WorkspaceId,
        cx: &Context<Self>,
    ) -> AnyElement {
        let muted = gpui_component::Theme::global(cx).muted_foreground;
        div()
            .h(px(40.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .child(
                div()
                    .debug_selector(|| "page-url".into())
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_xs()
                    .text_color(muted)
                    .child(gitterm_core::work::summary::shorten(&url, 100)),
            )
            .child(
                div()
                    .debug_selector(|| "page-store".into())
                    .text_xs()
                    .text_color(muted)
                    .child(store),
            )
            .child(
                Button::new("page-open-browser")
                    .debug_selector(|| "page-open-browser".into())
                    .ghost()
                    .label("Open in browser")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if gitterm_core::page_store::browser_url(&url).is_ok() {
                            cx.open_url(&url);
                        }
                    })),
            )
            .child(
                Button::new("page-close")
                    .debug_selector(|| "page-close".into())
                    .ghost()
                    .label("×")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_file_viewer(workspace, window, cx)
                    })),
            )
            .into_any_element()
    }

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
        self.open_aggregate_review(
            workspace,
            AggregateReview::Branch { checkout, vs_base },
            window,
            cx,
        );
    }

    pub(crate) fn open_edit_review(
        &mut self,
        workspace: WorkspaceId,
        open: gitterm_core::review::model::EditReview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_aggregate_review(workspace, AggregateReview::Edit(open), window, cx);
    }

    fn open_aggregate_review(
        &mut self,
        workspace: WorkspaceId,
        open: AggregateReview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let checkout = match &open {
            AggregateReview::Branch { checkout, .. } => checkout,
            AggregateReview::Edit(open) => &open.target.checkout,
        };
        let host = self.review_host(workspace, cx);
        let id = self.tab_viewer_pane_id(
            workspace,
            format!(
                "{}-branch-review",
                crate::content_pane_id(workspace, checkout)
            ),
            cx,
        );
        let Some(state) = self
            .workspaces
            .iter_mut()
            .find(|state| state.workspace.id == workspace)
        else {
            return;
        };
        let session = self
            .content
            .read(cx)
            .visits
            .get(&workspace)
            .map(|visit| visit.session)
            .unwrap_or(state.active_session);
        // Keep branch review separate from the workspace's status-driven review.
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
        review.clone().update(cx, |view, cx| match open {
            AggregateReview::Edit(open) => view.open_edit_review(open, host, cx),
            AggregateReview::Branch { checkout, vs_base } => view.open_branch_review(
                checkout,
                if vs_base {
                    gitterm_core::review::model::ReviewSource::VsBase
                } else {
                    gitterm_core::review::model::ReviewSource::Uncommitted
                },
                host,
                cx,
            ),
        });
        self.selected_files.remove(&workspace);
        self.content.update(cx, |content, cx| {
            content.visits.insert(
                workspace,
                ViewerVisit {
                    pane: id.clone(),
                    session,
                    line: None,
                    review: None,
                    file: None,
                },
            );
            cx.notify();
        });
        self.show_workspace_pane(workspace, &id, window, cx);
    }

    /// Live shares Iced's viewed marks; Preview writes only its own overlay.
    /// Turn snapshots remain read-only in both modes.
    pub(crate) fn review_host(
        &self,
        workspace: WorkspaceId,
        cx: &mut Context<Self>,
    ) -> gitterm_gpui_review::ReviewHost {
        let owner = self.owner.read(cx);
        let iced = owner.iced_catalog().map(|catalog| catalog.source.clone());
        let writer = owner.preview_writer();
        let active_chat = self
            .workspaces
            .iter()
            .find(|state| state.workspace.id == workspace)
            .and_then(|state| {
                state
                    .panes
                    .get(&crate::session_pane_id(state.active_session?))
            })
            .and_then(|pane| match &pane.read(cx).content {
                pane::PaneContent::PreviewChat(chat) => chat.read(cx).chat_session_id(),
                _ => None,
            });
        let live = crate::chat::policy::is_live(cx);
        let preview_marks_dir = (!live)
            .then(|| writer.as_ref().map(|writer| writer.preview().layout()))
            .flatten();
        let save_marks = if live {
            iced.clone().map(shared_marks_writer)
        } else {
            writer.zip(preview_marks_dir.clone()).map(|(writer, dir)| {
                let path = dir.join(gitterm_core::review::viewed::VIEWED_FILE);
                Arc::new(move |bytes: Vec<u8>| {
                    // Completion errors are reported by the writer's own thread.
                    writer
                        .submit(path.clone(), bytes)
                        .map(drop)
                        .map_err(|error| format!("save viewed marks: {error:#}"))
                }) as gitterm_gpui_review::SaveMarks
            })
        };
        let events = self.owner.clone();
        gitterm_gpui_review::ReviewHost {
            snapshot_roots: iced.as_ref().and_then(|dir| {
                gitterm_core::turn_snapshots::SnapshotRoots::new(
                    dir.join("turn-snapshots"),
                    std::env::temp_dir(),
                )
                .ok()
            }),
            iced_marks_dir: iced,
            preview_marks_dir,
            save_marks,
            active_chat,
            open_file: Some(std::rc::Rc::new(move |path, cx| {
                events.update(cx, |_, cx| {
                    cx.emit(FileOpen {
                        workspace,
                        path,
                        changed: None,
                        from_files: true,
                    })
                })
            })),
        }
    }

    pub(crate) fn open_file_viewer_at_line(
        &mut self,
        event: &FileOpen,
        line: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file_viewer(event, window, cx);
        self.content.update(cx, |content, cx| {
            if let Some(visit) = content.visits.get_mut(&event.workspace).filter(|visit| {
                visit
                    .file
                    .as_ref()
                    .is_some_and(|file| file.path == event.path)
            }) {
                visit.line = line;
            }
            cx.notify();
        });
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
            // The status review's pane is shared by the dock, but its body is
            // tab-owned, just as Iced keeps the diff state on TabState.
            let key = format!("workspace-{}-review", state.workspace.layout_key);
            let covered = self
                .content
                .read(cx)
                .visits
                .get(&event.workspace)
                .is_some_and(|visit| visit.pane == key);
            if !covered && let Some(pane) = state.panes.get(&key).cloned() {
                let review = cx
                    .new(|cx| gitterm_gpui_review::ReviewView::new(Default::default(), window, cx));
                pane.update(cx, |pane, _| {
                    pane.content = pane::PaneContent::Review(review.clone())
                });
                self.owner.update(cx, |owner, cx| {
                    owner.reviews.insert(event.workspace, review);
                    owner.reload_review(event.workspace, cx);
                });
            }
            self.open_review(event, window, cx);
        }
        let state = self
            .workspaces
            .iter()
            .find(|w| w.workspace.id == event.workspace)
            .unwrap();
        let id = if event.from_files {
            self.tab_viewer_pane_id(
                event.workspace,
                crate::content_pane_id(event.workspace, &event.path),
                cx,
            )
        } else {
            format!("workspace-{}-review", state.workspace.layout_key)
        };
        if state.panes.contains_key(&id) {
            let review = match &state.panes[&id].read(cx).content {
                pane::PaneContent::Review(view) => Some(view.clone()),
                _ => None,
            };
            self.content.update(cx, |content, cx| {
                content.visits.insert(
                    event.workspace,
                    ViewerVisit {
                        pane: id,
                        session,
                        line: None,
                        review,
                        file: Some(event.clone()),
                    },
                );
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
        // Agent configuration files live in the agent's home config folder.
        let agent_root = self.agent_file_root(event.workspace, &event.path, cx);
        let id = self.tab_viewer_pane_id(
            event.workspace,
            crate::content_pane_id(event.workspace, &event.path),
            cx,
        );
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
            .model
            .workspaces
            .iter()
            .find(|local| {
                local.id == event.workspace && !local.remote && event.path.starts_with(&local.root)
            })
            .map(|local| local.root.clone())
            .or_else(|| {
                self.owner
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
            })
            // The shared guidelines file lives in Iced's config dir, not a
            // workspace root; only that one file is opened from there.
            .or_else(|| {
                let source = &self.owner.read(cx).iced_catalog()?.source;
                (event.path == gitterm_core::guidelines::guidelines_path(source))
                    .then(|| source.clone())
            })
            .or_else(|| self.data.terminal_cwd(event.workspace, cx));
        let root = agent_root.or(root);
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
            self.registry
                .borrow_mut()
                .insert(id.clone(), pane.downgrade());
            state.panes.insert(id.clone(), pane);
        }
        let dark = gpui_component::Theme::global(cx).mode.is_dark();
        self.content.update(cx, |content, cx| {
            content.open(
                id.clone(),
                super::window::ContentRequest {
                    root: root.clone(),
                    relative: relative.to_owned(),
                    workspace: event.workspace.0,
                    pane: state.panes[&id].entity_id().as_u64(),
                    dark,
                },
                window,
                cx,
            )
        });
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
        let retained = self
            .content
            .read(cx)
            .visits
            .values()
            .any(|other| other.pane == visit.pane);
        if !retained && let Some(pane) = state.panes.get(&visit.pane).cloned() {
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
        self.owner.update(cx, |_, cx| {
            cx.emit(crate::files_view::FileViewerClosed(workspace))
        });
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
        let editor_line = visit.and_then(|visit| visit.line);
        let review = if let pane::PaneContent::Review(view) = &pane.read(cx).content {
            Some(view.clone())
        } else {
            None
        };
        let is_review = review.is_some();
        let local_source = self.local && self.data.terminal_cwd(workspace, cx).is_some();
        let source = visit.and_then(|visit| self.content.read(cx).source(&visit.pane));
        let page = visit.and_then(|visit| {
            let content = self.content.read(cx);
            content
                .page(&visit.pane)
                .map(|(url, store)| (url.to_owned(), store.label()))
        });
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
                            "e" => this.edit_selected_file(window, cx),
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
            .child(if let Some((url, store)) = page {
                self.page_header(url, store, workspace, cx)
            } else {
                div()
                    .h(px(40.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .px_2()
                    .child(div().flex_1().child(title))
                    .when(is_review, |header| header.child(DIFF_HINT))
                    .when_some(source, |header, (root, relative)| {
                        let (browser_root, browser_relative) = (root.clone(), relative.clone());
                        let editor_path = root.join(&relative);
                        header
                            .child(
                                Button::new("viewer-open-editor")
                                    .debug_selector(|| "viewer-open-editor".into())
                                    .ghost()
                                    .label("Open in editor")
                                    .tooltip(format!(
                                        "Open in {}",
                                        gitterm_core::editor::display_name(&self.editor)
                                    ))
                                    .disabled(!local_source)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_editor(
                                            super::actions::OpenEditor::configured(
                                                editor_path.clone(),
                                                editor_line,
                                            ),
                                            window,
                                            cx,
                                        )
                                    })),
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
                                        let dark = gpui_component::Theme::global(cx).mode.is_dark();
                                        this.open_viewer_in_browser(
                                            browser_root.clone(),
                                            browser_relative.clone(),
                                            dark,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                div()
                                    .debug_selector(|| "viewer-esc-hint".into())
                                    .text_xs()
                                    .text_color(gpui_component::Theme::global(cx).muted_foreground)
                                    .px_2()
                                    .child("Esc: close"),
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
                    )
                    .into_any_element()
            })
            .child(div().flex_1().min_h_0().child(pane.clone()))
    }
}

/// The active cover plus covers retained by hidden tabs. The workspace-facing
/// map keeps existing content routing independent of tab identity.
#[derive(Default)]
pub(super) struct ViewerVisits {
    visible: HashMap<WorkspaceId, ViewerVisit>,
    hidden: HashMap<(WorkspaceId, Option<SessionId>), ViewerVisit>,
}
impl std::ops::Deref for ViewerVisits {
    type Target = HashMap<WorkspaceId, ViewerVisit>;
    fn deref(&self) -> &Self::Target {
        &self.visible
    }
}
impl std::ops::DerefMut for ViewerVisits {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.visible
    }
}
impl ViewerVisits {
    pub(crate) fn activate(&mut self, workspace: WorkspaceId, session: Option<SessionId>) {
        if self
            .visible
            .get(&workspace)
            .is_some_and(|visit| visit.session == session)
        {
            return;
        }
        if let Some(visit) = self.visible.remove(&workspace) {
            self.hidden.insert((workspace, visit.session), visit);
        }
        if let Some(visit) = self.hidden.remove(&(workspace, session)) {
            self.visible.insert(workspace, visit);
        }
    }
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&WorkspaceId, &mut ViewerVisit) -> bool) {
        self.visible.retain(&mut keep);
        self.hidden
            .retain(|(workspace, _), visit| keep(workspace, visit));
    }
    pub(crate) fn values(&self) -> impl Iterator<Item = &ViewerVisit> {
        self.visible.values().chain(self.hidden.values())
    }
}

/// The review sends an overlay against Iced. Reconstitute this pane's desired
/// marks before applying its edits to the latest shared file.
fn shared_marks_writer(dir: std::path::PathBuf) -> gitterm_gpui_review::SaveMarks {
    use gitterm_core::review::viewed::ViewedMarks;
    let previous = std::sync::Mutex::new(ViewedMarks::load(&dir));
    Arc::new(move |bytes| {
        let overlay: ViewedMarks = serde_json::from_slice(&bytes)
            .map_err(|error| format!("cannot parse viewed marks to save: {error}"))?;
        let mut previous = previous
            .lock()
            .map_err(|error| format!("save viewed marks: {error}"))?;
        let before = previous.as_ref().map_err(Clone::clone)?;
        let mut desired = before.clone();
        desired.checkouts.extend(overlay.checkouts);
        desired.save_changes(before, &dir)?;
        *previous = Ok(desired);
        Ok(())
    })
}

#[cfg(test)]
mod marks_tests {
    use super::shared_marks_writer;
    use gitterm_core::review::viewed::{VIEWED_FILE, ViewedMarks};

    #[test]
    fn review_host_shared_writer_preserves_other_client_edits_and_clears_marks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(VIEWED_FILE),
            br#"{"checkouts":{"/tmp/checkout":{"clear":"old"}}}"#,
        )
        .unwrap();
        let save = shared_marks_writer(dir.path().into());
        // Iced writes after this pane loaded its snapshot.
        std::fs::write(
            dir.path().join(VIEWED_FILE),
            br#"{"checkouts":{"/tmp/checkout":{"clear":"old","iced":"hash"}}}"#,
        )
        .unwrap();
        save(br#"{"checkouts":{"/tmp/checkout":{"gpui":"new"}}}"#.to_vec()).unwrap();
        let loaded = ViewedMarks::load(dir.path()).unwrap();
        assert_eq!(
            loaded.checkouts["/tmp/checkout"],
            std::collections::BTreeMap::from([
                ("iced".into(), "hash".into()),
                ("gpui".into(), "new".into())
            ])
        );
        save(br#"{"checkouts":{"/tmp/checkout":{}}}"#.to_vec()).unwrap();
        assert_eq!(
            ViewedMarks::load(dir.path()).unwrap().checkouts["/tmp/checkout"],
            std::collections::BTreeMap::from([("iced".into(), "hash".into())])
        );
    }
}

#[cfg(test)]
mod tab_context_tests {
    use super::*;
    use crate::{
        WorkspaceOwner, data::LocalData, layout::SidebarMode, workspace_status::WorkspaceStatus,
    };
    use ::core::prelude::v1::test;
    use std::{path::Path, rc::Rc, time::Instant};

    fn fixture(
        folder: &Path,
        config: &Path,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) -> Shell {
        let mut model = WorkspaceStatus::default();
        model.add(folder.to_path_buf(), Instant::now());
        let owner = cx.new(|cx| WorkspaceOwner::new(model, None, cx));
        let mut shell = Shell::with_owner(
            Rc::new(LocalData(owner.clone())),
            owner,
            config,
            true,
            window,
            cx,
        )
        .unwrap();
        shell.new_terminal(window, cx);
        shell
    }

    #[gpui::test]
    fn tab_context_gap1_switching_restores_file_and_diff_covers(cx: &mut TestAppContext) {
        let _app = crate::test_app::Cleanup::new(cx);
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let config = tempfile::tempdir().unwrap();
        std::fs::write(root.join("file.txt"), "fixture").unwrap();
        let (shell, cx) =
            cx.add_window_view(|window, cx| fixture(&root, config.path(), window, cx));
        let first = cx.read(|cx| shell.read(cx).active(cx).active_session.unwrap());
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.select_session_id(first, window, cx);
                shell.open_file_viewer(
                    &FileOpen {
                        workspace: WorkspaceId(1),
                        path: root.join("file.txt"),
                        from_files: true,
                        changed: None,
                    },
                    window,
                    cx,
                );
                shell.new_terminal(window, cx);
            })
        });
        let second = cx.read(|cx| {
            let shell = shell.read(cx);
            assert!(shell.session_viewer_pane(WorkspaceId(1), cx).is_none());
            shell.active(cx).active_session.unwrap()
        });
        assert_ne!(first, second);
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open_file_viewer(
                    &FileOpen {
                        workspace: WorkspaceId(1),
                        path: root.join("file.txt"),
                        from_files: true,
                        changed: None,
                    },
                    window,
                    cx,
                );
                assert_ne!(
                    shell.session_viewer_pane(WorkspaceId(1), cx),
                    Some(crate::content_pane_id(
                        WorkspaceId(1),
                        &root.join("file.txt")
                    ))
                );
                shell.close_file_viewer(WorkspaceId(1), window, cx);
            })
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open_file_viewer(
                    &FileOpen {
                        workspace: WorkspaceId(1),
                        path: root.join("file.txt"),
                        from_files: false,
                        changed: Some(crate::data::ChangedFile {
                            path: "file.txt".into(),
                            status: "M".into(),
                            staged: false,
                        }),
                    },
                    window,
                    cx,
                );
            })
        });
        let diff = cx.read(|cx| {
            shell.read(cx).content.read(cx).visits[&WorkspaceId(1)]
                .review
                .as_ref()
                .unwrap()
                .entity_id()
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.select_session_id(first, window, cx))
        });
        cx.read(|cx| {
            let shell = shell.read(cx);
            let id = crate::content_pane_id(WorkspaceId(1), &root.join("file.txt"));
            assert_eq!(
                shell.session_viewer_pane(WorkspaceId(1), cx),
                Some(id.clone())
            );
            assert_eq!(shell.content.read(cx).selected, Some(id));
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.select_session_id(second, window, cx))
        });
        cx.read(|cx| {
            let shell = shell.read(cx);
            assert_eq!(
                shell.content.read(cx).visits[&WorkspaceId(1)]
                    .review
                    .as_ref()
                    .unwrap()
                    .entity_id(),
                diff
            );
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.select_session_id(first, window, cx);
                shell.open_file_viewer(
                    &FileOpen {
                        workspace: WorkspaceId(1),
                        path: root.join("file.txt"),
                        from_files: false,
                        changed: Some(crate::data::ChangedFile {
                            path: "file.txt".into(),
                            status: "M".into(),
                            staged: false,
                        }),
                    },
                    window,
                    cx,
                );
            })
        });
        let first_diff = cx.read(|cx| {
            shell.read(cx).content.read(cx).visits[&WorkspaceId(1)]
                .review
                .as_ref()
                .unwrap()
                .entity_id()
        });
        assert_ne!(first_diff, diff);
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.select_session_id(second, window, cx);
                shell.close_file_viewer(WorkspaceId(1), window, cx);
                shell.select_session_id(first, window, cx);
            })
        });
        cx.read(|cx| {
            assert_eq!(
                shell.read(cx).content.read(cx).visits[&WorkspaceId(1)]
                    .review
                    .as_ref()
                    .unwrap()
                    .entity_id(),
                first_diff
            )
        });
    }

    #[gpui::test]
    fn tab_context_gap2_git_changes_follows_the_selected_checkout(cx: &mut TestAppContext) {
        let _app = crate::test_app::Cleanup::new(cx);
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        let checkout = checkout.path().canonicalize().unwrap();
        let config = tempfile::tempdir().unwrap();
        let (shell, cx) =
            cx.add_window_view(|window, cx| fixture(&root, config.path(), window, cx));
        let first = cx.read(|cx| shell.read(cx).active(cx).active_session.unwrap());
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.select_session_id(first, window, cx);
                shell.new_terminal_at(
                    gpui_component::dock::DockPlacement::Center,
                    Some(checkout.clone()),
                    window,
                    cx,
                );
            })
        });
        let second = cx.read(|cx| {
            let shell = shell.read(cx);
            assert_eq!(shell.owner.read(cx).model.workspaces[0].root, checkout);
            shell.active(cx).active_session.unwrap()
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.select_session_id(first, window, cx))
        });
        cx.read(|cx| assert_eq!(shell.read(cx).owner.read(cx).model.workspaces[0].root, root));
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.select_session_id(second, window, cx))
        });
        cx.read(|cx| {
            assert_eq!(
                shell.read(cx).owner.read(cx).model.workspaces[0].root,
                checkout
            )
        });
    }

    #[gpui::test]
    fn tab_context_gap3_sidebar_mode_is_owned_by_each_tab(cx: &mut TestAppContext) {
        let _app = crate::test_app::Cleanup::new(cx);
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let (shell, cx) =
            cx.add_window_view(|window, cx| fixture(root.path(), config.path(), window, cx));
        let first = cx.read(|cx| shell.read(cx).active(cx).active_session.unwrap());
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.select_session_id(first, window, cx);
                shell.update_regions(|regions| regions.sidebar_mode = SidebarMode::Files, cx);
                shell.new_terminal(window, cx);
            })
        });
        let second = cx.read(|cx| {
            assert_eq!(
                shell.read(cx).active(cx).store.regions.sidebar_mode,
                SidebarMode::Work
            );
            shell.read(cx).active(cx).active_session.unwrap()
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.update_regions(|regions| regions.sidebar_mode = SidebarMode::Git, cx);
                shell.select_session_id(first, window, cx);
            })
        });
        cx.read(|cx| {
            assert_eq!(
                shell.read(cx).active(cx).store.regions.sidebar_mode,
                SidebarMode::Files
            )
        });
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.select_session_id(second, window, cx))
        });
        cx.read(|cx| {
            assert_eq!(
                shell.read(cx).active(cx).store.regions.sidebar_mode,
                SidebarMode::Git
            )
        });
    }

    #[gpui::test]
    fn tab_context_gap4_new_plain_session_inherits_active_working_directory(
        cx: &mut TestAppContext,
    ) {
        let _app = crate::test_app::Cleanup::new(cx);
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let child = root.join("child");
        std::fs::create_dir(&child).unwrap();
        let config = tempfile::tempdir().unwrap();
        let (shell, cx) =
            cx.add_window_view(|window, cx| fixture(&root, config.path(), window, cx));
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                let first = shell.active(cx).active_session.unwrap();
                shell.select_session_id(first, window, cx);
                let pane = shell.active(cx).panes[&crate::session_pane_id(first)].clone();
                pane.update(cx, |pane, _| pane.terminal_saved_dir = Some(child.clone()));
                shell.new_terminal(window, cx);
            })
        });
        cx.read(|cx| {
            let shell = shell.read(cx);
            let id = shell.active(cx).active_session.unwrap();
            assert_eq!(
                shell.active(cx).panes[&crate::session_pane_id(id)]
                    .read(cx)
                    .terminal_cwd
                    .as_ref(),
                Some(&child)
            );
        });
    }
}
