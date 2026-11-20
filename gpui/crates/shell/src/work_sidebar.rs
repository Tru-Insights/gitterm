//! Standalone Iced Work sidebar port. The host owns the core reducer, executes
//! its effects, refreshes workspace facts, and subscribes to `WorkSidebarOutcome`.
//! This view only owns disclosure state and an unsaved run-command draft.
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
};

use gitterm_core::{
    chat_facts::ChildKind,
    chats,
    work::{
        state::{Pane, State},
        summary::{self, BranchStatus, Coordinator, Focus},
    },
};
use gpui::{Entity, EventEmitter, Subscription, Window, div, prelude::*, px, rgb};
use gpui_component::{
    Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};

/// Workspace/repository facts unavailable from the read-only Work reducer.
/// Paths are the same canonical repo paths supplied to the core summary.
#[derive(Clone, Debug)]
pub struct WorkspaceFacts {
    pub id: u64,
    pub name: String,
    pub dir: PathBuf,
    pub home: PathBuf,
    pub local: bool,
    pub repos: Vec<RepoFacts>,
    pub removed: Vec<PathBuf>,
    pub auto_added: Vec<PathBuf>,
    pub open_chats: HashSet<String>,
    pub owned_chats: Vec<String>,
    pub chat_owners: HashMap<String, String>,
    /// Host counts running tasks and chats, excluding chats counted as tasks.
    pub running_by_repo: HashMap<PathBuf, usize>,
    pub selected_checkout: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct RepoFacts {
    pub dir: PathBuf,
    pub local: bool,
    pub run_command: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkSidebarOutcome {
    OpenChat {
        workspace: u64,
        chat: String,
    },
    Focus {
        workspace: u64,
        focus: Focus,
    },
    AddRepo,
    /// A listed repo is removed; an unlisted touched repo is hidden.
    RemoveRepo {
        workspace: u64,
        repo: PathBuf,
    },
    UndoAutoRepo {
        workspace: u64,
        repos: Vec<PathBuf>,
    },
    EditRunCommand {
        repo: PathBuf,
        command: Option<String>,
    },
    DismissAutoRepos {
        workspace: u64,
    },
    RunRepoCommand {
        repo: PathBuf,
    },
    OpenReview {
        checkout: PathBuf,
        vs_base: bool,
    },
    OpenEditor {
        dir: PathBuf,
    },
    GoToChatOwner {
        chat: String,
    },
    MakeWorkspace {
        chat: String,
    },
    Refresh {
        workspace: u64,
    },
    OpenFind,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Fold {
    Repo(PathBuf),
    Branch(PathBuf),
    Chat(String),
    Focus,
    Older,
    Also,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Emit(WorkSidebarOutcome),
    Toggle(Fold),
    Edit(PathBuf),
    DismissAuto,
}

#[derive(Clone, Debug)]
struct Control {
    label: String,
    action: Action,
}

#[derive(Default)]
struct Row {
    label: String,
    badges: Vec<String>,
    age: Option<String>,
    details: Vec<String>,
    action: Option<Action>,
    run: Option<Control>,
    controls: Vec<Control>,
    detail_controls: Vec<Control>,
    fold: Option<Fold>,
    expanded: bool,
    selected: bool,
    quiet: bool,
    heading: bool,
    indent: bool,
    editor: Option<PathBuf>,
}

impl Row {
    fn text(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ..Self::default()
        }
    }

    fn control(&mut self, label: impl Into<String>, action: Action) {
        self.controls.push(Control {
            label: label.into(),
            action,
        });
    }
}

pub struct WorkSidebar {
    workspace: WorkspaceFacts,
    /// Shared with the host; no summary, polling, git or ownership logic here.
    state: Rc<RefCell<State>>,
    open: HashSet<Fold>,
    dismissed_auto: Vec<PathBuf>,
    editing: Option<(PathBuf, Entity<InputState>)>,
    edit_subscription: Option<Subscription>,
}

impl WorkSidebar {
    pub fn new(workspace: WorkspaceFacts, state: Rc<RefCell<State>>) -> Self {
        Self {
            workspace,
            state,
            open: HashSet::new(),
            dismissed_auto: vec![],
            editing: None,
            edit_subscription: None,
        }
    }

    /// Call after the host reduces Work intents or changes workspace facts.
    /// Retain one entity per workspace so disclosures and drafts survive switches.
    pub fn refresh(&mut self, workspace: WorkspaceFacts, cx: &mut Context<Self>) {
        assert_eq!(workspace.id, self.workspace.id, "one sidebar per workspace");
        if workspace.auto_added != self.workspace.auto_added {
            self.dismissed_auto.clear();
        }
        self.workspace = workspace;
        cx.notify();
    }

    fn rows(&self) -> Vec<Row> {
        let state = self.state.borrow();
        let pane = state.pane(self.workspace.id);
        let mut title = Row::text(&self.workspace.name);
        title.heading = true;
        title.control(
            "↻",
            Action::Emit(WorkSidebarOutcome::Refresh {
                workspace: self.workspace.id,
            }),
        );
        let mut rows = vec![title];
        self.repo_rows(pane, &mut rows);
        let Some(pane) = pane else {
            rows.push(Row::text("Loading…"));
            return rows;
        };
        let summary = &pane.summary;
        if summary.owned {
            let roots = if self.workspace.owned_chats.is_empty() {
                summary.coordinators.len()
            } else {
                self.workspace.owned_chats.len()
            };
            rows.push(Row::text(format!(
                "owns {roots} chat{} · new chats start in {}",
                if roots == 1 { "" } else { "s" },
                summary::folder_name(&self.workspace.dir)
            )));
        } else if !summary.coordinators.is_empty() {
            let mut focus = Row::text(format!(
                "Focus: {}",
                summary::shorten(&summary.focus_label(), 46)
            ));
            focus.fold = Some(Fold::Focus);
            focus.expanded = self.open.contains(&Fold::Focus);
            focus.action = Some(Action::Toggle(Fold::Focus));
            rows.push(focus);
            if self.open.contains(&Fold::Focus) {
                for (label, focus) in [
                    ("Most recently active chat".into(), Focus::Latest),
                    (
                        format!(
                            "All recent work · {} chats, last 7 days",
                            summary.recent_count()
                        ),
                        Focus::AllRecent,
                    ),
                ] {
                    let mut row = Row::text(label);
                    row.selected = summary.focus == focus;
                    row.action = Some(Action::Emit(WorkSidebarOutcome::Focus {
                        workspace: self.workspace.id,
                        focus,
                    }));
                    rows.push(row);
                }
                rows.push(Row::text(
                    "Repos and Issues show this work; or click a chat in Chats",
                ));
            }
        }
        if !summary.coordinators.is_empty() {
            let mut heading = Row::text("Chats");
            heading.heading = true;
            heading.badges.push(if summary.owned {
                summary.coordinators.len().to_string()
            } else if summary.recent_count() == 0 {
                "none in the last 7 days".into()
            } else {
                format!("{} in the last 7 days", summary.recent_count())
            });
            rows.push(heading);
            if summary.owned {
                for chat in &summary.coordinators {
                    rows.push(self.chat_row(chat, false, true, None));
                }
            } else {
                let recent: Vec<_> = summary.coordinators.iter().filter(|c| c.recent).collect();
                for chat in recent.iter().take(12) {
                    rows.push(self.chat_row(chat, summary.focused.contains(&chat.id), false, None));
                }
                Self::more_row(recent.len(), 12, &mut rows);
                let older: Vec<_> = summary.coordinators.iter().filter(|c| !c.recent).collect();
                if !older.is_empty() {
                    let expanded = self.open.contains(&Fold::Older);
                    let mut row = Row::text(format!("{} older {}", older.len(), marker(expanded)));
                    row.action = Some(Action::Toggle(Fold::Older));
                    rows.push(row);
                    if expanded {
                        for chat in older.iter().take(20) {
                            rows.push(self.chat_row(
                                chat,
                                summary.focused.contains(&chat.id),
                                false,
                                None,
                            ));
                        }
                        Self::more_row(older.len(), 20, &mut rows);
                    }
                }
            }
        }
        if !summary.also_worked.is_empty() {
            let mut also = Row::text("Also worked here");
            also.heading = true;
            also.badges.push(summary.also_worked.len().to_string());
            also.fold = Some(Fold::Also);
            also.expanded = self.open.contains(&Fold::Also);
            also.action = Some(Action::Toggle(Fold::Also));
            rows.push(also);
            if self.open.contains(&Fold::Also) {
                for item in summary.also_worked.iter().take(12) {
                    rows.push(self.chat_row(
                        &item.coordinator,
                        summary.focused.contains(&item.coordinator.id),
                        summary.owned,
                        Some(item.edits_here),
                    ));
                }
            }
        }
        for error in &pane.errors {
            rows.push(Row::text(summary::shorten(error, 200)));
        }
        rows
    }

    fn more_row(count: usize, limit: usize, rows: &mut Vec<Row>) {
        if count > limit {
            let mut row = Row::text(format!("… {} more in Find", count - limit));
            row.action = Some(Action::Emit(WorkSidebarOutcome::OpenFind));
            rows.push(row);
        }
    }

    fn repo_rows(&self, pane: Option<&Pane>, rows: &mut Vec<Row>) {
        let ws = &self.workspace;
        let listed: Vec<_> = ws.repos.iter().map(|repo| repo.dir.clone()).collect();
        let groups = summary::merge_repo_groups(
            &listed,
            &ws.removed,
            pane.map_or(&[], |p| p.summary.repos.as_slice()),
            pane.map_or(&[], |p| p.branches.as_slice()),
        );
        let mut header = Row::text("Repos");
        header.heading = true;
        header.badges.push(groups.len().to_string());
        if ws.local {
            header.control("+ Add repo…", Action::Emit(WorkSidebarOutcome::AddRepo));
        }
        rows.push(header);
        if !ws.auto_added.is_empty() && self.dismissed_auto != ws.auto_added {
            let names: Vec<_> = ws
                .auto_added
                .iter()
                .map(|dir| summary::folder_name(dir))
                .collect();
            let mut note = Row::text(format!(
                "{} added to this workspace's repos",
                names.join(", ")
            ));
            note.control(
                "Undo",
                Action::Emit(WorkSidebarOutcome::UndoAutoRepo {
                    workspace: ws.id,
                    repos: ws
                        .auto_added
                        .iter()
                        .filter(|dir| **dir != ws.home)
                        .cloned()
                        .collect(),
                }),
            );
            note.control("×", Action::DismissAuto);
            rows.push(note);
        }
        for group in groups {
            let repo = ws.repos.iter().find(|repo| repo.dir == group.dir);
            let local = repo.filter(|repo| repo.local);
            let (gone, live): (Vec<_>, Vec<_>) =
                group.rows.into_iter().partition(|row| row.owned.gone);
            let empty_note = (live.is_empty() && gone.is_empty()).then(|| {
                if group.edits.is_none() {
                    "no edits yet"
                } else if pane.is_some_and(|p| p.last_full_refresh.is_none()) {
                    "reading…"
                } else {
                    "no live checkout of this work"
                }
            });
            let fold = Fold::Repo(group.dir.clone());
            let expanded = self.open.contains(&fold);
            let mut row = Row::text(summary::folder_name(&group.dir));
            row.quiet = empty_note.is_some() && group.edits.is_none();
            row.action = Some(Action::Toggle(fold.clone()));
            row.expanded = expanded;
            let removable = group.dir != ws.home;
            if local.is_some() || !live.is_empty() || !gone.is_empty() || removable {
                row.fold = Some(fold);
            }
            if let Some(repo) = local {
                if self
                    .editing
                    .as_ref()
                    .is_some_and(|(dir, _)| *dir == group.dir)
                {
                    row.editor = Some(group.dir.clone());
                } else if let Some(command) = &repo.run_command {
                    row.run = Some(Control {
                        label: format!("▶ {command}"),
                        action: Action::Emit(WorkSidebarOutcome::RunRepoCommand {
                            repo: group.dir.clone(),
                        }),
                    });
                }
                if expanded {
                    row.control(
                        "Open in editor",
                        Action::Emit(WorkSidebarOutcome::OpenEditor {
                            dir: group.dir.clone(),
                        }),
                    );
                    if row.editor.is_none() {
                        row.control(
                            if repo.run_command.is_some() {
                                "✎"
                            } else {
                                "+ run command"
                            },
                            Action::Edit(group.dir.clone()),
                        );
                    }
                }
            }
            if expanded {
                if let Some(oldest) = live.iter().map(|row| row.checked_at).min() {
                    row.age = Some(summary::age_label(oldest));
                }
                if removable {
                    row.control(
                        "×",
                        Action::Emit(WorkSidebarOutcome::RemoveRepo {
                            workspace: ws.id,
                            repo: group.dir.clone(),
                        }),
                    );
                }
            }
            let mut facts = vec![summary::repo_facts(
                group.dir == ws.home,
                group.edits,
                empty_note,
            )];
            if !live.is_empty() {
                facts.push(format!(
                    "{} branch{}",
                    live.len(),
                    if live.len() == 1 { "" } else { "es" }
                ));
            }
            let prs = live.iter().filter(|row| row.pr_label().is_some()).count();
            if prs > 0 {
                facts.push(format!("{prs} PR{}", if prs == 1 { "" } else { "s" }));
            }
            if let Some(count) = ws
                .running_by_repo
                .get(&group.dir)
                .filter(|count| **count > 0)
            {
                facts.push(format!("{count} running"));
            }
            row.details.push(
                facts
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · "),
            );
            rows.push(row);
            if expanded {
                for branch in live {
                    rows.push(self.branch_row(branch));
                }
                if !gone.is_empty() {
                    let names: Vec<_> = gone
                        .iter()
                        .map(|row| row.owned.branch.as_deref().unwrap_or("(detached)"))
                        .collect();
                    rows.push(Row::text(format!(
                        "▸ {} folder{} gone · {}",
                        gone.len(),
                        if gone.len() == 1 { "" } else { "s" },
                        summary::shorten(&names.join(", "), 60)
                    )));
                }
            }
        }
    }

    fn branch_row(&self, branch: &BranchStatus) -> Row {
        let fold = Fold::Branch(branch.owned.checkout.clone());
        let expanded = self.open.contains(&fold);
        let mut row = Row::text(summary::shorten(
            branch.owned.branch.as_deref().unwrap_or("(detached)"),
            40,
        ));
        row.indent = true;
        row.selected = self.workspace.selected_checkout.as_ref() == Some(&branch.owned.checkout);
        row.fold = Some(fold);
        row.expanded = expanded;
        row.action = Some(Action::Emit(WorkSidebarOutcome::OpenReview {
            checkout: branch.owned.checkout.clone(),
            vs_base: branch.status.as_ref().is_some_and(|s| s.files == 0) && branch.base.is_some(),
        }));
        if let Some(fact) = summary::branch_fact(branch.status.as_ref()) {
            row.badges.push(fact.label());
        }
        if expanded {
            let mut parts = vec![summary::short_checkout(
                &branch.owned,
                dirs::home_dir().as_deref(),
            )];
            match (summary::vs_base_label(branch), &branch.base) {
                (Some(label), _) => parts.push(label),
                (None, None) => parts.push("no base".into()),
                _ => {}
            }
            if let Some(status) = &branch.status {
                parts.push(summary::push_label(status.push_state()));
            }
            parts.extend(branch.pr_label());
            parts.push(branch.owner_label());
            if let Some(at) = branch.last_activity() {
                parts.push(chats::format_age(at));
            }
            row.details.push(parts.join(" · "));
            row.detail_controls.push(Control {
                label: "Open in editor".into(),
                action: Action::Emit(WorkSidebarOutcome::OpenEditor {
                    dir: branch.owned.checkout.clone(),
                }),
            });
        }
        if let Some(error) = &branch.error {
            row.details.push(summary::shorten(error, 160));
        }
        row
    }

    fn chat_row(
        &self,
        chat: &Coordinator,
        focused: bool,
        owning: bool,
        edits_here: Option<u64>,
    ) -> Row {
        let fold = Fold::Chat(chat.id.clone());
        let expanded = self.open.contains(&fold);
        let mut row = Row::text(format!("✦ {}", summary::shorten(&chat.title, 70)));
        row.fold = Some(fold);
        row.expanded = expanded;
        row.selected = focused;
        row.action = Some(Action::Emit(if owning {
            WorkSidebarOutcome::OpenChat {
                workspace: self.workspace.id,
                chat: chat.id.clone(),
            }
        } else {
            WorkSidebarOutcome::Focus {
                workspace: self.workspace.id,
                focus: Focus::Coordinator(chat.id.clone()),
            }
        }));
        if self.workspace.open_chats.contains(&chat.id) {
            row.badges.push("open".into());
        }
        if let Some(edits) = edits_here {
            row.badges.push(format!(
                "{edits} edit{} here",
                if edits == 1 { "" } else { "s" }
            ));
        }
        if let Some(fan_out) = &chat.fan_out {
            row.badges.push(summary::shorten(fan_out, 28));
        }
        row.badges.push(chats::format_age(chat.active));
        if expanded {
            row.control(
                "↗",
                Action::Emit(WorkSidebarOutcome::OpenChat {
                    workspace: self.workspace.id,
                    chat: chat.id.clone(),
                }),
            );
            if let Some(latest) = &chat.latest {
                row.details
                    .push(format!("↳ {}", summary::shorten(latest, 90)));
            }
            for child in chat.children.iter().take(4) {
                let kind = match child.kind {
                    Some(ChildKind::Worker) => "worker",
                    Some(ChildKind::Review) => "review",
                    Some(ChildKind::Consult) => "consult",
                    Some(ChildKind::Subagent) => "subagent",
                    None => "chat",
                };
                row.details.push(format!(
                    "   {kind} · {} · {}",
                    summary::shorten(&child.title, 50),
                    chats::format_age(child.mtime)
                ));
            }
            if chat.children.len() > 4 {
                row.details
                    .push(format!("   … {} more", chat.children.len() - 4));
            }
        }
        let show_action = if owning {
            !self.workspace.owned_chats.contains(&chat.id)
        } else {
            focused
        };
        if show_action && (expanded || focused) {
            if let Some(owner) = self.workspace.chat_owners.get(&chat.id) {
                row.detail_controls.push(Control {
                    label: format!("→ Go to {}", summary::shorten(owner, 30)),
                    action: Action::Emit(WorkSidebarOutcome::GoToChatOwner {
                        chat: chat.id.clone(),
                    }),
                });
            } else {
                row.detail_controls.push(Control {
                    label: "⊞ Make workspace".into(),
                    action: Action::Emit(WorkSidebarOutcome::MakeWorkspace {
                        chat: chat.id.clone(),
                    }),
                });
            }
        }
        row
    }

    fn activate(&mut self, action: Action) -> Option<WorkSidebarOutcome> {
        match action {
            Action::Emit(WorkSidebarOutcome::RemoveRepo { ref repo, .. })
                if *repo == self.workspace.home =>
            {
                None
            }
            Action::Emit(outcome) => Some(outcome),
            Action::Toggle(fold) => {
                if !self.open.remove(&fold) {
                    self.open.insert(fold);
                }
                None
            }
            Action::DismissAuto => {
                self.dismissed_auto = self.workspace.auto_added.clone();
                Some(WorkSidebarOutcome::DismissAutoRepos {
                    workspace: self.workspace.id,
                })
            }
            Action::Edit(_) => None,
        }
    }

    fn dispatch(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Action::Edit(dir) = action {
            let Some(repo) = self
                .workspace
                .repos
                .iter()
                .find(|repo| repo.dir == dir && repo.local)
            else {
                return;
            };
            let value = repo.run_command.clone().unwrap_or_default();
            let input = cx.new(|cx| {
                let mut input = InputState::new(window, cx).placeholder("npm run dev");
                input.set_value(value, window, cx);
                input
            });
            self.edit_subscription =
                Some(
                    cx.subscribe_in(&input, window, |this, input, event, _, cx| {
                        if matches!(event, InputEvent::PressEnter { .. }) {
                            let value = input.read(cx).value().to_string();
                            if let Some((repo, _)) = this.editing.take() {
                                cx.emit(Self::run_command_outcome(repo, &value));
                            }
                        }
                        cx.notify();
                    }),
                );
            input.update(cx, |input, cx| input.focus(window, cx));
            self.editing = Some((dir, input));
        } else if let Some(outcome) = self.activate(action) {
            cx.emit(outcome);
        }
        cx.notify();
    }

    fn run_command_outcome(repo: PathBuf, value: &str) -> WorkSidebarOutcome {
        let command = value.trim();
        WorkSidebarOutcome::EditRunCommand {
            repo,
            command: (!command.is_empty()).then(|| command.to_string()),
        }
    }

    fn button(&self, id: String, control: Control, cx: &Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::current(cx);
        let color = match control.action {
            Action::Emit(WorkSidebarOutcome::RunRepoCommand { .. }) => p.green,
            Action::Edit(_)
            | Action::Toggle(_)
            | Action::Emit(WorkSidebarOutcome::RemoveRepo { .. }) => p.muted,
            _ => p.blue,
        };
        let tooltip = match control.label.as_str() {
            "↗" => "Open chat",
            "×" => "Remove or dismiss",
            "✎" => "Edit run command",
            "▸" => "Expand",
            "▾" => "Collapse",
            "↻" => "Refresh",
            _ => &control.label,
        }
        .to_string();
        Button::new(id)
            .ghost()
            .compact()
            .xsmall()
            .px_1()
            .text_size(px(10.))
            .text_color(rgb(color))
            .tooltip(tooltip)
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.dispatch(control.action.clone(), window, cx);
            }))
            .label(control.label)
    }
}

fn marker(expanded: bool) -> &'static str {
    if expanded { "▾" } else { "▸" }
}

impl EventEmitter<WorkSidebarOutcome> for WorkSidebar {}

impl Render for WorkSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::current(cx);
        let mut body = div()
            .id("work-sidebar")
            .debug_selector(|| "work-sidebar".into())
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_2()
            .bg(rgb(p.base))
            .text_color(rgb(p.text))
            .text_size(px(12.))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.editing.is_some() {
                    this.editing = None;
                    this.edit_subscription = None;
                    cx.stop_propagation();
                    cx.notify();
                }
            }));
        for (index, row) in self.rows().into_iter().enumerate() {
            let repo = matches!(row.fold, Some(Fold::Repo(_)));
            let mut label = div()
                .id(format!("work-label-{index}"))
                .min_w_0()
                .child(row.label);
            if row.quiet {
                label = label.text_color(rgb(p.muted));
            }
            if row.heading || repo {
                label = label.font_weight(gpui::FontWeight::BOLD);
            }
            if row.indent {
                label = label.font_family("Menlo");
            }
            let mut line = div().flex().items_center().gap_1().child(label);
            if let Some(run) = row.run {
                line = line.child(self.button(format!("work-run-{index}"), run, cx));
            }
            if row.editor.is_some()
                && let Some((_, input)) = &self.editing
            {
                line = line.child(
                    div()
                        .id("work-run-command-input")
                        .w(px(160.))
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(Input::new(input)),
                );
            }
            if !row.heading {
                line = line.child(div().flex_1());
            }
            for badge in row.badges {
                let color = if badge == "open" {
                    rgb(p.green).into()
                } else if badge.contains("dirty") {
                    gpui_component::Theme::global(cx).warning
                } else {
                    rgb(p.muted).into()
                };
                line = line.child(div().text_size(px(10.)).text_color(color).child(badge));
            }
            if row.heading {
                line = line.child(div().flex_1());
            }
            let mut age = row.age;
            for (control_index, control) in row.controls.into_iter().enumerate() {
                if matches!(
                    control.action,
                    Action::Emit(WorkSidebarOutcome::RemoveRepo { .. })
                ) && let Some(age) = age.take()
                {
                    line = line.child(
                        div()
                            .font_family("Menlo")
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child(age),
                    );
                }
                line = line.child(self.button(
                    format!("work-control-{index}-{control_index}"),
                    control,
                    cx,
                ));
            }
            if let Some(age) = age {
                line = line.child(
                    div()
                        .font_family("Menlo")
                        .text_size(px(10.))
                        .text_color(rgb(p.muted))
                        .child(age),
                );
            }
            if let Some(fold) = row.fold {
                let label = if fold == Fold::Focus {
                    if row.expanded { "▴" } else { "▾" }
                } else {
                    marker(row.expanded)
                };
                line = line.child(self.button(
                    format!("work-fold-{index}"),
                    Control {
                        label: label.into(),
                        action: Action::Toggle(fold),
                    },
                    cx,
                ));
            }
            let mut block = div()
                .w_full()
                .flex()
                .flex_col()
                .rounded_md()
                .px_1()
                .py_1()
                .child(line);
            if row.selected {
                block = block.bg(rgb(p.surface));
            }
            if row.indent {
                block = block.ml_2();
            }
            for detail in row.details {
                let mut detail = div()
                    .text_size(px(10.))
                    .text_color(rgb(p.muted))
                    .child(detail);
                if repo {
                    detail = detail.font_family("Menlo");
                }
                block = block.child(detail);
            }
            for (control_index, control) in row.detail_controls.into_iter().enumerate() {
                block = block.child(div().flex().child(self.button(
                    format!("work-detail-{index}-{control_index}"),
                    control,
                    cx,
                )));
            }
            body = if let Some(action) = row.action {
                body.child(
                    Button::new(format!("work-row-{index}"))
                        .ghost()
                        .compact()
                        .w_full()
                        .h_auto()
                        .p_0()
                        .text_color(rgb(p.text))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.dispatch(action.clone(), window, cx)
                        }))
                        .child(block),
                )
            } else {
                body.child(block)
            };
        }
        body
    }
}

#[cfg(test)]
#[path = "work_sidebar_tests.rs"]
mod tests;
