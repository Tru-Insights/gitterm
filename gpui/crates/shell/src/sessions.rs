//! Runtime identities belong to panes, independently of saved layout IDs.
use gitterm_core::session::{self, SavedIdentity, Target};
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct Removed(pub Target);

pub(crate) fn subscribe(
    pane: &gpui::Entity<crate::pane::Pane>,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<crate::Shell>,
) -> gpui::Subscription {
    cx.subscribe_in(pane, window, |shell, pane, event: &Removed, window, cx| {
        let missing = shell.workspaces.iter().any(|state| {
            state.store.terminals.is_some()
                && state.sessions.iter().any(|session| {
                    state.panes.get(&crate::session_pane_id(session.id)) == Some(pane)
                })
                && state
                    .dock
                    .read(cx)
                    .panel(gpui_component::dock::PanelId::from(pane.entity_id()))
                    .is_none()
        });
        if missing {
            shell.close_target(&event.0, window, cx);
        }
    })
}

pub(crate) fn fresh_target() -> Target {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let tab_id = NEXT.fetch_add(1, Ordering::Relaxed);
    let identity = SavedIdentity::default()
        .open(|| format!("gpui-{}-{tab_id}", std::process::id()))
        .identity;
    Target {
        tab_id,
        session_uid: identity.session_uid,
        task_id: identity.task_id,
        task_session_id: identity.task_session_id,
    }
}

pub(crate) fn can_close(current: &Target, requested: &Target, last: bool) -> bool {
    let state = session::State {
        target: current.clone(),
        control: session::TurnControl::Terminal,
        task_live: false,
        last_tab: last,
    };
    matches!(
        state.reduce(session::Intent::Close {
            target: requested.clone(),
            confirmed: false,
        }),
        session::Effect::Close(_)
    )
}

impl crate::Shell {
    pub(crate) fn close_target(
        &mut self,
        requested: &Target,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(state) = self.workspaces.iter_mut().find(|state| {
            state
                .panes
                .values()
                .any(|pane| pane.read(cx).session.as_ref() == Some(requested))
        }) else {
            return;
        };
        let Some(id) = state.sessions.iter().find_map(|session| {
            let pane = state.panes.get(&crate::session_pane_id(session.id))?;
            (pane.read(cx).session.as_ref() == Some(requested)).then_some(session.id)
        }) else {
            return;
        };
        let pane_id = crate::session_pane_id(id);
        let pane = &state.panes[&pane_id];
        if !can_close(
            pane.read(cx).session.as_ref().unwrap(),
            requested,
            state.sessions.len() <= 1,
        ) {
            // Dock removal precedes the close decision; keep a protected last tab visible.
            let workspace = state.workspace.id;
            self.show_workspace_pane(workspace, &pane_id, window, cx);
            return;
        }
        pane.update(cx, |pane, cx| pane.stop_target(requested, cx));
        self.owner.update(cx, |owner, _| {
            owner.chats.remove(&requested.session_uid);
        });
        let pane = state.panes.remove(&pane_id).unwrap();
        self.registry.borrow_mut().remove(&pane_id);
        if let Some(terminals) = &mut state.store.terminals {
            terminals.retain(|terminal| terminal.id != id.0);
        }
        state.sessions.retain(|session| session.id != id);
        if state.active_session == Some(id) {
            state.active_session = state.sessions.first().map(|session| session.id);
        }
        state
            .dock
            .update(cx, |dock, cx| dock.remove_panel(pane, window, cx));
        crate::save_layout(state, cx);
        self.focus.focus(window, cx);
        cx.notify();
    }
}

impl crate::Shell {
    fn picker_inputs(&self) -> crate::session_picker::PickerInputs {
        crate::session_picker::PickerInputs {
            chat: gitterm_core::settings::ChatDefaults::default().new_chat_selection(),
            local_workspace: self.local,
            // TODO(TRU-274 P-src): project the settings/ACP/task catalogs here.
            acp_chats: vec![],
            presets: vec![],
            task: None,
        }
    }
    pub(crate) fn open_session_picker(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use gpui::AppContext as _;
        if self.active_workspace(cx).is_none()
            || self.park_sheet.is_some()
            || self.new_work_sheet.is_some()
        {
            return;
        }
        let inputs = self.picker_inputs();
        let previous_focus = window.focused(cx).unwrap_or_else(|| self.focus.clone());
        let opening_workspace = self.active_workspace(cx);
        let picker = cx.new(|cx| crate::session_picker::SessionPicker::new(inputs.clone(), cx));
        self.picker_subscription = Some(cx.subscribe_in(
            &picker,
            window,
            move |shell, picker, event: &crate::session_picker::SessionPickerEvent, window, cx| {
                let label = match event {
                    crate::session_picker::SessionPickerEvent::Selected(outcome) => {
                        picker.read(cx).outcome_label(outcome)
                    }
                    _ => None,
                };
                shell.session_picker = None;
                shell.picker_subscription = None;
                shell.cover_sheet(false, window, cx);
                previous_focus.focus(window, cx);
                if opening_workspace == shell.active_workspace(cx)
                    && let crate::session_picker::SessionPickerEvent::Selected(outcome) = event
                {
                    shell.select_picker_outcome(
                        outcome,
                        label
                            .as_deref()
                            .expect("selected picker row has its Iced label"),
                        window,
                        cx,
                    );
                }
                cx.notify();
            },
        ));
        self.cover_sheet(true, window, cx);
        picker.update(cx, |picker, cx| picker.open(inputs, window, cx));
        self.session_picker = Some(picker);
        cx.notify();
    }
    pub(crate) fn select_picker_outcome(
        &mut self,
        outcome: &crate::session_picker::SessionPickerOutcome,
        label: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::session_picker::SessionPickerOutcome as O;
        match outcome {
            O::NewChat if self.local => self.new_claude_chat(None, window, cx),
            O::Terminal if self.local => self.new_terminal(window, cx),
            _ => {
                self.owner.update(cx, |owner, cx| {
                    owner.error = Some(format!("{label} · unavailable in preview"));
                    cx.notify();
                });
            }
        }
    }
    pub(crate) fn cover_sheet(
        &self,
        covered: bool,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        self.content.update(cx, |content, cx| {
            content.picker = covered;
            content.sync_covers(window, cx);
        });
    }
    pub(crate) fn ensure_work_sidebars(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use gitterm_core::work::state::{Inputs, Intent, Source};
        use gpui::AppContext as _;
        for workspace in &self.workspaces {
            let id = workspace.workspace.id;
            if self.work_sidebars.contains_key(&id) {
                continue;
            }
            let dir = self.data.terminal_cwd(id, cx).unwrap_or_default();
            // TODO(TRU-274 Work source slice): share the owner's Work reducer and
            // index when available. Until then this is a fixture-free empty pane.
            self.work_state
                .borrow_mut()
                .reduce(Intent::SetInputs(Inputs {
                    workspace: id.0,
                    source: Source::Folder(dir.clone()),
                    tasks: vec![],
                    focus: Default::default(),
                    documents: None,
                }));
            let facts = crate::work_sidebar::WorkspaceFacts {
                id: id.0,
                name: workspace.workspace.name.clone(),
                home: dir.clone(),
                dir,
                local: self.local,
                repos: vec![],
                removed: vec![],
                auto_added: vec![],
                open_chats: Default::default(),
                owned_chats: vec![],
                chat_owners: Default::default(),
                running_by_repo: Default::default(),
                selected_checkout: None,
            };
            let sidebar =
                cx.new(|_| crate::work_sidebar::WorkSidebar::new(facts, self.work_state.clone()));
            self.work_subscriptions.insert(
                id,
                cx.subscribe_in(
                    &sidebar,
                    window,
                    |shell, _, outcome: &crate::work_sidebar::WorkSidebarOutcome, window, cx| {
                        shell.work_outcome(outcome, window, cx);
                    },
                ),
            );
            self.work_sidebars.insert(id, sidebar);
        }
    }
    pub(crate) fn work_outcome(
        &mut self,
        outcome: &crate::work_sidebar::WorkSidebarOutcome,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::work_sidebar::WorkSidebarOutcome as O;
        match outcome {
            O::OpenChat { workspace, chat } => self.select_work_chat(*workspace, chat, window, cx),
            O::Focus { workspace, focus } => {
                self.select_workspace_id(crate::WorkspaceId(*workspace), window, cx);
                if let gitterm_core::work::summary::Focus::Coordinator(chat) = focus {
                    self.select_work_chat(*workspace, chat, window, cx);
                }
                if let Some(dir) = self.data.terminal_cwd(crate::WorkspaceId(*workspace), cx) {
                    self.work_state.borrow_mut().reduce(
                        gitterm_core::work::state::Intent::SetInputs(
                            gitterm_core::work::state::Inputs {
                                workspace: *workspace,
                                source: gitterm_core::work::state::Source::Folder(dir),
                                tasks: vec![],
                                focus: focus.clone(),
                                documents: None,
                            },
                        ),
                    );
                }
                cx.notify();
            }
            // TODO(TRU-274 Work actions slice): wire repo editing, run/review/editor,
            // ownership, refresh/index and Find after the owner exposes those paths.
            _ => {}
        }
    }
    fn select_work_chat(
        &mut self,
        workspace: u64,
        chat: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let selected = self
            .workspaces
            .iter()
            .filter(|s| s.on_bar())
            .find_map(|state| {
                if state.workspace.id.0 != workspace {
                    return None;
                }
                state
                    .sessions
                    .iter()
                    .enumerate()
                    .find_map(|(session_index, session)| {
                        let pane = state
                            .panes
                            .get(&crate::session_pane_id(session.id))?
                            .read(cx);
                        let crate::pane::PaneContent::PreviewChat(view) = &pane.content else {
                            return None;
                        };
                        view.read(cx)
                            .matches_chat(chat)
                            .then_some((state.workspace.id, session_index))
                    })
            });
        if let Some((workspace, session)) = selected {
            self.select_workspace_id(workspace, window, cx);
            self.select_session(session, window, cx);
        }
    }
}

impl crate::Shell {
    fn unavailable(&self, label: &str, cx: &mut gpui::Context<Self>) {
        self.owner.update(cx, |owner, cx| {
            owner.error = Some(format!("{label} · unavailable in preview"));
            cx.notify();
        });
    }
    pub(crate) fn bar_outcome(
        &mut self,
        outcome: crate::workspace_bar::Outcome,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::workspace_bar::Outcome as O;
        match outcome {
            O::SelectWorkspace(id) => self.select_workspace_id(id, window, cx),
            O::NewWork => self.open_new_work_sheet(window, cx),
            O::ToggleTerminal if self.active_workspace(cx).is_some() => {
                self.open_bottom_terminal(window, cx)
            }
            O::OpenSettings(_) => self.unavailable("Settings", cx),
            O::OpenReposMenu => self.unavailable("Repos ▾", cx),
            O::OpenReady => self.unavailable("Ready", cx),
            O::OpenParked => self.unavailable("Parked", cx),
            O::Browser(_) => self.unavailable("Browser", cx),
            O::OpenMoreMenu => self.unavailable("Help and browser actions", cx),
            O::ToggleTerminal => self.unavailable("Terminal", cx),
        }
    }
    pub(crate) fn spine_outcome(
        &mut self,
        outcome: crate::spine::Outcome,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::spine::Outcome as O;
        match outcome {
            O::SelectWorkspace(id) => self.select_workspace_id(id, window, cx),
            O::Jump { workspace, target } => {
                self.select_workspace_id(workspace, window, cx);
                if self.active_workspace(cx) == Some(workspace) {
                    match target {
                        crate::spine::Target::Tab(id) => {
                            if let Some(index) =
                                self.active(cx).sessions.iter().position(|s| s.id.0 == id)
                            {
                                self.select_session(index, window, cx);
                            }
                        }
                        crate::spine::Target::Task(_) => self.unavailable("Task", cx),
                    }
                }
            }
            O::ToggleUsage => self.unavailable("Usage", cx),
        }
    }
    pub(crate) fn strip_outcome(
        &mut self,
        outcome: crate::session_strip::Outcome,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::session_strip::Outcome as O;
        match outcome {
            O::SelectSession(id) => {
                if let Some(index) = self.active(cx).sessions.iter().position(|s| s.id == id) {
                    self.select_session(index, window, cx);
                }
            }
            O::CloseSession(id) => {
                let target = self
                    .active(cx)
                    .panes
                    .get(&crate::session_pane_id(id))
                    .and_then(|p| p.read(cx).session.clone());
                if let Some(target) = target {
                    self.close_target(&target, window, cx);
                } else {
                    self.unavailable("Closing Iced sessions", cx);
                }
            }
            O::NewSession { workspace, .. } if self.active_workspace(cx) == Some(workspace) => {
                self.open_session_picker(window, cx)
            }
            O::CloseWorkspace(id) if self.active_workspace(cx) == Some(id) => {
                self.open_park_sheet(window, cx)
            }
            O::BackToWorkspace | O::TaskOverview => self.unavailable("Task overview", cx),
            O::ToggleWorktreeFollow => self.unavailable("Worktree follow", cx),
            _ => {}
        }
    }
    pub(crate) fn open_new_work_sheet(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use gitterm_core::work::catalog::{
            NewWorkSheet, OpenWork, existing_work, new_work_recent_repos,
        };
        use gpui::AppContext as _;
        self.session_picker = None;
        self.picker_subscription = None;
        self.park_sheet = None;
        self.park_subscription = None;
        let owner = self.owner.read(cx);
        let saved = owner
            .iced_catalog()
            .map(|c| c.workspaces.as_slice())
            .unwrap_or_default();
        let open: Vec<_> = saved
            .iter()
            .enumerate()
            .map(|(i, w)| OpenWork {
                uid: i as u64 + 1,
                name: &w.name,
                dir: &w.dir,
                chats: &w.chats,
                claims_new_chats: false,
                running_park: self.workspaces[i].parked.is_some(),
            })
            .collect();
        let existing = existing_work(&open, &[]);
        // shortcut: the chat index is not hosted yet; use Iced's open-workspace
        // and tab candidates in catalog order until that source is connected.
        let candidates = saved.iter().filter(|w| !w.is_legacy_remote() && !matches!(w.location, Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })))
            .flat_map(|w| std::iter::once(std::path::PathBuf::from(w.home_dir())).chain(w.repos.iter().map(std::path::PathBuf::from)))
            .chain(saved.iter().filter(|w| w.location.is_none() && !w.is_legacy_remote()).flat_map(|w| w.tabs.iter().filter(|t| t.task_id.is_none()).map(|t| std::path::PathBuf::from(&t.dir))));
        let draft = NewWorkSheet {
            recent: new_work_recent_repos(candidates, |p| p.join(".git").is_dir()),
            ..Default::default()
        };
        let previous_focus = window.focused(cx).unwrap_or_else(|| self.focus.clone());
        let sheet = cx.new(|cx| {
            crate::workspace_sheet::WorkspaceSheet::new(
                draft,
                existing,
                dirs::home_dir(),
                window,
                cx,
            )
        });
        self.new_work_subscription = Some(cx.subscribe_in(
            &sheet,
            window,
            move |shell, _, outcome: &crate::workspace_sheet::WorkspaceSheetOutcome, window, cx| {
                shell.new_work_outcome(outcome, window, cx);
                if shell.new_work_sheet.is_none() {
                    previous_focus.focus(window, cx);
                }
            },
        ));
        self.new_work_sheet = Some(sheet);
        self.cover_sheet(true, window, cx);
        cx.notify();
    }
    pub(crate) fn new_work_outcome(
        &mut self,
        outcome: &crate::workspace_sheet::WorkspaceSheetOutcome,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::workspace_sheet::WorkspaceSheetOutcome as O;
        use gitterm_core::work::catalog::ExistingWorkAction as A;
        match outcome {
            O::Create { .. } => {
                if let Some(sheet) = &self.new_work_sheet {
                    sheet.update(cx, |sheet, cx| {
                        sheet.show_error("Creating work isn't available in the preview".into(), cx)
                    });
                }
            }
            O::OpenExisting { id } => {
                let id = match id {
                    A::Switch(id) => Some(crate::WorkspaceId(*id)),
                    A::Resume(key) | A::Reopen(key) => self
                        .workspaces
                        .iter()
                        .find(|w| {
                            w.workspace.name == key.name
                                && self
                                    .data
                                    .terminal_cwd(w.workspace.id, cx)
                                    .is_some_and(|d| d == std::path::Path::new(&key.dir))
                        })
                        .map(|w| w.workspace.id),
                };
                if let Some(id) = id.filter(|id| {
                    self.workspaces
                        .iter()
                        .any(|w| w.workspace.id == *id && w.on_bar())
                }) {
                    self.select_workspace_id(id, window, cx);
                } else if let Some(sheet) = &self.new_work_sheet {
                    sheet.update(cx, |sheet, cx| {
                        sheet.show_error("This work is unavailable in preview".into(), cx)
                    });
                }
            }
            O::Cancel => {
                self.new_work_sheet = None;
                self.new_work_subscription = None;
                self.cover_sheet(false, window, cx);
            }
            O::PickFolder { .. } => self.pick_folder(window, cx),
        }
        cx.notify();
    }

    pub(crate) fn load_active_iced_history(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let Some(state) = self.workspaces.iter().find(|w| w.workspace.id == workspace) else {
            return;
        };
        let Some(id) = state.active_session else {
            return;
        };
        let Some(tab) = self.data.imported_chat(workspace, id, cx) else {
            return;
        };
        let pane = state.panes[&crate::session_pane_id(id)].clone();
        if !matches!(
            pane.read(cx).content,
            crate::pane::PaneContent::IcedSession(_)
        ) {
            return;
        }
        let Some(conversation) = tab.chat_session_id else {
            return;
        };
        let saved = self
            .owner
            .read(cx)
            .chat_history(Some(std::path::Path::new(&tab.dir)))
            .into_iter()
            .find(|s| s.session_id.as_deref() == Some(&conversation));
        let provider = self
            .owner
            .read(cx)
            .preview()
            .map(|p| p.provider())
            .unwrap_or_else(|| std::path::PathBuf::from("/unavailable"));
        let shutdown = self.owner.read(cx).chat_shutdown.clone();
        // Headless tests must never inspect the user's harness directories.
        #[cfg(test)]
        let projects: Option<std::path::PathBuf> = None;
        #[cfg(not(test))]
        let projects = dirs::home_dir().map(|home| home.join(".claude/projects"));
        cx.spawn_in(window, async move |shell, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    saved.map(|s| (s, None)).map(Ok).unwrap_or_else(|| {
                        let projects =
                            projects.ok_or_else(|| "transcript source unavailable".to_string())?;
                        read_iced_history(&projects, &conversation, &tab.dir, provider)
                    })
                })
                .await;
            _ = shell.update_in(cx, |shell, window, cx| {
                let Some(state) = shell
                    .workspaces
                    .iter_mut()
                    .find(|w| w.workspace.id == workspace)
                else {
                    return;
                };
                if state.panes.get(&crate::session_pane_id(id)) != Some(&pane)
                    || !matches!(
                        pane.read(cx).content,
                        crate::pane::PaneContent::IcedSession(_)
                    )
                {
                    return;
                }
                match result {
                    Ok((saved, title)) => {
                        use gpui::AppContext as _;
                        let view = cx
                            .new(|cx| crate::chat::ChatPane::history(shutdown, saved, window, cx));
                        if let Some(title) = title {
                            if let Some(session) = state.sessions.iter_mut().find(|s| s.id == id) {
                                session.title = title.clone();
                            }
                            pane.update(cx, |p, _| p.title = title);
                        }
                        // Imported history has no live target and is never registered
                        // as a preview-owned runtime or persisted as a preview chat.
                        pane.update(cx, |p, cx| {
                            p.content = crate::pane::PaneContent::PreviewChat(view);
                            cx.notify();
                        });
                    }
                    Err(error) => pane.update(cx, |p, cx| {
                        p.content = crate::pane::PaneContent::IcedSession(format!(
                            "{}\nhistory unavailable in preview\n{error}",
                            p.title
                        ));
                        cx.notify();
                    }),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn read_iced_history(
    projects: &std::path::Path,
    conversation: &str,
    cwd: &str,
    provider: std::path::PathBuf,
) -> Result<(crate::chat::SavedChat, Option<String>), String> {
    use gitterm_core::harness::transcript::TranscriptEntry;
    use gitterm_gpui_chat::events::{Entry, Record};
    if conversation.is_empty()
        || !conversation
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("Invalid conversation identity".into());
    }
    let entries =
        std::fs::read_dir(projects).map_err(|e| format!("Read transcript projects: {e}"))?;
    let path = entries
        .filter_map(Result::ok)
        .map(|e| e.path().join(format!("{conversation}.jsonl")))
        .find(|p| p.is_file())
        .ok_or_else(|| "Transcript not found".to_string())?;
    let source = std::fs::read_to_string(&path)
        .map_err(|e| format!("Read transcript {}: {e}", path.display()))?;
    let records: Vec<_> = gitterm_core::chat::history::parse_claude_transcript(&source)
        .into_iter()
        .map(|entry| Record {
            at_ms: None,
            entry: match entry {
                TranscriptEntry::UserPrompt(text) => Entry::UserPrompt(text),
                TranscriptEntry::Harness(event) => Entry::Harness(event),
            },
        })
        .collect();
    if records.is_empty() {
        return Err("Transcript has no recorded history".into());
    }
    // Match Iced's latest generated title, then its first summary/user title.
    let mut generated = None;
    let mut first = None;
    for value in source
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if value["type"] == "ai-title" {
            generated = value["aiTitle"]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .or(generated);
        }
        if first.is_none() && value["type"] == "summary" {
            first = value["summary"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(str::to_owned);
        }
        if first.is_none()
            && value["type"] == "user"
            && value["isMeta"] != true
            && value["isSidechain"] != true
        {
            let text = match &value["message"]["content"] {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Array(blocks) => blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            if !text.trim().is_empty() && !text.starts_with('<') {
                let line = text
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or(&text)
                    .trim();
                let mut title: String = line.chars().take(90).collect();
                if line.chars().count() > 90 {
                    title.push('…');
                }
                first = Some(title);
            }
        }
    }
    let title = generated.or(first);
    Ok((
        crate::chat::SavedChat {
            uid: format!("iced-{conversation}"),
            cwd: cwd.into(),
            provider,
            session_id: Some(conversation.into()),
            records,
        },
        title,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn imported_transcript_is_read_only_and_uses_iced_generated_title() {
        use gitterm_gpui_chat::events::Entry;
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("fixture-project");
        std::fs::create_dir(&project).unwrap();
        let path = project.join("fixture-chat.jsonl");
        let source = concat!(
            "{\"type\":\"user\",\"isMeta\":true,\"message\":{\"content\":\"Ignore metadata\"}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":\"<system-reminder>skip</system-reminder>\"}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":\"Recorded question\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Recorded answer\"}]}}\n",
            "{\"type\":\"ai-title\",\"aiTitle\":\"Old title\"}\n",
            "{\"type\":\"ai-title\",\"aiTitle\":\" Current Iced title \"}\n"
        );
        std::fs::write(&path, source).unwrap();
        let (saved, title) = read_iced_history(
            dir.path(),
            "fixture-chat",
            "/fixture/cwd",
            "/fixture/provider".into(),
        )
        .unwrap();
        assert_eq!(title.as_deref(), Some("Current Iced title"));
        assert_eq!(saved.session_id.as_deref(), Some("fixture-chat"));
        assert!(
            saved.records.iter().any(
                |r| matches!(&r.entry, Entry::UserPrompt(text) if text == "Recorded question")
            )
        );
        assert!(
            saved
                .records
                .iter()
                .any(|r| matches!(&r.entry, Entry::Harness(_)))
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(
            read_iced_history(
                dir.path(),
                "../escape",
                "/fixture/cwd",
                "/fixture/provider".into()
            )
            .is_err()
        );
        assert!(
            read_iced_history(
                dir.path(),
                "missing",
                "/fixture/cwd",
                "/fixture/provider".into()
            )
            .is_err()
        );
        std::fs::write(
            project.join("empty.jsonl"),
            "{\"type\":\"ai-title\",\"aiTitle\":\"No history\"}\n",
        )
        .unwrap();
        assert!(
            read_iced_history(
                dir.path(),
                "empty",
                "/fixture/cwd",
                "/fixture/provider".into()
            )
            .is_err()
        );
    }
    #[test]
    fn runtime_replacement_and_every_stale_binding_are_rejected() {
        let current = fresh_target();
        assert!(can_close(&current, &current, false));
        assert!(!can_close(&current, &current, true));
        assert!(!can_close(&fresh_target(), &current, false));
        for field in 0..4 {
            let mut stale = current.clone();
            match field {
                0 => stale.tab_id += 1,
                1 => stale.session_uid.push('x'),
                2 => stale.task_id = Some("fixture".into()),
                _ => stale.task_session_id = Some("fixture".into()),
            }
            assert!(!can_close(&current, &stale, false));
        }
    }
}
