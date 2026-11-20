mod attachments;
#[path = "copy.rs"]
mod copy;
#[path = "find.rs"]
mod find;
#[path = "turns.rs"]
mod turns;
use copy::CopyTarget;
use find::TranscriptFind;

use crate::{
    ChatAppearance, ChatIntent,
    adapter::{Presentation, Receipt, ReceiptState, Update},
    cards::CardView,
    events::{Entry, HarnessEvent, Record},
    model::*,
};
use gpui::{
    App, AppContext, Bounds, ClickEvent, ClipboardItem, Context, Entity, EntityInputHandler,
    FocusHandle, Focusable, InteractiveElement, IntoElement, KeyBinding, ParentElement, Render,
    StatefulInteractiveElement, StyleRefinement, Styled, Subscription, Task, TitlebarOptions,
    Window, WindowBounds, WindowOptions, actions, base::TextSelection, div, prelude::FluentBuilder,
    px, rgb, size,
};
use gpui_component::{
    Disableable, Root, Sizable, WindowExt,
    button::Button,
    collapsible::Collapsible,
    diff::{Diff, DiffFile, DiffState},
    input::{
        Enter as InputEnter, Escape as InputEscape, IndentInline, Input, InputEvent, InputState,
        MoveDown, MoveUp, TextareaState,
    },
    menu::{ContextMenuExt, PopupMenuItem},
    message_scroller::{MessageScroller, MessageScrollerState},
    text::{RangeHighlight, SelectionFormat, TextView, TextViewMotion, TextViewState},
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    hash::{Hash, Hasher},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

actions!(
    native_chat,
    [
        Find,
        NextMatch,
        PreviousMatch,
        CloseFind,
        CopyPlain,
        CopyMarkdown,
        SubmitPrompt,
        Newline,
        SlashNext,
        SlashPrevious,
        FillSlash,
        SubmitSlash,
        CloseSlash
    ]
);

pub type IntentCallback = Rc<dyn Fn(ChatIntent, &mut App) -> Receipt>;
type TurnToggle = Rc<dyn Fn(u64, &mut App)>;

pub fn run(records: Vec<Record>) {
    let live = std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--chat-live");
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        init(cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(850.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("GitTerm · Native chat fixture".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                let chat = cx.new(|cx| {
                    let mut view = ChatView::new(records, window, cx);
                    if live {
                        view.set_intent_callback(
                            Rc::new(|intent, _| {
                                eprintln!("ChatIntent: {intent:?}");
                                Receipt::rejected("Fixture demo has no runtime")
                            }),
                            cx,
                        );
                        view.set_read_only(None, cx);
                        view.start_replay(&ClickEvent::default(), window, cx);
                    }
                    view
                });
                cx.new(|cx| Root::new(chat, window, cx))
            },
        )
        .expect("open native chat fixture viewer");
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
}

/// Register chat shortcuts, scoped to the focused NativeChat subtree.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-f", Find, Some("NativeChat")),
        KeyBinding::new("cmd-g", NextMatch, Some("NativeChat")),
        KeyBinding::new("cmd-shift-g", PreviousMatch, Some("NativeChat")),
        KeyBinding::new("escape", CloseFind, Some("NativeChat")),
        KeyBinding::new("cmd-c", CopyPlain, Some("NativeChat")),
        KeyBinding::new("cmd-shift-c", CopyMarkdown, Some("NativeChat")),
    ]);
}

/// Owns the document model and retained selection/fold state independently of list layout.
/// Rich-text state is initialized only for visited rows; it is not the event source of truth.
pub struct ChatView {
    appearance: ChatAppearance,
    width: gitterm_core::settings::ChatWidth,
    records: Arc<Vec<Record>>,
    preview: bool,
    composer: Entity<TextareaState>,
    composer_state: crate::composer::ComposerState,
    slash_scroll: gpui::ScrollHandle,
    roster: Vec<roster::RosterEntry>,
    roster_hidden: HashSet<String>,
    activity_tick: Option<Task<()>>,
    /// Raw fixture ingress only; live projection is always supplied by the owner.
    fixture_composer: gitterm_core::chat::state::State,
    callback: Option<IntentCallback>,
    images: Vec<gitterm_core::chat::types::ImageAttachment>,
    image_previews: Vec<Option<Arc<gpui::Image>>>,
    image_queue: VecDeque<attachments::Source>,
    preparing_image: Option<Task<()>>,
    submission: Option<Receipt>,
    control_receipt: Option<Receipt>,
    projection_runtime: Option<gitterm_core::chat::state::RuntimeStamp>,
    submitted_draft: Option<gitterm_core::chat::types::UserPrompt>,
    read_only_reason: Option<String>,
    turn_running: bool,
    new_messages: bool,
    cards: HashMap<u64, Entity<CardView>>,
    recent_rows: VecDeque<usize>,
    folded: HashMap<u64, bool>,
    folded_turns: HashSet<u64>,
    model: ChatModel,
    row_views: Vec<Option<Entity<RowView>>>,
    scroller: Entity<MessageScrollerState>,
    find_input: Entity<InputState>,
    find: TranscriptFind,
    find_open: bool,
    selection_copied: Option<Task<()>>,
    focus: FocusHandle,
    replay: Option<Task<()>>,
    replay_position: usize,
    _subscriptions: Vec<Subscription>,
}
impl gpui::EventEmitter<roster::RosterOutcome> for ChatView {}

impl ChatView {
    pub fn new(records: Vec<Record>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = ChatModel::from_records(&records);
        let scroller = cx.new(|cx| MessageScrollerState::new(model.rows.len(), cx));
        let find_input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in chat"));
        let mut subscriptions = vec![cx.subscribe_in(
            &find_input,
            window,
            |this, input, event, window, cx| match event {
                InputEvent::Change => {
                    this.change_find(input.read(cx).value().to_string(), window, cx)
                }
                InputEvent::PressEnter { shift, .. } => this.navigate(*shift, window, cx),
                _ => {}
            },
        )];
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 8)
                .submit_on_enter(true)
                .placeholder("Message… (Enter to send, Shift+Enter for newline)")
        });
        subscriptions.push(
            cx.subscribe_in(&composer, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.composer_state.slash_index = 0;
                    this.composer_state.slash_dismissed = false;
                }
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.send(window, cx);
                }
                cx.notify();
            }),
        );
        let turn_running = records.iter().fold(false, |running, record| {
            running_after(&record.entry, running)
        });
        subscriptions.push(cx.observe(&scroller, |this, _, cx| {
            if this.scroller.read(cx).is_following_tail() {
                this.new_messages = false;
            }
            cx.notify();
        }));
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut fixture_composer = gitterm_core::chat::state::State::default();
        for record in &records {
            if let Entry::Harness(event) = &record.entry {
                fixture_composer.reduce(gitterm_core::chat::state::Intent::Event(
                    gitterm_core::chat::state::AgentEvent::Harness(event.clone()),
                ));
                if matches!(event, HarnessEvent::Ready { .. }) {
                    fixture_composer.conversation =
                        vec![gitterm_core::chat::state::AgentEvent::Harness(
                            event.clone(),
                        )];
                }
            }
        }
        let mut composer_state = crate::composer::ComposerState::default();
        composer_state.project(&fixture_composer, None);
        Self {
            appearance: ChatAppearance::default(),
            width: Default::default(),
            records: Arc::new(records),
            preview: false,
            composer,
            composer_state,
            slash_scroll: gpui::ScrollHandle::new(),
            roster: vec![],
            roster_hidden: HashSet::new(),
            activity_tick: None,
            fixture_composer,
            callback: None,
            images: vec![],
            image_previews: vec![],
            image_queue: VecDeque::new(),
            preparing_image: None,
            submission: None,
            control_receipt: None,
            projection_runtime: None,
            submitted_draft: None,
            read_only_reason: Some("Recorded fixture · read-only".into()),
            turn_running,
            new_messages: false,
            cards: HashMap::new(),
            recent_rows: VecDeque::new(),
            folded: HashMap::new(),
            folded_turns: HashSet::new(),
            row_views: vec![None; model.rows.len()],
            model,
            scroller,
            find_input,
            find: TranscriptFind::default(),
            find_open: false,
            selection_copied: None,
            focus,
            replay: None,
            replay_position: 0,
            _subscriptions: subscriptions,
        }
    }
    /// Change the native column without discarding folds, drafts or selection.
    pub fn set_width(&mut self, width: gitterm_core::settings::ChatWidth, cx: &mut Context<Self>) {
        if self.width != width {
            self.width = width;
            self.scroller.update(cx, |state, cx| {
                state.remeasure_items(0..self.model.rows.len(), cx)
            });
            cx.notify();
        }
    }

    /// Iced's CSS rem is the document's 16px root size, independent of chat fonts.
    fn column_width(&self) -> Option<f32> {
        use gitterm_core::settings::ChatWidth;
        match self.width {
            ChatWidth::Narrow => Some(46. * 16. + 28.),
            ChatWidth::Medium => Some(60. * 16. + 28.),
            ChatWidth::Wide => Some(72. * 16. + 28.),
            ChatWidth::Full => None,
        }
    }

    /// Apply host settings to retained rows without discarding folds, drafts or selection.
    pub fn set_appearance(&mut self, appearance: ChatAppearance, cx: &mut Context<Self>) {
        let appearance = appearance.normalized();
        if self.appearance == appearance {
            return;
        }
        self.appearance = appearance;
        for row in self.row_views.iter().flatten() {
            row.update(cx, |row, cx| {
                row.appearance = appearance;
                cx.notify();
            });
        }
        self.scroller.update(cx, |state, cx| {
            state.remeasure_items(0..self.model.rows.len(), cx);
        });
        if let Some(saved) = &self.find.saved_scroller {
            saved.update(cx, |state, cx| {
                state.remeasure_items(0..self.model.rows.len(), cx);
            });
        }
        cx.notify();
    }

    /// Supply the chat's worker/task roster without giving the view runtime ownership.
    // TODO: shell::chat::ChatPane should project its task/delegation registry here
    // once the shared workspace owner exposes those workers to this pane.
    pub fn set_roster(&mut self, rows: Vec<roster::RosterEntry>, cx: &mut Context<Self>) {
        self.roster = rows;
        self.sync_activity_tick(cx);
        cx.notify();
    }

    pub(crate) fn roster_outcome(
        &mut self,
        outcome: roster::RosterOutcome,
        cx: &mut Context<Self>,
    ) {
        let id = match &outcome {
            roster::RosterOutcome::Open { id } | roster::RosterOutcome::Dismiss { id } => id,
        };
        if !self.roster.iter().any(|row| &row.id == id) || self.roster_hidden.contains(id) {
            return;
        }
        if let roster::RosterOutcome::Dismiss { id } = &outcome {
            self.roster_hidden.insert(id.clone());
            self.sync_activity_tick(cx);
        }
        cx.emit(outcome);
        cx.notify();
    }

    pub fn set_status_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.composer_state.notice = Some(error);
        cx.notify();
    }

    fn sync_activity_tick(&mut self, cx: &mut Context<Self>) {
        let live = self.composer_state.streaming
            || self
                .roster
                .iter()
                .any(|row| row.state.is_live() && !self.roster_hidden.contains(&row.id));
        if !live {
            self.activity_tick = None;
        } else if self.activity_tick.is_none() {
            self.activity_tick = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(500))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
    }

    /// Retain the embedded session's capability restrictions without extra chrome.
    pub fn set_preview(&mut self, cx: &mut Context<Self>) {
        self.preview = true;
        for card in self.cards.values() {
            card.update(cx, |card, cx| {
                card.preview = true;
                cx.notify();
            });
        }
        cx.notify();
    }
    /// Install the app-owned delivery seam. No session/harness is started by the view.
    pub fn set_intent_callback(&mut self, callback: IntentCallback, cx: &mut Context<Self>) {
        self.callback = Some(callback);
        for card in self.cards.values() {
            card.update(cx, |card, cx| card.set_callback(self.callback.clone(), cx));
        }
        cx.notify();
    }
    pub fn set_read_only(&mut self, reason: Option<String>, cx: &mut Context<Self>) {
        self.read_only_reason = reason;
        if self.read_only_reason.is_some() {
            self.image_queue.clear();
            self.preparing_image = None;
        }
        self.composer.update(cx, |input, cx| {
            input.set_disabled(self.read_only_reason.is_some(), cx)
        });
        for row in &self.model.rows {
            if let Some(card) = self.cards.get(&row.id) {
                card.update(cx, |card, cx| {
                    card.sync(row.clone(), self.read_only_reason.is_some(), cx)
                });
            }
        }
        cx.notify();
    }
    /// Replace externally prepared image payloads; native ingress also supplies thumbnails.
    pub fn set_images(
        &mut self,
        images: Vec<gitterm_core::chat::types::ImageAttachment>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        gitterm_core::chat::types::validate_images(&images)?;
        self.image_previews = vec![None; images.len()];
        self.images = images;
        cx.notify();
        Ok(())
    }
    fn submission_blocked(&self) -> bool {
        self.submission.as_ref().is_some_and(|receipt| {
            !matches!(
                receipt.state,
                ReceiptState::Rejected(_) | ReceiptState::Written | ReceiptState::Resolved
            )
        })
    }
    fn send_blocked(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.read_only_reason.is_some()
            || self.callback.is_none()
            || self.submission_blocked()
            || self.composer_state.pending_request
            || self.preparing_image.is_some()
            || self.composer.update(cx, |input, cx| {
                input.marked_text_range(window, cx).is_some()
            })
            || (self.composer.read(cx).value().trim().is_empty() && self.images.is_empty())
    }
    pub(crate) fn withdraw(&mut self, message_id: String, cx: &mut Context<Self>) {
        if self.read_only_reason.is_none()
            && let Some(callback) = &self.callback
        {
            self.control_receipt = Some(callback(ChatIntent::Withdraw { message_id }, cx));
            cx.notify();
        }
    }
    pub(crate) fn reopen_slash(&mut self, cx: &mut Context<Self>) {
        self.composer_state.slash_dismissed = false;
        cx.notify();
    }
    pub(crate) fn select_slash(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.read_only_reason.is_some()
            || self.composer.update(cx, |input, cx| {
                input.marked_text_range(window, cx).is_some()
            })
        {
            return;
        }
        self.composer.update(cx, |input, cx| {
            let draft = format!("/{name} ");
            let end = draft.len();
            input.set_value(draft, window, cx);
            input.set_selected_range(end..end, cx);
            input.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }
    fn slash_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.read_only_reason.is_some()
            || !self.composer.read(cx).focus_handle(cx).is_focused(window)
            || self.composer.update(cx, |input, cx| {
                input.marked_text_range(window, cx).is_some()
            })
        {
            return false;
        }
        let draft = self.composer.read(cx).value();
        if self.composer.read(cx).selected_range().start != draft.len() {
            return false;
        }
        match self.composer_state.slash_key(&draft, key) {
            crate::composer::SlashOutcome::Ignored => return false,
            crate::composer::SlashOutcome::Handled => {}
            crate::composer::SlashOutcome::Fill { name, submit } => {
                self.select_slash(&name, window, cx);
                if submit {
                    self.send(window, cx);
                }
            }
        }
        self.slash_scroll
            .scroll_to_item(self.composer_state.slash_index + 1);
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
        true
    }
    pub(crate) fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.send_blocked(window, cx) {
            return;
        }
        let text = self.composer.read(cx).value().to_string();
        if text.trim().is_empty() && self.images.is_empty() {
            return;
        }
        if let Some(callback) = &self.callback {
            self.submitted_draft = Some(gitterm_core::chat::types::UserPrompt {
                text: text.clone(),
                images: self.images.clone(),
                id: None,
            });
            let receipt = callback(
                ChatIntent::Send {
                    text,
                    images: self.images.clone(),
                },
                cx,
            );
            self.submission = Some(receipt.clone());
            self.update_receipt(receipt, window, cx);
        }
        cx.notify();
    }
    /// Apply only the matching attempt. Keep rejected/uncertain drafts intact;
    /// a write or terminal receipt clears only the exact draft, preserving newer edits.
    pub fn update_receipt(
        &mut self,
        receipt: Receipt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .submission
            .as_ref()
            .is_some_and(|old| old.accepts_update(&receipt))
            && matches!(
                receipt.state,
                ReceiptState::Written | ReceiptState::Resolved
            )
            && let Some(draft) = self.submitted_draft.take()
            && self.composer.read(cx).value().as_str() == draft.text
            && self.images == draft.images
        {
            self.composer
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.images.clear();
            self.image_previews.clear();
        }
        self.sync_receipt(&receipt, cx);
    }
    fn sync_receipt(&mut self, receipt: &Receipt, cx: &mut Context<Self>) {
        if self
            .submission
            .as_ref()
            .is_some_and(|old| old.accepts_update(receipt))
        {
            self.submission = Some(receipt.clone());
        }
        if self
            .control_receipt
            .as_ref()
            .is_some_and(|old| old.accepts_update(receipt))
        {
            self.control_receipt = Some(receipt.clone());
        }
        for card in self.cards.values() {
            card.update(cx, |card, cx| card.update_receipt(receipt, cx));
        }
        cx.notify();
    }
    /// Consume reducer presentation effects even when State::revision is unchanged.
    pub fn present(
        &mut self,
        source: &gitterm_core::chat::state::RuntimeStamp,
        state: &gitterm_core::chat::state::State,
        update: Update,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !state.accepts_stream(source, &source.target) {
            return;
        }
        if self
            .projection_runtime
            .as_ref()
            .is_some_and(|previous| !state.accepts_stream(previous, &source.target))
        {
            self.cards.clear();
            self.composer_state.notice = None;
            self.row_views.fill(None);
            if let Some(receipt) = &mut self.submission
                && matches!(
                    receipt.state,
                    ReceiptState::ChannelAccepted | ReceiptState::Written
                )
            {
                receipt.state = ReceiptState::Uncertain(
                    "Runtime replaced before an authoritative outcome".into(),
                );
            }
        }
        self.projection_runtime = Some(source.clone());
        self.composer_state.project(state, update.event.as_ref());
        self.sync_activity_tick(cx);
        match update.presentation {
            Some(Presentation::Replay) => {
                self.model = ChatModel::from_conversation(&state.conversation);
                for row in &mut self.model.rows {
                    if let Some(request) = &mut row.request
                        && !request.resolved
                    {
                        request.identity =
                            state.request_identity(source, &request.owner, &request.id);
                        request.resolved = request.identity.is_none();
                    }
                }
                // Row IDs are positional after prepend; never reuse a card under a new request.
                let old_cards = std::mem::take(&mut self.cards);
                for row in &self.model.rows {
                    if row.request.is_some()
                        && let Some(card) = old_cards
                            .values()
                            .find(|card| card.read(cx).matches_request(row))
                    {
                        card.update(cx, |card, cx| {
                            card.sync(row.clone(), self.read_only_reason.is_some(), cx)
                        });
                        self.cards.insert(row.id, card.clone());
                    }
                }
                self.row_views = vec![None; self.model.rows.len()];
                self.recent_rows.clear();
                self.folded.clear();
                self.folded_turns.clear();
                self.scroller
                    .update(cx, |scroller, cx| scroller.reset(self.model.rows.len(), cx));
                self.reset_find(cx);
            }
            Some(Presentation::Append(events)) => {
                for event in events {
                    let following = self.scroller.read(cx).is_following_tail();
                    let count = self.model.rows.len();
                    let changed = self.model.apply_agent(&event);
                    self.refresh_rows(count, following, changed, cx);
                }
            }
            None => {}
        }
        for row in &mut self.model.rows {
            if let Some(request) = &mut row.request
                && !request.resolved
            {
                request.identity = state.request_identity(source, &request.owner, &request.id);
                request.resolved = request.identity.is_none();
            }
            if let Some(card) = self.cards.get(&row.id) {
                card.update(cx, |card, cx| {
                    card.sync(row.clone(), self.read_only_reason.is_some(), cx)
                });
            }
        }
        // Create the first card even before the transcript has ever been measured.
        self.pending_panel(window, cx);
        for receipt in update.receipts {
            self.update_receipt(receipt, window, cx);
        }
        self.turn_running = matches!(
            state.state,
            gitterm_core::chat::types::AgentSessionState::Streaming
        );
        cx.notify();
    }
    pub(crate) fn stop(&mut self, cx: &mut Context<Self>) {
        if self.read_only_reason.is_none()
            && self.turn_running
            && let Some(callback) = &self.callback
        {
            self.control_receipt = Some(callback(ChatIntent::Stop, cx));
            cx.notify();
        }
    }
    fn jump(&mut self, cx: &mut Context<Self>) {
        self.new_messages = false;
        self.scroller
            .update(cx, |state, cx| state.scroll_to_end(cx));
        cx.notify();
    }
    /// Retained scroll state for embedding hosts (independent of dock layout).
    pub fn scroller(&self) -> &Entity<MessageScrollerState> {
        &self.scroller
    }
    fn row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<RowView> {
        self.recent_rows.retain(|value| *value != index);
        self.recent_rows.push_back(index);
        // Rich text/diff entities are bounded even after walking through long history.
        while self.recent_rows.len() > 96 {
            if let Some(old) = self.recent_rows.pop_front()
                && let Some(view) = self.row_views[old].take()
            {
                self.folded.insert(view.read(cx).row.id, view.read(cx).open);
            }
        }
        if let Some(view) = &self.row_views[index] {
            return view.clone();
        }
        let row = self.model.rows[index].clone();
        let groups = turns::group(&self.model.rows);
        let turn = groups
            .iter()
            .find(|turn| turn.user_row == index || turn.contains(index))
            .cloned();
        let turn_collapsed = turn.as_ref().is_some_and(|turn| {
            self.folded_turns
                .contains(&self.model.rows[turn.user_row].id)
        });
        let turn_rows = turn.as_ref().map_or(0, |turn| turn.end - turn.user_row - 1);
        let turn_toggle: TurnToggle = {
            let owner = cx.entity().downgrade();
            Rc::new(move |id, cx| {
                _ = owner.update(cx, |view, cx| view.toggle_turn(id, cx));
            })
        };
        let user_turn = row.kind == RowKind::User && turn_rows > 0;
        let turn_id = row.id;
        let scroller = self.scroller.clone();
        let open = self.folded.get(&row.id).copied();
        let view = cx.new(|cx| {
            let mut view = RowView::new(row, index, scroller, cx);
            if let Some(open) = open {
                view.open = open;
            }
            view.appearance = self.appearance;
            view.turn_collapsed = turn_collapsed;
            view.hidden_by_turn =
                turn_collapsed && turn.as_ref().is_some_and(|turn| turn.contains(index));
            view.turn_toggle = turn_toggle;
            view.user_turn = user_turn;
            view.turn_id = turn_id;
            view.turn_rows = turn_rows;
            view
        });
        self.row_views[index] = Some(view.clone());
        self.update_row_find(index, cx);
        view
    }
    fn pending_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let pending = self.model.pending_rows();
        if pending.is_empty() {
            return div().into_any_element();
        }
        // Worker delivery is gated; it must not strand a parent's answerable request.
        let position = pending
            .iter()
            .position(|&index| {
                self.model.rows[index]
                    .request
                    .as_ref()
                    .unwrap()
                    .owner
                    .is_empty()
            })
            .unwrap_or(0);
        let index = pending[position];
        let row = self.model.rows[index].clone();
        let card = self
            .cards
            .entry(row.id)
            .or_insert_with(|| {
                cx.new(|cx| {
                    let mut card = CardView::new(
                        row,
                        self.callback.clone(),
                        self.read_only_reason.is_some(),
                        window,
                        cx,
                    );
                    card.preview = self.preview;
                    card
                })
            })
            .clone();
        div()
            .id("pending-request-panel")
            .debug_selector(|| "pending-request-panel".into())
            .max_h(window.viewport_size().height * 0.45)
            .overflow_y_scroll()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(10.))
            .border_1()
            .border_color(rgb(self.appearance.palette().border))
            .rounded(px(6.))
            .when(position > 0, |panel| {
                panel.child("Earlier worker requests · read-only")
            })
            .when(pending.len() > 1, |panel| {
                panel.child(format!("{} of {}", position + 1, pending.len()))
            })
            .child(card)
            .into_any_element()
    }
    fn toggle_turn(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.folded_turns.insert(id) {
            self.folded_turns.remove(&id);
        }
        self.sync_turn_visibility(cx);
    }
    fn sync_turn_visibility(&mut self, cx: &mut Context<Self>) {
        let groups = turns::group(&self.model.rows);
        let hidden = (0..self.model.rows.len())
            .map(|index| {
                turns::containing(&groups, index).is_some_and(|turn| {
                    self.folded_turns
                        .contains(&self.model.rows[turn.user_row].id)
                })
            })
            .collect::<Vec<_>>();
        let folded_turns = self.folded_turns.clone();
        for (index, view) in self.row_views.iter().enumerate() {
            if let Some(view) = view {
                view.update(cx, |row, cx| {
                    if row.hidden_by_turn != hidden[index]
                        || (row.user_turn
                            && row.turn_collapsed != folded_turns.contains(&row.turn_id))
                    {
                        row.hidden_by_turn = hidden[index];
                        row.turn_collapsed = row.user_turn && folded_turns.contains(&row.turn_id);
                        cx.notify();
                    }
                });
            }
        }
        self.scroller
            .update(cx, |state, cx| state.remeasure_items(0..hidden.len(), cx));
        cx.notify();
    }
    fn start_replay(&mut self, _: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.replay.take();
        self.model = ChatModel::default();
        self.row_views.clear();
        self.cards.clear();
        self.recent_rows.clear();
        self.folded.clear();
        self.turn_running = false;
        self.fixture_composer = gitterm_core::chat::state::State::default();
        self.composer_state = crate::composer::ComposerState::default();
        self.new_messages = false;
        self.replay_position = 0;
        self.scroller.update(cx, |state, cx| state.reset(0, cx));
        self.reset_find(cx);
        let records = self.records.clone();
        self.replay = Some(cx.spawn(async move |this, cx| {
            for record in records.iter() {
                cx.background_executor()
                    .timer(Duration::from_millis(35))
                    .await;
                if this
                    .update(cx, |this, cx| this.append_record(record, cx))
                    .is_err()
                {
                    return;
                }
            }
            _ = this.update(cx, |this, cx| {
                this.replay = None;
                cx.notify();
            });
        }));
        cx.notify();
    }
    /// Append one normalized live event. Only ingestion changes the transcript.
    pub fn append_event(&mut self, event: HarnessEvent, cx: &mut Context<Self>) {
        self.append_record(
            &Record {
                at_ms: None,
                entry: Entry::Harness(event),
            },
            cx,
        );
    }
    /// Append a timestamped transcript/fixture entry using the same live path.
    pub fn append_record(&mut self, record: &Record, cx: &mut Context<Self>) {
        self.turn_running = running_after(&record.entry, self.turn_running);
        if let Entry::Harness(event) = &record.entry {
            let event = gitterm_core::chat::state::AgentEvent::Harness(event.clone());
            let effects = self
                .fixture_composer
                .reduce(gitterm_core::chat::state::Intent::Event(event.clone()));
            if matches!(
                event,
                gitterm_core::chat::state::AgentEvent::Harness(HarnessEvent::Ready { .. })
            ) {
                self.fixture_composer.conversation = vec![event];
            }
            self.composer_state.project(
                &self.fixture_composer,
                match &effects {
                    gitterm_core::chat::state::Effect::Event(effects) => Some(effects),
                    _ => None,
                },
            );
        }
        let following = self.scroller.read(cx).is_following_tail();
        let count = self.model.rows.len();
        let changed = self.model.apply(record);
        self.sync_activity_tick(cx);
        self.refresh_rows(count, following, changed, cx);
    }
    fn refresh_rows(
        &mut self,
        count: usize,
        following: bool,
        changed: Vec<usize>,
        cx: &mut Context<Self>,
    ) {
        self.row_views.resize(self.model.rows.len(), None);
        if !changed.is_empty() && !following {
            self.new_messages = true;
        }
        self.scroller.update(cx, |state, cx| {
            state.append(self.model.rows.len() - count, cx);
            for &index in &changed {
                state.remeasure_items(index..index + 1, cx);
            }
        });
        for &index in &changed {
            if let Some(card) = self.cards.get(&self.model.rows[index].id) {
                card.update(cx, |card, cx| {
                    card.sync(
                        self.model.rows[index].clone(),
                        self.read_only_reason.is_some(),
                        cx,
                    )
                });
            }
            if let Some(view) = &self.row_views[index] {
                let row = self.model.rows[index].clone();
                view.update(cx, |view, cx| view.set_row(row, cx));
            }
        }
        if self.find_open && !self.find.query.is_empty() {
            self.find.refresh(&self.model, &changed);
            for index in 0..self.row_views.len() {
                self.update_row_find(index, cx);
            }
        }
        self.replay_position += 1;
        cx.notify();
    }
}
impl Focusable for ChatView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.appearance.palette();
        let send_blocked = self.send_blocked(window, cx);
        let owner = cx.entity().downgrade();
        let copy_owner = owner.clone();
        let composer_focused = self.composer.read(cx).focus_handle(cx).is_focused(window);
        let mut context = String::from("NativeChat");
        if composer_focused && self.read_only_reason.is_none() {
            context.push_str(" IcedComposer");
            let draft = self.composer.read(cx).value();
            if self.composer.read(cx).selected_range().start == draft.len()
                && !self.composer_state.slash_matches(&draft).is_empty()
            {
                context.push_str(" IcedSlash");
            }
        }
        let column = div()
            .id("native-chat")
            .debug_selector(|| "native-chat".into())
            .key_context(context.as_str())
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &SubmitPrompt, window, cx| this.send(window, cx)))
            .on_action(cx.listener(|this, _: &Newline, window, cx| {
                if this.read_only_reason.is_none()
                    && this.composer.read(cx).focus_handle(cx).is_focused(window)
                {
                    window.dispatch_action(
                        Box::new(InputEnter {
                            shift: true,
                            secondary: false,
                        }),
                        cx,
                    );
                }
            }))
            .on_action(cx.listener(|this, _: &SlashNext, w, cx| {
                this.slash_key("down", w, cx);
            }))
            .on_action(cx.listener(|this, _: &SlashPrevious, w, cx| {
                this.slash_key("up", w, cx);
            }))
            .on_action(cx.listener(|this, _: &FillSlash, w, cx| {
                this.slash_key("tab", w, cx);
            }))
            .on_action(cx.listener(|this, _: &SubmitSlash, w, cx| {
                this.slash_key("enter", w, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseSlash, w, cx| {
                this.slash_key("escape", w, cx);
            }))
            .capture_action(cx.listener(|this, action: &InputEnter, window, cx| {
                if action.shift || action.secondary || !this.slash_key("enter", window, cx) {
                    cx.propagate();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, window, cx| {
                if !this.slash_key("down", window, cx) {
                    cx.propagate();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveUp, window, cx| {
                if !this.slash_key("up", window, cx) {
                    cx.propagate();
                }
            }))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                if !this.slash_key("tab", window, cx) {
                    cx.propagate();
                }
            }))
            .capture_action(cx.listener(|this, _: &InputEscape, window, cx| {
                if !this.slash_key("escape", window, cx) {
                    cx.propagate();
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.composer_state.slash_dismissed = true;
                cx.notify();
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    // A child input/selection may already have claimed focus during bubbling.
                    if !this.focus.contains_focused(window, cx) {
                        this.focus.focus(window, cx);
                    }
                }),
            )
            // Textarea propagates submit Enter; consume it so platform text input
            // cannot insert a newline into an empty draft after the submit event.
            .on_action(cx.listener(|this, _: &InputEnter, window, cx| {
                if !this.composer.read(cx).focus_handle(cx).is_focused(window) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(Self::open_find))
            .on_action(cx.listener(Self::close_find))
            .on_action(
                cx.listener(|this, _: &NextMatch, window, cx| this.navigate(false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PreviousMatch, window, cx| this.navigate(true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &CopyPlain, window, cx| this.copy(false, window, cx)))
            .on_action(
                cx.listener(|this, _: &CopyMarkdown, window, cx| this.copy(true, window, cx)),
            )
            .size_full()
            .when_some(self.column_width(), |column, width| column.max_w(px(width)))
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(10.))
            .bg(rgb(palette.base))
            .text_color(rgb(palette.fg))
            .font_family(".SystemUIFont")
            .text_size(px(self.appearance.font_size))
            .when(self.find_open, |view| view.child(self.find_bar(cx)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .bg(rgb(palette.alt))
                    .rounded(px(8.))
                    .child(
                        MessageScroller::new(
                            "messages",
                            self.scroller.clone(),
                            move |index, window, cx| {
                                owner
                                    .update(cx, |this, cx| this.row(index, window, cx))
                                    .map(IntoElement::into_any_element)
                                    .unwrap_or_else(|_| div().into_any_element())
                            },
                        )
                        .jump_button(false)
                        .with_content_style(StyleRefinement::default().px(px(14.)).py(px(12.))),
                    )
                    .context_menu(move |menu, window, cx| {
                        Self::selection_menu(copy_owner.clone(), menu, window, cx)
                    }),
            )
            .when(
                self.new_messages && !self.scroller.read(cx).is_following_tail(),
                |view| {
                    view.child(
                        Button::new("new-messages")
                            .label("New messages · Jump to latest")
                            .on_click(cx.listener(|this, _, _, cx| this.jump(cx))),
                    )
                },
            )
            .child(
                div()
                    .id("chat-footer")
                    .debug_selector(|| "chat-footer".into())
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .w_full()
                    .mx_auto()
                    .px(px(8.))
                    .pt(px(6.))
                    .pb(px(8.))
                    .bg(rgb(palette.footer))
                    .border_1()
                    .border_color(rgb(palette.border))
                    .rounded(px(10.))
                    .child(self.composer_state.render_activity(self.appearance))
                    .child(self.pending_panel(window, cx))
                    .children(
                        self.submission
                            .iter()
                            .chain(self.control_receipt.iter())
                            .filter(|receipt| {
                                matches!(
                                    receipt.state,
                                    ReceiptState::Rejected(_) | ReceiptState::Uncertain(_)
                                )
                            })
                            .enumerate()
                            .map(|(index, receipt)| {
                                div()
                                    .id(("chat-command-error", index))
                                    .debug_selector(|| "chat-command-error".into())
                                    .text_size(px(self.appearance.size(12.)))
                                    .text_color(rgb(palette.err))
                                    .child(receipt.state.label())
                            }),
                    )
                    .children((!self.images.is_empty()).then(|| self.render_attachments(cx)))
                    .child(self.composer_state.render(
                        &self.composer,
                        self.read_only_reason.clone().or_else(|| {
                            self.callback
                                .is_none()
                                .then(|| "No session adapter attached".into())
                        }),
                        send_blocked,
                        &self.slash_scroll,
                        self.appearance,
                        window,
                        cx,
                    ))
                    .child(roster::render(
                        &self.roster,
                        &self.roster_hidden,
                        crate::composer::now_ms(),
                        self.appearance,
                        cx,
                    )),
            );
        div()
            .size_full()
            .flex()
            .justify_center()
            .bg(rgb(palette.base))
            .child(column)
    }
}

struct RowView {
    appearance: ChatAppearance,
    sent_images: Vec<Result<attachments::Thumbnail, String>>,
    preparing_sent_images: Option<Task<()>>,
    row: Row,
    text: Entity<TextViewState>,
    diffs: Vec<Entity<DiffState>>,
    diff_error: Option<String>,
    open: bool,
    query: String,
    active: Option<usize>,
    reveal_pending: bool,
    highlight_signature: Option<u64>,
    copied: HashMap<CopyTarget, Task<()>>,
    index: usize,
    scroller: Entity<MessageScrollerState>,
    _subscription: Subscription,
    turn_collapsed: bool,
    hidden_by_turn: bool,
    user_turn: bool,
    turn_id: u64,
    turn_rows: usize,
    turn_toggle: TurnToggle,
}
impl RowView {
    fn new(
        row: Row,
        index: usize,
        scroller: Entity<MessageScrollerState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = cx.new(|cx| {
            TextViewState::markdown(&row.body, cx)
                .selectable(true)
                .scrollable(false)
                .motion(TextViewMotion::default().with_stream_fade(Duration::from_millis(280)))
        });
        let subscription = cx.observe(&text, |this, _, cx| {
            this.scroller.update(cx, |state, cx| {
                state.remeasure_items(this.index..this.index + 1, cx);
            });
            cx.notify();
        });
        let open = !row.collapsible();
        let mut view = Self {
            appearance: ChatAppearance::default(),
            sent_images: vec![],
            preparing_sent_images: None,
            row,
            text,
            diffs: vec![],
            diff_error: None,
            open,
            query: String::new(),
            active: None,
            reveal_pending: false,
            highlight_signature: None,
            copied: HashMap::new(),
            index,
            scroller,
            _subscription: subscription,
            turn_collapsed: false,
            hidden_by_turn: false,
            user_turn: false,
            turn_id: 0,
            turn_rows: 0,
            turn_toggle: Rc::new(|_, _| {}),
        };
        view.make_diffs(cx);
        view.prepare_sent_images(cx);
        view
    }
    fn set_row(&mut self, row: Row, cx: &mut Context<Self>) {
        let edits_changed = self.row.edits() != row.edits();
        let images_changed = self.row.images != row.images;
        self.text
            .update(cx, |text, cx| text.set_text(&row.body, cx));
        self.row = row;
        if edits_changed {
            self.make_diffs(cx);
        }
        if images_changed {
            self.prepare_sent_images(cx);
        }
        cx.notify();
    }
    fn make_diffs(&mut self, cx: &mut Context<Self>) {
        self.diffs.clear();
        self.diff_error = None;
        for edit in self.row.edits() {
            let patch = edit_patch(&edit);
            match DiffFile::parse(&patch) {
                Ok(files) => self.diffs.push(cx.new(|cx| DiffState::new(files, cx))),
                Err(error) => {
                    self.diff_error =
                        Some(format!("Cannot display diff for {}: {error}", edit.path))
                }
            }
        }
    }
    fn toggle(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.open = !self.open;
        self.scroller.update(cx, |state, cx| {
            state.remeasure_items(self.index..self.index + 1, cx);
        });
        cx.notify();
    }
}
impl Render for RowView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.hidden_by_turn {
            return div().h(px(0.)).mb(px(0.)).into_any_element();
        }
        self.highlight(cx);
        let palette = self.appearance.palette();
        let color = match self.row.kind {
            RowKind::User => palette.user,
            RowKind::Thinking | RowKind::Status => palette.muted,
            RowKind::Tool if self.row.tool.as_ref().is_some_and(|tool| tool.is_error) => {
                palette.err
            }
            RowKind::Tool
                if self
                    .row
                    .tool
                    .as_ref()
                    .is_some_and(|tool| tool.output.is_some()) =>
            {
                palette.result
            }
            RowKind::Tool => palette.tool,
            RowKind::Error => palette.err,
            RowKind::Permission | RowKind::Question => palette.warn,
            RowKind::Worker | RowKind::Delegation => palette.result,
            _ => palette.fg,
        };
        let content = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(10.))
            .children(
                self.diff_error
                    .clone()
                    .map(|error| div().text_color(rgb(palette.err)).child(error)),
            )
            .children(
                self.diffs
                    .iter()
                    .map(|diff| div().h(px(260.)).child(Diff::new(diff))),
            )
            .children((!self.row.images.is_empty()).then(|| self.render_sent_images()))
            .child(self.copyable_text(cx));
        let header = div()
            .text_size(px(self.appearance.size(13.)))
            .text_color(rgb(color))
            .child(self.row.title.clone())
            .when(self.row.kind == RowKind::Assistant, |header| {
                header.child(self.reply_copy_buttons(cx))
            })
            .when(self.user_turn, |header| {
                let id = self.turn_id;
                let toggle = self.turn_toggle.clone();
                header.child(
                    Button::new(("turn-fold", id))
                        .small()
                        .label(if self.turn_collapsed {
                            format!("Unfold turn · {} rows", self.turn_rows)
                        } else {
                            format!("Fold turn · {} rows", self.turn_rows)
                        })
                        .on_click(move |_, _, cx| toggle(id, cx)),
                )
            });
        div()
            .id(("row", self.row.id))
            .text_size(px(self.appearance.font_size))
            .text_color(rgb(match self.row.kind {
                RowKind::Thinking | RowKind::Status => palette.muted,
                RowKind::Error => palette.err,
                _ => palette.fg,
            }))
            .font_family(".SystemUIFont")
            .w_full()
            .max_w(px(960.))
            .mx_auto()
            .mb(px(10.))
            .when(!self.row.scope.is_empty(), |view| {
                view.ml(px(18.))
                    .border_l_2()
                    .border_color(rgb(palette.border))
            })
            .when(self.row.kind == RowKind::User, |view| {
                view.max_w(px(816.))
                    .ml_auto()
                    .mr_0()
                    .bg(rgb(palette.block))
                    .border_1()
                    .border_color(rgb(palette.border))
                    .rounded(px(10.))
            })
            .when(
                matches!(
                    self.row.kind,
                    RowKind::Tool | RowKind::Permission | RowKind::Question | RowKind::Delegation
                ),
                |view| {
                    view.bg(rgb(palette.base))
                        .border_1()
                        .border_l_3()
                        .border_color(rgb(color))
                        .rounded(px(6.))
                },
            )
            .child(if self.row.collapsible() {
                Collapsible::new()
                    .open(self.open)
                    .child(
                        Button::new("fold")
                            .small()
                            .label(format!(
                                "{} {}",
                                if self.open { "▾" } else { "▸" },
                                self.row.title
                            ))
                            .text_color(rgb(color))
                            .on_click(cx.listener(Self::toggle)),
                    )
                    .content(content)
                    .into_any_element()
            } else {
                div().child(header).child(content).into_any_element()
            })
            .into_any_element()
    }
}

fn running_after(entry: &Entry, previous: bool) -> bool {
    match entry {
        Entry::Harness(HarnessEvent::TurnStarted { .. }) => true,
        Entry::Harness(
            HarnessEvent::TurnCompleted { .. }
            | HarnessEvent::ProcessExited { .. }
            | HarnessEvent::ConversationReset { .. }
            | HarnessEvent::RemoteDisconnected { .. },
        ) => false,
        Entry::Harness(HarnessEvent::HistoryReplayed { entries }) => {
            entries.iter().fold(previous, |running, entry| {
                running_after(
                    &match entry {
                        gitterm_core::harness::transcript::TranscriptEntry::UserPrompt(text) => {
                            Entry::UserPrompt(text.clone())
                        }
                        gitterm_core::harness::transcript::TranscriptEntry::Harness(event) => {
                            Entry::Harness(event.clone())
                        }
                    },
                    running,
                )
            })
        }
        _ => previous,
    }
}

/// Full replacement fragments, matching the HTML Edit/MultiEdit/Write preview.
/// No filesystem read is needed, and no full-file context is invented.
fn edit_patch(edit: &EditDiff) -> String {
    let old_path = serde_json::to_string(&format!("a/{}", edit.path)).expect("string JSON");
    let new_path = serde_json::to_string(&format!("b/{}", edit.path)).expect("string JSON");
    let old: Vec<_> = edit.old.lines().collect();
    let new: Vec<_> = edit.new.lines().collect();
    let mut patch = format!(
        "--- {old_path}\n+++ {new_path}\n@@ -{},{} +{},{} @@\n",
        usize::from(!old.is_empty()),
        old.len(),
        usize::from(!new.is_empty()),
        new.len()
    );
    for (prefix, lines) in [('-', old), ('+', new)] {
        for line in lines {
            patch.push(prefix);
            patch.push_str(line);
            patch.push('\n');
        }
    }
    patch
}

#[cfg(test)]
mod tests;
