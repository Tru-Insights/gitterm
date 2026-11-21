//! Shell hookup for remote machines, read-only: the in-app folder picker,
//! the connection form, legacy remote-session Refresh and "Show {machine}
//! chats". Mirrors Iced's handlers; nothing here writes Iced's config.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use gitterm_core::{
    folder_picker as rules,
    remote::{
        config::{RemoteAgentConfig, WorkspaceLocationConfig},
        ssh_sessions::{
            RemoteHostConfig, RemoteSessionEntry, claude_session_name, codex_session_name,
            session_status,
        },
    },
};
use gpui::{
    AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, ParentElement as _,
    Styled as _, Subscription, Window, div, px, rgb,
};
use gpui_component::{
    Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
};

use crate::{
    Shell,
    connect_form::{ConnectForm, ConnectFormOutcome},
    folder_picker::{FolderPicker, FolderPickerOutcome, Machine, PickerFacts},
    remote_client::RemoteClient,
    theme::Palette,
    workspace_sheet::FolderPickerPurpose,
};

/// Iced's machine-wide legacy remote-session facts.
#[derive(Default)]
pub(crate) struct RemoteSessions {
    pub loading: bool,
    pub sessions: Vec<RemoteSessionEntry>,
    pub error: Option<String>,
}

pub(crate) struct RemoteState {
    pub client: Arc<dyn RemoteClient>,
    pub homes: Rc<RefCell<HashMap<Machine, String>>>,
    pub last_dirs: Rc<RefCell<HashMap<Machine, String>>>,
    pub picker: Option<(Entity<FolderPicker>, Subscription)>,
    /// Focus to return to when the picker closes.
    pub picker_return_focus: Option<gpui::FocusHandle>,
    pub connect: Option<(Entity<ConnectForm>, Subscription)>,
    pub sessions: RemoteSessions,
    pub remotes: Vec<RemoteAgentConfig>,
}

impl Default for RemoteState {
    fn default() -> Self {
        Self {
            #[cfg(not(test))]
            client: Arc::new(crate::remote_client::AgentClient),
            #[cfg(test)]
            client: Arc::new(crate::remote_client::Offline),
            homes: Default::default(),
            last_dirs: Default::default(),
            picker: None,
            picker_return_focus: None,
            connect: None,
            sessions: RemoteSessions::default(),
            remotes: vec![],
        }
    }
}

impl Shell {
    #[cfg(test)]
    pub(crate) fn set_remote_client(&mut self, client: Arc<dyn RemoteClient>) {
        self.remote.client = client;
    }

    #[cfg(test)]
    pub(crate) fn set_local_home(&mut self, home: &Path) {
        self.remote
            .homes
            .borrow_mut()
            .insert(None, home.to_string_lossy().into_owned());
    }

    pub(crate) fn folder_picker(&self) -> Option<&Entity<FolderPicker>> {
        self.remote.picker.as_ref().map(|(picker, _)| picker)
    }

    pub(crate) fn connect_form(&self) -> Option<&Entity<ConnectForm>> {
        self.remote.connect.as_ref().map(|(form, _)| form)
    }

    /// Iced's configured remotes as last read.
    pub(crate) fn configured_remotes(&self) -> &[RemoteAgentConfig] {
        &self.remote.remotes
    }

    /// Re-read Iced's configured remotes (read-only); a config that cannot
    /// be read keeps no remotes and shows the error.
    pub(crate) fn reload_remotes(&mut self, cx: &mut Context<Self>) -> Vec<RemoteAgentConfig> {
        let source = self
            .owner
            .read(cx)
            .iced_catalog()
            .map(|catalog| catalog.source.clone());
        self.remote.remotes = match source.map(|source| crate::remote_client::read_remotes(&source))
        {
            None => vec![],
            Some(Ok(remotes)) => remotes,
            Some(Err(error)) => {
                let error = format!("Remote machines: {error:#}");
                eprintln!("{error}");
                self.owner.update(cx, |owner, cx| {
                    owner.error = Some(error);
                    cx.notify();
                });
                vec![]
            }
        };
        self.remote.remotes.clone()
    }

    /// The machine of the active workspace (None = this Mac).
    pub(crate) fn active_machine(&self, cx: &Context<Self>) -> Machine {
        let id = self.active_workspace(cx)?;
        match self.owner.read(cx).iced_workspace(id)?.location.as_ref()? {
            WorkspaceLocationConfig::RemoteAgent { remote_id, .. } => Some(remote_id.clone()),
            WorkspaceLocationConfig::Local { .. } => None,
        }
    }

    /// The picker's Recent folders on each machine: its chat index's repo
    /// roots, newest first, then the open workspaces on it.
    fn picker_recents(
        &self,
        remotes: &[RemoteAgentConfig],
        cx: &Context<Self>,
    ) -> HashMap<Machine, Vec<PathBuf>> {
        let owner = self.owner.read(cx);
        let saved = owner
            .iced_catalog()
            .map(|c| c.workspaces.as_slice())
            .unwrap_or_default();
        let find = self.find_state.borrow();
        std::iter::once(None)
            .chain(remotes.iter().map(|r| Some(r.id.clone())))
            .map(|machine| {
                let open: Vec<PathBuf> = saved
                    .iter()
                    .filter_map(|w| match (&w.location, &machine) {
                        (
                            Some(WorkspaceLocationConfig::RemoteAgent {
                                remote_id, root, ..
                            }),
                            Some(id),
                        ) if remote_id == id => Some(PathBuf::from(root)),
                        (Some(WorkspaceLocationConfig::RemoteAgent { .. }), _) => None,
                        (_, None) if !w.is_legacy_remote() => Some(PathBuf::from(&w.dir)),
                        _ => None,
                    })
                    .collect();
                let entries = find
                    .index(&machine)
                    .map(|index| index.entries.as_slice())
                    .unwrap_or_default();
                let recents = rules::recents(entries, &open, usize::MAX);
                (machine, recents)
            })
            .collect()
    }

    /// Open the in-app picker on `remote_id`'s machine (None = this Mac).
    pub(crate) fn open_folder_picker(
        &mut self,
        purpose: FolderPickerPurpose,
        remote_id: Machine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bar_menu = None;
        // Tests set a fixture home; they never list the user's real one.
        #[cfg(not(test))]
        if !self.remote.homes.borrow().contains_key(&None)
            && let Some(home) = dirs::home_dir()
        {
            self.remote
                .homes
                .borrow_mut()
                .insert(None, home.to_string_lossy().into_owned());
        }
        let local_home = self.remote.homes.borrow().get(&None).cloned();
        let start_hint = match (purpose, local_home) {
            (FolderPickerPurpose::NewWork, Some(home)) => self
                .new_work_sheet
                .as_ref()
                .and_then(|sheet| {
                    rules::repos_common_parent_below_home(
                        &sheet.read(cx).draft().recent,
                        Path::new(&home),
                    )
                })
                .map(|dir| dir.to_string_lossy().into_owned()),
            _ => None,
        };
        let remotes = self.reload_remotes(cx);
        let facts = PickerFacts {
            recents: self.picker_recents(&remotes, cx),
            remotes,
            client: self.remote.client.clone(),
            homes: self.remote.homes.clone(),
            last_dirs: self.remote.last_dirs.clone(),
            start_hint,
        };
        let picker = cx.new(|cx| FolderPicker::new(purpose, remote_id, facts, window, cx));
        let subscription = cx.subscribe_in(
            &picker,
            window,
            |shell, _, outcome: &FolderPickerOutcome, window, cx| {
                shell.folder_picker_outcome(outcome, window, cx)
            },
        );
        // Opened from a menu, the focused element is the closing popover:
        // focus returns to the shell (a sheet beneath refocuses itself).
        self.remote.picker_return_focus = self.new_work_sheet.is_none().then(|| self.focus.clone());
        self.remote.picker = Some((picker, subscription));
        self.cover_sheet(true, window, cx);
        // Iced refreshes stale chat indexes so the Recent list fills in.
        self.reduce_find(
            gitterm_core::find::state::Intent::LoadIndex {
                machine: None,
                now: std::time::Instant::now(),
                force: false,
            },
            window,
            cx,
        );
        cx.notify();
    }

    /// The chat index changed: Recent follows it.
    pub(crate) fn refresh_picker_recents(&self, cx: &mut Context<Self>) {
        if let Some(picker) = self.folder_picker().cloned() {
            let recents = self.picker_recents(self.configured_remotes(), cx);
            picker.update(cx, |picker, cx| picker.set_recents(recents, cx));
        }
    }

    /// Drop the picker and give focus back to where it was opened from.
    fn drop_folder_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.remote.picker = None;
        if let Some(focus) = self.remote.picker_return_focus.take() {
            focus.focus(window, cx);
        }
    }

    fn close_folder_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.remote.picker.is_some() {
            self.drop_folder_picker(window, cx);
            self.cover_sheet(self.new_work_sheet.is_some(), window, cx);
            if let Some(sheet) = &self.new_work_sheet {
                sheet.update(cx, |sheet, cx| {
                    sheet.folder_picker_finished(None, window, cx)
                });
            }
        }
        cx.notify();
    }

    fn folder_picker_outcome(
        &mut self,
        outcome: &FolderPickerOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            FolderPickerOutcome::Cancel => self.close_folder_picker(window, cx),
            FolderPickerOutcome::ConnectMachine => self.open_connect_form(window, cx),
            FolderPickerOutcome::NativeDialog { purpose } => {
                // The sheet beneath stays in its picking state for the OS picker.
                self.drop_folder_picker(window, cx);
                self.pick_folder(
                    crate::folder_picker::native_dialog_title(*purpose),
                    window,
                    cx,
                );
            }
            FolderPickerOutcome::Confirm {
                purpose: _,
                remote_id,
                dir,
            } => {
                self.remote
                    .last_dirs
                    .borrow_mut()
                    .insert(remote_id.clone(), dir.clone());
                if remote_id.is_some() {
                    // A remote folder would become an Iced workspace (a config write).
                    self.close_folder_picker(window, cx);
                    self.chrome_unavailable("Use this folder", cx);
                    return;
                }
                // Today's behaviour for a folder on this Mac.
                self.drop_folder_picker(window, cx);
                let sheet = self.new_work_sheet.clone();
                self.finish_folder_pick(Ok(Some(PathBuf::from(dir))), sheet, window, cx);
            }
        }
    }

    pub(crate) fn open_connect_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.bar_menu = None;
        let form = cx.new(|cx| ConnectForm::new(window, cx));
        let subscription = cx.subscribe_in(
            &form,
            window,
            |shell, _, outcome: &ConnectFormOutcome, window, cx| match outcome {
                ConnectFormOutcome::Close => {
                    shell.remote.connect = None;
                    // The picker beneath keeps its focus target.
                    if let Some(picker) = shell.folder_picker().cloned() {
                        let remotes = shell.reload_remotes(cx);
                        picker.update(cx, |picker, cx| picker.set_remotes(remotes, cx));
                        picker.focus_handle(cx).focus(window, cx);
                    } else {
                        shell.cover_sheet(shell.new_work_sheet.is_some(), window, cx);
                    }
                    cx.notify();
                }
            },
        );
        self.remote.connect = Some((form, subscription));
        self.cover_sheet(true, window, cx);
        cx.notify();
    }

    /// "Show {machine} chats": Everywhere, only that machine's section open.
    pub(crate) fn show_machine_chats(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bar_menu = None;
        let machine: Machine = match key.split_once(':') {
            Some(("agent", remote_id)) => Some(remote_id.to_string()),
            _ => None,
        };
        let mut filter = self.find_state.borrow().filter().clone();
        filter.scope = gitterm_core::chats::ChatScope::Everywhere;
        self.reduce_find(
            gitterm_core::find::state::Intent::SetFilter(filter),
            window,
            cx,
        );
        self.open_find_sidebar(window, cx);
        self.ensure_find_views(window, cx);
        let owner = self.find_owner(cx);
        if let Some(views) = self.find_views.get(&owner) {
            views.sidebar.update(cx, |sidebar, cx| {
                sidebar.show_only_machine(machine.clone(), cx)
            });
        }
        self.load_remote_indexes(window, cx);
        cx.notify();
    }

    /// Everywhere lists every configured machine: refresh their stale indexes.
    pub(crate) fn load_remote_indexes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for remote in self.configured_remotes().to_vec() {
            self.reduce_find(
                gitterm_core::find::state::Intent::LoadIndex {
                    machine: Some(remote.id),
                    now: std::time::Instant::now(),
                    force: false,
                },
                window,
                cx,
            );
        }
    }

    /// A remote machine's chat index through the client seam.
    pub(crate) fn load_remote_index(
        &mut self,
        stamp: gitterm_core::find::state::IndexStamp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let remote = stamp.machine.as_deref().and_then(|id| {
            self.configured_remotes()
                .iter()
                .find(|r| r.id == id)
                .cloned()
        });
        let client = self.remote.client.clone();
        cx.spawn_in(window, async move |shell, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match remote {
                        Some(remote) => client.list_chats(&remote),
                        None => Err("This remote is no longer configured".into()),
                    }
                })
                .await;
            _ = shell.update_in(cx, |shell, window, cx| {
                shell.reduce_find(
                    gitterm_core::find::state::Intent::IndexFinished {
                        stamp,
                        result,
                        now: std::time::Instant::now(),
                    },
                    window,
                    cx,
                )
            });
        })
        .detach();
    }

    /// The active workspace's legacy SSH remote session, if it is one.
    fn active_remote_session(
        &self,
        cx: &Context<Self>,
    ) -> Option<gitterm_core::workspace_config::RemoteSessionConfig> {
        let id = self.active_workspace(cx)?;
        let saved = self.owner.read(cx).iced_workspace(id)?;
        saved
            .is_legacy_remote()
            .then(|| saved.remote_session.clone())
            .flatten()
    }

    /// Iced's remote-content Refresh: reload the host's tmux sessions.
    pub(crate) fn refresh_remote_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.active_remote_session(cx) else {
            self.remote.sessions = RemoteSessions {
                loading: false,
                sessions: vec![],
                error: Some("Select a remote workspace to refresh sessions".into()),
            };
            cx.notify();
            return;
        };
        let host = RemoteHostConfig::from_session(&session);
        self.remote.sessions.loading = true;
        self.remote.sessions.error = None;
        let client = self.remote.client.clone();
        cx.spawn_in(window, async move |shell, cx| {
            let snapshot = cx
                .background_executor()
                .spawn(async move { client.list_sessions(&host) })
                .await;
            _ = shell.update(cx, |shell, cx| {
                let sessions = &mut shell.remote.sessions;
                sessions.loading = false;
                if let Some(error) = snapshot.error {
                    sessions.sessions.clear();
                    sessions.error = Some(format!("{}: {}", snapshot.host_name, error));
                } else {
                    sessions.sessions = snapshot.sessions;
                    sessions.error = None;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Iced's legacy remote workspace home, or None for any other workspace.
    pub(crate) fn remote_session_home(
        &self,
        cx: &Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let session = self.active_remote_session(cx)?;
        let name = self.active(cx).workspace.name.clone();
        let p = Palette::current(cx);
        let runtime = |name: &str| {
            self.remote
                .sessions
                .sessions
                .iter()
                .find(|s| s.name == name)
        };
        let main = runtime(&session.session_name);
        let codex = runtime(&codex_session_name(&session.session_name));
        let claude = runtime(&claude_session_name(&session.session_name));
        let line = |text: String, color: u32, size: f32| {
            div().text_size(px(size)).text_color(rgb(color)).child(text)
        };
        let unavailable = |id: &'static str, label: &'static str, enabled: bool| {
            Button::new(id)
                .debug_selector(move || id.into())
                .ghost()
                .small()
                .label(label)
                .disabled(!enabled)
                .on_click(cx.listener(move |this, _, _, cx| this.chrome_unavailable(label, cx)))
        };
        let mut detail = div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .p(px(10.))
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().flex_1().child(session.label.clone()))
                    .child(
                        line(session_status(main), p.subtext0, 10.)
                            .debug_selector(|| "remote-session-status".into()),
                    ),
            )
            .child(line(session.host_name.clone(), p.subtext0, 11.))
            .child(line(session.remote_dir.clone(), p.subtext0, 11.))
            .child(line(session.session_name.clone(), p.muted, 10.));
        if let Some(entry) = main {
            detail = detail.child(line(format!("windows: {}", entry.windows), p.muted, 10.));
        }
        if let Some(command) = &session.codex_command {
            detail = detail.child(line(
                format!("Codex {} - {command}", session_status(codex)),
                p.muted,
                10.,
            ));
        }
        if let Some(command) = &session.claude_command {
            detail = detail.child(line(
                format!("Claude {} - {command}", session_status(claude)),
                p.muted,
                10.,
            ));
        }
        let mut home = div()
            .id("remote-session-home")
            .debug_selector(|| "remote-session-home".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(24.))
            .child(line(name, p.text, 18.))
            .child(line(session.remote_dir.clone(), p.subtext0, 13.))
            .child(line(session.ssh_target.clone(), p.subtext0, 12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        Button::new("remote-session-refresh")
                            .debug_selector(|| "remote-session-refresh".into())
                            .ghost()
                            .small()
                            .label("Refresh")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.refresh_remote_sessions(window, cx)
                            })),
                    )
                    .child(unavailable("remote-session-shell", "Shell", true))
                    .child(unavailable(
                        "remote-session-codex",
                        "Codex",
                        session.codex_command.is_some(),
                    ))
                    .child(unavailable(
                        "remote-session-claude",
                        "Claude",
                        session.claude_command.is_some(),
                    )),
            )
            .child(detail);
        if self.remote.sessions.loading {
            home = home.child(line("Loading remote sessions...".into(), p.subtext0, 12.));
        }
        if let Some(error) = &self.remote.sessions.error {
            home = home.child(
                line(error.clone(), p.red, 12.).debug_selector(|| "remote-session-error".into()),
            );
        }
        Some(home)
    }

    /// Overlays above every other sheet: the picker, then the connect form.
    pub(crate) fn remote_overlays(&self) -> Vec<gpui::AnyView> {
        self.folder_picker()
            .map(|p| p.clone().into())
            .into_iter()
            .chain(self.connect_form().map(|f| f.clone().into()))
            .collect()
    }
}
