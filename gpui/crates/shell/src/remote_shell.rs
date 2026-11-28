//! Shell hookup for remote machines: the in-app folder picker,
//! the connection form, legacy remote-session Refresh, "Show {machine}
//! chats", each configured agent's connection state and the phone panel's
//! controls. Live saves through core; Preview stays read-only.

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

struct RemoteTabSave {
    key: gitterm_core::park::Key,
    location: Option<WorkspaceLocationConfig>,
    tabs: Vec<gitterm_core::workspace_config::WorkspaceTabConfig>,
    active_tab: Option<usize>,
}

fn remote_workspace_index(
    file: &gitterm_core::workspace_config::WorkspacesFile,
    key: &gitterm_core::park::Key,
    location: &Option<WorkspaceLocationConfig>,
) -> Result<usize, String> {
    let indices: Vec<_> = file
        .workspaces
        .iter()
        .enumerate()
        .filter(|(_, w)| key.matches(w))
        .map(|(i, _)| i)
        .collect();
    if indices.len() != 1 {
        return Err(format!(
            "Remote workspace {} is missing or ambiguous",
            key.name
        ));
    }
    let index = indices[0];
    if file.workspaces[index].location != *location {
        return Err("Remote workspace changed before saving its terminals".into());
    }
    Ok(index)
}

fn validate_remote_launch(
    store: &gitterm_core::desktop_store::DesktopStore,
    key: &gitterm_core::park::Key,
    location: &Option<WorkspaceLocationConfig>,
) -> Result<(), String> {
    use gitterm_core::desktop_store::LoadState;
    match store.snapshot().wait()?.workspaces.loaded {
        LoadState::Valid(file) => remote_workspace_index(&file, key, location).map(|_| ()),
        LoadState::Missing => Err(format!("Remote workspace {} is missing", key.name)),
        LoadState::Refused(error) => Err(error),
    }
}

fn save_remote_tabs(
    store: &gitterm_core::desktop_store::DesktopStore,
    save: RemoteTabSave,
) -> Result<crate::workspace_config::LiveWorkspaceSaved, String> {
    store
        .transact_workspaces(move |file| {
            let index = remote_workspace_index(file, &save.key, &save.location)?;
            let workspace = &mut file.workspaces[index];
            workspace.tabs = save.tabs;
            workspace.active_tab = save.active_tab;
            Ok(crate::workspace_config::LiveWorkspaceSaved {
                workspace: workspace.clone(),
                repos: file.repos.clone(),
                closed: file
                    .workspaces
                    .iter()
                    .filter(|workspace| !workspace.active)
                    .cloned()
                    .collect(),
            })
        })
        .wait()
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
                purpose,
                remote_id,
                dir,
            } => {
                self.remote
                    .last_dirs
                    .borrow_mut()
                    .insert(remote_id.clone(), dir.clone());
                if let Some(remote_id) = remote_id {
                    self.close_folder_picker(window, cx);
                    self.use_remote_folder(*purpose, remote_id, dir, window, cx);
                    return;
                }
                // Today's behaviour for a folder on this Mac.
                self.drop_folder_picker(window, cx);
                let sheet = self.new_work_sheet.clone();
                self.finish_folder_pick(Ok(Some(PathBuf::from(dir))), sheet, window, cx);
            }
        }
    }

    fn use_remote_folder(
        &mut self,
        purpose: FolderPickerPurpose,
        remote_id: &str,
        dir: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !crate::chat::policy::is_live(cx) {
            self.chrome_unavailable("Use this folder", cx);
            return;
        }
        // Iced only accepts remote folders for these two purposes.
        if !matches!(
            purpose,
            FolderPickerPurpose::NewWorkspace | FolderPickerPurpose::OpenFolder
        ) {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some("Repos join from this Mac".into());
                cx.notify();
            });
            return;
        }
        if self.owner.read(cx).workspace_save_pending {
            return;
        }
        let remotes = self.reload_remotes(cx);
        let remote = remotes.iter().find(|r| r.id == remote_id).cloned();
        let store = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .map(|s| s.0.clone());
        let (Some(remote), Some(store)) = (remote, store) else {
            self.owner.update(cx, |owner, cx| {
                owner.error =
                    Some("Use this folder: remote or Live workspace store unavailable".into());
                cx.notify();
            });
            return;
        };
        let active = self
            .active_workspace(cx)
            .and_then(|id| self.owner.read(cx).iced_workspace(id))
            .map(gitterm_core::workspace_config::WorkspaceTarget::of);
        let running: Vec<_> = self
            .workspaces
            .iter()
            .filter(|w| w.parked.as_ref().is_some_and(|p| p.running))
            .filter_map(|w| self.owner.read(cx).iced_workspace(w.workspace.id))
            .map(gitterm_core::workspace_config::WorkspaceTarget::of)
            .collect();
        let dir = dir.to_string();
        self.owner
            .update(cx, |owner, _| owner.workspace_save_pending = true);
        let receipt = store.transact_workspaces(move |file| {
            // Iced stores the configured definitions with every workspace save.
            file.remote_agents = remotes;
            let active = active
                .and_then(|target| target.index(file).ok())
                .unwrap_or(usize::MAX);
            let workspace = file.open_remote_folder(
                &remote.id,
                &remote.name,
                &dir,
                purpose == FolderPickerPurpose::NewWorkspace,
                active,
                &running,
            )?;
            Ok(crate::workspace_config::LiveWorkspaceSaved {
                workspace,
                repos: file.repos.clone(),
                closed: file
                    .workspaces
                    .iter()
                    .filter(|w| !w.active)
                    .cloned()
                    .collect(),
            })
        });
        cx.spawn_in(window, async move |shell, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { receipt.wait() })
                .await;
            _ = shell.update_in(cx, |shell, window, cx| {
                shell
                    .owner
                    .update(cx, |owner, _| owner.workspace_save_pending = false);
                let result = result.map_err(anyhow::Error::msg).and_then(|saved| {
                    let id = shell
                        .owner
                        .update(cx, |owner, cx| owner.publish_workspace(saved, false, cx))
                        .ok_or_else(|| anyhow::anyhow!("Remote workspace unavailable"))?;
                    shell.mount_workspace(id, window, cx)?;
                    shell.mount_remote_folder_browser(id, window, cx)?;
                    if let Some(state) = shell.workspaces.iter_mut().find(|w| w.workspace.id == id)
                    {
                        state.parked = None;
                    }
                    shell.owner.update(cx, |owner, _| {
                        owner.stopped_workspaces.remove(&id);
                    });
                    shell.select_workspace_id(id, window, cx);
                    if let Some(session) = shell.workspace(id).active_session {
                        shell.select_session_id(session, window, cx);
                    }
                    shell.sync_git_visibility(false, cx);
                    Ok(())
                });
                if let Err(error) = result {
                    shell.owner.update(cx, |owner, cx| {
                        owner.error = Some(format!("Use this folder: {error:#}"));
                        cx.notify();
                    });
                }
            });
        })
        .detach();
    }

    fn mount_remote_folder_browser(
        &mut self,
        workspace: crate::WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let id = self
            .data
            .active_session(workspace, cx)
            .ok_or_else(|| anyhow::anyhow!("Remote browser identity unavailable"))?;
        let session = self
            .data
            .sessions(workspace, cx)
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| anyhow::anyhow!("Remote browser tab unavailable"))?;
        let pane_id = crate::session_pane_id(id);
        if !self.workspace(workspace).panes.contains_key(&pane_id) {
            let text = self
                .data
                .imported_session(workspace, id, cx)
                .ok_or_else(|| anyhow::anyhow!("Remote browser metadata unavailable"))?;
            let pane = cx.new(|cx| {
                crate::pane::Pane::new(
                    pane_id.clone(),
                    session.title.clone(),
                    crate::pane::PaneKind::IcedSession(text),
                    workspace,
                    self.data.clone(),
                    window,
                    cx,
                )
            });
            self._subscriptions
                .push(crate::sessions::subscribe(&pane, window, cx));
            self.registry
                .borrow_mut()
                .insert(pane_id.clone(), pane.downgrade());
            let state = self
                .workspaces
                .iter_mut()
                .find(|w| w.workspace.id == workspace)
                .expect("mounted workspace");
            state.panes.insert(pane_id, pane.clone());
            state.sessions.push(session);
            state.dock.update(cx, |dock, cx| {
                dock.add_panel_view(
                    crate::panel_handle(pane),
                    gpui_component::dock::DockPlacement::Center,
                    None,
                    window,
                    cx,
                )
            });
        }
        // A Run tab need not be persisted. Append only the new browser;
        // reconciling the full saved inventory would drop those live panes.
        self.workspaces
            .iter_mut()
            .find(|w| w.workspace.id == workspace)
            .expect("mounted workspace")
            .active_session = Some(id);
        Ok(())
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
    /// phones as it opens; Live can pair and revoke through the same gate.
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
            &mut gitterm_core::remote::phone_panel::PhonePanel<crate::phone::PairingImage>,
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
        self.run_phone_panel_call(panel, call, cx);
    }

    fn run_phone_panel_call(
        &mut self,
        panel: Entity<PhonePanel>,
        call: PhoneCall<RemoteAgentConfig>,
        cx: &mut Context<Self>,
    ) {
        let PhoneCall {
            remote_id,
            request,
            client: remote,
        } = call;
        enum Answer {
            List(Result<Vec<gitterm_core::remote::phone_panel::PairedPhoneInfo>, String>),
            Create(Result<gitterm_core::remote::phone_panel::PhonePairing, String>),
            Revoke(Result<bool, String>),
        }
        let list_generation = panel.update(cx, |panel, _| {
            if request == PhoneRequest::List {
                panel.list_generation += 1;
            }
            panel.list_generation
        });
        let client = self.remote.client.clone();
        cx.spawn(async move |shell, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match request {
                        PhoneRequest::List => Answer::List(client.list_phones(&remote)),
                        PhoneRequest::Create { device_name, scope } => Answer::Create(
                            client.create_phone_pairing(&remote, device_name, scope.into()),
                        ),
                        PhoneRequest::Revoke { id } => {
                            Answer::Revoke(client.revoke_phone(&remote, id))
                        }
                    }
                })
                .await;
            _ = shell.update(cx, |shell, cx| {
                // A closed/reopened panel on the same machine is a new owner.
                if shell.phone_panel() != Some(&panel) {
                    return;
                }
                match result {
                    Answer::List(result) => panel.update(cx, |panel, cx| {
                        panel.phones_loaded_at(list_generation, &remote_id, result, cx);
                    }),
                    Answer::Create(result) => panel.update(cx, |panel, cx| {
                        panel.pairing_created(&remote_id, result, cx)
                    }),
                    Answer::Revoke(result) => shell.phone_panel_call(
                        |panel, resolve| panel.revoked(&remote_id, result, resolve),
                        cx,
                    ),
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
        let services = match crate::chat::policy::current(preview, cx) {
            crate::chat::policy::ChatLaunchPolicy::Live { services } => services,
            crate::chat::policy::ChatLaunchPolicy::Preview(preview) => {
                ServiceContext::preview(preview).map_err(|_| refused())?
            }
        };
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

    /// Resolve Iced's legacy host inference without rewriting saved location bytes.
    fn remote_agent_location(
        &self,
        saved: &gitterm_core::workspace_config::WorkspaceConfig,
    ) -> Option<WorkspaceLocationConfig> {
        saved.location.clone().or_else(|| {
            let session = saved.remote_session.as_ref()?;
            gitterm_core::remote::launch::remote_agent_location_for_session(
                self.configured_remotes(),
                &gitterm_core::remote::launch::RemoteSessionDefinition::from(session),
            )
        })
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
            .filter(|_| {
                !matches!(
                    self.remote_agent_location(saved),
                    Some(WorkspaceLocationConfig::RemoteAgent { .. })
                )
            })
            .flatten()
    }

    fn remote_tab_save(
        &self,
        workspace: crate::WorkspaceId,
        cx: &gpui::App,
    ) -> Option<RemoteTabSave> {
        if !self.live_chat_enabled(cx) {
            return None;
        }
        let saved = self.owner.read(cx).iced_workspace(workspace)?;
        if !saved.is_legacy_remote()
            && !matches!(
                self.remote_agent_location(saved),
                Some(WorkspaceLocationConfig::RemoteAgent { .. })
            )
        {
            return None;
        }
        let state = self.workspace(workspace);
        if state.parked.as_ref().is_some_and(|parked| !parked.running) {
            return None;
        }
        Some(RemoteTabSave {
            key: gitterm_core::park::Key {
                name: saved.name.clone(),
                dir: saved.dir.clone(),
            },
            location: saved.location.clone(),
            tabs: self.live_workspace_tabs(state.runtime_uid, cx),
            active_tab: self.live_active_tab(state.runtime_uid, cx),
        })
    }

    pub(crate) fn persist_remote_tabs(
        &mut self,
        workspace: crate::WorkspaceId,
        cx: &mut Context<Self>,
    ) {
        let Some(save) = self.remote_tab_save(workspace, cx) else {
            return;
        };
        let Some(store) = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .map(|store| store.0.clone())
        else {
            self.owner.update(cx, |owner, cx| {
                owner.error =
                    Some("Save remote terminals: Live workspace store unavailable".into());
                cx.notify();
            });
            return;
        };
        self.owner
            .update(cx, |owner, _| owner.workspace_save_pending = true);
        let saved = cx
            .background_executor()
            .spawn(async move { save_remote_tabs(&store, save) });
        cx.spawn(async move |shell, cx| {
            let saved = saved.await;
            let _ = shell.update(cx, |shell, cx| {
                shell.owner.update(cx, |owner, cx| {
                    owner.workspace_save_pending = false;
                    match saved {
                        Ok(saved) => {
                            owner.publish_workspace(saved, false, cx);
                        }
                        Err(error) => {
                            owner.error = Some(format!("Save remote terminals: {error}"));
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
    }

    pub(crate) fn save_remote_tabs_on_quit(&self, cx: &gpui::App) -> gpui::Task<()> {
        let store = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .map(|store| store.0.clone());
        let saves: Vec<_> = self
            .workspaces
            .iter()
            .filter_map(|state| self.remote_tab_save(state.workspace.id, cx))
            .collect();
        cx.background_executor().spawn(async move {
            if let Some(store) = store {
                for save in saves {
                    if let Err(error) = save_remote_tabs(&store, save) {
                        eprintln!("Save remote terminals on quit: {error}");
                    }
                }
            }
        })
    }

    pub(crate) fn remote_chat_binding(
        &mut self,
        workspace: crate::WorkspaceId,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Option<crate::chat::remote::Binding>> {
        if !self.live_chat_enabled(cx) {
            return Ok(None);
        }
        let location = self
            .owner
            .read(cx)
            .iced_workspace(workspace)
            .and_then(|w| self.remote_agent_location(w));
        let Some(WorkspaceLocationConfig::RemoteAgent { remote_id, .. }) = location else {
            anyhow::ensure!(
                !self
                    .owner
                    .read(cx)
                    .iced_workspace(workspace)
                    .is_some_and(|w| w.is_legacy_remote()),
                "This remote workspace has no configured agent for native chats"
            );
            return Ok(None);
        };
        let agent = self
            .fresh_remote_launch_config(&remote_id, cx)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No remote agent named {remote_id} is configured in remote-agents.json."
                )
            })?;
        Ok(Some(crate::chat::remote::Binding {
            agent,
            client: self.remote.client.clone(),
        }))
    }

    fn fresh_remote_launch_config(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Option<RemoteAgentConfig> {
        // Iced prefers a current file entry, then its startup snapshot.
        let previous = self
            .remote
            .remotes
            .iter()
            .find(|remote| remote.id == id)
            .cloned();
        self.reload_remotes(cx)
            .into_iter()
            .find(|remote| remote.id == id)
            .or(previous)
    }

    /// The SSH/tmux Shell, Codex and Claude buttons share Iced's selection
    /// and command bytes. Reopening a command selects its existing tab.
    pub(crate) fn open_remote_session_terminal(
        &mut self,
        kind: gitterm_core::remote::launch::SessionKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gitterm_core::remote::launch;
        if self.owner.read(cx).workspace_save_pending {
            return;
        }
        if !self.live_chat_enabled(cx) {
            self.preview_unavailable("Remote terminal", cx);
            return;
        }
        let Some(config) = self.active_remote_session(cx) else {
            return;
        };
        let session = launch::RemoteSessionDefinition::from(&config);
        let Some((name, command, title)) = launch::session_command(&session, kind) else {
            return;
        };
        let command = launch::remote_attach_command(&session, &name, &command);
        if let Some(id) = self.active(cx).sessions.iter().find_map(|s| {
            let pane = self
                .active(cx)
                .panes
                .get(&crate::session_pane_id(s.id))?
                .read(cx);
            (pane.startup_command.as_deref() == Some(&command)).then_some(s.id)
        }) {
            self.select_session_id(id, window, cx);
            return;
        }
        let cwd = launch::attach_cwd(true, Path::new(&config.remote_dir), dirs::home_dir());
        self.open_remote_attach_terminal(
            self.active(cx).workspace.id,
            command,
            cwd,
            title.into(),
            window,
            cx,
        );
    }

    fn open_remote_attach_terminal(
        &mut self,
        workspace: crate::WorkspaceId,
        command: String,
        cwd: PathBuf,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = cx
            .try_global::<crate::task_host::GpuiTaskHost>()
            .map(|h| h.0.clone())
        else {
            return;
        };
        let tab = gitterm_core::workspace_config::WorkspaceTabConfig {
            dir: cwd.to_string_lossy().into_owned(),
            startup_command: Some(command),
            extra: Default::default(),
            repo_dir: None,
            tab_kind: None,
            agent_config: None,
            chat_session_id: None,
            task_id: None,
            task_session_id: None,
            worker_shutdown_refused: false,
            session_uid: None,
        };
        let name = self.workspace(workspace).workspace.name.clone();
        match host.update(cx, |host, cx| {
            host.open_remote_terminal(&name, &tab, cwd, cx)
        }) {
            Ok(mut launched) => {
                launched.title = title;
                self.adopt_task_terminal_in(workspace, &host, &launched, window, cx);
                self.persist_remote_tabs(workspace, cx);
            }
            Err(error) => self.owner.update(cx, |owner, cx| {
                owner.error = Some(format!("Remote terminal: {error}"));
                cx.notify();
            }),
        }
    }

    /// Returns true when this is a remote-agent workspace (including errors).
    /// Starting and attaching are separate: restart only replays the attach.
    pub(crate) fn launch_remote_agent_terminal(
        &mut self,
        preset: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        use gitterm_core::remote::launch;
        let Some(workspace) = self.active_workspace(cx) else {
            return false;
        };
        let Some(saved) = self.owner.read(cx).iced_workspace(workspace).cloned() else {
            return false;
        };
        let Some(WorkspaceLocationConfig::RemoteAgent {
            remote_id,
            workspace_id,
            root,
        }) = self.remote_agent_location(&saved)
        else {
            return false;
        };
        if self.owner.read(cx).workspace_save_pending {
            return true;
        }
        if !self.live_chat_enabled(cx) {
            self.preview_unavailable("Remote terminal", cx);
            return true;
        }
        let admitted = cx
            .try_global::<crate::task_host::GpuiTaskHost>()
            .ok_or_else(|| "Live task host unavailable".to_string())
            .and_then(|host| host.0.read(cx).authorize_remote_session());
        if let Err(error) = admitted {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some(error);
                cx.notify();
            });
            return true;
        }
        let Some(remote) = self.fresh_remote_launch_config(&remote_id, cx) else {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some("Remote agent configuration unavailable".into());
                cx.notify();
            });
            return true;
        };
        // A terminal bridge browses locally. The Files tab's saved directory
        // is remote-native, just as Iced's SourcePath::Remote ownership test.
        // Session ids follow the owner's rename alias (go-live 12).
        let imported = self.owner.read(cx).iced_sessions(&saved);
        let state = self.active(cx);
        let remote_dir = state.active_session.and_then(|id| {
            let pane = state.panes.get(&crate::session_pane_id(id))?.read(cx);
            if pane.startup_command.is_some() {
                return None;
            }
            let index = imported.iter().position(|s| s.id == id)?;
            saved.tabs.get(index).map(|tab| tab.dir.as_str())
        });
        let launch = launch::agent_session_launch(
            preset,
            Some(&remote),
            &root,
            remote_dir,
            saved.claude_in_chrome,
        );
        let kind = launch.kind.clone();
        let runtime_uid = state.runtime_uid;
        let workspace_id = workspace_id.clone();
        let client = self.remote.client.clone();
        let store = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .map(|s| s.0.clone());
        let key = gitterm_core::park::Key {
            name: saved.name.clone(),
            dir: saved.dir.clone(),
        };
        let location = saved.location.clone();
        cx.spawn_in(window, async move |shell, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let store = store.ok_or_else(|| "Live workspace store unavailable".to_string())?;
                    validate_remote_launch(&store, &key, &location)?;
                    client
                        .start_session(&remote, workspace_id, launch)
                        .map(|id| (remote, id))
                })
                .await;
            let mut result = Some(result);
            loop {
                let completed = shell.update_in(cx, |shell, window, cx| {
                    // A closed/replaced workspace cannot receive this completion.
                    if !shell.workspaces.iter().any(|state| {
                        state.workspace.id == workspace
                            && state.runtime_uid == runtime_uid
                            && (state.on_bar() || state.parked.as_ref().is_some_and(|p| p.running))
                    }) {
                        return true;
                    }
                    if shell.owner.read(cx).workspace_save_pending { return false; }
                    let Some(result) = result.take() else { return true; };
                    match result {
                        Ok((remote, id)) => {
                            let Some(remote) = shell.fresh_remote_launch_config(&remote.id, cx) else {
                                shell.owner.update(cx, |owner, cx| {
                                    owner.error = Some(format!("Remote session {id} started but its agent configuration is unavailable"));
                                    cx.notify();
                                });
                                return true;
                            };
                            if shell.workspace(workspace).on_bar() {
                                shell.select_workspace_id(workspace, window, cx);
                            }
                            let command = launch::remote_session_attach_command(
                                &launch::agent_binary(),
                                &remote,
                                &id,
                            );
                            let mut label = kind.clone();
                            if let Some(first) = label.get_mut(0..1) {
                                first.make_ascii_uppercase();
                            }
                            label = format!("{label} @ {}", remote.name);
                            shell.open_remote_attach_terminal(
                                workspace,
                                command,
                                dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
                                label,
                                window,
                                cx,
                            );
                        }
                        Err(error) => shell.owner.update(cx, |owner, cx| {
                            owner.error = Some(format!("Could not start remote session: {error}"));
                            cx.notify();
                        }),
                    }
                    true
                }).unwrap_or(true);
                if completed { break; }
                // A second start may finish while the first tab is being saved.
                // Wait for that receipt, then attach this already-started session.
                cx.background_executor().timer(std::time::Duration::from_millis(10)).await;
            }
        })
        .detach();
        true
    }

    pub(crate) fn restore_remote_terminals(
        &mut self,
        workspace: crate::WorkspaceId,
        saved: &gitterm_core::workspace_config::WorkspaceConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use gitterm_core::remote::{config::WorkspaceLocationConfig, launch};
        if !self.live_chat_enabled(cx) {
            return;
        }
        let Some(host) = cx
            .try_global::<crate::task_host::GpuiTaskHost>()
            .map(|h| h.0.clone())
        else {
            return;
        };
        let remote_agent = matches!(
            self.remote_agent_location(saved),
            Some(WorkspaceLocationConfig::RemoteAgent { .. })
        );
        let imported = self.owner.read(cx).iced_sessions(saved);
        let sessions = self.workspace(workspace).sessions.clone();
        for session in sessions {
            let pane = self.workspace(workspace).panes[&crate::session_pane_id(session.id)].clone();
            if pane.read(cx).session.is_some() {
                continue;
            }
            let Some(index) = imported.iter().position(|s| s.id == session.id) else {
                continue;
            };
            let Some(tab) = saved.tabs.get(index) else {
                continue;
            };
            pane.update(cx, |pane, _| pane.imported_tab = Some(tab.clone()));
            if (remote_agent && tab.startup_command.is_none())
                || (!remote_agent
                    && tab.tab_kind.as_deref() == Some("agent")
                    && tab.agent_config.is_some())
            {
                continue;
            }
            let mut tab = tab.clone();
            // Iced's remote-agent terminal restore constructs a plain bridge;
            // its saved chat/task fields do not bind a local conversation.
            if remote_agent {
                tab.chat_session_id = None;
            }
            let paths = launch::restored_terminal_paths(remote_agent, &tab, dirs::home_dir());
            match host.update(cx, |host, cx| {
                host.open_remote_terminal(&saved.name, &tab, paths.cwd.clone(), cx)
            }) {
                Ok(launched) => {
                    if let Some(view) =
                        host.update(cx, |host, _| host.take_terminal(&launched.session_id))
                    {
                        self.adopt_terminal_view(&pane, view, launched.target, window, cx);
                        pane.update(cx, |pane, cx| {
                            pane.terminal_cwd = Some(launched.cwd);
                            pane.terminal_saved_dir = Some(paths.current_dir);
                            pane.exited = false;
                            pane.startup_command = launched.startup_command;
                            pane.chat_session_id = launched.chat_session_id;
                            if let Some(config) = &saved.remote_session {
                                pane.title = launch::tab_title(
                                    &launch::RemoteSessionDefinition::from(config),
                                    pane.startup_command.as_deref(),
                                );
                            }
                            cx.notify();
                        });
                    }
                }
                Err(error) => pane.update(cx, |pane, cx| {
                    pane.content = crate::pane::PaneContent::Error(format!(
                        "Restore remote terminal: {error}"
                    ));
                    cx.notify();
                }),
            }
        }
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
        let terminal_button = |id: &'static str, label: &'static str, enabled: bool, kind| {
            Button::new(id)
                .debug_selector(move || id.into())
                .ghost()
                .small()
                .label(label)
                .disabled(!enabled)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_remote_session_terminal(kind, window, cx)
                }))
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
                    .child(terminal_button(
                        "remote-session-shell",
                        "Shell",
                        true,
                        gitterm_core::remote::launch::SessionKind::Shell,
                    ))
                    .child(terminal_button(
                        "remote-session-codex",
                        "Codex",
                        session.codex_command.is_some(),
                        gitterm_core::remote::launch::SessionKind::Codex,
                    ))
                    .child(terminal_button(
                        "remote-session-claude",
                        "Claude",
                        session.claude_command.is_some(),
                        gitterm_core::remote::launch::SessionKind::Claude,
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
    fn golive19_remote_tab_save_rejects_changed_identity_without_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("workspaces.json");
        let bytes = br#"{"active_workspace":0,"workspaces":[{"name":"Placeholder","abbrev":"PH","dir":"/srv/root","color":"blue","location":{"kind":"remote_agent","remote_id":"placeholder","workspace_id":"placeholder-workspace","root":"/srv/root"},"tabs":[]}]}"#;
        std::fs::write(&path, bytes).unwrap();
        let store = gitterm_core::desktop_store::DesktopStore::open(root.path()).unwrap();
        let result = save_remote_tabs(
            &store,
            RemoteTabSave {
                key: gitterm_core::park::Key {
                    name: "Placeholder".into(),
                    dir: "/srv/root".into(),
                },
                location: None,
                tabs: vec![],
                active_tab: None,
            },
        );
        assert!(result.err().unwrap().contains("changed before saving"));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

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
        let _test_app = crate::test_app::Cleanup::new(cx);
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
        let _test_app = crate::test_app::Cleanup::new(cx);
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

#[cfg(test)]
mod parity_live_controls_tests {
    use super::*;
    use crate::remote_shell_tests::{FakeRemote, Fixture, click, draw, fixture, snapshot};
    use gitterm_core::remote::{
        client::RemoteAgentHandshake,
        phone_panel::{PairedPhoneInfo, PhonePairing},
        source::SourceDirListing,
        ssh_sessions::RemoteSessionsSnapshot,
    };
    use gpui::{TestAppContext, VisualTestContext};
    use std::sync::Mutex;

    struct Phones {
        reads: Arc<FakeRemote>,
        calls: Mutex<Vec<&'static str>>,
    }
    impl RemoteClient for Phones {
        fn handshake(&self, r: &RemoteAgentConfig) -> Result<RemoteAgentHandshake, String> {
            self.reads.handshake(r)
        }
        fn list_dir(&self, r: &RemoteAgentConfig, dir: String) -> SourceDirListing {
            self.reads.list_dir(r, dir)
        }
        fn list_chats(
            &self,
            r: &RemoteAgentConfig,
        ) -> Result<Vec<gitterm_core::chats::ChatIndexEntry>, String> {
            self.reads.list_chats(r)
        }
        fn list_sessions(&self, r: &RemoteHostConfig) -> RemoteSessionsSnapshot {
            self.reads.list_sessions(r)
        }
        fn list_phones(&self, _: &RemoteAgentConfig) -> Result<Vec<PairedPhoneInfo>, String> {
            self.calls.lock().unwrap().push("list");
            Ok(vec![])
        }
        fn create_phone_pairing(
            &self,
            _: &RemoteAgentConfig,
            device_name: String,
            scope: String,
        ) -> Result<PhonePairing, String> {
            assert_eq!(device_name, "iPhone");
            assert_eq!(scope, "view");
            self.calls.lock().unwrap().push("create");
            Ok(PhonePairing {
                pairing_url: "http://fixture.invalid/pair#placeholder".into(),
                short_code: "ABCD-EFGH".into(),
                scope,
                expires_unix_secs: 2000,
            })
        }
        fn revoke_phone(&self, _: &RemoteAgentConfig, id: String) -> Result<bool, String> {
            assert_eq!(id, "placeholder");
            self.calls.lock().unwrap().push("revoke");
            Ok(true)
        }
    }
    fn enable(f: &Fixture, cx: &mut VisualTestContext) {
        let root = f.iced();
        cx.update(|_, cx| {
            cx.set_global(crate::workspace_config::LiveWorkspaces(
                gitterm_core::desktop_store::DesktopStore::open(&root).unwrap(),
            ));
            let services = ServiceContext::production(&root, "parity-test")
                .unwrap()
                .for_gpui_live();
            crate::chat::policy::install_live(services, cx).unwrap();
        });
    }
    fn settle(f: &Fixture, cx: &mut VisualTestContext) {
        // DesktopStore owns an OS thread; yield until its acknowledgement lands.
        for _ in 0..1000 {
            draw(cx);
            if !f
                .shell
                .read_with(cx, |s, cx| s.owner.read(cx).workspace_save_pending)
            {
                return;
            }
            std::thread::yield_now();
        }
        panic!("workspace acknowledgement did not arrive");
    }
    fn disk(f: &Fixture) -> gitterm_core::workspace_config::WorkspacesFile {
        serde_json::from_slice(&std::fs::read(f.iced().join("workspaces.json")).unwrap()).unwrap()
    }

    #[gpui::test]
    fn parity_live_remote_folder_picker_creates_and_opens_browser(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (f, cx) = fixture(cx);
        enable(&f, cx);
        f.shell.update_in(cx, |s, window, cx| {
            s.open_folder_picker(
                FolderPickerPurpose::NewWorkspace,
                Some("mini".into()),
                window,
                cx,
            )
        });
        draw(cx);
        click("folder-picker-confirm", cx);
        settle(&f, cx);
        let file = disk(&f);
        let remote = file
            .workspaces
            .iter()
            .find(|w| w.name == "Mini:fixture")
            .unwrap();
        assert_eq!(remote.dir, "/home/fixture");
        assert_eq!(remote.tabs.len(), 1);
        assert_eq!(remote.tabs[0].dir, "/home/fixture");
        assert!(remote.tabs[0].startup_command.is_none());
        f.shell.update_in(cx, |s, window, cx| {
            let workspace = s.active(cx).workspace.id;
            let id = crate::data::SessionId(u64::MAX - 7);
            let pane_id = crate::session_pane_id(id);
            let pane = cx.new(|cx| {
                crate::pane::Pane::new(
                    pane_id.clone(),
                    "Ephemeral".into(),
                    crate::pane::PaneKind::IcedSession("placeholder".into()),
                    workspace,
                    s.data.clone(),
                    window,
                    cx,
                )
            });
            let state = s
                .workspaces
                .iter_mut()
                .find(|w| w.workspace.id == workspace)
                .expect("mounted workspace");
            state.sessions.push(crate::data::Session {
                id,
                title: "Ephemeral".into(),
                kind: crate::data::SessionKind::Terminal,
            });
            state.panes.insert(pane_id, pane);
        });
        f.shell.update_in(cx, |s, window, cx| {
            s.folder_picker_outcome(
                &FolderPickerOutcome::Confirm {
                    purpose: FolderPickerPurpose::OpenFolder,
                    remote_id: Some("mini".into()),
                    dir: "/home/fixture/code".into(),
                },
                window,
                cx,
            )
        });
        settle(&f, cx);
        let file = disk(&f);
        let remote = file
            .workspaces
            .iter()
            .find(|w| w.name == "Mini:fixture")
            .unwrap();
        assert_eq!(remote.tabs.len(), 2);
        assert_eq!(remote.active_tab, Some(1));
        assert_eq!(remote.tabs[1].dir, "/home/fixture/code");
        assert_eq!(remote.tabs[1].repo_dir.as_deref(), Some("/home/fixture"));
        f.shell.read_with(cx, |s, cx| {
            assert!(
                s.owner.read(cx).error.is_none(),
                "{:?}",
                s.owner.read(cx).error
            );
            let active = s.active(cx);
            assert_eq!(active.sessions.len(), 3);
            assert!(active.sessions.iter().any(|s| s.title == "Ephemeral"));
            assert_eq!(active.active_session, active.sessions.last().map(|s| s.id));
        });
    }

    #[gpui::test]
    fn parity_live_remote_folder_machine_identity_is_not_a_path(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (f, cx) = fixture(cx);
        let path = f.iced().join("remote-agents.json");
        let mut remotes: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        remotes["remote_agents"].as_array_mut().unwrap().push(serde_json::json!({"id":"second","name":"Second","endpoint":"http://127.0.0.1:9","auth":{"type":"token","token_ref":"env:PLACEHOLDER_UNSET"}}));
        std::fs::write(path, serde_json::to_vec(&remotes).unwrap()).unwrap();
        enable(&f, cx);
        let mut ids = Vec::new();
        for remote in ["mini", "second"] {
            f.shell.update_in(cx, |s, window, cx| {
                s.folder_picker_outcome(
                    &FolderPickerOutcome::Confirm {
                        purpose: FolderPickerPurpose::OpenFolder,
                        remote_id: Some(remote.into()),
                        dir: "/srv/project".into(),
                    },
                    window,
                    cx,
                )
            });
            settle(&f, cx);
            f.shell.read_with(cx, |s, cx| {
                let id = s.active(cx).workspace.id;
                ids.push(id);
                assert_eq!(
                    crate::workspace_config::workspace_remote_id(
                        s.owner.read(cx).iced_workspace(id).unwrap()
                    )
                    .as_deref(),
                    Some(remote)
                );
                assert!(s.owner.read(cx).error.is_none());
            });
        }
        assert_ne!(ids[0], ids[1]);
        let file = disk(&f);
        assert_eq!(
            file.workspaces
                .iter()
                .filter(|w| w.name == "project")
                .count(),
            2
        );
    }

    #[gpui::test]
    fn parity_live_remote_folder_reopens_running_and_stopped_parks_once(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (f, cx) = fixture(cx);
        enable(&f, cx);
        f.shell.update_in(cx, |s, window, cx| {
            s.folder_picker_outcome(
                &FolderPickerOutcome::Confirm {
                    purpose: FolderPickerPurpose::OpenFolder,
                    remote_id: Some("mini".into()),
                    dir: "/srv/project".into(),
                },
                window,
                cx,
            )
        });
        settle(&f, cx);
        let workspace = f.shell.read_with(cx, |s, cx| s.active(cx).workspace.id);
        for running in [true, false] {
            let store = cx.read(|cx| {
                cx.global::<crate::workspace_config::LiveWorkspaces>()
                    .0
                    .clone()
            });
            let saved = store
                .transact_workspaces(|file| {
                    let workspace = file
                        .workspaces
                        .iter_mut()
                        .find(|w| w.name == "project")
                        .unwrap();
                    workspace.active = false;
                    workspace.parked = Some(gitterm_core::workspace_config::ParkedInfo {
                        note: "placeholder".into(),
                        parked_at: "2026-01-01T00:00:00Z".into(),
                    });
                    Ok(crate::workspace_config::LiveWorkspaceSaved {
                        workspace: workspace.clone(),
                        repos: file.repos.clone(),
                        closed: file
                            .workspaces
                            .iter()
                            .filter(|w| !w.active)
                            .cloned()
                            .collect(),
                    })
                })
                .wait()
                .unwrap();
            f.shell.update_in(cx, |s, window, cx| {
                s.owner.update(cx, |owner, cx| {
                    owner.publish_workspace(saved, false, cx);
                    if !running {
                        owner.stopped_workspaces.insert(workspace);
                    }
                });
                s.workspaces
                    .iter_mut()
                    .find(|w| w.workspace.id == workspace)
                    .unwrap()
                    .parked = Some(crate::park::Parked {
                    info: gitterm_core::park::Info::new(
                        "placeholder",
                        "2026-01-01T00:00:00Z".into(),
                    ),
                    running,
                    done: false,
                });
                s.select_workspace(0, window, cx);
                s.folder_picker_outcome(
                    &FolderPickerOutcome::Confirm {
                        purpose: FolderPickerPurpose::OpenFolder,
                        remote_id: Some("mini".into()),
                        dir: "/srv/project/sub".into(),
                    },
                    window,
                    cx,
                );
            });
            settle(&f, cx);
            let file = disk(&f);
            let saved = file
                .workspaces
                .iter()
                .find(|w| w.name == "project")
                .unwrap();
            assert_eq!(saved.tabs.len(), if running { 2 } else { 1 });
            assert!(saved.active && saved.parked.is_none());
            f.shell.read_with(cx, |s, cx| {
                assert_eq!(s.active(cx).workspace.id, workspace);
                assert!(s.active(cx).parked.is_none());
                assert!(!s.owner.read(cx).stopped_workspaces.contains(&workspace));
                assert!(
                    s.owner.read(cx).error.is_none(),
                    "{:?}",
                    s.owner.read(cx).error
                );
            });
        }
    }

    #[gpui::test]
    fn parity_live_phone_pair_revoke_qr_and_closed_panel_reply(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (f, cx) = fixture(cx);
        enable(&f, cx);
        let before = snapshot(&f.iced());
        let client = Arc::new(Phones {
            reads: f.fake.clone(),
            calls: Mutex::new(vec![]),
        });
        f.shell.update_in(cx, |s, window, cx| {
            s.set_remote_client(client.clone());
            s.open_phone_panel("agent:mini", window, cx);
        });
        draw(cx);
        click("phone-create", cx);
        assert!(cx.debug_bounds("phone-pairing-qr").is_some());
        f.shell.update(cx, |s, cx| {
            s.phone_panel_call(
                |panel, resolve| panel.revoke("placeholder".into(), resolve),
                cx,
            )
        });
        draw(cx);
        assert_eq!(
            *client.calls.lock().unwrap(),
            ["list", "create", "revoke", "list"]
        );
        // Finish the old create only after its originating panel was replaced.
        f.shell.update_in(cx, |s, window, cx| {
            s.phone_panel_call(|panel, resolve| panel.create(resolve), cx);
            s.open_phone_panel("agent:mini", window, cx);
        });
        draw(cx);
        f.shell.read_with(cx, |s, cx| {
            assert!(s.phone_panel().unwrap().read(cx).panel.pairing.is_none())
        });
        assert!(cx.debug_bounds("phone-pairing-qr").is_none());
        let panel = f
            .shell
            .read_with(cx, |s, _| s.phone_panel().unwrap().clone());
        panel.update(cx, |panel, cx| {
            let stale = panel.list_generation;
            panel.list_generation += 1;
            panel.phones_loaded_at(panel.list_generation, "mini", Ok(vec![]), cx);
            panel.phones_loaded_at(stale, "mini", Err("obsolete placeholder reply".into()), cx);
            assert!(panel.panel.error.is_none());
        });
        assert_eq!(snapshot(&f.iced()), before); // pairings live only on the agent.
    }

    #[gpui::test]
    fn parity_preview_refuses_all_three_writes_before_client_or_store(cx: &mut TestAppContext) {
        let _test_app = crate::test_app::Cleanup::new(cx);
        let (f, cx) = fixture(cx);
        let before = snapshot(&f.iced());
        let client = Arc::new(Phones {
            reads: f.fake.clone(),
            calls: Mutex::new(vec![]),
        });
        f.shell.update_in(cx, |s, window, cx| {
            s.set_remote_client(client.clone());
            s.folder_picker_outcome(
                &FolderPickerOutcome::Confirm {
                    purpose: FolderPickerPurpose::NewWorkspace,
                    remote_id: Some("mini".into()),
                    dir: "/srv/placeholder".into(),
                },
                window,
                cx,
            );
        });
        f.shell.read_with(cx, |s, cx| {
            assert!(
                s.owner
                    .read(cx)
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("not available in preview")
            )
        });
        f.shell.update_in(cx, |s, window, cx| {
            s.open_phone_panel("agent:mini", window, cx)
        });
        draw(cx);
        click("phone-create", cx);
        f.shell.update(cx, |s, cx| {
            s.phone_panel_call(|p, r| p.revoke("placeholder".into(), r), cx)
        });
        draw(cx);
        assert_eq!(*client.calls.lock().unwrap(), ["list"]);
        f.shell.update(cx, |s, cx| {
            s.usage_intent(
                crate::usage::UsageIntent::PinChanged(Some(
                    gitterm_core::plan_limits::RoutingPin {
                        provider: gitterm_core::plan_limits::Provider::Codex,
                        until: 2000,
                    },
                )),
                cx,
            )
        });
        f.shell.read_with(cx, |s, cx| {
            assert!(
                s.owner
                    .read(cx)
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("not available in preview")
            )
        });
        assert_eq!(snapshot(&f.iced()), before);
    }
}
