//! Standalone Iced Close/Park presentation (TRU-274); no host side effects.
//!
//! Host hookup: route header × and Cmd+Shift+W through `WorkspaceSheet::open`
//! using a fresh `Shell::park_projection` and facts scoped to that workspace's
//! machine. Mount `ParkSheet::workspace`, subscribe to `Outcome`, and capture
//! `WorkspaceSheet::target` before moving the model. Before applying an outcome,
//! resolve that target against a fresh projection, then call `Shell::park_intent`
//! with `Outcome::intent` and the captured target (None for list actions).
//! Use a real timestamp for Park. Cancel dismisses and restores the main focus.
//! Resume dismisses the list; Done leaves it open and refreshes its rows.
//! The host still owns snapshots, persistence, process teardown and wake/focus
//! behavior; workspace teardown uses core park policy, not per-tab close guards.

use gitterm_core::{chats::ChatIndexEntry, park, work::summary};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, KeyDownEvent, Render,
    Subscription, Window, div, prelude::*, px,
};
use gpui_component::{
    ActiveTheme, Sizable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
};

#[cfg(test)]
#[path = "park_sheet_tests.rs"]
mod tests;

const ALTERNATIVE: &str = "Park keeps its tabs to come back to; Close removes it for good.";
const STOP_DESCRIPTION: &str = "It leaves the bar and keeps its tabs. Its terminals and chats stop; Resume from ⏸ parked brings them back.";
const RUN_DESCRIPTION: &str = "It leaves the bar and keeps its tabs. Its terminals and chats keep running; it comes back to the bar when a chat needs you, finishes or fails.";
const WORKING_WARNING: &str = "● a chat is still working; parking stops it";
const READY_DESCRIPTION: &str = "The next PR of each merge plan, and what changed on the work's PRs. GitTerm only watches; you approve and merge from the workspace overview.";
const LIST_DESCRIPTION: &str = "Resume brings a workspace back with its tabs. Done takes it off this list; its chats stay in Chats.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Close {
        target: park::Target,
    },
    Park {
        note: String,
        keep_running: bool,
    },
    Resume {
        id: park::Key,
    },
    Done {
        id: park::Key,
    },
    /// Ready list: `Open` (live work) or `Resume` (parked work) of a group.
    ReadyGo(ReadyHome),
    Cancel,
}

impl Outcome {
    /// Translate to shared policy, never execute consequences here. The host
    /// must revalidate the captured workspace target before reducing the intent.
    pub fn intent(&self, target: Option<&park::Target>, stamp: String) -> Option<park::Intent> {
        match self {
            Self::Close { target } => Some(park::Intent::Close(target.uid)),
            Self::Park { note, keep_running } => Some(park::Intent::Park {
                uid: target?.uid,
                mode: if *keep_running {
                    park::Mode::KeepRunning
                } else {
                    park::Mode::Stop
                },
                info: park::Info::new(note, stamp),
            }),
            Self::Resume { id } => Some(park::Intent::Resume(id.clone())),
            Self::Done { id } => Some(park::Intent::Done(id.clone())),
            Self::ReadyGo(_) | Self::Cancel => None,
        }
    }
}

/// Where a Ready group's button goes: an open workspace or a parked entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadyHome {
    Open(crate::data::WorkspaceId),
    Parked(park::Key),
}
#[derive(Clone, Debug)]
pub struct ReadyNext {
    pub number: u64,
    pub repo: String,
    pub title: String,
}
#[derive(Clone, Debug)]
pub struct ReadyNews {
    pub number: u64,
    pub label: String,
    pub text: String,
    pub color: u32,
}
/// One home of Iced's Ready list, supplied in Iced order.
#[derive(Clone, Debug)]
pub struct ReadyGroup {
    pub name: String,
    pub home: ReadyHome,
    pub next: Vec<ReadyNext>,
    pub news: Vec<ReadyNews>,
}
impl ReadyGroup {
    /// Iced: `Open` for live work, `Resume` for parked work.
    pub fn button_label(&self) -> &'static str {
        match self.home {
            ReadyHome::Open(_) => "Open",
            ReadyHome::Parked(_) => "Resume",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Close,
    Park,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SheetAction {
    Cancel,
    Park,
    Close,
}

impl SheetAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cancel => "Cancel",
            Self::Park => "Park",
            Self::Close => "Close",
        }
    }
}

/// Presentation facts from the host's current workspace. Terminal count includes
/// bottom terminals; chat entries must already be scoped to this machine.
#[derive(Default)]
pub struct WorkspaceFacts<'a> {
    pub owns_chats: bool,
    pub terminals: usize,
    pub chats: usize,
    pub others_have_chats: bool,
    pub shared_repos: &'a [(String, String)],
    pub note_chats: &'a [&'a ChatIndexEntry],
    pub working_chat: bool,
}

/// Match Iced's note scope: owned roots and children, else hosted tab chats,
/// excluding chats (and their children) owned by another workspace.
pub fn note_chats<'a>(
    entries: &'a [ChatIndexEntry],
    own_roots: &[String],
    own_tab_ids: &[String],
    other_roots: &[String],
) -> Vec<&'a ChatIndexEntry> {
    summary::chats_under(
        entries,
        if own_roots.is_empty() {
            own_tab_ids
        } else {
            own_roots
        },
    )
    .into_iter()
    .filter(|entry| {
        !other_roots.contains(&entry.id)
            && !entry
                .root_id
                .as_ref()
                .is_some_and(|root| other_roots.contains(root))
    })
    .collect()
}

fn prefill(chats: &[&ChatIndexEntry]) -> String {
    chats
        .iter()
        .filter(|chat| chat.root_id.is_none())
        .max_by_key(|chat| chat.mtime)
        .map(|chat| chat.latest.clone().unwrap_or_else(|| chat.title.clone()))
        .unwrap_or_default()
}

#[derive(Clone, Debug)]
pub struct WorkspaceSheet {
    pub target: park::Target,
    pub kind: Kind,
    pub name: String,
    note: String,
    prefilled: String,
    keep_running: bool,
    working_chat: bool,
    close_lines: Vec<String>,
}

impl WorkspaceSheet {
    /// Both entry points use this guard. Core protects the last visible workspace
    /// even if there are other keep-running workspaces off the bar.
    pub fn open(state: &park::State, uid: u64, facts: WorkspaceFacts<'_>) -> Option<Self> {
        let park::Effect::Close { target, key } = state.reduce(park::Intent::Close(uid)) else {
            return None;
        };
        let kind = if facts.owns_chats {
            Kind::Park
        } else {
            Kind::Close
        };
        let note = if kind == Kind::Park {
            prefill(facts.note_chats)
        } else {
            String::new()
        };
        let mut close_lines = vec![stops_line(facts.terminals, facts.chats)];
        if facts.others_have_chats {
            close_lines.push("Chats in other workspaces keep running.".into());
        }
        close_lines.extend(
            facts
                .shared_repos
                .iter()
                .map(|(repo, ws)| format!("{repo} stays open in {ws}.")),
        );
        close_lines.push(ALTERNATIVE.into());
        Some(Self {
            target,
            kind,
            name: key.name,
            prefilled: note.clone(),
            note,
            keep_running: false,
            working_chat: facts.working_chat,
            close_lines,
        })
    }

    pub fn note(&self) -> &str {
        &self.note
    }
    pub fn keep_running(&self) -> bool {
        self.keep_running
    }
    pub fn set_note(&mut self, note: String) {
        if self.kind == Kind::Park {
            self.note = note;
        }
    }
    pub fn set_keep_running(&mut self, value: bool) {
        if self.kind == Kind::Park {
            self.keep_running = value;
        }
    }
    /// A late index may refill only a note that still matches the old prefill.
    pub fn refill(&mut self, chats: &[&ChatIndexEntry]) {
        if self.kind == Kind::Park && self.note == self.prefilled {
            self.note = prefill(chats);
            self.prefilled = self.note.clone();
        }
    }
    pub fn title(&self) -> String {
        match self.kind {
            Kind::Close => format!("Close {}?", self.name),
            Kind::Park => format!("Park {}", self.name),
        }
    }
    pub fn lines(&self) -> Vec<String> {
        if self.kind == Kind::Close {
            self.close_lines.clone()
        } else {
            vec![
                if self.keep_running {
                    RUN_DESCRIPTION
                } else {
                    STOP_DESCRIPTION
                }
                .into(),
            ]
        }
    }
    pub fn warning(&self) -> Option<&'static str> {
        (self.kind == Kind::Park && self.working_chat && !self.keep_running)
            .then_some(WORKING_WARNING)
    }
    pub fn buttons(&self) -> &'static [SheetAction] {
        match self.kind {
            Kind::Close => &[SheetAction::Cancel, SheetAction::Park, SheetAction::Close],
            Kind::Park => &[SheetAction::Cancel, SheetAction::Park],
        }
    }
    pub fn activate(&self, action: SheetAction) -> Option<Outcome> {
        if !self.buttons().contains(&action) {
            return None;
        }
        Some(match action {
            SheetAction::Cancel => Outcome::Cancel,
            SheetAction::Close => Outcome::Close {
                target: self.target.clone(),
            },
            SheetAction::Park => Outcome::Park {
                note: self.note.clone(),
                keep_running: self.keep_running,
            },
        })
    }
    pub fn key(&self, key: &str) -> Option<Outcome> {
        match key {
            "escape" => Some(Outcome::Cancel),
            "enter" if self.kind == Kind::Park => self.activate(SheetAction::Park),
            _ => None,
        }
    }
}

fn stops_line(terminals: usize, chats: usize) -> String {
    let plural = |n, one, many| format!("{n} {}", if n == 1 { one } else { many });
    match (terminals, chats) {
        (0, 0) => "Nothing is running in it.".into(),
        (t, 0) => format!(
            "Its {} {}.",
            plural(t, "terminal", "terminals"),
            if t == 1 { "stops" } else { "stop" }
        ),
        (0, c) => format!(
            "Its {} {}.",
            plural(c, "chat", "chats"),
            if c == 1 { "stops" } else { "stop" }
        ),
        (t, c) => format!(
            "Its {} and {} stop.",
            plural(t, "terminal", "terminals"),
            plural(c, "chat", "chats")
        ),
    }
}

/// List metadata is supplied in Iced order: live parked workspaces first, then
/// stopped entries newest first (legacy entries retain file order).
#[derive(Clone, Debug)]
pub struct ParkedRow {
    pub id: park::Key,
    pub chats: usize,
    pub info: Option<park::Info>,
    pub running: bool,
    pub color: Hsla,
    pub pr_news: Option<(String, Hsla)>,
}

impl ParkedRow {
    pub fn resume(&self) -> Outcome {
        Outcome::Resume {
            id: self.id.clone(),
        }
    }
    pub fn done(&self) -> Outcome {
        Outcome::Done {
            id: self.id.clone(),
        }
    }
    pub fn owned_label(&self) -> String {
        match self.chats {
            0 => "folder".into(),
            1 => "1 chat".into(),
            n => format!("{n} chats"),
        }
    }
    pub fn note(&self) -> &str {
        self.info
            .as_ref()
            .map(|info| info.note.as_str())
            .filter(|note| !note.is_empty())
            .unwrap_or("No note")
    }
}

enum Content {
    Workspace(WorkspaceSheet),
    Parked(Vec<ParkedRow>),
    Ready(Vec<ReadyGroup>),
}

/// Mount as a full-size overlay. The backdrop deliberately does not dismiss on
/// click. Outcomes are emitted once per workspace sheet, including note Enter.
pub struct ParkSheet {
    content: Content,
    input: Option<Entity<InputState>>,
    focus: FocusHandle,
    completed: bool,
    _subscriptions: Vec<Subscription>,
}
impl EventEmitter<Outcome> for ParkSheet {}
impl Focusable for ParkSheet {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ParkSheet {
    /// Iced's `park_sheet` (Park or Close), not its Parked and Ready lists.
    pub(crate) fn is_workspace_sheet(&self) -> bool {
        matches!(self.content, Content::Workspace(_))
    }
    pub(crate) fn submit(&mut self, cx: &mut Context<Self>) {
        if let Content::Workspace(model) = &self.content {
            self.emit(model.key("enter"), cx);
        }
    }
    pub fn workspace(model: WorkspaceSheet, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let mut subscriptions = vec![];
        let input = (model.kind == Kind::Park).then(|| {
            let input = cx.new(|cx| {
                let mut input = InputState::new(window, cx).placeholder("Where you left off");
                input.set_value(model.note.clone(), window, cx);
                input.focus(window, cx);
                input.select_all(window, cx);
                input
            });
            subscriptions.push(
                cx.subscribe_in(&input, window, |this, input, event, _, cx| {
                    if let Content::Workspace(model) = &mut this.content {
                        match event {
                            InputEvent::Change => {
                                model.set_note(input.read(cx).value().to_string())
                            }
                            InputEvent::PressEnter { .. } => {
                                let outcome = model.key("enter");
                                this.emit(outcome, cx);
                            }
                            _ => {}
                        }
                    }
                    cx.notify();
                }),
            );
            input
        });
        if input.is_none() {
            focus.focus(window, cx);
        }
        Self {
            content: Content::Workspace(model),
            input,
            focus,
            completed: false,
            _subscriptions: subscriptions,
        }
    }
    pub fn parked(rows: Vec<ParkedRow>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            content: Content::Parked(rows),
            input: None,
            focus,
            completed: false,
            _subscriptions: vec![],
        }
    }
    pub fn ready(groups: Vec<ReadyGroup>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            content: Content::Ready(groups),
            input: None,
            focus,
            completed: false,
            _subscriptions: vec![],
        }
    }
    /// Host calls when an asynchronously refreshed, correctly scoped index lands.
    pub fn refill(
        &mut self,
        chats: &[&ChatIndexEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Content::Workspace(model) = &mut self.content {
            model.refill(chats);
            if let Some(input) = &self.input
                && input.read(cx).value().as_str() != model.note()
            {
                input.update(cx, |input, cx| {
                    input.set_value(model.note.clone(), window, cx)
                });
            }
        }
        cx.notify();
    }
    #[cfg(test)]
    pub(crate) fn ready_groups(&self) -> Option<&[ReadyGroup]> {
        match &self.content {
            Content::Ready(groups) => Some(groups),
            _ => None,
        }
    }
    #[cfg(test)]
    pub(crate) fn parked_rows(&self) -> Option<&[ParkedRow]> {
        match &self.content {
            Content::Parked(rows) => Some(rows),
            _ => None,
        }
    }
    pub fn set_rows(&mut self, rows: Vec<ParkedRow>, cx: &mut Context<Self>) {
        if let Content::Parked(current) = &mut self.content {
            *current = rows;
        }
        cx.notify();
    }
    fn emit(&mut self, outcome: Option<Outcome>, cx: &mut Context<Self>) {
        if self.completed {
            return;
        }
        if let Some(outcome) = outcome {
            self.completed = !matches!(outcome, Outcome::Done { .. });
            cx.emit(outcome);
            cx.notify();
        }
    }
    /// Escape, for a caller that holds focus elsewhere (a terminal behind
    /// the sheet): the same outcome as the sheet's own Escape key.
    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
        self.activate(SheetAction::Cancel, cx);
    }
    fn activate(&mut self, action: SheetAction, cx: &mut Context<Self>) {
        let outcome = match &self.content {
            Content::Workspace(model) => model.activate(action),
            Content::Parked(_) | Content::Ready(_) => Some(Outcome::Cancel),
        };
        self.emit(outcome, cx);
    }
    fn ready_group(
        &self,
        index: usize,
        group: &ReadyGroup,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let go = Outcome::ReadyGo(group.home.clone());
        let mut card = div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .py(px(6.))
            .px_2()
            .rounded(px(6.))
            .bg(cx.theme().background)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex_1()
                            .font_family("Menlo")
                            .child(group.name.clone()),
                    )
                    .child(
                        Button::new(("ready-go", index))
                            .small()
                            .ghost()
                            .label(group.button_label())
                            .debug_selector(move || format!("ready-go-{index}"))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.emit(Some(go.clone()), cx)),
                            ),
                    ),
            );
        for next in &group.next {
            card = card.child(
                div()
                    .flex()
                    .gap(px(8.))
                    .text_xs()
                    .child(div().text_color(muted).child(format!("#{}", next.number)))
                    .child(
                        div()
                            .flex_1()
                            .text_color(cx.theme().primary)
                            .child(format!("next: {}", next.title)),
                    )
                    .child(div().text_color(muted).child(next.repo.clone())),
            );
        }
        for news in &group.news {
            card = card.child(
                div()
                    .flex()
                    .gap(px(8.))
                    .text_xs()
                    .child(div().text_color(muted).child(format!("#{}", news.number)))
                    .child(
                        div()
                            .flex_1()
                            .text_color(gpui::rgb(news.color))
                            .child(news.text.clone()),
                    ),
            );
        }
        card
    }
    fn row(&self, index: usize, row: &ParkedRow, cx: &mut Context<Self>) -> impl IntoElement {
        let mut head = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .font_family("Menlo")
                    .text_color(row.color)
                    .child(row.id.name.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(row.owned_label()),
            );
        if row.running {
            head = head.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().success)
                    .child("● running"),
            );
        }
        if let Some(age) = row
            .info
            .as_ref()
            .and_then(|info| summary::parse_stamp(&info.parked_at))
            .map(summary::age_label)
        {
            head = head.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(age),
            );
        }
        let mut lines = div()
            .flex_1()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(head)
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(row.note().to_string()),
            );
        if let Some((news, color)) = &row.pr_news {
            lines = lines.child(div().text_xs().text_color(*color).child(news.clone()));
        }
        let resume = row.resume();
        let done = row.done();
        div()
            .flex()
            .items_center()
            .gap_2()
            .py(px(6.))
            .px_2()
            .rounded(px(6.))
            .bg(cx.theme().background)
            .child(lines)
            .child(
                Button::new(("resume-parked", index))
                    .small()
                    .primary()
                    .label("Resume")
                    .debug_selector(move || format!("parked-resume-{index}"))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.emit(Some(resume.clone()), cx)),
                    ),
            )
            .child(
                Button::new(("done-parked", index))
                    .small()
                    .ghost()
                    .label("Done")
                    .debug_selector(move || format!("parked-done-{index}"))
                    .on_click(cx.listener(move |this, _, _, cx| this.emit(Some(done.clone()), cx))),
            )
    }
}

impl Render for ParkSheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut card = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .w(px(488.))
            .py(px(20.))
            .px(px(24.))
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(px(8.));
        match &self.content {
            Content::Workspace(model) => {
                card = card.child(div().child(model.title()));
                for line in model.lines() {
                    card = card.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(line),
                    );
                }
                if let Some(input) = &self.input {
                    card = card.child(Input::new(input).small()).child(
                        Checkbox::new("park-keep-running")
                            .debug_selector(|| "keep-running".into())
                            .label("Keep running")
                            .checked(model.keep_running())
                            .on_click(cx.listener(|this, value: &bool, _, cx| {
                                if let Content::Workspace(model) = &mut this.content {
                                    model.set_keep_running(*value);
                                }
                                cx.notify();
                            })),
                    );
                }
                if let Some(warning) = model.warning() {
                    card = card.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().warning)
                            .child(warning),
                    );
                }
                let mut buttons = div().flex().justify_end().items_center().gap_2();
                for &action in model.buttons() {
                    let primary = action == *model.buttons().last().unwrap();
                    let button = Button::new(action.label())
                        .small()
                        .label(action.label())
                        .debug_selector(move || action.label().into())
                        .when(primary, |button| button.primary())
                        .on_click(cx.listener(move |this, _, _, cx| this.activate(action, cx)));
                    buttons = buttons.child(button);
                }
                card = card.child(buttons);
            }
            Content::Ready(groups) => {
                let mut list = div()
                    .id("ready-rows")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .max_h(px(460.))
                    .overflow_y_scroll();
                if groups.is_empty() {
                    list = list.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Nothing to merge and no news."),
                    );
                }
                for (index, group) in groups.iter().enumerate() {
                    list = list.child(self.ready_group(index, group, cx));
                }
                card = card
                    .w(px(568.))
                    .child("Ready to merge")
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(READY_DESCRIPTION),
                    )
                    .child(list)
                    .child(
                        div().flex().justify_end().child(
                            Button::new("close-ready-list")
                                .small()
                                .label("Close")
                                .debug_selector(|| "ready-close".into())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.activate(SheetAction::Cancel, cx)
                                })),
                        ),
                    );
            }
            Content::Parked(rows) => {
                let mut list = div()
                    .id("parked-rows")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .max_h(px(420.))
                    .overflow_y_scroll();
                if rows.is_empty() {
                    list = list.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Nothing parked."),
                    );
                }
                for (index, row) in rows.iter().enumerate() {
                    list = list.child(self.row(index, row, cx));
                }
                card = card
                    .w(px(568.))
                    .child("Parked")
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(LIST_DESCRIPTION),
                    )
                    .child(list)
                    .child(
                        div().flex().justify_end().child(
                            Button::new("close-parked-list")
                                .small()
                                .label("Close")
                                .debug_selector(|| "parked-close".into())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.activate(SheetAction::Cancel, cx)
                                })),
                        ),
                    );
            }
        }
        div()
            .id("close-park-overlay")
            .track_focus(&self.focus)
            .key_context("IcedPark")
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().background.opacity(0.8))
            .child(card)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let key = event.keystroke.key.as_str();
                let outcome = match &this.content {
                    Content::Workspace(model) => model.key(key),
                    Content::Parked(_) | Content::Ready(_) if key == "escape" => {
                        Some(Outcome::Cancel)
                    }
                    _ => None,
                };
                if outcome.is_some() {
                    cx.stop_propagation();
                    this.emit(outcome, cx);
                }
            }))
    }
}
