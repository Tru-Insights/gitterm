//! Standalone Iced Find sidebar and centre preview. No discovery or launch I/O.
//!
//! Host hookup: retain `Rc<RefCell<State>>`, with one selection owner (the tab's
//! stable session UID). Mount `FindSidebar` in the left content region and
//! `FindPreview` in the centre while that owner has a selection. Subscribe to
//! `FindOutcome` on both. Reduce `outcome.intent(state, owner)` and execute core
//! effects asynchronously, returning their exact index/preview stamps. Open
//! focuses the existing conversation; Resume maps Session/Chat/RecreateDirectory
//! to Iced's ResumeChatAsTab/ResumeChatAsChat/ResumeChatRecreateDir behavior.
//! Revalidate ownership, reachability and cwd at action time in the host. On
//! workspace changes update Filter.machine/workspace_root; refresh both entities
//! after reductions or host-fact changes. Closing returns to the host's overview.

use std::{cell::RefCell, collections::HashSet, rc::Rc};

use gitterm_core::{
    chats::{self, ChatBackend, ChatIndexEntry, ChatRef, ChatScope},
    find::state::{Intent, Machine, State},
};
use gpui::{Entity, EventEmitter, Subscription, Window, div, prelude::*, px, rgb};
use gpui_component::{
    Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};

use crate::theme::Palette;

#[cfg(test)]
#[path = "find_sidebar_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeMode {
    Session,
    Chat,
    RecreateDirectory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindOutcome {
    SetQuery(String),
    SetScope(ChatScope),
    SetBackend(Option<ChatBackend>),
    Select { id: ChatRef },
    ClosePreview,
    Open { id: ChatRef },
    Resume { id: ChatRef, mode: ResumeMode },
}

impl FindOutcome {
    /// Translate view input to the existing reducer. Open/Resume are host
    /// navigation/launch operations, never reducer or filesystem work here.
    pub fn intent(&self, state: &State, owner: &str) -> Option<Intent> {
        let mut filter = state.filter().clone();
        match self {
            Self::SetQuery(query) => filter.query = query.clone(),
            Self::SetScope(scope) => filter.scope = *scope,
            Self::SetBackend(backend) => filter.backend = *backend,
            Self::Select { id } => {
                let entry = entry(state, id)?;
                return Some(Intent::Select {
                    owner: owner.into(),
                    chat: id.clone(),
                    path: entry.path.clone(),
                    backend: entry.backend,
                });
            }
            Self::ClosePreview => return Some(Intent::ClosePreview(owner.into())),
            Self::Open { .. } | Self::Resume { .. } => return None,
        }
        Some(Intent::SetFilter(filter))
    }
}

/// In display order: local machine, then configured remotes (including offline
/// ones). The active machine is `State::filter().machine`, not a second filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineFacts {
    pub id: Machine,
    pub name: String,
    pub status: String,
    pub reachable: bool,
    pub claude_chat: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenSession {
    pub id: ChatRef,
    pub title: String,
    pub workspace: String,
    /// Session-strip marker and branch/about text, resolved by the host.
    pub marker: String,
    pub detail: String,
    pub selected: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindFacts {
    pub machines: Vec<MachineFacts>,
    /// Iced's open-tab order: needs-you, working, idle. Independent of filters.
    pub open_sessions: Vec<OpenSession>,
    /// Running remote native chats with no local tab. Resume joins the process.
    pub running_remote: Vec<ChatRef>,
}

impl FindFacts {
    fn machine(&self, id: &Machine) -> Option<&MachineFacts> {
        self.machines.iter().find(|machine| machine.id == *id)
    }

    fn machine_name(&self, id: &Machine) -> String {
        self.machine(id)
            .map(|machine| machine.name.clone())
            .unwrap_or_else(|| id.clone().unwrap_or_else(|| "This Mac".into()))
    }

    fn open(&self, id: &ChatRef) -> Option<&OpenSession> {
        self.open_sessions.iter().find(|session| session.id == *id)
    }
}

fn entry<'a>(state: &'a State, id: &ChatRef) -> Option<&'a ChatIndexEntry> {
    state
        .index(&id.remote_id)?
        .entries
        .iter()
        .find(|e| e.id == id.id)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Control {
    label: String,
    outcome: FindOutcome,
}

#[derive(Debug)]
struct ChatRow {
    id: ChatRef,
    title: String,
    workspace: String,
    age: String,
    backend: ChatBackend,
    details: Vec<String>,
    selected: bool,
    child: bool,
    fold: Option<bool>,
    open: Option<Control>,
}

#[derive(Debug)]
enum Row {
    Machine {
        id: Machine,
        label: String,
        count: usize,
        expanded: bool,
    },
    Group {
        label: String,
        count: usize,
    },
    Chat(ChatRow),
    Note(String),
}

#[derive(Default)]
struct Disclosures {
    machines: HashSet<Machine>,
    roots: HashSet<ChatRef>,
    open_collapsed: bool,
}

fn chat_row(
    entry: &ChatIndexEntry,
    machine: &Machine,
    state: &State,
    owner: &str,
    facts: &FindFacts,
    child: bool,
    fold: Option<bool>,
) -> ChatRow {
    let id = ChatRef::new(machine.as_deref(), &entry.id);
    let open = facts.open(&id);
    let mut details = vec![];
    if let Some(latest) = &entry.latest {
        details.push(format!("↳ {latest}"));
    }
    if let Some(label) = (!state.filter().query.is_empty())
        .then(|| entry.query_match(&state.filter().query.to_lowercase()))
        .flatten()
        .and_then(|matched| matched.label())
    {
        details.push(label);
    }
    if let Some(facts) = &entry.facts {
        let capped = |items: Vec<String>| {
            let mut line = items.iter().take(3).cloned().collect::<Vec<_>>().join(" ");
            if items.len() > 3 {
                line.push_str(&format!(" +{}", items.len() - 3));
            }
            (!line.is_empty()).then_some(line)
        };
        let parts = [
            capped(facts.issue_keys.clone()),
            capped(facts.edited_repo_names()),
            facts.fan_out_line(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !parts.is_empty() {
            details.push(parts.join(" · "));
        }
    }
    let mut meta = vec![];
    if let Some(branch) = &entry.branch {
        meta.push(branch.clone());
    }
    if open.is_none() && entry.possibly_running() {
        meta.push("◐ possibly running".into());
    }
    if entry.is_worktree {
        meta.push("worktree".into());
    }
    if machine.is_some() {
        meta.push(format!("@ {}", facts.machine_name(machine)));
    }
    if entry.dead_cwd {
        meta.push("⚠ dir gone".into());
    }
    if !meta.is_empty() {
        details.push(meta.join(" · "));
    }
    ChatRow {
        id: id.clone(),
        title: entry.title.clone(),
        workspace: entry.group_name(),
        age: chats::format_age(entry.mtime),
        backend: entry.backend,
        details,
        selected: state.selected(owner).is_some_and(|s| s.chat == id),
        child,
        fold,
        open: open.map(|session| Control {
            label: format!("● open in {}", session.workspace),
            outcome: FindOutcome::Open { id },
        }),
    }
}

fn rows(state: &State, owner: &str, facts: &FindFacts, folds: &Disclosures) -> Vec<Row> {
    let filter = state.filter();
    let everywhere = filter.scope == ChatScope::Everywhere;
    let searching = !filter.query.is_empty();
    // Core owns filtering and index order. Partition its result by the host's
    // configured-machine order rather than the reducer's HashMap iteration.
    let visible = state.filtered_entries();
    let machines = if everywhere {
        facts.machines.iter().map(|m| m.id.clone()).collect()
    } else {
        vec![filter.machine.clone()]
    };
    let mut rows = vec![];
    for machine in machines {
        let entries: Vec<_> = visible
            .iter()
            .filter(|(m, _)| *m == machine)
            .map(|(_, e)| *e)
            .collect();
        let expanded = !everywhere || searching || folds.machines.contains(&machine);
        rows.push(Row::Machine {
            id: machine.clone(),
            label: facts.machine_name(&machine).to_uppercase(),
            count: entries.len(),
            expanded,
        });
        if !expanded {
            continue;
        }
        if let Some(index) = state.index(&machine)
            && let Some(error) = &index.error
        {
            rows.push(Row::Note(error.clone()));
        }
        if entries.is_empty() {
            let note = if state.loading_index(&machine) {
                "indexing conversations…"
            } else if everywhere && searching {
                "no matches"
            } else if everywhere {
                "no conversations indexed"
            } else if searching {
                "no matches in this scope"
            } else {
                "no conversations in this scope"
            };
            rows.push(Row::Note(note.into()));
        }
        let all = state
            .index(&machine)
            .map(|i| i.entries.as_slice())
            .unwrap_or_default();
        let folded = chats::fold_under_roots(&entries, all);
        let mut groups: Vec<(&std::path::Path, Vec<_>)> = vec![];
        for row in &folded {
            let root = row.root.group_root();
            if let Some((_, group)) = groups.iter_mut().find(|(key, _)| *key == root) {
                group.push(row);
            } else {
                groups.push((root, vec![row]));
            }
        }
        for (_, group) in groups {
            rows.push(Row::Group {
                label: group[0].root.group_name().to_uppercase(),
                count: group.len(),
            });
            for folded in group {
                let root = folded.root;
                let id = ChatRef::new(machine.as_deref(), &root.id);
                let has_children = !folded.children.is_empty()
                    || root.facts.as_ref().is_some_and(|f| !f.children.is_empty());
                let expanded = has_children
                    && (folds.roots.contains(&id)
                        || searching
                            && (!folded.children.is_empty()
                                || matches!(
                                    root.query_match(&filter.query.to_lowercase()),
                                    Some(chats::QueryMatch::Child(_))
                                )));
                rows.push(Row::Chat(chat_row(
                    root,
                    &machine,
                    state,
                    owner,
                    facts,
                    false,
                    has_children.then_some(expanded),
                )));
                if !expanded {
                    continue;
                }
                let children = if searching {
                    folded.children.clone()
                } else {
                    all.iter()
                        .filter(|e| e.root_id.as_deref() == Some(&root.id))
                        .collect()
                };
                for child in children {
                    rows.push(Row::Chat(chat_row(
                        child, &machine, state, owner, facts, true, None,
                    )));
                }
                if let Some(facts) = &root.facts {
                    let others: Vec<_> = facts
                        .children
                        .iter()
                        .filter(|child| {
                            !all.iter().any(|e| {
                                e.root_id.as_deref() == Some(&root.id) && e.id == child.session_id
                            })
                        })
                        .filter(|child| {
                            !searching
                                || child.title.as_ref().is_some_and(|t| {
                                    t.to_lowercase().contains(&filter.query.to_lowercase())
                                })
                        })
                        .collect();
                    for child in others.iter().rev().take(8) {
                        let kind = match child.kind {
                            gitterm_core::chat_facts::ChildKind::Subagent => "subagent",
                            gitterm_core::chat_facts::ChildKind::Worker => "worker",
                            gitterm_core::chat_facts::ChildKind::Review => "review",
                            gitterm_core::chat_facts::ChildKind::Consult => "consult",
                        };
                        rows.push(Row::Note(format!(
                            "{kind} · {}",
                            child.title.as_deref().unwrap_or("untitled")
                        )));
                    }
                    if others.len() > 8 {
                        rows.push(Row::Note(format!("… {} more", others.len() - 8)));
                    }
                }
            }
        }
    }
    rows
}

fn scopes(state: &State, facts: &FindFacts) -> Vec<Control> {
    [
        ("This workspace".into(), ChatScope::Workspace),
        (
            facts.machine_name(&state.filter().machine),
            ChatScope::Machine,
        ),
        ("Everywhere".into(), ChatScope::Everywhere),
    ]
    .into_iter()
    .map(|(label, scope)| Control {
        label,
        outcome: FindOutcome::SetScope(scope),
    })
    .collect()
}

fn backends() -> Vec<Control> {
    std::iter::once(Control {
        label: "All".into(),
        outcome: FindOutcome::SetBackend(None),
    })
    .chain(ChatBackend::ALL.into_iter().map(|b| Control {
        label: b.label().into(),
        outcome: FindOutcome::SetBackend(Some(b)),
    }))
    .collect()
}

pub struct FindSidebar {
    state: Rc<RefCell<State>>,
    owner: String,
    facts: FindFacts,
    folds: Disclosures,
    query: Entity<InputState>,
    _subscription: Subscription,
}

impl EventEmitter<FindOutcome> for FindSidebar {}

impl FindSidebar {
    pub fn new(
        state: Rc<RefCell<State>>,
        owner: String,
        facts: FindFacts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("search conversations…");
            input.set_value(state.borrow().filter().query.clone(), window, cx);
            input
        });
        let subscription = cx.subscribe(&query, |this, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                let query = input.read(cx).value().to_string();
                if query != this.state.borrow().filter().query {
                    cx.emit(FindOutcome::SetQuery(query));
                }
            }
        });
        Self {
            state,
            owner,
            facts,
            folds: Disclosures::default(),
            query,
            _subscription: subscription,
        }
    }

    pub fn refresh(&mut self, facts: FindFacts, window: &mut Window, cx: &mut Context<Self>) {
        self.facts = facts;
        let query = self.state.borrow().filter().query.clone();
        if self.query.read(cx).value().as_str() != query {
            self.query
                .update(cx, |input, cx| input.set_value(query, window, cx));
        }
        cx.notify();
    }

    fn control(
        &self,
        id: String,
        control: Control,
        active: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let p = Palette::current(cx);
        let selector = id.clone();
        Button::new(id)
            .debug_selector(move || selector.clone())
            .label(control.label)
            .ghost()
            .xsmall()
            .when(active, |b| b.bg(rgb(p.surface)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(control.outcome.clone());
            }))
    }
}

impl Render for FindSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let state = self.state.borrow();
        let mut scope = div().flex().gap_1();
        for (i, control) in scopes(&state, &self.facts).into_iter().enumerate() {
            let active = control.outcome == FindOutcome::SetScope(state.filter().scope);
            scope = scope.child(self.control(format!("find-scope-{i}"), control, active, cx));
        }
        let mut backend = div().flex().gap_1();
        for (i, control) in backends().into_iter().enumerate() {
            let active = control.outcome == FindOutcome::SetBackend(state.filter().backend);
            backend = backend.child(self.control(format!("find-backend-{i}"), control, active, cx));
        }
        let mut list = div()
            .id("find-results")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_2();
        if !self.facts.open_sessions.is_empty() {
            list = list.child(
                Button::new("find-open")
                    .label(format!(
                        "{} OPEN {}",
                        if self.folds.open_collapsed {
                            "▸"
                        } else {
                            "▾"
                        },
                        self.facts.open_sessions.len()
                    ))
                    .ghost()
                    .xsmall()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.folds.open_collapsed = !this.folds.open_collapsed;
                        cx.notify();
                    })),
            );
            if !self.folds.open_collapsed {
                for (i, session) in self.facts.open_sessions.iter().enumerate() {
                    let control = Control {
                        label: format!(
                            "{} {}\n{}{}",
                            session.marker,
                            session.title,
                            session.workspace,
                            if session.detail.is_empty() {
                                String::new()
                            } else {
                                format!(" · {}", session.detail)
                            }
                        ),
                        outcome: FindOutcome::Open {
                            id: session.id.clone(),
                        },
                    };
                    list = list.child(self.control(
                        format!("find-open-{i}"),
                        control,
                        session.selected,
                        cx,
                    ));
                }
            }
        }
        for (i, row) in rows(&state, &self.owner, &self.facts, &self.folds)
            .into_iter()
            .enumerate()
        {
            match row {
                Row::Machine {
                    id,
                    label,
                    count,
                    expanded,
                } => {
                    let status = self
                        .facts
                        .machine(&id)
                        .map(|m| m.status.as_str())
                        .unwrap_or_default();
                    let label = if state.filter().scope == ChatScope::Everywhere {
                        format!(
                            "{} {label} {count} {status}",
                            if expanded { "▾" } else { "▸" }
                        )
                    } else {
                        format!("{label} {status}")
                    };
                    list = list.child(
                        Button::new(format!("find-machine-{i}"))
                            .label(label)
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.folds.machines.remove(&id) {
                                    this.folds.machines.insert(id.clone());
                                }
                                cx.notify();
                            })),
                    );
                }
                Row::Group { label, count } => {
                    list = list.child(
                        div()
                            .py_1()
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child(format!("{label} {count}")),
                    )
                }
                Row::Note(note) => {
                    list = list.child(div().py_1().text_color(rgb(p.muted)).child(note))
                }
                Row::Chat(row) => {
                    let id = row.id.clone();
                    let mut content = div()
                        .id(format!("find-chat-{i}"))
                        .debug_selector(move || format!("find-chat-{i}"))
                        .rounded_md()
                        .p_2()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(p.surface)))
                        .when(row.selected, |s| s.bg(rgb(p.surface)))
                        .when(row.child, |s| s.ml(px(18.)))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(FindOutcome::Select { id: id.clone() })
                        }))
                        .child(
                            div()
                                .flex()
                                .gap_1()
                                .child(
                                    div()
                                        .text_color(backend_color(row.backend, cx))
                                        .child(format!("● {}", row.backend.label())),
                                )
                                .child(row.title),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(p.muted))
                                .child(format!("{} · {}", row.workspace, row.age)),
                        );
                    for detail in row.details {
                        content = content.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(p.muted))
                                .child(detail),
                        );
                    }
                    if let Some(control) = row.open {
                        content = content.child(div().child(self.control(
                            format!("find-focus-{i}"),
                            control,
                            false,
                            cx,
                        )));
                    }
                    if let Some(expanded) = row.fold {
                        let id = row.id;
                        content = content.child(
                            Button::new(format!("find-fold-{i}"))
                                .label(if expanded { "▾" } else { "▸" })
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if !this.folds.roots.remove(&id) {
                                        this.folds.roots.insert(id.clone());
                                    }
                                    cx.notify();
                                })),
                        );
                    }
                    list = list.child(content);
                }
            }
        }
        div()
            .id("find-sidebar")
            .debug_selector(|| "find-sidebar".into())
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .min_h_0()
            .bg(rgb(p.base))
            .text_color(rgb(p.text))
            .text_size(px(12.))
            .child(div().px_2().py_1().child(Input::new(&self.query).small()))
            .child(div().px_2().child(scope))
            .child(div().px_2().child(backend))
            .child(list)
    }
}

fn backend_color(backend: ChatBackend, cx: &gpui::App) -> gpui::Hsla {
    let dark = gpui_component::Theme::global(cx).mode.is_dark();
    rgb(match (backend, dark) {
        (ChatBackend::Claude, true) => 0xfab387,
        (ChatBackend::Claude, false) => 0xfe640b,
        (ChatBackend::Codex, true) => 0x89b4fa,
        (ChatBackend::Codex, false) => 0x1e66f5,
        (ChatBackend::Pi, true) => 0xcba6f7,
        (ChatBackend::Pi, false) => 0x8839ef,
    })
    .into()
}

fn preview_actions(
    entry: &ChatIndexEntry,
    id: &ChatRef,
    facts: &FindFacts,
) -> (Vec<Control>, String) {
    let resume = |label: &str, mode| Control {
        label: label.into(),
        outcome: FindOutcome::Resume {
            id: id.clone(),
            mode,
        },
    };
    if facts.open(id).is_some() {
        return (
            vec![Control {
                label: "Go to Session".into(),
                outcome: FindOutcome::Open { id: id.clone() },
            }],
            "already open in a session — one process per conversation".into(),
        );
    }
    let remote = id.remote_id.is_some();
    let machine = facts.machine(&id.remote_id);
    let name = facts.machine_name(&id.remote_id);
    if remote && machine.is_none_or(|m| !m.reachable) {
        return (
            vec![],
            format!(
                "{name} is unreachable — shown from the cached index; resume when it reconnects"
            ),
        );
    }
    if remote && facts.running_remote.contains(id) {
        return (
            vec![resume("Open Chat", ResumeMode::Chat)],
            format!("● running on {name}"),
        );
    }
    if entry.dead_cwd {
        return if remote {
            (
                vec![],
                "the recorded directory is gone on the remote machine".into(),
            )
        } else {
            (
                vec![resume(
                    "Recreate directory & resume",
                    ResumeMode::RecreateDirectory,
                )],
                "the recorded directory was deleted; resume only works from it".into(),
            )
        };
    }
    let mut controls = vec![resume("Resume as Session", ResumeMode::Session)];
    if entry.backend == ChatBackend::Claude && (!remote || machine.is_some_and(|m| m.claude_chat)) {
        controls.push(resume("Resume as Chat", ResumeMode::Chat));
    }
    let note = if entry.possibly_running() {
        "◐ transcript is still growing — may be running in a terminal GitTerm didn't start".into()
    } else if remote {
        format!("starts an agentd session on {name}")
    } else {
        "resumes in the recorded directory".into()
    };
    (controls, note)
}

/// Companion centre view; subscribe to the same outcomes as the sidebar.
pub struct FindPreview {
    state: Rc<RefCell<State>>,
    owner: String,
    facts: FindFacts,
    raw_expanded: bool,
}

impl EventEmitter<FindOutcome> for FindPreview {}

impl FindPreview {
    pub fn new(state: Rc<RefCell<State>>, owner: String, facts: FindFacts) -> Self {
        Self {
            state,
            owner,
            facts,
            raw_expanded: false,
        }
    }

    pub fn refresh(&mut self, facts: FindFacts, cx: &mut Context<Self>) {
        self.facts = facts;
        cx.notify();
    }
}

impl Render for FindPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let state = self.state.borrow();
        let mut pane = div()
            .id("find-preview")
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(p.base))
            .text_color(rgb(p.text))
            .text_size(px(12.));
        let Some(selection) = state.selected(&self.owner) else {
            return pane;
        };
        let close = Button::new("find-close-preview")
            .debug_selector(|| "find-close-preview".into())
            .label("✕")
            .ghost()
            .small()
            .on_click(cx.listener(|_, _, _, cx| cx.emit(FindOutcome::ClosePreview)));
        let Some(entry) = entry(&state, &selection.chat) else {
            return pane
                .child(div().p_4().child("conversation no longer in the index"))
                .child(close);
        };
        let preview = state.preview(&self.owner);
        let (actions, note) = preview_actions(entry, &selection.chat, &self.facts);
        let mut controls = div().flex().flex_wrap().gap_2();
        for (i, control) in actions.into_iter().enumerate() {
            controls = controls.child(
                Button::new(format!("find-preview-action-{i}"))
                    .debug_selector(move || format!("find-preview-action-{i}"))
                    .label(control.label)
                    .outline()
                    .small()
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(control.outcome.clone()))),
            );
        }
        let mut place = vec![
            self.facts.machine_name(&selection.chat.remote_id),
            entry.group_name(),
        ];
        if let Some(branch) = &entry.branch {
            place.push(branch.clone());
        }
        let count = preview
            .and_then(|p| p.message_count)
            .map(|n| format!(" · {n} messages"))
            .unwrap_or_default();
        pane =
            pane.child(
                div()
                    .px_4()
                    .py_3()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_size(px(15.)).child(entry.title.clone()))
                            .child(div().text_color(backend_color(entry.backend, cx)).child(
                                format!("● {} · {}", entry.backend.label(), place.join(" · ")),
                            ))
                            .child(div().text_color(rgb(p.muted)).child(format!(
                                "{} · {} ago{count}",
                                chats::format_size(entry.size),
                                chats::format_age(entry.mtime)
                            )))
                            .child(format!(
                                "{}{}",
                                entry.cwd.display(),
                                if entry.dead_cwd {
                                    " — directory no longer exists"
                                } else {
                                    ""
                                }
                            ))
                            .child(controls)
                            .child(div().text_color(rgb(p.muted)).child(note)),
                    )
                    .child(close),
            );
        let mut body = div()
            .id("find-preview-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_3();
        body = body.child(
            Button::new("find-preview-conversation")
                .debug_selector(|| "find-preview-conversation".into())
                .label(format!(
                    "{} CONVERSATION {}",
                    if self.raw_expanded { "▾" } else { "▸" },
                    preview
                        .map(|p| format!("last {}", p.messages.len()))
                        .unwrap_or_default()
                ))
                .ghost()
                .xsmall()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.raw_expanded = !this.raw_expanded;
                    cx.notify();
                })),
        );
        if let Some(error) = state.preview_error(&self.owner) {
            body = body.child(div().text_color(rgb(p.red)).child(error.to_string()));
        }
        if self.raw_expanded {
            match preview {
                None if state.preview_error(&self.owner).is_none() => {
                    body = body.child("loading preview…")
                }
                None => {}
                Some(preview) => {
                    if let Some(count) = preview
                        .message_count
                        .filter(|n| *n > preview.messages.len() as u64)
                    {
                        body = body.child(format!(
                            "— {} earlier messages —",
                            count - preview.messages.len() as u64
                        ));
                    }
                    if preview.messages.is_empty() {
                        body = body.child("no renderable messages in the transcript tail");
                    }
                    for message in &preview.messages {
                        body = body.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(9.))
                                        .text_color(if message.is_user {
                                            rgb(p.green).into()
                                        } else {
                                            backend_color(entry.backend, cx)
                                        })
                                        .child(if message.is_user {
                                            "YOU".into()
                                        } else {
                                            entry.backend.label().to_uppercase()
                                        }),
                                )
                                .child(message.text.clone()),
                        );
                    }
                    body =
                        body.child(div().text_color(rgb(p.muted)).child(
                            "end of transcript · read-only preview (resume lands in slice 2)",
                        ));
                }
            }
        }
        pane.child(body)
    }
}

/// Retained per tab, including subscriptions while another tab is selected.
pub(crate) struct FindViews {
    pub(crate) sidebar: Entity<FindSidebar>,
    preview: Entity<FindPreview>,
    _subscriptions: [Subscription; 2],
}

impl crate::Shell {
    fn find_owner_for(
        &self,
        workspace: &crate::WorkspaceDock,
        session: Option<crate::data::SessionId>,
        cx: &gpui::App,
    ) -> String {
        if let Some(session) = session
            && let Some(pane) = workspace.panes.get(&crate::session_pane_id(session))
        {
            if let Some(target) = &pane.read(cx).session {
                return target.session_uid.clone();
            }
            if let Some(saved) = self.owner.read(cx).iced_workspace(workspace.workspace.id)
                && let Some(index) = crate::data::iced_sessions(saved)
                    .iter()
                    .position(|s| s.id == session)
                && let Some(uid) = saved
                    .tabs
                    .get(index)
                    .and_then(|tab| tab.session_uid.as_ref())
            {
                return uid.clone();
            }
            // Legacy imported tabs have no UID or runtime target. Their retained
            // entity identity is stable for this tab's lifetime, never its index.
            return format!("gpui-find-tab-{}", pane.entity_id());
        }
        format!("gpui-find-workspace-{}", workspace.runtime_uid)
    }

    pub(crate) fn find_owner(&self, cx: &gpui::App) -> String {
        let workspace = self.active(cx);
        self.find_owner_for(workspace, workspace.active_session, cx)
    }

    fn find_conversation(
        &self,
        workspace: &crate::WorkspaceDock,
        session: &crate::data::Session,
        cx: &gpui::App,
    ) -> Option<ChatRef> {
        // Remote workspaces are intentionally absent from this local-only host.
        let cwd = self.data.terminal_cwd(workspace.workspace.id, cx)?;
        if let Some(saved) = self.owner.read(cx).iced_workspace(workspace.workspace.id)
            && let Some(index) = crate::data::iced_sessions(saved)
                .iter()
                .position(|s| s.id == session.id)
            && let Some(id) = saved
                .tabs
                .get(index)
                .and_then(|t| t.chat_session_id.as_deref())
        {
            return Some(ChatRef::new(None, id));
        }
        let pane = workspace
            .panes
            .get(&crate::session_pane_id(session.id))?
            .read(cx);
        let crate::pane::PaneContent::PreviewChat(view) = &pane.content else {
            return None;
        };
        self.owner
            .read(cx)
            .chat_history(Some(&cwd))
            .iter()
            .find_map(|saved| {
                let id = saved.session_id.as_deref().unwrap_or(&saved.uid);
                view.read(cx)
                    .matches_chat(id)
                    .then(|| ChatRef::new(None, id))
            })
    }

    fn find_facts(&self, cx: &gpui::App) -> FindFacts {
        let mut sessions = vec![];
        for workspace in &self.workspaces {
            for session in &workspace.sessions {
                let Some(id) = self.find_conversation(workspace, session, cx) else {
                    continue;
                };
                let Some(pane) = workspace.panes.get(&crate::session_pane_id(session.id)) else {
                    continue;
                };
                let pane = pane.read(cx);
                let title = pane.tab_title();
                // shortcut: native request priority/wait facts are private to
                // ChatPane; use exposed title/streaming facts until shared
                // attention accessors are available to this host.
                let needs_you = title.starts_with('✳');
                let working = match &pane.content {
                    crate::pane::PaneContent::PreviewChat(view) => view.read(cx).is_working(),
                    _ => title.trim_start().chars().next().is_some_and(|ch| {
                        matches!(ch, '◐' | '◑' | '◒' | '◓' | '✢' | '✶' | '✻' | '✽' | '·')
                            || ('\u{2800}'..='\u{28ff}').contains(&ch)
                    }),
                };
                sessions.push((
                    if needs_you {
                        0
                    } else if working {
                        1
                    } else {
                        2
                    },
                    OpenSession {
                        id,
                        title,
                        workspace: workspace.workspace.name.clone(),
                        marker: if needs_you {
                            "●"
                        } else if working {
                            "▶"
                        } else {
                            "✦"
                        }
                        .into(),
                        detail: self.data.status(workspace.workspace.id, cx).branch,
                        selected: Some(workspace.workspace.id) == self.active_workspace(cx)
                            && workspace.active_session == Some(session.id),
                    },
                ));
            }
        }
        // Stable sort preserves workspace/tab order among equal attention classes.
        sessions.sort_by_key(|(class, _)| *class);
        FindFacts {
            machines: vec![MachineFacts {
                id: None,
                name: "This Mac".into(),
                status: String::new(),
                reachable: true,
                claude_chat: false,
            }],
            open_sessions: sessions.into_iter().map(|(_, session)| session).collect(),
            running_remote: vec![],
        }
    }

    pub(crate) fn ensure_find_views(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.active_workspace(cx).is_none() {
            return;
        }
        let mut alive = HashSet::new();
        for workspace in &self.workspaces {
            if workspace.sessions.is_empty() {
                alive.insert(self.find_owner_for(workspace, None, cx));
            }
            for session in &workspace.sessions {
                alive.insert(self.find_owner_for(workspace, Some(session.id), cx));
            }
        }
        self.find_views.retain(|owner, _| {
            if alive.contains(owner) {
                true
            } else {
                self.find_state
                    .borrow_mut()
                    .reduce(Intent::ClosePreview(owner.clone()));
                false
            }
        });
        let owner = self.find_owner(cx);
        let facts = self.find_facts(cx);
        if !self.find_views.contains_key(&owner) {
            let sidebar = cx.new(|cx| {
                FindSidebar::new(
                    self.find_state.clone(),
                    owner.clone(),
                    facts.clone(),
                    window,
                    cx,
                )
            });
            let preview =
                cx.new(|_| FindPreview::new(self.find_state.clone(), owner.clone(), facts.clone()));
            let sidebar_owner = owner.clone();
            let preview_owner = owner.clone();
            let subscriptions = [
                cx.subscribe_in(
                    &sidebar,
                    window,
                    move |shell, _, outcome: &FindOutcome, window, cx| {
                        shell.find_outcome(&sidebar_owner, outcome, window, cx)
                    },
                ),
                cx.subscribe_in(
                    &preview,
                    window,
                    move |shell, _, outcome: &FindOutcome, window, cx| {
                        shell.find_outcome(&preview_owner, outcome, window, cx)
                    },
                ),
            ];
            self.find_views.insert(
                owner.clone(),
                FindViews {
                    sidebar,
                    preview,
                    _subscriptions: subscriptions,
                },
            );
        }
        let mut filter = self.find_state.borrow().filter().clone();
        let root = self.data.terminal_cwd(self.active(cx).workspace.id, cx);
        if filter.machine.is_some() || filter.workspace_root != root {
            filter.machine = None;
            filter.workspace_root = root;
            self.reduce_find(Intent::SetFilter(filter), window, cx);
        }
        if self
            .find_views
            .values()
            .any(|views| views.sidebar.read(cx).facts != facts)
        {
            self.refresh_find(window, cx);
        }
        let regions = &self.active(cx).store.regions;
        if regions.sidebar_mode == crate::SidebarMode::Find && !regions.sidebar_collapsed {
            self.reduce_find(
                Intent::LoadIndex {
                    machine: None,
                    now: std::time::Instant::now(),
                    force: false,
                },
                window,
                cx,
            );
        }
    }

    fn refresh_find(&self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let facts = self.find_facts(cx);
        for views in self.find_views.values() {
            views
                .sidebar
                .update(cx, |sidebar, cx| sidebar.refresh(facts.clone(), window, cx));
            views
                .preview
                .update(cx, |preview, cx| preview.refresh(facts.clone(), cx));
        }
    }

    fn reduce_find(&mut self, intent: Intent, window: &mut Window, cx: &mut gpui::Context<Self>) {
        use gitterm_core::find::state::Effect;
        let loading_index = matches!(&intent, Intent::LoadIndex { .. });
        let effects = self.find_state.borrow_mut().reduce(intent);
        // A freshness no-op must not schedule another render/load loop.
        if effects.is_empty() && loading_index {
            return;
        }
        for effect in effects {
            match effect {
                Effect::LoadIndex(stamp) => {
                    #[cfg(test)]
                    let home = self.find_home.clone();
                    #[cfg(not(test))]
                    let home = dirs::home_dir();
                    cx.spawn_in(window, async move |shell, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move {
                                home.map(|home| gitterm_core::find::build_local_index(&home))
                                    .ok_or_else(|| {
                                        "Local transcript source not available in preview".into()
                                    })
                            })
                            .await;
                        _ = shell.update_in(cx, |shell, window, cx| {
                            shell.reduce_find(
                                Intent::IndexFinished {
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
                Effect::LoadPreview(stamp) => {
                    let source = stamp.clone();
                    cx.spawn_in(window, async move |shell, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move {
                                if source.chat.remote_id.is_some() {
                                    return Err("Remote previews not available in preview".into());
                                }
                                std::fs::File::open(&source.path)
                                    .map_err(|e| format!("Read {}: {e}", source.path.display()))?;
                                Ok(gitterm_core::find::load_preview(
                                    &source.path,
                                    source.backend,
                                ))
                            })
                            .await;
                        _ = shell.update_in(cx, |shell, window, cx| {
                            shell.reduce_find(Intent::PreviewFinished { stamp, result }, window, cx)
                        });
                    })
                    .detach();
                }
                Effect::FilterChanged | Effect::IndexChanged(_) => {}
            }
        }
        self.refresh_find(window, cx);
        cx.notify();
    }

    pub(crate) fn find_outcome(
        &mut self,
        owner: &str,
        outcome: &FindOutcome,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let intent = outcome.intent(&self.find_state.borrow(), owner);
        if let Some(intent) = intent {
            self.reduce_find(intent, window, cx);
            cx.notify();
            return;
        }
        let (FindOutcome::Open { id } | FindOutcome::Resume { id, .. }) = outcome else {
            return;
        };
        let target = self.workspaces.iter().find_map(|workspace| {
            workspace
                .sessions
                .iter()
                .enumerate()
                .find_map(|(index, session)| {
                    (self.find_conversation(workspace, session, cx).as_ref() == Some(id))
                        .then_some((workspace.workspace.id, index))
                })
        });
        if let Some((workspace, session)) = target {
            self.find_state
                .borrow_mut()
                .reduce(Intent::ClosePreview(owner.into()));
            self.reopen_workspace(workspace, window, cx);
            self.select_workspace_id(workspace, window, cx);
            self.close_file_viewer(workspace, window, cx);
            self.select_session(session, window, cx);
            let target_owner = self.find_owner(cx);
            self.find_state
                .borrow_mut()
                .reduce(Intent::ClosePreview(target_owner));
            self.refresh_find(window, cx);
            cx.notify();
        } else {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some("Conversation resume not available in preview".into());
                cx.notify();
            });
        }
    }

    pub(crate) fn find_content(&self, cx: &gpui::Context<Self>) -> gpui::Stateful<gpui::Div> {
        let owner = self.find_owner(cx);
        if self.find_state.borrow().selected(&owner).is_some()
            && let Some(views) = self.find_views.get(&owner)
        {
            return div()
                .id("find-centre")
                .debug_selector(|| "find-centre".into())
                .flex_1()
                .min_w_0()
                .h_full()
                .child(views.preview.clone());
        }
        self.file_viewer_content(cx)
    }
}
