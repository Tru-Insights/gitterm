mod attachments;
#[path = "copy.rs"]
mod copy;
#[path = "find.rs"]
mod find;
mod keys;
#[path = "review/ui.rs"]
pub(crate) mod review_ui;
mod scroll;
mod subagent;
mod tool_runs;
mod transcript;
pub use scroll::TranscriptScrollerState;
use scroll::TranscriptScrollerState as MessageScrollerState;
mod visual;
pub use visual::{
    VisualFactory, VisualHost, VisualMessage, VisualPlacement, VisualSurface, visual_link,
};
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
    Transformation, Window, WindowBounds, WindowOptions, actions, base::TextSelection, div,
    prelude::FluentBuilder, px, radians, rgb, size, svg,
};
use gpui_component::{
    Disableable, Root, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    collapsible::Collapsible,
    input::{
        Copy as InputCopy, Enter as InputEnter, Escape as InputEscape, IndentInline, Input,
        InputEvent, InputState, MoveDown, MoveUp, TextareaState,
    },
    menu::{ContextMenuExt, PopupMenuItem},
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
        CloseSlash,
        CycleMode,
        SubmitReview,
        ScrollLineUp,
        ScrollLineDown,
        ScrollPageUp,
        ScrollPageDown,
        ScrollTop,
        ScrollEnd
    ]
);

pub type IntentCallback = Rc<dyn Fn(ChatIntent, &mut App) -> Receipt>;
type RowAction = Rc<dyn Fn(crate::interactions::TranscriptAction, &mut App)>;
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
        // A browser scrolls the focused scroll box with these keys. Space is
        // left out: bound here it would also swallow typing in the inputs.
        KeyBinding::new("up", ScrollLineUp, Some("NativeChat")),
        KeyBinding::new("down", ScrollLineDown, Some("NativeChat")),
        KeyBinding::new("pageup", ScrollPageUp, Some("NativeChat")),
        KeyBinding::new("pagedown", ScrollPageDown, Some("NativeChat")),
        KeyBinding::new("home", ScrollTop, Some("NativeChat")),
        KeyBinding::new("end", ScrollEnd, Some("NativeChat")),
        KeyBinding::new("cmd-up", ScrollTop, Some("NativeChat")),
        KeyBinding::new("cmd-down", ScrollEnd, Some("NativeChat")),
    ]);
}

/// Owns the document model and retained selection/fold state independently of list layout.
/// Rich-text state is initialized only for visited rows; it is not the event source of truth.
pub struct ChatView {
    appearance: ChatAppearance,
    width: gitterm_core::settings::ChatWidth,
    records: Arc<Vec<Record>>,
    history_start: usize,
    imported_offer: crate::imported::ImportedOffer,
    flashed_row: Option<u64>,
    flash_timer: Option<Task<()>>,
    preview: bool,
    composer: Entity<TextareaState>,
    composer_state: crate::composer::ComposerState,
    pub(crate) review: review_ui::ReviewComposer,
    slash_scroll: gpui::ScrollHandle,
    roster: Vec<roster::RosterEntry>,
    roster_hidden: HashSet<String>,
    activity_tick: Option<Task<()>>,
    /// Raw fixture ingress only; live projection is always supplied by the owner.
    fixture_composer: gitterm_core::chat::state::State,
    /// The live state seen before the session was ready, so a pre-start pick can
    /// re-project the chips at once. Dropped once the session is ready.
    shadow: Option<gitterm_core::chat::state::State>,
    checkout_source: Option<crate::composer::CheckoutSource>,
    dismiss_gate: Option<crate::interactions::DismissGate>,
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
    /// Row views by recency, each with the frame that last drew it.
    recent_rows: VecDeque<(usize, u64)>,
    /// Counts `render` calls, so eviction knows which rows this frame drew.
    frame: u64,
    folded: HashMap<u64, bool>,
    retained_edit_expansion: HashMap<u64, HashSet<usize>>,
    /// Iced's `card.selected`, kept while a card's row view is evicted.
    retained_finding_selection: HashMap<u64, HashSet<String>>,
    /// The checkout's HEAD for the cards' "branch has moved" banner.
    checkout_head: Option<String>,
    folded_turns: HashSet<u64>,
    open_runs: HashSet<u64>,
    open_work: HashSet<u64>,
    /// Rows whose nested subagent block is open, kept while a row view is evicted.
    open_subs: HashSet<u64>,
    /// Completed turns whose live runs are gone (Iced's `unwrapLiveRun`).
    completed_seen: usize,
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
    /// The visual reply shown over the chat, as Iced's `.html-render.expanded`.
    expanded_visual: Option<Entity<visual::VisualReply>>,
    _subscriptions: Vec<Subscription>,
}
impl gpui::EventEmitter<roster::RosterOutcome> for ChatView {}
impl gpui::EventEmitter<crate::review::ReviewOutcome> for ChatView {}

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
                .placeholder(COMPOSER_PLACEHOLDER)
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
                    if let HarnessEvent::Ready {
                        permission_mode: Some(mode),
                        ..
                    } = event
                    {
                        fixture_composer.acknowledged.mode = Some(mode.clone());
                    }
                    fixture_composer.conversation =
                        vec![gitterm_core::chat::state::AgentEvent::Harness(
                            event.clone(),
                        )];
                }
            }
        }
        let mut composer_state = crate::composer::ComposerState::default();
        composer_state.project(&fixture_composer, None);
        let mut this = Self {
            appearance: ChatAppearance::default(),
            width: Default::default(),
            imported_offer: records
                .iter()
                .rev()
                .find_map(|record| {
                    if let Entry::ImportedOffer(offer) = &record.entry {
                        Some(offer.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default(),
            history_start: scroll::initial_start(&model),
            flashed_row: None,
            flash_timer: None,
            records: Arc::new(records),
            preview: false,
            composer,
            composer_state,
            review: review_ui::ReviewComposer::new(window, cx),
            slash_scroll: gpui::ScrollHandle::new(),
            roster: vec![],
            roster_hidden: HashSet::new(),
            activity_tick: None,
            fixture_composer,
            shadow: None,
            checkout_source: None,
            dismiss_gate: None,
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
            expanded_visual: None,
            recent_rows: VecDeque::new(),
            frame: 0,
            folded: HashMap::new(),
            retained_edit_expansion: HashMap::new(),
            retained_finding_selection: HashMap::new(),
            checkout_head: None,
            folded_turns: HashSet::new(),
            open_runs: HashSet::new(),
            open_work: HashSet::new(),
            open_subs: HashSet::new(),
            completed_seen: model.completed_turns.len(),
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
        };
        this.open_folds_for_attention();
        this
    }
    /// Change the native column without discarding folds, drafts or selection.
    pub fn set_width(&mut self, width: gitterm_core::settings::ChatWidth, cx: &mut Context<Self>) {
        if self.width != width {
            self.width = width;
            let width = self.column_width().map(|width| width - 28.);
            for row in self.row_views.iter().flatten() {
                row.update(cx, |row, cx| {
                    row.column_width = width;
                    cx.notify();
                });
            }
            self.scroller.update(cx, |state, cx| {
                state.remeasure_items(0..self.model.rows.len(), cx)
            });
            cx.notify();
        }
    }

    /// Iced fixes the document root at 14px, independently of the chat font.
    fn column_width(&self) -> Option<f32> {
        use gitterm_core::settings::ChatWidth;
        match self.width {
            ChatWidth::Narrow => Some(46. * 14. + 28.),
            ChatWidth::Medium => Some(60. * 14. + 28.),
            ChatWidth::Wide => Some(72. * 14. + 28.),
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
    pub fn set_roster(&mut self, rows: Vec<roster::RosterEntry>, cx: &mut Context<Self>) {
        self.roster = rows;
        self.sync_activity_tick(cx);
        cx.notify();
    }

    pub fn set_delegations(
        &mut self,
        cards: Vec<crate::delegation::DelegationCard>,
        cx: &mut Context<Self>,
    ) {
        let count = self.model.rows.len();
        let following = self.scroller.read(cx).is_following_tail();
        let mut changed = Vec::new();
        // Registry order is newest-first for the roster; new transcript cards
        // arrive oldest-first when replay has no stored anchor for them.
        for mut card in cards.into_iter().rev() {
            card.checkout_head = self.checkout_head.clone();
            changed.extend(self.model.upsert_delegation(card));
        }
        if !changed.is_empty() {
            self.refresh_rows(count, following, changed, cx);
        }
    }

    /// Iced's `__setDelegationHead`: the chat checkout's HEAD, which a
    /// review card compares with the commit it reviewed.
    pub fn set_checkout_head(&mut self, head: Option<String>, cx: &mut Context<Self>) {
        if self.checkout_head == head {
            return;
        }
        self.checkout_head = head;
        let cards: Vec<_> = self
            .model
            .rows
            .iter()
            .filter_map(|row| row.delegation.clone())
            .collect();
        let count = self.model.rows.len();
        let following = self.scroller.read(cx).is_following_tail();
        let mut changed = Vec::new();
        for mut card in cards {
            card.checkout_head = self.checkout_head.clone();
            changed.extend(self.model.upsert_delegation(card));
        }
        if !changed.is_empty() {
            self.refresh_rows(count, following, changed, cx);
        }
    }

    pub(crate) fn roster_outcome(
        &mut self,
        outcome: roster::RosterOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = match &outcome {
            roster::RosterOutcome::Open { id } | roster::RosterOutcome::Dismiss { id } => id,
            _ => return,
        };
        if !self.roster.iter().any(|row| &row.id == id) || self.roster_hidden.contains(id) {
            return;
        }
        match &outcome {
            roster::RosterOutcome::Dismiss { id } => {
                self.roster_hidden.insert(id.clone());
                self.sync_activity_tick(cx);
            }
            roster::RosterOutcome::Open { id } => self.reveal_roster(id, window, cx),
            _ => unreachable!(),
        }
        cx.emit(outcome);
        cx.notify();
    }

    /// Iced's `__reviewNote`: a warning note in the transcript saying why an
    /// action was not done.
    pub fn note_warning(&mut self, text: String, cx: &mut Context<Self>) {
        let following = self.scroller.read(cx).is_following_tail();
        let count = self.model.rows.len();
        let changed = self.model.push_warning(&text);
        self.refresh_rows(count, following, changed, cx);
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
        self.sync_queued(cx);
        cx.notify();
    }
    pub fn set_read_only(&mut self, reason: Option<String>, cx: &mut Context<Self>) {
        self.read_only_reason = reason;
        if self.read_only_reason.is_some() {
            self.composer_state.picker = None;
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
        self.sync_queued(cx);
        cx.notify();
    }
    /// Saved tab settings fill gaps in history; recorded acknowledgements win.
    /// This is display state only and grants no runtime or picker authority.
    pub fn set_history_config(
        &mut self,
        config: &gitterm_core::chat::types::AgentBackendConfig,
        cx: &mut Context<Self>,
    ) {
        self.composer_state.backend = Some(config.backend());
        let acknowledged = &mut self.fixture_composer.acknowledged;
        if acknowledged.model.is_none() {
            acknowledged.model = config
                .chat_model()
                .filter(|m| !m.is_empty())
                .map(str::to_owned);
        }
        if acknowledged.effort.is_none() {
            acknowledged.effort = Some(gitterm_core::chat::state::ConfirmedEffort {
                effort: config.chat_effort().map(str::to_owned),
                applied: None,
            });
        }
        if acknowledged.mode.is_none() {
            use gitterm_core::chat::types::AgentBackendConfig;
            acknowledged.mode = match config {
                AgentBackendConfig::Claude {
                    permission_mode, ..
                } => Some(permission_mode.as_deref().unwrap_or("default").into()),
                AgentBackendConfig::Acp { agent, mode, .. } => {
                    Some(mode.as_deref().unwrap_or(agent.default_mode()).into())
                }
                AgentBackendConfig::Pi { .. } => None,
            };
        }
        self.composer_state.project(&self.fixture_composer, None);
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
    pub fn set_backend(
        &mut self,
        backend: gitterm_core::chat::types::AgentBackend,
        cx: &mut Context<Self>,
    ) {
        self.composer_state.backend = Some(backend);
        // Notes name the harness. A transcript built from saved records
        // before the backend was known is rebuilt with it (same rows).
        if self.model.backend != Some(backend) {
            self.model.backend = Some(backend);
            if self.projection_runtime.is_none() && self.replay.is_none() {
                let rebuilt = ChatModel::from_records_for(&self.records, Some(backend));
                if rebuilt.rows.len() == self.model.rows.len() {
                    let live = self.model.live;
                    self.model = rebuilt;
                    self.model.live = live;
                    for (index, view) in self.row_views.iter().enumerate() {
                        if let Some(view) = view {
                            let row = self.model.rows[index].clone();
                            view.update(cx, |view, cx| view.set_row(row, cx));
                        }
                    }
                }
            }
        }
        if backend != gitterm_core::chat::types::AgentBackend::Claude {
            self.review.open = false;
        }
        self.sync_queued(cx);
        cx.notify();
    }
    /// Picks made before the session is ready are held and sent when it starts.
    fn settings_enabled(&self) -> bool {
        self.read_only_reason.is_none() && self.callback.is_some()
    }
    /// The host's source for the checkout menu, asked again whenever it opens.
    pub fn set_checkout_source(
        &mut self,
        source: crate::composer::CheckoutSource,
        cx: &mut Context<Self>,
    ) {
        self.checkout_source = Some(source);
        cx.notify();
    }
    /// The host's gate for delegation card dismissals.
    pub fn set_dismiss_gate(
        &mut self,
        gate: crate::interactions::DismissGate,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_gate = Some(gate);
        cx.notify();
    }
    fn reproject(&mut self) {
        let state = self.shadow.as_ref().unwrap_or(&self.fixture_composer);
        self.composer_state.project(state, None);
    }
    /// Send the next pre-start pick once the session is ready. Deferred, because
    /// the host may be mid-update when it presents the ready state.
    fn schedule_prestart(&mut self, cx: &mut Context<Self>) {
        if self.composer_state.commands_ready
            && !self.composer_state.prestart.is_empty()
            && !self.composer_state.prestart_sent
            && self.composer_state.pending_setting.is_none()
        {
            cx.spawn(async move |this, cx| {
                _ = this.update(cx, |this, cx| this.send_prestart(cx));
            })
            .detach();
        }
    }
    fn send_prestart(&mut self, cx: &mut Context<Self>) {
        use gitterm_core::chat::types::SettingChange as C;
        let state = &mut self.composer_state;
        if !state.commands_ready
            || state.prestart_sent
            || state.pending_setting.is_some()
            || self.read_only_reason.is_some()
        {
            return;
        }
        let confirmed = state.confirmed.clone();
        state.prestart.retain(|change| match change {
            C::Model(value) => confirmed.model.as_ref() != Some(value),
            C::Mode(value) => confirmed.mode.as_ref() != Some(value),
            C::Effort(value) => confirmed.effort.as_ref().map(|e| &e.effort) != Some(value),
        });
        let (Some(change), Some(callback)) =
            (state.prestart.first().cloned(), self.callback.clone())
        else {
            return;
        };
        state.prestart_sent = true;
        let receipt = callback(ChatIntent::ChangeSetting { change }, cx);
        self.control_receipt = Some(receipt.clone());
        if let ReceiptState::Rejected(reason) | ReceiptState::Uncertain(reason) = receipt.state {
            let state = &mut self.composer_state;
            state.prestart.remove(0);
            state.prestart_sent = false;
            state.notice = Some(reason);
            self.reproject();
        }
        cx.notify();
    }
    pub(crate) fn choose_checkout(
        &mut self,
        path: &str,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer_state.picker = None;
        // Iced's set_checkout rewrites the tab and workspace files; the preview
        // may not, so the choice is shown but not applied.
        if self.composer_state.checkout_locked.is_none()
            && self.composer_state.checkout_path.as_deref() != Some(path)
        {
            self.composer_state.checkout_notice =
                Some(format!("{label} · {}", crate::composer::NOT_IN_PREVIEW));
        }
        self.focus_composer(window, cx);
        cx.notify();
    }
    /// Iced posts `open_checkout` to launch the configured editor; the preview
    /// launches nothing and says so as the other Open in editor controls do.
    pub(crate) fn open_checkout_in_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composer_state.picker = None;
        self.composer_state.notice = Some(format!(
            "Open in editor · {}",
            crate::composer::NOT_IN_PREVIEW
        ));
        self.focus_composer(window, cx);
        cx.notify();
    }
    pub(crate) fn close_picker(&mut self, cx: &mut Context<Self>) {
        self.composer_state.picker = None;
        cx.notify();
    }
    pub(crate) fn toggle_picker(
        &mut self,
        picker: crate::pickers::Picker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if picker == crate::pickers::Picker::Checkout {
            if self.read_only_reason.is_some() {
                return;
            }
            // A locked chat's menu lists only Open in editor, as in Iced.
            if self.composer_state.checkout_locked.is_none()
                && let Some(source) = &self.checkout_source
            {
                self.composer_state.checkout_choices = source(cx);
            }
        } else if !self.settings_enabled() || self.composer_state.catalog.options(picker).is_empty()
        {
            return;
        }
        self.focus_composer(window, cx);
        self.review.open = false;
        self.composer_state.picker = if self.composer_state.picker == Some(picker) {
            None
        } else {
            Some(picker)
        };
        cx.notify();
    }
    pub(crate) fn choose_setting(
        &mut self,
        change: gitterm_core::chat::types::SettingChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer_state.picker = None;
        if self.settings_enabled() && self.composer_state.catalog.offers(&change) {
            let selected = self
                .composer_state
                .catalog
                .models
                .iter()
                .chain(&self.composer_state.catalog.efforts)
                .chain(&self.composer_state.catalog.modes)
                .any(|row| row.change == change && row.selected);
            if !selected && !self.composer_state.commands_ready {
                let state = &mut self.composer_state;
                state
                    .prestart
                    .retain(|held| std::mem::discriminant(held) != std::mem::discriminant(&change));
                state.prestart.push(change);
                self.reproject();
            } else if !selected && let Some(callback) = &self.callback {
                let receipt = callback(ChatIntent::ChangeSetting { change }, cx);
                self.control_receipt = Some(receipt.clone());
                if let ReceiptState::Rejected(reason) | ReceiptState::Uncertain(reason) =
                    receipt.state
                {
                    self.composer_state.notice = Some(reason);
                }
            }
        }
        self.focus_composer(window, cx);
        cx.notify();
    }
    fn cycle_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(change) = self
            .composer_state
            .catalog
            .cycle_mode(self.composer_state.pending_setting.as_ref())
        {
            self.choose_setting(change, window, cx);
        }
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
            // Iced empties the box and attachments as it posts the message; a
            // rejected or uncertain receipt puts the draft back (below).
            self.composer
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.images.clear();
            self.image_previews.clear();
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
        if self
            .submission
            .as_ref()
            .is_some_and(|old| old.accepts_update(&receipt))
            && matches!(
                receipt.state,
                ReceiptState::Rejected(_) | ReceiptState::Uncertain(_)
            )
            && let Some(draft) = self.submitted_draft.take()
        {
            // Newer edits win; only what the user has not retyped comes back.
            if self.composer.read(cx).value().is_empty() {
                self.composer
                    .update(cx, |input, cx| input.set_value(draft.text, window, cx));
            }
            if self.images.is_empty() {
                self.image_previews = vec![None; draft.images.len()];
                self.images = draft.images;
            }
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
        self.shadow = (!self.composer_state.commands_ready).then(|| {
            let mut shadow = gitterm_core::chat::state::State::default();
            shadow.conversation = state.conversation.clone();
            shadow.session_id = state.session_id.clone();
            shadow.acknowledged = state.acknowledged.clone();
            shadow.checkout = state.checkout.clone();
            shadow
        });
        self.schedule_prestart(cx);
        self.sync_activity_tick(cx);
        match update.presentation {
            Some(Presentation::Replay) => {
                self.model = ChatModel::from_conversation_for(
                    &state.conversation,
                    self.composer_state.backend,
                );
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
                self.retained_edit_expansion.clear();
                self.retained_finding_selection.clear();
                self.folded_turns.clear();
                self.open_runs.clear();
                self.open_work.clear();
                self.open_subs.clear();
                self.completed_seen = self.model.completed_turns.len();
                self.open_folds_for_attention();
                self.history_start = scroll::initial_start(&self.model);
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
        self.sync_queued(cx);
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
        self.trim_history(cx);
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
        self.recent_rows.retain(|(value, _)| *value != index);
        self.recent_rows.push_back((index, self.frame));
        // Rich text/diff entities are bounded even after walking through long
        // history, but never below what this frame draws: closed folds lay
        // out each hidden row, and a view dropped mid-frame is rebuilt on the
        // next, its long text unparsed and without height (the reader's place
        // then jumps), every frame.
        while self.recent_rows.len() > 96
            && self
                .recent_rows
                .front()
                .is_some_and(|(_, frame)| *frame != self.frame)
        {
            if let Some((old, _)) = self.recent_rows.pop_front()
                && let Some(view) = self.row_views[old].take()
            {
                let row = view.read(cx);
                self.folded.insert(row.row.id, row.open);
                if row.sub_open {
                    self.open_subs.insert(row.row.id);
                } else {
                    self.open_subs.remove(&row.row.id);
                }
                self.retained_edit_expansion
                    .insert(row.row.id, row.expanded_edits.clone());
                if let Some(selected) = &row.selected_findings {
                    self.retained_finding_selection
                        .insert(row.row.id, selected.clone());
                }
            }
        }
        if let Some(view) = &self.row_views[index] {
            return view.clone();
        }
        let row = self.model.rows[index].clone();
        let work_folds = turns::work_folds(&self.model);
        let run = tool_runs::group(&self.model, &work_folds)
            .into_iter()
            .find(|run| run.range.contains(&index));
        let run_open = run.as_ref().is_none_or(|run| {
            self.open_runs
                .contains(&self.model.rows[run.range.start].id)
        });
        let hidden_by_run = run
            .as_ref()
            .is_some_and(|run| run.range.start != index && !run_open);
        let run_tooltip = run
            .as_ref()
            .map(|run| tool_runs::tooltip(&self.model.rows[run.range.clone()]))
            .unwrap_or_default();
        let run_live = run.as_ref().is_some_and(|run| {
            self.model
                .live_turn_start()
                .is_some_and(|live| run.range.start >= live)
        });
        let run_summary = run
            .filter(|run| run.range.start == index)
            .map(|run| run.label);
        let groups = turns::group(&self.model.rows);
        let turn = groups
            .iter()
            .find(|turn| turn.user_row == index || turn.contains(index))
            .cloned();
        let turn_collapsed = turn.as_ref().is_some_and(|turn| {
            self.folded_turns
                .contains(&self.model.rows[turn.user_row].id)
        });
        let work = work_folds
            .into_iter()
            .find(|fold| fold.range.contains(&index));
        let work_open = work.as_ref().is_none_or(|fold| {
            self.open_work
                .contains(&self.model.rows[fold.range.start].id)
        });
        let hidden_by_work = work
            .as_ref()
            .is_some_and(|fold| fold.range.start != index && !work_open);
        let in_work = work.is_some();
        let work_first = work.as_ref().is_some_and(|fold| fold.range.start == index);
        let work_last = work
            .as_ref()
            .is_some_and(|fold| fold.range.end == index + 1);
        let work_failed = work.as_ref().map_or(0, |fold| fold.failed);
        let work_summary = work
            .filter(|fold| fold.range.start == index)
            .map(|fold| fold.label);
        let work_owner = cx.entity().downgrade();
        let work_toggle: TurnToggle = Rc::new(move |id, cx| {
            _ = work_owner.update(cx, |view, cx| {
                let opening = view.open_work.insert(id);
                if !opening {
                    view.open_work.remove(&id);
                }
                view.scroller
                    .update(cx, |state, cx| state.hold_for_toggle(opening, true, cx));
                view.sync_runs(cx);
            });
        });
        let run_owner = cx.entity().downgrade();
        let run_toggle: TurnToggle = Rc::new(move |id, cx| {
            _ = run_owner.update(cx, |view, cx| {
                let opening = view.open_runs.insert(id);
                if !opening {
                    view.open_runs.remove(&id);
                }
                view.scroller
                    .update(cx, |state, cx| state.hold_for_toggle(opening, true, cx));
                view.sync_runs(cx);
            });
        });
        let owner = cx.entity().downgrade();
        let action: RowAction = Rc::new(move |action, cx| {
            _ = owner.update(cx, |view, cx| view.transcript_action(action, cx));
        });
        let visual_owner = cx.entity().downgrade();
        let visual_expand: visual::VisualExpand = Rc::new(move |reply, cx| {
            _ = visual_owner.update(cx, |view, cx| {
                view.expanded_visual = reply;
                cx.notify();
            });
        });
        let scroller = self.scroller.clone();
        let open = self.folded.get(&row.id).copied();
        let queued = self.queued_tag(&row);
        let block_gap = self.block_top_gap(index);
        let view = cx.new(|cx| {
            let mut view = RowView::new(row, index, scroller, cx);
            if let Some(open) = open {
                view.open = open;
            }
            view.sub_open = self.open_subs.contains(&view.row.id);
            view.cwd = self.composer_state.checkout_path.clone();
            view.expanded_edits = self
                .retained_edit_expansion
                .get(&view.row.id)
                .cloned()
                .unwrap_or_default();
            view.selected_findings = self.retained_finding_selection.get(&view.row.id).cloned();
            view.action = action;
            view.flashed = self.flashed_row == Some(view.row.id);
            view.busy = self.composer_state.streaming;
            view.appearance = self.appearance;
            view.column_width = self.column_width().map(|width| width - 28.);
            view.work_toggle = work_toggle;
            view.work_summary = work_summary;
            view.work_failed = work_failed;
            view.work_open = work_open;
            view.in_work = in_work;
            view.work_first = work_first;
            view.work_last = work_last;
            view.hidden_by_work = hidden_by_work;
            view.run_toggle = run_toggle;
            view.run_summary = run_summary;
            view.run_live = run_live;
            view.run_tooltip = run_tooltip;
            view.run_open = run_open;
            view.hidden_by_run = hidden_by_run;
            view.hidden_by_turn =
                turn_collapsed && turn.as_ref().is_some_and(|turn| turn.contains(index));
            view.visual_expand = visual_expand;
            view.queued = queued;
            view.top_gap = block_gap;
            view.sync_visual(cx);
            view
        });
        self.row_views[index] = Some(view.clone());
        self.update_row_find(index, cx);
        view
    }
    fn sync_runs(&mut self, cx: &mut Context<Self>) {
        let work_folds = turns::work_folds(&self.model);
        let runs = tool_runs::group(&self.model, &work_folds);
        for (index, view) in self.row_views.iter().enumerate() {
            let Some(view) = view else { continue };
            let run = runs.iter().find(|run| run.range.contains(&index));
            let tooltip = run
                .map(|run| tool_runs::tooltip(&self.model.rows[run.range.clone()]))
                .unwrap_or_default();
            let summary = run
                .filter(|run| run.range.start == index)
                .map(|run| run.label.clone());
            let run_live = run.is_some_and(|run| {
                self.model
                    .live_turn_start()
                    .is_some_and(|live| run.range.start >= live)
            });
            let open = run.is_none_or(|run| {
                self.open_runs
                    .contains(&self.model.rows[run.range.start].id)
            });
            let hidden = run.is_some_and(|run| run.range.start != index && !open);
            let work = work_folds.iter().find(|fold| fold.range.contains(&index));
            let work_summary = work
                .filter(|fold| fold.range.start == index)
                .map(|fold| fold.label.clone());
            let work_failed = work.map_or(0, |fold| fold.failed);
            let work_open = work.is_none_or(|fold| {
                self.open_work
                    .contains(&self.model.rows[fold.range.start].id)
            });
            let hidden_by_work = work.is_some_and(|fold| fold.range.start != index && !work_open);
            let in_work = work.is_some();
            let work_first = work.is_some_and(|fold| fold.range.start == index);
            let work_last = work.is_some_and(|fold| fold.range.end == index + 1);
            view.update(cx, |view, cx| {
                if view.work_summary != work_summary
                    || view.work_failed != work_failed
                    || view.work_open != work_open
                    || view.in_work != in_work
                    || view.work_first != work_first
                    || view.work_last != work_last
                    || view.hidden_by_work != hidden_by_work
                    || view.run_summary != summary
                    || view.run_live != run_live
                    || view.run_tooltip != tooltip
                    || view.run_open != open
                    || view.hidden_by_run != hidden
                {
                    view.work_summary = work_summary;
                    view.work_failed = work_failed;
                    view.work_open = work_open;
                    view.in_work = in_work;
                    view.work_first = work_first;
                    view.work_last = work_last;
                    view.hidden_by_work = hidden_by_work;
                    view.run_summary = summary;
                    view.run_live = run_live;
                    view.run_tooltip = tooltip;
                    view.run_open = open;
                    view.hidden_by_run = hidden;
                    view.scroller
                        .update(cx, |state, cx| state.remeasure_items(index..index + 1, cx));
                    cx.notify();
                }
            });
        }
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
            .px(px(12.))
            .py(px(10.))
            .border_1()
            .border_color(rgb(self.appearance.palette().warn))
            .bg(rgb(self.appearance.palette().block))
            .rounded(px(8.))
            .when(position > 0, |panel| {
                panel.child("Earlier worker requests · read-only")
            })
            .when(pending.len() > 1, |panel| {
                panel.child(format!("{} of {}", position + 1, pending.len()))
            })
            .child(
                div()
                    .debug_selector(|| "pending-request-card".into())
                    .child(card),
            )
            .into_any_element()
    }
    #[cfg(test)]
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
        for (index, view) in self.row_views.iter().enumerate() {
            if let Some(view) = view {
                view.update(cx, |row, cx| {
                    if row.hidden_by_turn != hidden[index] {
                        row.hidden_by_turn = hidden[index];
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
        self.open_runs.clear();
        self.open_work.clear();
        self.open_subs.clear();
        self.completed_seen = 0;
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
        if let Entry::ImportedOffer(offer) = &record.entry {
            self.imported_offer = offer.clone();
        }
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
                if let gitterm_core::chat::state::AgentEvent::Harness(HarnessEvent::Ready {
                    permission_mode: Some(mode),
                    ..
                }) = &event
                {
                    self.fixture_composer.acknowledged.mode = Some(mode.clone());
                }
                self.fixture_composer.conversation = vec![event];
            }
            self.composer_state.project(
                &self.fixture_composer,
                match &effects {
                    gitterm_core::chat::state::Effect::Event(effects) => Some(effects),
                    _ => None,
                },
            );
            self.schedule_prestart(cx);
        }
        let following = self.scroller.read(cx).is_following_tail();
        let count = self.model.rows.len();
        let changed = self.model.apply(record);
        self.sync_activity_tick(cx);
        self.refresh_rows(count, following, changed, cx);
    }
    fn queued_tag(&self, row: &Row) -> Option<crate::composer::QueuedTag> {
        let id = row.prompt_id.as_deref()?;
        self.composer_state.queued_tag(
            id,
            self.read_only_reason.is_some() || self.callback.is_none(),
        )
    }
    /// The queue lives in core state; each live bubble follows its message.
    pub(crate) fn sync_queued(&mut self, cx: &mut Context<Self>) {
        for (index, view) in self.row_views.iter().enumerate() {
            let Some(view) = view else { continue };
            let tag = self.queued_tag(&self.model.rows[index]);
            if view.read(cx).queued != tag {
                view.update(cx, |view, cx| {
                    view.queued = tag;
                    cx.notify();
                });
                self.scroller.update(cx, |state, cx| {
                    state.remeasure_items(index..index + 1, cx);
                });
            }
        }
    }
    fn refresh_rows(
        &mut self,
        count: usize,
        following: bool,
        changed: Vec<usize>,
        cx: &mut Context<Self>,
    ) {
        self.row_views.resize(self.model.rows.len(), None);
        // Iced opens a live thinking block (a replayed one starts closed)...
        for row in &self.model.rows[count..] {
            if row.kind == RowKind::Thinking {
                self.folded.insert(row.id, true);
            }
        }
        // ...and closes a tool card when its result arrives, even one opened by hand.
        for id in std::mem::take(&mut self.model.completions) {
            self.folded.insert(id, false);
            if let Some(view) = self
                .model
                .rows
                .iter()
                .position(|row| row.id == id)
                .and_then(|index| self.row_views[index].as_ref())
            {
                view.update(cx, |view, _| view.open = false);
            }
        }
        // Iced removes a finished turn's live runs; its folds start fresh.
        for turn in &self.model.completed_turns[self.completed_seen..] {
            for row in &self.model.rows[turn.rows.clone()] {
                self.open_runs.remove(&row.id);
            }
        }
        self.completed_seen = self.model.completed_turns.len();
        // Iced's `openFoldsAround` for a subagent that resumed or asks.
        for id in std::mem::take(&mut self.model.reopened) {
            if let Some(index) = self.model.rows.iter().position(|row| row.id == id) {
                self.open_folds_around(index);
            }
        }
        if following && !self.find_open {
            self.trim_history(cx);
        }
        self.restore_history_after_fold(cx);
        self.sync_block_gaps(cx);
        if !changed.is_empty() && !following {
            self.new_messages = true;
        }
        let prompt = self.model.rows[count..]
            .iter()
            .any(|row| row.kind == RowKind::User);
        let output = self.model.rows.len() > count || !changed.is_empty();
        self.scroller.update(cx, |state, cx| {
            state.append(self.model.rows.len() - count, cx);
            for &index in &changed {
                state.remeasure_items(index..index + 1, cx);
            }
            if output {
                state.follow_output(cx);
            }
            if prompt {
                state.follow_prompt(cx);
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
        self.sync_runs(cx);
        self.sync_queued(cx);
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
/// Iced's prompt placeholders: a writable chat, then `readOnlyReason` set.
const COMPOSER_PLACEHOLDER: &str = "Message… (Enter to send, Shift+Enter for newline)";
const READ_ONLY_PLACEHOLDER: &str = "Read-only chat";
impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frame += 1;
        let palette = self.appearance.palette();
        // `set_read_only` has no window; the placeholder follows it here.
        let placeholder = if self.read_only_reason.is_some() {
            READ_ONLY_PLACEHOLDER
        } else {
            COMPOSER_PLACEHOLDER
        };
        if self.composer.read(cx).presentation().placeholder().as_ref() != placeholder {
            self.composer.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx)
            });
        }
        let send_blocked = self.send_blocked(window, cx);
        let copy_owner = cx.entity().downgrade();
        let composer_focused = self.composer.read(cx).focus_handle(cx).is_focused(window);
        let mut context = String::from("NativeChat");
        if self.review.open {
            context.push_str(" IcedReview");
        }
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
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape"
                    && let Some(visual) = this.expanded_visual.clone()
                {
                    visual.update(cx, |visual, cx| visual.collapse(cx));
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                if this.review.open && event.keystroke.key == "escape" {
                    this.close_review(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|this, _: &SubmitReview, _, cx| this.submit_review(cx)))
            .on_action(cx.listener(|this, _: &CycleMode, window, cx| this.cycle_mode(window, cx)))
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
                // The native Input binding owns Cmd+Enter before the ancestor
                // host keymap. Route its secondary Enter through the same outcome.
                if this.review.open && action.secondary {
                    this.submit_review(cx);
                    window.prevent_default();
                    cx.stop_propagation();
                } else if action.shift || action.secondary || !this.slash_key("enter", window, cx) {
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
                if this.review.open {
                    this.close_review(window, cx);
                } else if this.composer_state.picker.take().is_some() {
                    cx.notify();
                } else if !this.slash_key("escape", window, cx) {
                    cx.propagate();
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                if this.review.open {
                    this.close_review(window, cx);
                }
                this.composer_state.picker = None;
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
            .capture_action(cx.listener(|this, _: &InputCopy, window, cx| {
                if this.copy_focused_text(window, cx) {
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|this, _: &ScrollLineUp, w, cx| {
                this.scroll_key(scroll::ScrollKey::LineUp, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollLineDown, w, cx| {
                this.scroll_key(scroll::ScrollKey::LineDown, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollPageUp, w, cx| {
                this.scroll_key(scroll::ScrollKey::PageUp, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollPageDown, w, cx| {
                this.scroll_key(scroll::ScrollKey::PageDown, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollTop, w, cx| {
                this.scroll_key(scroll::ScrollKey::Top, w, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollEnd, w, cx| {
                this.scroll_key(scroll::ScrollKey::End, w, cx)
            }))
            .on_action(cx.listener(Self::open_find))
            .on_action(cx.listener(|this, action: &CloseFind, window, cx| {
                if this.review.open {
                    this.close_review(window, cx);
                } else if this.find_open {
                    this.close_find(action, window, cx);
                } else if this.composer_state.picker.take().is_some() {
                    cx.notify();
                } else if this.model.pending_rows().is_empty() {
                    // Iced forwards Escape to the host unless a page layer
                    // (find, a menu, the review popover, the request panel)
                    // is open.
                    cx.propagate();
                }
            }))
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
                    .debug_selector(|| "chat-transcript".into())
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .bg(rgb(palette.alt))
                    .rounded(px(8.))
                    .child(self.messages(cx))
                    .context_menu(move |menu, window, cx| {
                        Self::selection_menu(copy_owner.clone(), menu, window, cx)
                    })
                    // Iced's `#jump`: a small button over the transcript's
                    // bottom-right corner; the transcript keeps its height.
                    .when(!self.scroller.read(cx).is_following_tail(), |view| {
                        view.child(
                            Button::new("new-messages")
                                .absolute()
                                .right(px(18.))
                                .bottom(px(12.))
                                .h_auto()
                                .px(px(10.))
                                .py(px(4.))
                                .rounded(px(12.))
                                .line_height(gpui::relative(1.2))
                                // Button's own label is a fixed `text_sm`; Iced's is 12/14.
                                .accessibility_label("↓ Latest")
                                .child(
                                    div()
                                        .text_size(px(self.appearance.size(12.)))
                                        .child("↓ Latest"),
                                )
                                .debug_selector(|| "chat-latest".into())
                                .on_click(cx.listener(|this, _, _, cx| this.jump(cx))),
                        )
                    }),
            )
            .child(
                div()
                    .when_some(self.column_width(), |footer, width| footer.max_w(px(width)))
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
                    .child(self.render_imported_offer(cx))
                    // Iced shows no activity until a turn runs, and a
                    // read-only chat never runs one: no `Ready` there.
                    .children(
                        (self.read_only_reason.is_none() || self.composer_state.status != "Ready")
                            .then(|| self.composer_state.render_activity(self.appearance)),
                    )
                    .child(self.pending_panel(window, cx))
                    // Iced's `#read-only-note`, above the composer.
                    .children(self.read_only_reason.clone().map(|reason| {
                        div()
                            .id("chat-read-only-note")
                            .debug_selector(|| "chat-read-only-note".into())
                            .text_size(px(self.appearance.size(12.)))
                            .text_color(rgb(palette.muted))
                            .child(reason)
                    }))
                    .children(
                        // A prompt's ambiguous delivery is marked on its bubble
                        // (Iced's `.delivery-uncertain`), so only its refusal
                        // reaches the footer; a control command has no bubble.
                        self.submission
                            .iter()
                            .filter(|receipt| matches!(receipt.state, ReceiptState::Rejected(_)))
                            .chain(self.control_receipt.iter().filter(|receipt| {
                                matches!(
                                    receipt.state,
                                    ReceiptState::Rejected(_) | ReceiptState::Uncertain(_)
                                )
                            }))
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
                        &self.review,
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
            .relative()
            .child(column)
            .children(self.expanded_visual.clone().map(|visual| {
                div()
                    .absolute()
                    .top(px(8.))
                    .left(px(8.))
                    .right(px(8.))
                    .bottom(px(8.))
                    .flex()
                    .flex_col()
                    .child(visual)
            }))
    }
}

struct RowView {
    action: RowAction,
    flashed: bool,
    busy: bool,
    expanded_edits: HashSet<usize>,
    /// Iced's `card.selected`: unset until first rendered, then every finding id.
    selected_findings: Option<HashSet<String>>,
    run_tooltip: String,
    work_summary: Option<String>,
    work_open: bool,
    /// Inside a finished turn's fold: opened, Iced indents it as `.tf-body`.
    in_work: bool,
    work_first: bool,
    work_last: bool,
    hidden_by_work: bool,
    work_toggle: TurnToggle,
    column_width: Option<f32>,
    run_summary: Option<String>,
    run_live: bool,
    run_open: bool,
    hidden_by_run: bool,
    run_toggle: TurnToggle,
    appearance: ChatAppearance,
    sent_images: Vec<Result<attachments::Thumbnail, String>>,
    preparing_sent_images: Option<Task<()>>,
    row: Row,
    text: Entity<TextViewState>,
    // The component keys its highlighter by Arc identity. Keep immutable theme
    // configuration with the row so redraws do not interrupt a selection drag.
    markdown_highlight_theme: Arc<gpui_component::highlighter::HighlightTheme>,
    output_text: Option<Entity<TextViewState>>,
    open: bool,
    /// Iced's nested `details.subagent`, closed until the human opens it.
    sub_open: bool,
    sub_text: Option<Entity<TextViewState>>,
    /// Iced's red `N failed` on this row's work fold summary.
    work_failed: usize,
    query: String,
    active: Option<usize>,
    reveal_pending: bool,
    highlight_signature: Option<u64>,
    copied: HashMap<CopyTarget, Task<()>>,
    index: usize,
    scroller: Entity<MessageScrollerState>,
    _subscription: Subscription,
    hidden_by_turn: bool,
    /// Iced's top margin left over once it collapses into the bottom
    /// margin of the block above (`block_top_gap`).
    top_gap: f32,
    visual: Option<Entity<visual::VisualReply>>,
    visual_expand: visual::VisualExpand,
    visual_subscription: Option<Subscription>,
    /// Iced's queued tag on this prompt's bubble (C04).
    queued: Option<crate::composer::QueuedTag>,
    /// Iced's `chatCwd()`: an edit card shows its path relative to it.
    cwd: Option<String>,
    /// The result `<pre>` shows the whole output (find with tools on).
    output_full: bool,
}
impl RowView {
    fn new(
        row: Row,
        index: usize,
        scroller: Entity<MessageScrollerState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = cx.new(|cx| {
            TextViewState::markdown(&copy::markdown_body(&row), cx)
                .selectable(true)
                .scrollable(row.tool.is_some())
                .motion(TextViewMotion::default().with_stream_fade(Duration::from_millis(280)))
        });
        let subscription = cx.observe(&text, |this, _, cx| {
            this.scroller.update(cx, |state, cx| {
                state.remeasure_items(this.index..this.index + 1, cx);
            });
            cx.notify();
        });
        let open = match (&row.delegation, &row.tool) {
            (Some(card), _) => card.delegation.dismissed_at.is_none(),
            // Iced opens a tool card, live or replayed, until its result.
            (None, Some(tool)) => tool.output.is_none(),
            // A delegation placeholder starts open, as Iced's newCard.
            (None, None) if row.kind == RowKind::Delegation && row.delivery.is_none() => true,
            (None, None) => !row.collapsible(),
        };
        let mut view = Self {
            action: Rc::new(|_, _| {}),
            flashed: false,
            busy: false,
            expanded_edits: HashSet::new(),
            selected_findings: None,
            run_tooltip: String::new(),
            work_summary: None,
            work_open: true,
            in_work: false,
            work_first: false,
            work_last: false,
            hidden_by_work: false,
            work_toggle: Rc::new(|_, _| {}),
            column_width: Some(840.),
            run_summary: None,
            run_live: false,
            run_open: true,
            hidden_by_run: false,
            run_toggle: Rc::new(|_, _| {}),
            appearance: ChatAppearance::default(),
            sent_images: vec![],
            preparing_sent_images: None,
            row,
            text,
            markdown_highlight_theme: copy::assistant_highlight_theme(),
            output_text: None,
            open,
            sub_open: false,
            sub_text: None,
            work_failed: 0,
            query: String::new(),
            active: None,
            reveal_pending: false,
            highlight_signature: None,
            copied: HashMap::new(),
            index,
            scroller,
            _subscription: subscription,
            hidden_by_turn: false,
            top_gap: 0.,
            visual: None,
            visual_expand: Rc::new(|_, _| {}),
            visual_subscription: None,
            queued: None,
            cwd: None,
            output_full: false,
        };
        view.update_output_text(cx);
        view.update_sub_text(cx);
        view.prepare_sent_images(cx);
        view
    }
    fn set_row(&mut self, row: Row, cx: &mut Context<Self>) {
        let images_changed = self.row.images != row.images;
        // Iced's showEdit replaces the diff view, collapsing Show all.
        if self.row.edits() != row.edits() {
            self.expanded_edits.clear();
        }
        self.text
            .update(cx, |text, cx| text.set_text(&copy::markdown_body(&row), cx));
        if self
            .row
            .delegation
            .as_ref()
            .is_some_and(|card| card.delegation.dismissed_at.is_none())
            && row
                .delegation
                .as_ref()
                .is_some_and(|card| card.delegation.dismissed_at.is_some())
        {
            self.open = false;
        }
        self.row = row;
        self.update_output_text(cx);
        self.update_sub_text(cx);
        if self.visual.is_none() {
            self.sync_visual(cx);
            if self.visual.is_some() {
                self.scroller.update(cx, |state, cx| {
                    state.remeasure_items(self.index..self.index + 1, cx);
                });
            }
        }
        if images_changed {
            self.prepare_sent_images(cx);
        }
        cx.notify();
    }
    /// A completed html_render result gets its visual reply once. Called
    /// while the list lays the row out, so the caller remeasures.
    fn sync_visual(&mut self, cx: &mut Context<Self>) {
        if self.visual.is_some() || self.row.kind != RowKind::Visual {
            return;
        }
        let Some(request) = self.row.tool.as_ref().and_then(visual::visual_request) else {
            return;
        };
        let (id, light) = (self.row.id, self.appearance.light);
        let expand = self.visual_expand.clone();
        let reply = cx.new(|cx| visual::VisualReply::new(id, request, light, expand, cx));
        // Expanding moves the reply over the chat; the row gives up its space.
        self.visual_subscription = Some(cx.observe(&reply, |this, _, cx| {
            this.scroller.update(cx, |state, cx| {
                state.remeasure_items(this.index..this.index + 1, cx);
            });
            cx.notify();
        }));
        self.visual = Some(reply);
    }
    /// Iced's `.queued-tag`: the state line and, while queued, the `×`.
    fn render_queued_tag(&self) -> Option<impl IntoElement> {
        let tag = self.queued.as_ref()?;
        let palette = self.appearance.palette();
        let id = tag.id.clone();
        let action = self.action.clone();
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .whitespace_normal()
                .text_size(px(self.appearance.size(11.)))
                .text_color(rgb(palette.muted))
                .child(tag.label.clone())
                .children(tag.withdraw.map(|enabled| {
                    let selector = format!("withdraw-{id}");
                    Button::new(("withdraw", self.row.id))
                        .debug_selector(move || selector.clone())
                        .xsmall()
                        .label("×")
                        .tooltip(tag.tooltip.clone())
                        .accessibility_label("Withdraw this message")
                        .disabled(!enabled)
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            action(
                                crate::interactions::TranscriptAction::Withdraw {
                                    message_id: id.clone(),
                                },
                                cx,
                            );
                        })
                })),
        )
    }
    fn toggle(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.open = !self.open;
        let opening = self.open;
        self.scroller.update(cx, |state, cx| {
            state.hold_for_toggle(opening, false, cx);
            state.remeasure_items(self.index..self.index + 1, cx);
        });
        cx.notify();
    }
}
impl Render for RowView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(visual) = &self.visual
            && visual.read(cx).light != self.appearance.light
        {
            let light = self.appearance.light;
            visual.update(cx, |visual, cx| visual.set_light(light, cx));
        }
        // Iced's `.html-render`: its own block, the reply alone.
        let visual = self
            .visual
            .clone()
            .filter(|visual| !visual.read(cx).expanded);
        if self.hidden_by_turn || self.row.hidden() {
            return div().h(px(0.)).mb(px(0.)).into_any_element();
        }
        if self.row.kind == RowKind::Visual
            || self.hidden_by_work
            || (self.hidden_by_run && self.work_summary.is_none())
        {
            return div()
                .w_full()
                .when_some(self.column_width, |row, width| row.max_w(px(width)))
                .mx_auto()
                .children(visual)
                .into_any_element();
        }
        self.highlight(cx);
        let palette = self.appearance.palette();
        let color = match self.row.kind {
            RowKind::User => palette.user,
            RowKind::Thinking | RowKind::Status => palette.muted,
            RowKind::Warning => palette.warn,
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
            RowKind::Delegation if self.row.delivery.is_none() => {
                self.row.delegation.as_ref().map_or(palette.result, |card| {
                    transcript::card_border(card, palette)
                })
            }
            RowKind::Worker | RowKind::Delegation => palette.result,
            _ => palette.fg,
        };
        let content = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .when(self.row.kind == RowKind::User, |content| {
                content.px(px(12.)).py(px(8.))
            })
            .when(self.row.kind == RowKind::Thinking, |content| {
                content
                    .mt(px(4.))
                    .pt(px(4.))
                    .pl(px(14.))
                    .border_l_2()
                    .border_color(rgb(palette.border))
            })
            .children((!self.row.images.is_empty()).then(|| self.render_sent_images()))
            .when(self.row.delivery.is_some(), |content| {
                content.pl(px(22.)).pr(px(10.)).pb(px(8.))
            })
            .child(self.copyable_text(cx).when(
                self.row.kind == RowKind::Assistant && self.row.scope.is_empty(),
                |text| {
                    text.style(copy::assistant_style(
                        self.appearance,
                        window.rem_size(),
                        self.markdown_highlight_theme.clone(),
                    ))
                    .blockquote_style(copy::assistant_quote_style(self.appearance))
                    .list_style(copy::assistant_list_style(self.appearance))
                },
            ))
            // Iced's `.user-note` for a relayed prompt's images.
            .when(self.row.relayed_images > 0, |content| {
                content.child(
                    div()
                        .text_size(px(self.appearance.size(11.)))
                        .text_color(rgb(palette.muted))
                        .child(if self.row.relayed_images == 1 {
                            "1 image attached on the sending device".to_string()
                        } else {
                            format!(
                                "{} images attached on the sending device",
                                self.row.relayed_images
                            )
                        }),
                )
            })
            .children(self.render_queued_tag());
        let header = div()
            .text_size(px(self.appearance.size(13.)))
            .text_color(rgb(color))
            .child(self.row.title.clone());
        let run_summary = self.run_summary.clone().map(|label| {
            let toggle = self.run_toggle.clone();
            let id = self.row.id;
            Button::new(("tool-run", id))
                .ghost()
                .h_auto()
                .w_full()
                .px_0()
                .mt(px(4.))
                .mb(px(4.))
                .justify_start()
                .debug_selector(move || format!("tool-run-{id}"))
                .flex()
                .items_baseline()
                .gap(px(6.))
                .py(px(2.))
                .cursor_pointer()
                .text_size(px(self.appearance.size(12.5)))
                .text_color(rgb(palette.muted))
                .tooltip(self.run_tooltip.clone())
                // The button centres its content; Iced's summary starts at the left.
                .child(
                    div()
                        .debug_selector(move || format!("tool-run-label-{id}"))
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_baseline()
                        .gap(px(6.))
                        .hover(|style| style.text_color(rgb(palette.fg)))
                        .child(
                            div()
                                .min_w_0()
                                .when(self.run_live, |label| label.text_ellipsis())
                                .child(label),
                        )
                        .child(
                            svg()
                                .data(b"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 12 12'><path d='M2 2 L6 6 L2 10' fill='none' stroke='currentColor' stroke-width='1.5' stroke-linecap='round' stroke-linejoin='round'/></svg>")
                                .with_transformation(Transformation::rotate(radians(if self.run_open {
                                    std::f32::consts::FRAC_PI_2
                                } else {
                                    0.
                                })))
                                .w(px(12.))
                                .h(px(12.)),
                        ),
                )
                .on_click(move |_, _, cx| toggle(id, cx))
                .into_any_element()
        });
        let show_work = self.work_summary.is_none() || self.work_open;
        let show_card = show_work && (self.run_summary.is_none() || self.run_open);
        let work_summary = self.work_summary.clone().map(|label| {
            let toggle = self.work_toggle.clone();
            let id = self.row.id;
            Button::new(("work-fold", id))
                .ghost()
                .h_auto()
                .w_full()
                .px_0()
                .justify_start()
                .debug_selector(move || format!("work-fold-{id}"))
                .flex()
                .gap(px(6.))
                .py(px(2.))
                .cursor_pointer()
                .text_size(px(self.appearance.size(13.)))
                .text_color(rgb(palette.muted))
                .child(
                    div()
                        .debug_selector(move || format!("work-fold-label-{id}"))
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_baseline()
                        .gap(px(6.))
                        .hover(|style| style.text_color(rgb(palette.fg)))
                        .child(label)
                        // Iced's `.tr-failed`.
                        .when(self.work_failed > 0, |summary| {
                            summary.child(
                                div()
                                    .debug_selector(move || format!("work-fold-failed-{id}"))
                                    .text_color(rgb(palette.err))
                                    .child(format!("{} failed", self.work_failed)),
                            )
                        })
                        .child(
                            svg()
                                .data(b"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 12 12'><path d='M2 2 L6 6 L2 10' fill='none' stroke='currentColor' stroke-width='1.5' stroke-linecap='round' stroke-linejoin='round'/></svg>")
                                .with_transformation(Transformation::rotate(radians(if self.work_open {
                                    std::f32::consts::FRAC_PI_2
                                } else {
                                    0.
                                })))
                                .w(px(12.))
                                .h(px(12.)),
                        ),
                )
                .on_click(move |_, _, cx| toggle(id, cx))
        });
        let row =
            div()
                .id(("row", self.row.id))
                .debug_selector({
                    let id = self.row.id;
                    move || format!("transcript-row-{id}")
                })
                .group("chat-row")
                .text_size(px(self.appearance.font_size))
                .text_color(rgb(copy::transcript_text_color(
                    self.appearance,
                    self.row.kind,
                )))
                .font_family(".SystemUIFont")
                .w_full()
                // Iced's bottom margin per block; the next block's smaller
                // top margin collapses into it.
                .mb(px(block_bottom_margin(&self.row)))
                .when(!self.row.scope.is_empty(), |view| {
                    view.ml(px(18.))
                        .border_l_2()
                        .border_color(rgb(palette.border))
                })
                .when(self.row.delivery.is_some(), |view| {
                    let id = self.row.id;
                    view.debug_selector(move || format!("delivery-card-{id}"))
                })
                // Iced's `details.dlg` and `.delivery`: margin 8px 0, the top
                // collapsing into the block above (`top_gap`), the bottom
                // being the card's own `block_bottom_margin`.
                .mt(px(self.top_gap))
                .when(self.row.kind == RowKind::User, |view| {
                    view.w_auto()
                        .max_w(gpui::relative(0.85))
                        .self_end()
                        .bg(rgb(palette.block))
                        .border_1()
                        .border_color(rgb(palette.border))
                        .rounded(px(10.))
                })
                // Iced's `.user-msg.queued` / `.unsent`: dashed and muted.
                .when_some(self.queued.as_ref(), |view, tag| {
                    view.border_dashed()
                        .text_color(rgb(palette.muted))
                        .when(tag.withdraw.is_none(), |view| view.opacity(0.7))
                })
                .when(
                    matches!(
                        self.row.kind,
                        RowKind::Permission | RowKind::Question | RowKind::Delegation
                    ),
                    |view| {
                        view.bg(rgb(palette.base))
                            .border_1()
                            .border_l_3()
                            .border_color(rgb(color))
                            .rounded(px(6.))
                    },
                )
                // Delivery and delegation cards: a neutral 1px border with a
                // 3px left edge in the card's state colour (`color`). Insets
                // are from the padding box, so the edge reaches back over the
                // border; drawn as a left border it follows the 6px corners.
                .when(self.row.kind == RowKind::Delegation, |row| {
                    let id = self.row.id;
                    row.relative().border_color(rgb(palette.border)).child(
                        div()
                            .debug_selector(move || format!("card-edge-{id}"))
                            .absolute()
                            .left(px(-3.))
                            .top(px(-1.))
                            .bottom(px(-1.))
                            .w(px(12.))
                            .rounded_tl(px(6.))
                            .rounded_bl(px(6.))
                            .border_l_3()
                            .border_color(rgb(color)),
                    )
                })
                // Iced's `.flash`: a 1px accent outline; a tool card draws its own.
                .when(self.flashed && self.row.kind != RowKind::Tool, |row| {
                    row.relative().child(flash_outline(
                        self.row.id,
                        [1., 1., 1., 3.],
                        6.,
                        palette.accent,
                    ))
                })
                // Iced's `title`: a reconnect notice's error.
                .when_some(self.row.tooltip.clone(), |row, tip| {
                    row.tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                    })
                })
                .when(self.in_work, |row| {
                    row.mb(px(if self.work_last { 8. } else { 0. }))
                })
                .children(run_summary)
                .when(show_card, |row| {
                    row.child(if self.row.delegation.is_some() {
                        self.render_delegation(cx).into_any_element()
                    } else if self.row.kind == RowKind::Tool {
                        self.render_tool(cx)
                    } else if self.row.collapsible() {
                        div()
                            .when(self.row.kind == RowKind::Tool, |card| {
                                card.bg(rgb(palette.base))
                                    .border_1()
                                    .border_l_3()
                                    .border_color(rgb(color))
                                    .rounded(px(6.))
                                    .px(px(10.))
                                    .py(px(5.))
                            })
                            .child(
                                Collapsible::new()
                                    .open(self.open)
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .child(
                                                Button::new("fold")
                                                    .ghost()
                                                    .h_auto()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .p_0()
                                                    .justify_start()
                                                    .debug_selector(move || "row-fold".into())
                                                    .flex()
                                                    .justify_start()
                                                    .cursor_pointer()
                                                    .text_size(px(self.appearance.size(13.)))
                                                    .text_color(rgb(color))
                                                    .when(
                                                        self.row.kind == RowKind::Thinking,
                                                        |row| row.italic(),
                                                    )
                                                    .when(self.row.delivery.is_none(), |header| {
                                                        let id = self.row.id;
                                                        header.child(
                                                            div()
                                                                .debug_selector(move || {
                                                                    format!("row-fold-label-{id}")
                                                                })
                                                                .flex_1()
                                                                .min_w_0()
                                                                .text_ellipsis()
                                                                .child(format!(
                                                                    "{} {}",
                                                                    if self.open {
                                                                        "▾"
                                                                    } else {
                                                                        "▸"
                                                                    },
                                                                    self.row.title
                                                                )),
                                                        )
                                                    })
                                                    .when_some(
                                                        self.row.delivery.as_ref(),
                                                        |header, delivery| {
                                                            header
                                                    .px(px(10.))
                                                    .py(px(6.))
                                                    .gap(px(8.))
                                                    .items_baseline()
                                                    .child(
                                                        div()
                                                            .text_color(rgb(palette.muted))
                                                            .text_size(px(self
                                                                .appearance
                                                                .size(10.)))
                                                            .child(if self.open {
                                                                "▾"
                                                            } else {
                                                                "▸"
                                                            }),
                                                    )
                                                    // Iced's `.delivery-kind`, then its id inline.
                                                    .child(
                                                        div()
                                                            .min_w_0()
                                                            .whitespace_normal()
                                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                                            .child(delivery.header.clone()),
                                                    )
                                                    .when(
                                                        !delivery.delegation_id.is_empty(),
                                                        |header| {
                                                            header.child(
                                                                div()
                                                                    .flex_shrink_0()
                                                                    .font_family("Menlo")
                                                                    .text_size(px(self
                                                                        .appearance
                                                                        .size(11.)))
                                                                    .text_color(rgb(palette.muted))
                                                                    .child(
                                                                        delivery
                                                                            .delegation_id
                                                                            .clone(),
                                                                    ),
                                                            )
                                                        },
                                                    )
                                                        },
                                                    )
                                                    .on_click(cx.listener(Self::toggle)),
                                            )
                                            .children(self.row.edits().first().map(|edit| {
                                                let action = self.action.clone();
                                                let path = std::path::PathBuf::from(&edit.path);
                                                Button::new("edit-review")
                                                    .ghost()
                                                    .small()
                                                    .label("Review")
                                                    .debug_selector({
                                                        let id = self.row.id;
                                                        move || format!("edit-review-{id}")
                                                    })
                                                    .tooltip(format!(
                                                        "Open {} in the review pane",
                                                        edit.path
                                                    ))
                                                    .on_click(move |_, _, cx| {
                                                        cx.stop_propagation();
                                                        action(
                                        crate::interactions::TranscriptAction::OpenFile {
                                            path: path.clone(),
                                            review: true,
                                        },
                                        cx,
                                    );
                                                    })
                                            })),
                                    )
                                    .content(content),
                            )
                            .into_any_element()
                    } else if self.row.kind == RowKind::Error {
                        // Iced's `.error-block`.
                        div()
                            .px(px(10.))
                            .py(px(8.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(rgb(palette.err))
                            .bg(gpui::rgba((palette.err << 8) | 0x14))
                            .child(content)
                            .into_any_element()
                    } else if let RowKind::Legacy(block) = self.row.kind {
                        self.render_legacy(block, content)
                    } else {
                        div()
                            .when(
                                !matches!(
                                    self.row.kind,
                                    RowKind::Assistant
                                        | RowKind::User
                                        | RowKind::Status
                                        | RowKind::Warning
                                ),
                                |row| row.child(header),
                            )
                            .child(content)
                            .into_any_element()
                    })
                })
                .into_any_element();
        let id = self.row.id;
        let work_summary = work_summary.map(|header| {
            div()
                .debug_selector(move || format!("work-fold-margin-{id}"))
                .mt(px(4.))
                .mb_0()
                .child(header)
        });
        // Iced's `.tf-body`: one bordered column, so each row's gap sits inside it.
        let row = if self.in_work {
            div()
                .debug_selector(move || format!("work-body-{id}"))
                .mt(px(if self.work_first { 4. } else { 0. }))
                .mb(px(if self.work_last { 2. } else { 0. }))
                .pl(px(12.))
                .border_l_2()
                .border_color(rgb(palette.border))
                .child(row)
                .into_any_element()
        } else {
            row.into_any_element()
        };
        div()
            .w_full()
            .when_some(self.column_width, |row, width| row.max_w(px(width)))
            .mx_auto()
            // A user row needs a flex cross-axis so its auto width can fit its
            // contents while `self_end` holds the bubble against the right edge.
            .when(self.row.kind == RowKind::User, |row| row.flex().flex_col())
            .children(work_summary)
            .when(show_work, |outer| outer.child(row))
            .children(visual)
            .into_any_element()
    }
}

/// Iced's `outline: 1px solid var(--accent)`: drawn over the card, so the
/// layout does not move. An absolute child is placed from the padding box,
/// so `border` is the card's border widths (top, right, bottom, left).
fn flash_outline(id: u64, border: [f32; 4], radius: f32, color: u32) -> gpui::Div {
    let [top, right, bottom, left] = border.map(|width| px(-(width + 1.)));
    div()
        .debug_selector(move || format!("flash-outline-{id}"))
        .absolute()
        .top(top)
        .right(right)
        .bottom(bottom)
        .left(left)
        .rounded(px(radius + 1.))
        .border_1()
        .border_color(rgb(color))
}

/// Iced's bottom margin per block; the next block's smaller top margin
/// collapses into it.
fn block_bottom_margin(row: &Row) -> f32 {
    if row.delivery.is_some() || row.delegation.is_some() {
        return 8.;
    }
    match row.kind {
        RowKind::User => 12.,
        RowKind::Thinking | RowKind::Legacy(_) => 8.,
        RowKind::Tool | RowKind::Error | RowKind::Status | RowKind::Warning => 6.,
        _ => 10.,
    }
}

impl ChatView {
    /// CSS top margins collapse into the preceding visible block's bottom
    /// margin: only the excess is left, all of it with nothing above.
    fn block_top_gap(&self, index: usize) -> f32 {
        let row = &self.model.rows[index];
        let top = if row.delivery.is_some() || row.delegation.is_some() {
            8.
        } else if row.kind == RowKind::Assistant && row.scope.is_empty() {
            6.
        } else {
            return 0.;
        };
        self.model.rows[..index]
            .iter()
            .rev()
            .find(|previous| !previous.hidden())
            .map_or(top, |previous| {
                (top - block_bottom_margin(previous)).max(0.)
            })
    }
    fn sync_block_gaps(&mut self, cx: &mut Context<Self>) {
        for index in 0..self.row_views.len() {
            let Some(view) = self.row_views[index].clone() else {
                continue;
            };
            let gap = self.block_top_gap(index);
            if view.read(cx).top_gap != gap {
                view.update(cx, |view, cx| {
                    view.top_gap = gap;
                    cx.notify();
                });
                self.scroller.update(cx, |state, cx| {
                    _ = state.remeasure_items(index..index + 1, cx)
                });
            }
        }
    }
}

/// The markdown a row's text view shows. A delegation card draws its
/// other blocks itself (Iced's `.dlg-body` classes); only its report is
/// markdown. `row.body` stays the whole card, for find and copy.
fn display_body(row: &Row) -> String {
    if let Some(tool) = &row.tool {
        // The input `<pre>`; an edit card shows its diff instead.
        let input = crate::model::tool_input_text(tool);
        return if input.is_empty() || !row.edits().is_empty() {
            String::new()
        } else {
            crate::model::code(&input, "json")
        };
    }
    row.delegation
        .as_ref()
        .map_or_else(|| row.body.clone(), |card| card.report())
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

#[cfg(test)]
mod fold_tests;
#[cfg(test)]
mod polish_tests;
#[cfg(test)]
mod row_tests;
#[cfg(test)]
mod tests;
