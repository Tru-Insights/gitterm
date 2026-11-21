//! Iced's in-app folder picker (TRU-149): machine chips, path field, recent
//! folders and a folder listing for this Mac or a configured remote.
//!
//! Listing is read-only (local `read_dir`, or the remote agent through the
//! host's `RemoteClient`). The picker never acts on a folder: Confirm,
//! NativeDialog, ConnectMachine and Cancel go to the host as outcomes.

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
        config::RemoteAgentConfig,
        source::{FilesState, SourceDirListing, SourcePath, local_list_dir},
    },
};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px, rgb, rgba,
};
use gpui_component::{
    Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
};

use crate::{
    remote_client::{PreviewSourceHost, RemoteClient},
    theme::{Palette, UI_FONT_SIZE},
    workspace_sheet::FolderPickerPurpose,
};

/// None = this Mac.
pub type Machine = Option<String>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderPickerOutcome {
    Cancel,
    /// "Use this folder": the picker is done; the host acts on `dir`.
    Confirm {
        purpose: FolderPickerPurpose,
        remote_id: Machine,
        dir: String,
    },
    /// "Use macOS dialog…": the host closes the picker and opens the OS picker.
    NativeDialog {
        purpose: FolderPickerPurpose,
    },
    /// "Connect a machine…": the connection form opens over the picker.
    ConnectMachine,
}

/// Host facts the picker reads; homes and last-chosen folders outlive it.
pub struct PickerFacts {
    pub remotes: Vec<RemoteAgentConfig>,
    pub client: Arc<dyn RemoteClient>,
    pub homes: Rc<RefCell<HashMap<Machine, String>>>,
    pub last_dirs: Rc<RefCell<HashMap<Machine, String>>>,
    pub recents: HashMap<Machine, Vec<PathBuf>>,
    /// Where to open when no folder was chosen yet on the machine.
    pub start_hint: Option<String>,
}

const MAX_RECENT_FOLDERS: usize = 8;

pub struct FolderPicker {
    purpose: FolderPickerPurpose,
    facts: PickerFacts,
    remote_id: Machine,
    files: FilesState,
    path: Entity<InputState>,
    path_input: String,
    path_error: Option<String>,
    /// Any navigation supersedes a pending typed-path check.
    typed_seq: u64,
    /// Opened at "/" before the remote's home was known: the handshake
    /// that reports it moves the picker there unless the user navigated.
    awaiting_home: bool,
    _subscription: Subscription,
}

impl EventEmitter<FolderPickerOutcome> for FolderPicker {}

impl FolderPicker {
    pub fn new(
        purpose: FolderPickerPurpose,
        remote_id: Machine,
        facts: PickerFacts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = cx.new(|cx| {
            let input = InputState::new(window, cx).placeholder("/path/to/folder or ~/folder");
            input.focus(window, cx);
            input
        });
        let subscription =
            cx.subscribe_in(
                &path,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::Change => {
                        let value = input.read(cx).value().to_string();
                        if value != this.path_input {
                            this.path_input = value;
                            this.path_error = None;
                            cx.notify();
                        }
                    }
                    InputEvent::PressEnter { .. } => this.submit_path(window, cx),
                    _ => {}
                },
            );
        let mut picker = Self {
            purpose,
            facts,
            remote_id: None,
            files: FilesState::at(SourcePath::Local(PathBuf::from("/"))),
            path,
            path_input: String::new(),
            path_error: None,
            typed_seq: 0,
            awaiting_home: false,
            _subscription: subscription,
        };
        picker.select_machine(remote_id, window, cx);
        picker
    }

    pub fn machine(&self) -> &Machine {
        &self.remote_id
    }

    pub fn files(&self) -> &FilesState {
        &self.files
    }

    pub fn path_error(&self) -> Option<&str> {
        self.path_error.as_deref()
    }

    pub fn set_recents(&mut self, recents: HashMap<Machine, Vec<PathBuf>>, cx: &mut Context<Self>) {
        self.facts.recents = recents;
        cx.notify();
    }

    /// The connection form closed: list any machine it added.
    pub fn set_remotes(&mut self, remotes: Vec<RemoteAgentConfig>, cx: &mut Context<Self>) {
        self.facts.remotes = remotes;
        cx.notify();
    }

    fn home(&self, machine: &Machine) -> Option<String> {
        self.facts.homes.borrow().get(machine).cloned()
    }

    fn remote(&self, id: &str) -> Option<RemoteAgentConfig> {
        self.facts.remotes.iter().find(|r| r.id == id).cloned()
    }

    fn path_for(machine: &Machine, dir: String) -> SourcePath {
        match machine {
            None => SourcePath::Local(PathBuf::from(dir)),
            Some(_) => SourcePath::Remote(dir),
        }
    }

    fn set_path_input(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        self.path_input = value.clone();
        self.path
            .update(cx, |input, cx| input.set_value(value, window, cx));
    }

    /// Point the picker at a machine and list its start directory.
    pub fn select_machine(
        &mut self,
        remote_id: Machine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let last = self.facts.last_dirs.borrow().get(&remote_id).cloned();
        let home = self.home(&remote_id);
        let start = rules::start_dir(
            last.as_deref(),
            self.facts.start_hint.as_deref(),
            home.as_deref(),
        );
        self.awaiting_home = last.is_none() && home.is_none() && remote_id.is_some();
        if let Some(remote) = remote_id
            .as_deref()
            .filter(|_| home.is_none())
            .and_then(|id| self.remote(id))
        {
            self.learn_home(remote, window, cx);
        }
        self.remote_id = remote_id.clone();
        // Keep `files` (and its seq) so a listing still in flight for the
        // previous machine is dropped, not applied here.
        self.files.entries.clear();
        self.files.parent = None;
        self.path_error = None;
        self.set_path_input(start.clone(), window, cx);
        self.list(Self::path_for(&remote_id, start), false, window, cx);
        cx.notify();
    }

    fn learn_home(
        &mut self,
        remote: RemoteAgentConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.facts.client.clone();
        let id = remote.id.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.handshake(&remote) })
                .await;
            _ = this.update_in(cx, |this, window, cx| match result {
                Ok(handshake) if !handshake.home_dir.is_empty() => {
                    this.facts
                        .homes
                        .borrow_mut()
                        .insert(Some(id.clone()), handshake.home_dir.clone());
                    if this.awaiting_home && this.remote_id.as_deref() == Some(id.as_str()) {
                        this.awaiting_home = false;
                        this.list(SourcePath::Remote(handshake.home_dir), false, window, cx);
                    }
                    cx.notify();
                }
                Ok(_) => {}
                Err(error) => eprintln!("[picker] handshake with {id} failed: {error}"),
            });
        })
        .detach();
    }

    /// List `dir`. Plain navigation moves the picker now; a typed path is
    /// only checked, and moves it once it lists cleanly.
    fn list(&mut self, dir: SourcePath, typed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let job: Result<Box<dyn FnOnce() -> SourceDirListing + Send>, String> =
            match (&self.remote_id, &dir) {
                (None, SourcePath::Local(path)) => {
                    let path = path.clone();
                    Ok(Box::new(move || {
                        let root = path.ancestors().last().unwrap_or(&path).to_path_buf();
                        local_list_dir::<PreviewSourceHost>(0, &root, path, true)
                    }))
                }
                (Some(id), SourcePath::Remote(path)) => match self.remote(id) {
                    Some(remote) => {
                        let client = self.facts.client.clone();
                        let path = path.clone();
                        Ok(Box::new(move || client.list_dir(&remote, path)))
                    }
                    None => Err(format!(
                        "Remote \"{id}\" is not configured in remote-agents.json"
                    )),
                },
                (machine, dir) => Err(format!(
                    "internal error: path {} does not belong to machine {machine:?}",
                    dir.display()
                )),
            };
        self.typed_seq = self.typed_seq.wrapping_add(1);
        let seq = if typed {
            self.typed_seq
        } else {
            self.files.begin_request(dir.clone())
        };
        let job = match job {
            Ok(job) => job,
            Err(error) => {
                eprintln!("[picker] cannot list {}: {error}", dir.display());
                if typed {
                    self.path_error = Some(error);
                } else {
                    self.files.loading = false;
                    self.files.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        cx.spawn_in(window, async move |this, cx| {
            let listing = cx.background_executor().spawn(async move { job() }).await;
            _ = this.update_in(cx, |this, window, cx| {
                this.loaded(seq, listing, typed, window, cx)
            });
        })
        .detach();
        cx.notify();
    }

    fn loaded(
        &mut self,
        seq: u64,
        listing: SourceDirListing,
        typed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(error) = &listing.error {
            eprintln!(
                "[picker] listing {} on {:?} failed: {error}",
                listing.current_dir.display(),
                self.remote_id
            );
        }
        if typed {
            if seq != self.typed_seq {
                return;
            }
            if let Some(error) = listing.error {
                self.path_error = Some(error);
                cx.notify();
                return;
            }
            let applied = self.files.begin_request(listing.current_dir.clone());
            self.files.apply(applied, listing);
            self.path_error = None;
        } else if seq == self.files.seq {
            self.files.apply(seq, listing);
        } else {
            return;
        }
        let shown = self.files.dir.display();
        self.set_path_input(shown, window, cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn path_input_for_test(
        &mut self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.path.update(cx, |input, cx| {
            input.set_value(value.to_owned(), window, cx)
        });
    }

    pub fn navigate(&mut self, dir: SourcePath, window: &mut Window, cx: &mut Context<Self>) {
        self.awaiting_home = false;
        self.path_error = None;
        self.list(dir, false, window, cx);
    }

    pub fn up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.files.parent.clone() {
            self.navigate(parent, window, cx);
        }
    }

    pub fn submit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.path_input = self.path.read(cx).value().to_string();
        let home = self.home(&self.remote_id);
        match rules::typed_path(&self.path_input, home.as_deref()) {
            Ok(path) => {
                self.awaiting_home = false;
                let dir = Self::path_for(&self.remote_id, path);
                self.list(dir, true, window, cx);
            }
            Err(error) => {
                self.path_error = Some(error);
                cx.notify();
            }
        }
    }

    fn can_confirm(&self) -> bool {
        !self.files.loading && self.files.error.is_none()
    }

    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        if !self.can_confirm() {
            return;
        }
        cx.emit(FolderPickerOutcome::Confirm {
            purpose: self.purpose,
            remote_id: self.remote_id.clone(),
            dir: self.files.dir.display(),
        });
    }

    fn title(&self) -> &'static str {
        match self.purpose {
            FolderPickerPurpose::NewWorkspace => "New workspace",
            FolderPickerPurpose::OpenFolder => "Open folder",
            FolderPickerPurpose::NewWork => "Add a repo to the new work",
        }
    }
}

/// The OS picker's title for each purpose, as Iced sets it.
pub fn native_dialog_title(purpose: FolderPickerPurpose) -> &'static str {
    match purpose {
        FolderPickerPurpose::NewWorkspace => "Select Workspace Folder",
        FolderPickerPurpose::OpenFolder => "Select Folder",
        FolderPickerPurpose::NewWork => "Add a Repo to the Work",
    }
}

impl Focusable for FolderPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.path.focus_handle(cx)
    }
}

fn shown_name(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| dir.to_string_lossy().to_string())
}

impl Render for FolderPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let small = UI_FONT_SIZE - 1.;
        let dark = p.base == 0x1e1e2e;
        let border = p.border;
        let muted = p.muted;
        let machine = self.remote_id.clone();
        let home = self.home(&machine);
        let text = |value: String, color: u32| {
            div()
                .text_size(px(small))
                .text_color(rgb(color))
                .child(value)
        };
        let row = |id: String| {
            let selector = id.clone();
            div()
                .id(SharedString::from(id))
                .debug_selector(move || selector.clone())
                .flex()
                .items_center()
                .gap(px(8.))
                .w_full()
                .px(px(8.))
                .py(px(3.))
                .rounded(px(3.))
                .cursor_pointer()
                .hover(move |s| s.bg(rgb(p.surface)))
        };

        let mut body = div()
            .debug_selector(|| "folder-picker-card".into())
            .flex()
            .flex_col()
            .gap(px(8.))
            .w(px(548.))
            .p(px(14.))
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(border))
            .bg(rgb(p.mantle))
            .text_size(px(small))
            .text_color(rgb(p.text))
            .child(div().text_size(px(UI_FONT_SIZE)).child(self.title()));

        // Machine selector: this Mac, each configured agent, and the entry
        // that adds a machine. New work picks on this Mac only.
        if self.purpose != FolderPickerPurpose::NewWork {
            let chip = |selector: String, label: String, selected: bool| {
                div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector.clone())
                    .px(px(10.))
                    .py(px(3.))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(if selected { p.blue } else { border }))
                    .text_size(px(small))
                    .text_color(rgb(if selected { p.blue } else { p.subtext0 }))
                    .when(selected, |s| s.bg(rgb(p.surface)))
                    .hover(move |s| s.bg(rgb(p.surface)))
                    .cursor_pointer()
                    .child(label)
            };
            let mut machines = div().flex().flex_wrap().gap(px(6.));
            let mut entries: Vec<(Machine, String)> = vec![(None, "This Mac".into())];
            entries.extend(
                self.facts
                    .remotes
                    .iter()
                    .map(|r| (Some(r.id.clone()), r.name.clone())),
            );
            for (index, (id, name)) in entries.into_iter().enumerate() {
                let selected = id == machine;
                machines = machines.child(
                    chip(format!("folder-picker-machine-{index}"), name, selected).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.select_machine(id.clone(), window, cx)
                        }),
                    ),
                );
            }
            machines = machines.child(
                chip(
                    "folder-picker-connect".into(),
                    "Connect a machine…".into(),
                    false,
                )
                .on_click(cx.listener(|_, _, _, cx| cx.emit(FolderPickerOutcome::ConnectMachine))),
            );
            body = body.child(machines);
        }

        body = body.child(
            div().debug_selector(|| "folder-picker-path".into()).child(
                Input::new(&self.path)
                    .small()
                    .text_size(px(small))
                    .bg(rgb(p.base)),
            ),
        );
        if let Some(error) = &self.path_error {
            body = body.child(
                text(error.clone(), p.red).debug_selector(|| "folder-picker-path-error".into()),
            );
        }

        let recents = self
            .facts
            .recents
            .get(&machine)
            .map(|dirs| {
                dirs.iter()
                    .take(MAX_RECENT_FOLDERS)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        body = body.child(text("Recent".into(), p.subtext0));
        if recents.is_empty() {
            body = body.child(text("No recent folders on this machine yet".into(), muted));
        } else {
            let mut list = div().flex().flex_col().gap(px(1.));
            for (index, dir) in recents.into_iter().enumerate() {
                let full = dir.to_string_lossy().to_string();
                let target = Self::path_for(&machine, full.clone());
                list = list.child(
                    row(format!("folder-picker-recent-{index}"))
                        .child(text(shown_name(&dir), p.text))
                        .child(text(rules::tilde(&full, home.as_deref()), muted))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.navigate(target.clone(), window, cx)
                        })),
                );
            }
            body = body.child(list);
        }

        body = body.child(
            if self.files.loading {
                text("Loading…".into(), p.subtext0)
            } else {
                text(
                    rules::tilde(&self.files.dir.display(), home.as_deref()),
                    p.blue,
                )
            }
            .debug_selector(|| "folder-picker-dir".into()),
        );
        if let Some(error) = &self.files.error {
            body = body
                .child(text(error.clone(), p.red).debug_selector(|| "folder-picker-error".into()));
        }

        let mut list = div()
            .id("folder-picker-list")
            .flex()
            .flex_col()
            .gap(px(1.))
            .h(px(240.))
            .overflow_y_scroll();
        if self.files.parent.is_some() {
            list = list.child(
                row("folder-picker-parent".into())
                    .child(text(".. (parent)".into(), p.subtext0))
                    .on_click(cx.listener(|this, _, window, cx| this.up(window, cx))),
            );
        }
        for (index, entry) in self.files.entries.iter().filter(|e| e.is_dir).enumerate() {
            let target = entry.path.clone();
            let mut line = row(format!("folder-picker-dir-{index}"))
                .child(text(format!("📁 {}", entry.name), p.text));
            if entry.is_git_repo {
                line = line.child(text("git".into(), p.blue));
            }
            list =
                list.child(line.on_click(cx.listener(move |this, _, window, cx| {
                    this.navigate(target.clone(), window, cx)
                })));
        }
        body = body.child(list);

        let purpose = self.purpose;
        let mut actions = div().flex().items_center().gap(px(8.));
        if machine.is_none() {
            actions = actions.child(
                Button::new("folder-picker-native")
                    .debug_selector(|| "folder-picker-native".into())
                    .ghost()
                    .small()
                    .label("Use macOS dialog…")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(FolderPickerOutcome::NativeDialog { purpose })
                    })),
            );
        }
        actions = actions
            .child(div().flex_1())
            .child(
                Button::new("folder-picker-cancel")
                    .debug_selector(|| "folder-picker-cancel".into())
                    .outline()
                    .small()
                    .label("Cancel")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(FolderPickerOutcome::Cancel))),
            )
            .child(
                Button::new("folder-picker-confirm")
                    .debug_selector(|| "folder-picker-confirm".into())
                    .primary()
                    .small()
                    .label("Use this folder")
                    .disabled(!self.can_confirm())
                    .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
            );
        body = body.child(actions);

        div()
            .id("folder-picker")
            .debug_selector(|| "folder-picker".into())
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(if dark { 0x11111bcc } else { 0xdce0e8cc }))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(FolderPickerOutcome::Cancel)))
            .on_key_down(cx.listener(|_, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.emit(FolderPickerOutcome::Cancel);
                    cx.stop_propagation();
                }
            }))
            .child(
                body.id("folder-picker-panel")
                    // A click inside the panel is not a backdrop click.
                    .on_click(|_, _, cx| cx.stop_propagation()),
            )
    }
}
