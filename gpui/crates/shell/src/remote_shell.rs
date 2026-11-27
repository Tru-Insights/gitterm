//! Shell hookup for remote machines: the in-app folder picker,
//! the connection form, legacy remote-session Refresh, "Show {machine}
//! chats", each configured agent's connection state and the phone panel's
//! list. Live connect saves through core; Preview stays read-only.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use gitterm_core::{
    folder_picker as rules,
    preview::service::{ServiceContext, classify_phone_request, preview_notice},
    remote::{
        config::{RemoteAgentConfig, WorkspaceLocationConfig},
        phone_panel::{PhoneCall, PhoneRequest},
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
    phone::{PhonePanel, PhonePanelOutcome},
    remote_client::RemoteClient,
    theme::Palette,
    workspace_bar::{self, News, SavedFacts},
    workspace_sheet::FolderPickerPurpose,
};

/// Iced's `PHONE_CAPABILITY` (src/agentd/server.rs): the agent serves the
/// phone web app, so its machine menu offers "Pair a phone…".
const PHONE_CAPABILITY: &str = "phone";

/// A configured agent's connection, as Iced's `RemoteAgentConnectionStatus`
/// shows it on the bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Connection {
    Connecting,
    Connected { capabilities: Vec<String> },
    Unreachable,
}

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
    pub phone: Option<(Entity<PhonePanel>, Subscription)>,
    /// Closed forms keep their completion subscriptions until their write ends.
    pending_connect: Vec<(Entity<ConnectForm>, Subscription)>,
    connection_generations: HashMap<String, u64>,
    pub sessions: RemoteSessions,
    pub remotes: Vec<RemoteAgentConfig>,
    /// One handshake per configured agent id this session.
    pub connections: HashMap<String, Connection>,
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
            phone: None,
            pending_connect: Vec::new(),
            connection_generations: HashMap::new(),
            sessions: RemoteSessions::default(),
            remotes: vec![],
            connections: HashMap::new(),
        }
    }
}

impl RemoteState {
    fn begin_connection(&mut self, id: &str) -> u64 {
        let generation = self
            .connection_generations
            .entry(id.to_owned())
            .or_default();
        *generation += 1;
        self.connections
            .insert(id.to_owned(), Connection::Connecting);
        *generation
    }

    fn finish_connection(&mut self, id: &str, generation: u64, connection: Connection) -> bool {
        if self.connection_generations.get(id) != Some(&generation) {
            return false;
        }
        self.connections.insert(id.to_owned(), connection);
        true
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

    pub(crate) fn phone_panel(&self) -> Option<&Entity<PhonePanel>> {
        self.remote.phone.as_ref().map(|(panel, _)| panel)
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
        self.retain_pending_connect(cx);
        let live = cx
            .try_global::<crate::connect_form::LiveConnectRoot>()
            .cloned();
        let known = self.configured_remotes().to_vec();
        let form = cx.new(|cx| {
            let mut form = ConnectForm::new(window, cx);
            if let Some(root) = live {
                form.enable_live(root, known);
            }
            form
        });
        let subscription = cx.subscribe_in(
            &form,
            window,
            |shell, form, outcome: &ConnectFormOutcome, window, cx| match outcome {
                ConnectFormOutcome::Saved { agent, attempt } => {
                    let remotes = shell.reload_remotes(cx);
                    if let Some(picker) = shell.folder_picker().cloned() {
                        picker.update(cx, |picker, cx| picker.set_remotes(remotes, cx));
                    }
                    shell.connect_saved_remote(agent.clone(), *attempt, form.clone(), cx);
                    shell
                        .remote
                        .pending_connect
                        .retain(|(pending, _)| pending != form);
                }
                ConnectFormOutcome::SaveFailed(error) => {
                    if shell.connect_form() != Some(form) {
                        shell.owner.update(cx, |owner, cx| {
                            owner.error = Some(format!("Save remote connection: {error}"));
                            cx.notify();
                        });
                    }
                    shell
                        .remote
                        .pending_connect
                        .retain(|(pending, _)| pending != form);
                    cx.notify();
                }
                ConnectFormOutcome::Close => {
                    if shell.connect_form() != Some(form) {
                        return;
                    }
                    shell.retain_pending_connect(cx);
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

    fn retain_pending_connect(&mut self, cx: &gpui::App) {
        if let Some((form, subscription)) = self.remote.connect.take()
            && form.read(cx).is_saving()
        {
            self.remote.pending_connect.push((form, subscription));
        }
    }

    /// A form retry forces a fresh handshake. Match the complete saved identity
    /// before consuming its answer or closing the form (including disk edits).
    fn connect_saved_remote(
        &mut self,
        remote: RemoteAgentConfig,
        attempt: u64,
        form: Entity<ConnectForm>,
        cx: &mut Context<Self>,
    ) {
        let generation = self.remote.begin_connection(&remote.id);
        let client = self.remote.client.clone();
        let root = cx
            .global::<crate::connect_form::LiveConnectRoot>()
            .root()
            .to_path_buf();
        cx.spawn(async move |shell, cx| {
            let request = remote.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let result = client.handshake(&request);
                    let current =
                        crate::remote_client::read_remotes(&root)
                            .ok()
                            .is_some_and(|agents| {
                                agents.iter().any(|agent| {
                                    agent.id == request.id && agent.endpoint == request.endpoint
                                })
                            });
                    if !current
                        || result.as_ref().is_ok_and(|h| {
                            h.remote_id != request.id || h.endpoint != request.endpoint
                        })
                    {
                        Err(gitterm_core::remote::connect::IDENTITY_CHANGED.into())
                    } else {
                        result
                    }
                })
                .await;
            _ = shell.update(cx, |shell, cx| {
                if !shell
                    .configured_remotes()
                    .iter()
                    .any(|agent| agent.id == remote.id && agent.endpoint == remote.endpoint)
                {
                    return;
                }
                let connection = match &result {
                    Ok(h) => Connection::Connected {
                        capabilities: h.capabilities.clone(),
                    },
                    Err(_) => Connection::Unreachable,
                };
                if !shell
                    .remote
                    .finish_connection(&remote.id, generation, connection)
                {
                    return;
                }
                if form.read(cx).saved_matches(&remote, attempt) {
                    let active = shell.connect_form() == Some(&form);
                    match result {
                        Ok(_) if active => {
                            form.update(cx, |_, cx| cx.emit(ConnectFormOutcome::Close))
                        }
                        Ok(_) => {}
                        Err(error) => {
                            form.update(cx, |form, cx| form.connection_failed(&error, cx));
                            if !active {
                                let error = form.read(cx).status().map(str::to_owned);
                                shell.owner.update(cx, |owner, cx| {
                                    owner.error = error;
                                    cx.notify();
                                });
                            }
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Handshake each configured agent not yet asked, off the UI thread, as
    /// Iced connects its remotes: the bar shows the answer and the machine
    /// menu offers "Pair a phone…" when the agent serves phones. Same client
    /// and token lookup as the folder picker's handshake.
    ///
    /// shortcut: a failed handshake is not retried (the menu's Reconnect… is
    /// a preview notice) and an edited endpoint keeps the first answer; retry
    /// when the preview gets Reconnect.
    pub(crate) fn connect_remotes(&mut self, cx: &mut Context<Self>) {
        let remotes: Vec<_> = self
            .remote
            .remotes
            .iter()
            .filter(|remote| !self.remote.connections.contains_key(&remote.id))
            .cloned()
            .collect();
        for remote in remotes {
            let generation = self.remote.begin_connection(&remote.id);
            let client = self.remote.client.clone();
            cx.spawn(async move |shell, cx| {
                let id = remote.id.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { client.handshake(&remote) })
                    .await;
                let connection = match result {
                    Ok(handshake) => Connection::Connected {
                        capabilities: handshake.capabilities,
                    },
                    Err(error) => {
                        eprintln!("[remote] handshake with {id} failed: {error}");
                        Connection::Unreachable
                    }
                };
                _ = shell.update(cx, |shell, cx| {
                    if !shell.remote.finish_connection(&id, generation, connection) {
                        return;
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// The saved facts with each configured agent's connection applied:
    /// Iced's bar word and colour, and `serves_phones` from the handshake.
    pub(crate) fn connected_facts(
        &self,
        mut facts: SavedFacts,
        colors: workspace_bar::Colors,
    ) -> SavedFacts {
        for machine in &mut facts.repo_machines {
            let Some(connection) = machine
                .key
                .strip_prefix("agent:")
                .and_then(|id| self.remote.connections.get(id))
            else {
                continue;
            };
            let (label, color) = match connection {
                Connection::Connecting => ("connecting", colors.yellow),
                Connection::Connected { .. } => ("connected", colors.green),
                Connection::Unreachable => ("unreachable", colors.red),
            };
            machine.status = Some(News {
                label: label.into(),
                color,
            });
            machine.connected = matches!(connection, Connection::Connected { .. });
            machine.serves_phones = matches!(connection, Connection::Connected { capabilities }
                if capabilities.iter().any(|c| c == PHONE_CAPABILITY));
        }
        facts.machines = workspace_bar::machine_news(&facts.repo_machines);
        facts
    }

    /// "Pair a phone…" (machine key): Iced's panel. It lists the paired
    /// phones as it opens; pairing and revoking are refused by the gate.
    pub(crate) fn open_phone_panel(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bar_menu = None;
        let remote_id = key.strip_prefix("agent:").unwrap_or(key);
        // Iced names the machine from its config, else by its id.
        let machine_name = self
            .saved_facts
            .snapshot()
            .repo_machines
            .into_iter()
            .find(|machine| machine.key == key)
            .map_or_else(|| remote_id.to_string(), |machine| machine.name);
        let panel = cx.new(|cx| PhonePanel::new(remote_id.to_string(), machine_name, window, cx));
        let subscription = cx.subscribe_in(
            &panel,
            window,
            |shell, _, outcome: &PhonePanelOutcome, window, cx| match outcome {
                PhonePanelOutcome::Close => {
                    shell.remote.phone = None;
                    shell.cover_sheet(
                        shell.new_work_sheet.is_some() || shell.folder_picker().is_some(),
                        window,
                        cx,
                    );
                    cx.notify();
                }
                PhonePanelOutcome::Create => {
                    shell.phone_panel_call(|panel, resolve| panel.create(resolve), cx)
                }
                PhonePanelOutcome::Revoke(id) => {
                    let id = id.clone();
                    shell.phone_panel_call(|panel, resolve| panel.revoke(id, resolve), cx)
                }
            },
        );
        self.remote.phone = Some((panel, subscription));
        self.cover_sheet(true, window, cx);
        // Iced lists the phones as the panel opens.
        self.phone_panel_call(|panel, resolve| panel.list(resolve), cx);
        cx.notify();
    }

    /// Run a panel rule with Iced's client lookup (the configured remote,
    /// re-read), then ask the preview gate before running the call it starts.
    fn phone_panel_call(
        &mut self,
        rule: impl FnOnce(
            &mut gitterm_core::remote::phone_panel::PhonePanel<()>,
            &dyn Fn(&str) -> Result<RemoteAgentConfig, String>,
        ) -> Option<PhoneCall<RemoteAgentConfig>>,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.phone_panel().cloned() else {
            return;
        };
        let remotes = self.reload_remotes(cx);
        // Iced's `remote_chat_client` text.
        let resolve = |id: &str| {
            remotes
                .iter()
                .find(|remote| remote.id == id)
                .cloned()
                .ok_or_else(|| {
                    format!("No remote agent named {id} is configured in remote-agents.json.")
                })
        };
        let call = panel.update(cx, |panel, cx| {
            let call = rule(&mut panel.panel, &resolve);
            cx.notify();
            call
        });
        let Some(call) = call else {
            return;
        };
        if let Err(notice) = self.phone_gate(&call, cx) {
            panel.update(cx, |panel, cx| panel.refused(notice.clone(), cx));
            self.owner.update(cx, |owner, cx| {
                owner.error = Some(notice);
                cx.notify();
            });
            return;
        }
        let PhoneCall {
            remote_id,
            request,
            client: remote,
        } = call;
        if request != PhoneRequest::List {
            // Only a production context passes a write, and this shell
            // builds a preview one; it has no client to grant or revoke with.
            let notice = preview_notice(classify_phone_request(&request).label);
            panel.update(cx, |panel, cx| panel.refused(notice, cx));
            return;
        }
        let client = self.remote.client.clone();
        cx.spawn(async move |shell, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.list_phones(&remote) })
                .await;
            _ = shell.update(cx, |shell, cx| {
                if let Some(panel) = shell.phone_panel().cloned() {
                    panel.update(cx, |panel, cx| panel.phones_loaded(&remote_id, result, cx));
                }
            });
        })
        .detach();
    }

    /// `ServiceContext::phone(&call)` on this shell's preview roots; the
    /// notice when it refuses, or when there are no roots to decide on.
    fn phone_gate(
        &self,
        call: &PhoneCall<RemoteAgentConfig>,
        cx: &Context<Self>,
    ) -> Result<(), String> {
        let refused = || preview_notice(classify_phone_request(&call.request).label);
        let Some(preview) = self.owner.read(cx).preview() else {
            return Err(refused());
        };
        let services = ServiceContext::preview(preview).map_err(|error| {
            eprintln!("[phone] preview services: {error}");
            refused()
        })?;
        services
            .phone(call)
            .map_err(|denied| denied.notice().to_string())
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
            .chain(self.phone_panel().map(|p| p.clone().into()))
            .collect()
    }
}

#[cfg(test)]
mod connect_tests {
    use super::*;
    use crate::remote_shell_tests::{click, draw, fixture};
    use gpui::TestAppContext;

    #[test]
    fn obsolete_handshake_cannot_replace_a_newer_connection() {
        let mut state = RemoteState::default();
        let startup = state.begin_connection("mini");
        let retry = state.begin_connection("mini");
        let connected = Connection::Connected {
            capabilities: vec!["phone".into()],
        };
        assert!(state.finish_connection("mini", retry, connected.clone()));
        assert!(!state.finish_connection("mini", startup, Connection::Unreachable));
        assert_eq!(state.connections.get("mini"), Some(&connected));
        let other = state.begin_connection("other");
        assert!(state.finish_connection("other", other, Connection::Unreachable));
        assert_eq!(state.connections.get("mini"), Some(&connected));
        let newer = state.begin_connection("mini");
        assert!(!state.finish_connection("mini", retry, connected));
        assert!(state.finish_connection("mini", newer, Connection::Unreachable));
    }

    #[gpui::test]
    fn closed_form_delivers_save_success_and_failure_to_the_shell(cx: &mut TestAppContext) {
        let (f, cx) = fixture(cx);
        let source = f.iced();
        cx.update(|window, cx| {
            cx.set_global(crate::connect_form::LiveConnectRoot::new(Arc::new(
                gitterm_core::state_lock::StateLock::acquire(&source).unwrap(),
            )));
            f.shell.update(cx, |shell, cx| {
                shell.open_folder_picker(FolderPickerPurpose::NewWorkspace, None, window, cx)
            });
        });
        for fail in [false, true] {
            cx.update(|window, cx| {
                f.shell
                    .update(cx, |shell, cx| shell.open_connect_form(window, cx))
            });
            click("connect-form-name", cx);
            cx.simulate_input(if fail { "Failed Mac" } else { "Saved Mac" });
            click("connect-form-endpoint", cx);
            cx.simulate_input("127.0.0.1:9");
            click("connect-form-token", cx);
            cx.simulate_input("synthetic-closed-form-token");
            if fail {
                std::fs::write(source.join("remote-agents.json"), "{ malformed").unwrap();
            }
            cx.update(|_, cx| {
                let form = f.shell.read(cx).connect_form().unwrap().clone();
                form.update(cx, |form, cx| {
                    form.submit(cx);
                    cx.emit(ConnectFormOutcome::Close);
                });
            });
            cx.read(|cx| {
                assert!(f.shell.read(cx).connect_form().is_none());
                assert_eq!(f.shell.read(cx).remote.pending_connect.len(), 1);
            });
            draw(cx);
            if !fail {
                assert!(
                    cx.debug_bounds("folder-picker-machine-2").is_some(),
                    "picker did not refresh the saved remote"
                );
            }
            cx.read(|cx| {
                let shell = f.shell.read(cx);
                assert!(shell.remote.pending_connect.is_empty());
                if fail {
                    let error = shell.owner.read(cx).error.as_deref().unwrap();
                    assert!(error.contains(gitterm_core::remote::connect::MALFORMED));
                    assert!(!error.contains("synthetic-closed-form-token"));
                    assert_eq!(
                        std::fs::read(source.join("remote-agents.json")).unwrap(),
                        b"{ malformed"
                    );
                } else {
                    assert!(
                        shell
                            .configured_remotes()
                            .iter()
                            .any(|remote| remote.id == "saved-mac")
                    );
                    assert!(f.fake.calls().contains(&"handshake saved-mac".to_string()));
                    assert!(matches!(
                        shell.remote.connections.get("saved-mac"),
                        Some(Connection::Connected { .. })
                    ));
                }
            });
        }
    }

    #[gpui::test]
    fn live_form_save_reloads_remotes_and_closes_only_after_matching_handshake(
        cx: &mut TestAppContext,
    ) {
        let (f, cx) = fixture(cx);
        let source = f.iced();
        cx.update(|window, cx| {
            cx.set_global(crate::connect_form::LiveConnectRoot::new(Arc::new(
                gitterm_core::state_lock::StateLock::acquire(&source).unwrap(),
            )));
            f.shell
                .update(cx, |shell, cx| shell.open_connect_form(window, cx));
        });
        draw(cx);
        // Drive the actual fields/buttons, not a second implementation of submit.
        click("connect-form-name", cx);
        cx.simulate_input("Another Mac");
        click("connect-form-endpoint", cx);
        cx.simulate_input("fixture.invalid:7777");
        click("connect-form-token", cx);
        cx.simulate_input("synthetic-connect-secret");
        click("connect-form-connect", cx);
        draw(cx);
        let file = gitterm_core::remote::connect::load_remote_agents(&source).unwrap();
        let form_state = cx.read(|cx| {
            f.shell.read(cx).connect_form().map(|form| {
                (
                    form.read(cx).draft(cx),
                    form.read(cx).status().map(str::to_string),
                )
            })
        });
        assert_eq!(file.remote_agents.len(), 2, "form: {form_state:?}");
        assert_eq!(file.remote_agents[1].id, "another-mac");
        assert!(
            f.fake
                .calls()
                .contains(&"handshake another-mac".to_string())
        );
        cx.read(|cx| {
            assert!(f.shell.read(cx).connect_form().is_none());
            assert!(matches!(
                f.shell.read(cx).remote.connections.get("another-mac"),
                Some(Connection::Connected { .. })
            ));
        });
    }
}
