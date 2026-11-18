//! Plan limits in the Usage view and on the rail (TRU-211): the Claude Code
//! and Codex windows `/usage` shows. The reading, the rules and the words
//! live in `gitterm::plan_limits`; this holds the state, the background
//! work and the widgets.
//!
//! Claude readings arrive on `TurnCompleted.rate_limit` from any chat tab,
//! local only, and are taken in `update()` (a cheap in-memory merge).
//! The Codex reading is a tail read of the newest rollout, run off the UI
//! thread from `Tick` every [`CODEX_READ_EVERY`], and at once when the
//! Usage view opens or refreshes. Changes are written to
//! `plan-limits.json` off the UI thread, one write at a time.

use super::*;

use gitterm::harness::RateLimitReading;
use gitterm::plan_limits::{self, Level, PlanLimits, PlanLimitsFile, Provider};

/// How often the newest Codex rollout is read again.
const CODEX_READ_EVERY: Duration = Duration::from_secs(60);
/// The rail badge's text size.
const BADGE_FONT: f32 = 9.0;
/// The badge row's height when there is a badge.
const BADGE_HEIGHT: f32 = 12.0;

fn daily_usage(samples: &[plan_limits::UsageSample], now: i64) -> [Option<f64>; 7] {
    let Some(today) = chrono::DateTime::from_timestamp(now, 0)
        .map(|at| at.with_timezone(&chrono::Local).date_naive())
    else {
        return [None; 7];
    };
    let mut last_by_day: [Option<(i64, f64)>; 7] = [None; 7];
    let mut prior = None;
    for sample in samples {
        if sample.observed_at > now {
            continue;
        }
        let Some(date) = chrono::DateTime::from_timestamp(sample.observed_at, 0)
            .map(|at| at.with_timezone(&chrono::Local).date_naive())
        else {
            continue;
        };
        let age = (today - date).num_days();
        if age >= 7 {
            if prior.is_none_or(|(at, _)| sample.observed_at > at) {
                prior = Some((sample.observed_at, sample.used_percent));
            }
        } else if age >= 0 {
            let day = 6 - age as usize;
            if last_by_day[day].is_none_or(|(at, _)| sample.observed_at > at) {
                last_by_day[day] = Some((sample.observed_at, sample.used_percent));
            }
        }
    }

    let mut previous = prior.map(|(_, value)| value);
    std::array::from_fn(|day| {
        let (_, value) = last_by_day[day]?;
        let used = (value - previous.unwrap_or(0.0)).max(0.0);
        previous = Some(value);
        Some(used)
    })
}

#[derive(Debug, Clone)]
pub enum PlanLimitsEvent {
    /// A Codex read finished: the newest account-wide reading, if any.
    CodexRead(Option<Box<PlanLimits>>),
    PinChanged(Option<plan_limits::RoutingPin>),
    Saved(Result<(), String>),
}

#[derive(Debug, Default)]
pub struct PlanLimitsState {
    /// The last reading of each provider (`plan-limits.json`).
    file: PlanLimitsFile,
    codex_in_flight: bool,
    codex_read_at: Option<Instant>,
    /// A change not yet written.
    dirty: bool,
    save_in_flight: bool,
}

impl PlanLimitsState {
    pub(super) fn evaluation_for_launch(&self) -> plan_limits::PolicyEvaluation {
        plan_limits::evaluate_policy(&self.file, now_unix())
    }

    fn evaluate(&mut self, chats: &[String], now: i64) {
        if self.file.prune_history(now) {
            self.dirty = true;
        }
        let evaluation = plan_limits::evaluate_policy(&self.file, now);
        if self.file.pin != evaluation.pin {
            self.file.pin = evaluation.pin.clone();
            self.dirty = true;
        }
        if self.file.policy.as_ref() != Some(&evaluation.policy) {
            self.file.policy = Some(evaluation.policy);
            self.dirty = true;
        }
        self.queue_notices(chats, &evaluation.notices);
    }

    fn queue_notices(&mut self, chats: &[String], lines: &[String]) {
        let before = self.file.pending_notices.len();
        self.file
            .pending_notices
            .retain(|chat, _| chats.contains(chat));
        self.dirty |= before != self.file.pending_notices.len();
        if lines.is_empty() {
            return;
        }
        for chat in chats {
            let pending = self.file.pending_notices.entry(chat.clone()).or_default();
            if lines.iter().any(|line| line.starts_with("Routing:")) {
                let before = pending.len();
                pending.retain(|line| !line.starts_with("Routing:"));
                self.dirty |= before != pending.len();
            }
            for line in lines {
                if !pending.contains(line) {
                    pending.push(line.clone());
                    self.dirty = true;
                }
            }
        }
    }

    fn notice(&self, chat: &str, busy: bool) -> Option<String> {
        if busy {
            return None;
        }
        self.file.pending_notices.get(chat).map(|lines| {
            gitterm::delegations::from_gitterm(&format!("GitTerm plan limits: {}\nRelay this heads-up to the user and follow `plan_limits` routing for new work. An explicit user model choice wins; never restart running workers to change routing.", lines.join("\n")))
        })
    }

    fn finish_notice(&mut self, chat: &str, accepted: bool) {
        if accepted {
            self.dirty |= self.file.pending_notices.remove(chat).is_some();
        } else {
            eprintln!("[plan-limits] coordinator {chat} did not accept the notice; retaining it for retry");
        }
    }

    /// The persisted readings from the config dir (a small read).
    pub fn load(config_dir: &Path) -> Self {
        Self {
            file: PlanLimitsFile::load(&PlanLimitsFile::path_in(config_dir)),
            ..Self::default()
        }
    }

    fn finish_save(&mut self, result: Result<(), String>, path: &Path) -> bool {
        self.save_in_flight = false;
        if let Err(error) = result {
            eprintln!("[plan-limits] not saved to {}: {error}", path.display());
            self.dirty = true;
            return false;
        }
        true
    }
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

pub(super) fn local_limit_source(workspaces: &[Workspace], tab_id: usize) -> bool {
    workspaces.iter().any(|ws| {
        matches!(ws.location, WorkspaceLocation::Local { .. })
            && ws.tabs.iter().any(|tab| tab.id == tab_id)
    })
}

fn is_plan_limit_coordinator(tab: &TabState, children: &HashSet<String>) -> bool {
    let Some(session) = tab.agent_session() else {
        return false;
    };
    ![
        tab.chat_session_id.as_ref(),
        session.session_id.as_ref(),
        tab.task_session_id.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(|id| children.contains(id))
}

fn running_plan_limit_workers(
    workspaces: &[Workspace],
    provider: Provider,
    store: Option<&TaskStore>,
) -> usize {
    workspaces
        .iter()
        .filter(|ws| matches!(ws.location, WorkspaceLocation::Local { .. }))
        .flat_map(|ws| &ws.tabs)
        .filter(|tab| task_session_is_working(tab, store))
        .filter(|tab| {
            let worker_provider = tab
                .startup_command()
                .map(gitterm::workers::provider_for_command)
                .unwrap_or_else(|| {
                    if tab
                        .agent_session()
                        .is_some_and(|s| s.backend() == tab::AgentBackend::Claude)
                    {
                        gitterm::workers::WorkerProvider::Claude
                    } else {
                        gitterm::workers::WorkerProvider::Other
                    }
                });
            matches!(
                (provider, worker_provider),
                (Provider::Claude, gitterm::workers::WorkerProvider::Claude)
                    | (Provider::Codex, gitterm::workers::WorkerProvider::Codex)
            )
        })
        .count()
}

impl App {
    pub(super) fn plan_limit_running_workers(&self, provider: Provider) -> usize {
        running_plan_limit_workers(&self.workspaces, provider, self.task_store.as_ref())
    }

    pub(super) fn plan_limits_routing(&self) -> plan_limits::RoutingReport {
        plan_limits::evaluate_policy(&self.plan_limits.file, now_unix())
            .report
            .routing
    }

    pub(super) fn plan_limits_snapshot(&self) -> Result<serde_json::Value, String> {
        serde_json::to_value(
            plan_limits::evaluate_policy(&self.plan_limits.file, now_unix()).report,
        )
        .map_err(|error| format!("cannot encode plan_limits result: {error}"))
    }

    fn plan_limit_chats(&self) -> Vec<(usize, String, bool)> {
        if self.task_mcp.is_none() {
            return Vec::new();
        }
        let mut children: HashSet<String> = self
            .chat_child_links()
            .into_iter()
            .filter(|link| link.parent != link.child.session_id)
            .map(|link| link.child.session_id)
            .chain(
                self.chat_index
                    .iter()
                    .filter(|entry| entry.root_id.is_some())
                    .map(|entry| entry.id.clone()),
            )
            .collect();
        if let Some(store) = &self.task_store {
            for delegation in store.delegations() {
                if let gitterm::tasks::DelegationChild::TaskSession {
                    task_session_id: Some(id),
                    ..
                } = &delegation.child
                {
                    children.insert(id.clone());
                }
            }
            for task in store.tasks().iter().filter(|task| {
                task.created_by.as_ref().is_some_and(|creator| {
                    creator.kind == gitterm::tasks::TaskCreatorKind::Coordinator
                })
            }) {
                children.extend(
                    task.sessions
                        .iter()
                        .map(|session| session.task_session_id.clone()),
                );
            }
        }
        self.workspaces
            .iter()
            .filter(|ws| matches!(ws.location, WorkspaceLocation::Local { .. }))
            .flat_map(|ws| &ws.tabs)
            .filter_map(|tab| {
                let session = tab.agent_session()?;
                is_plan_limit_coordinator(tab, &children).then(|| {
                    (
                        tab.id,
                        tab.session_uid.clone(),
                        !matches!(session.state, tab::AgentSessionState::Idle),
                    )
                })
            })
            .collect()
    }

    fn evaluate_plan_limits(&mut self) {
        let chats = self
            .plan_limit_chats()
            .into_iter()
            .map(|(_, uid, _)| uid)
            .collect::<Vec<_>>();
        self.plan_limits.evaluate(&chats, now_unix());
    }

    /// As with PR news, busy chats wait until the turn ends. Submit through
    /// the normal path synchronously so a rejected send remains pending.
    pub(super) fn tell_plan_limit_coordinators(&mut self) -> Task<Event> {
        let mut tasks = Vec::new();
        for (id, uid, busy) in self.plan_limit_chats() {
            let Some(prompt) = self.plan_limits.notice(&uid, busy) else {
                continue;
            };
            tasks.push(self.update(Event::AgentSubmitPrompt(id, prompt.into())));
            let accepted = self
                .workspaces
                .iter()
                .flat_map(|ws| &ws.tabs)
                .find(|tab| tab.id == id)
                .and_then(|tab| tab.agent_session())
                .is_some_and(|session| matches!(session.state, tab::AgentSessionState::Streaming));
            self.plan_limits.finish_notice(&uid, accepted);
        }
        Task::batch(tasks)
    }

    /// A Claude chat's turn reported its plan limits: the latest wins.
    pub(super) fn observe_claude_rate_limit(&mut self, reading: &RateLimitReading) {
        let mut limits = plan_limits::claude_limits(&reading.info, reading.observed_at);
        limits.local = Some(true);
        if self.plan_limits.file.absorb(limits) {
            self.plan_limits.dirty = true;
            self.evaluate_plan_limits();
        }
    }

    /// Tick: a Codex read when one is due, and a save when anything
    /// changed.
    pub(super) fn poll_plan_limits(&mut self) -> Task<Event> {
        self.evaluate_plan_limits();
        let due = self
            .plan_limits
            .codex_read_at
            .is_none_or(|at| at.elapsed() >= CODEX_READ_EVERY);
        let read = if due {
            self.read_codex_limits()
        } else {
            Task::none()
        };
        Task::batch([
            read,
            self.tell_plan_limit_coordinators(),
            self.save_plan_limits(),
            self.start_queued_delegations(),
        ])
    }

    /// Read the newest Codex rollout's limits now, unless a read runs.
    pub(super) fn read_codex_limits(&mut self) -> Task<Event> {
        if self.plan_limits.codex_in_flight {
            return Task::none();
        }
        let Some(sources) = gitterm::usage::UsageSources::local() else {
            eprintln!("[plan-limits] cannot find the home directory; Codex limits not read");
            self.plan_limits.codex_read_at = Some(Instant::now());
            return Task::none();
        };
        self.plan_limits.codex_in_flight = true;
        self.plan_limits.codex_read_at = Some(Instant::now());
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    plan_limits::latest_codex_limits(&sources.codex_sessions)
                })
                .await
            },
            |result| match result {
                Ok(limits) => Event::PlanLimits(PlanLimitsEvent::CodexRead(limits.map(Box::new))),
                Err(error) => {
                    eprintln!("[plan-limits] Codex read task failed: {error}");
                    Event::PlanLimits(PlanLimitsEvent::CodexRead(None))
                }
            },
        )
    }

    /// Writes the readings off the UI thread, one write at a time.
    fn save_plan_limits(&mut self) -> Task<Event> {
        if !self.plan_limits.dirty || self.plan_limits.save_in_flight {
            return Task::none();
        }
        self.plan_limits.dirty = false;
        self.plan_limits.save_in_flight = true;
        let file = self.plan_limits.file.clone();
        let path = PlanLimitsFile::path_in(&config::global_config_dir());
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || file.save(&path))
                    .await
                    .unwrap_or_else(|error| Err(format!("plan limits save task failed: {error}")))
            },
            |result| Event::PlanLimits(PlanLimitsEvent::Saved(result)),
        )
    }

    pub(super) fn update_plan_limits(&mut self, event: PlanLimitsEvent) -> Task<Event> {
        match event {
            PlanLimitsEvent::CodexRead(limits) => {
                self.plan_limits.codex_in_flight = false;
                if let Some(limits) = limits {
                    if self.plan_limits.file.absorb(*limits) {
                        self.plan_limits.dirty = true;
                        self.evaluate_plan_limits();
                        return self.save_plan_limits();
                    }
                }
            }
            PlanLimitsEvent::Saved(result) => {
                let path = PlanLimitsFile::path_in(&config::global_config_dir());
                if !self.plan_limits.finish_save(result, &path) {
                    return Task::none();
                }
                return self.save_plan_limits();
            }
            PlanLimitsEvent::PinChanged(pin) => {
                if self.plan_limits.file.pin == pin {
                    return Task::none();
                }
                self.plan_limits.file.pin = pin.clone();
                self.plan_limits.dirty = true;
                self.evaluate_plan_limits();
                let chats = self
                    .plan_limit_chats()
                    .into_iter()
                    .map(|(_, uid, _)| uid)
                    .collect::<Vec<_>>();
                let routing = self.plan_limits_routing();
                self.plan_limits.queue_notices(
                    &chats,
                    &[plan_limits::pin_change_notice(
                        pin.as_ref(),
                        now_unix(),
                        &routing,
                    )],
                );
                return Task::batch([self.tell_plan_limit_coordinators(), self.save_plan_limits()]);
            }
        }
        Task::none()
    }

    /// A level's colour; `normal` below the warning threshold.
    fn plan_level_color(&self, level: Level, normal: iced::Color) -> iced::Color {
        match level {
            Level::Normal => normal,
            Level::Warning => self.theme.warning(),
            Level::Danger => self.theme.danger(),
        }
    }

    /// The rail badge under the Usage glyph: always the same widgets, of
    /// no height while there is nothing to show, so the rail keeps its
    /// tree when a reading arrives.
    pub(super) fn view_plan_limits_badge(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let badge = plan_limits::badge(
            self.plan_limits.file.readings(),
            &self.plan_limits.file.history,
            now_unix(),
        );
        let (label, color, height) = match &badge {
            Some(badge) => (
                badge.text.clone(),
                self.plan_level_color(badge.level, self.theme.subtext0()),
                BADGE_HEIGHT,
            ),
            None => (String::new(), self.theme.subtext0(), 0.0),
        };
        container(
            text(label)
                .size(BADGE_FONT)
                .color(color)
                .wrapping(iced::widget::text::Wrapping::None),
        )
        .width(Length::Fixed(SPINE_WIDTH - 1.0))
        .height(Length::Fixed(height))
        .center_x(Length::Fixed(SPINE_WIDTH - 1.0))
        .into()
    }

    /// The Usage glyph's tooltip: the shortcut, then each window's % and
    /// reset, alerts, and "as of" for an old reading.
    pub(super) fn plan_limits_tooltip(&self) -> String {
        let now = now_unix();
        let mut lines = vec!["Usage  ⌘⇧U".to_string()];
        for reading in self.plan_limits.file.readings() {
            lines.extend(plan_limits::tooltip_lines(
                reading,
                &self.plan_limits.file.history,
                now,
                &chrono::Local,
            ));
        }
        lines.join("\n")
    }

    /// "Plan limits" at the top of the Usage view: one block per provider.
    pub(super) fn view_plan_limits_section(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();
        let font_tiny = font_small - 1.0;
        let card_bg = theme.bg_surface();
        let card_border = theme.surface0();
        let now = now_unix();
        let mut blocks = Row::new().spacing(12);
        for provider in [Provider::Claude, Provider::Codex] {
            let block = self.view_plan_limits_block(provider, now, font, font_small, font_tiny);
            blocks = blocks.push(
                container(block)
                    .padding(14)
                    .width(Length::FillPortion(1))
                    .style(move |_| container::Style {
                        background: Some(card_bg.into()),
                        border: iced::Border {
                            color: card_border,
                            width: 1.0,
                            radius: 6.0.into(),
                        },
                        ..Default::default()
                    }),
            );
        }
        Column::new()
            .spacing(8)
            .push(text("Plan limits").size(font).color(theme.text_primary()))
            .push(blocks)
            .into()
    }

    fn view_plan_limits_block(
        &self,
        provider: Provider,
        now: i64,
        font: f32,
        font_small: f32,
        font_tiny: f32,
    ) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let muted = theme.text_muted();
        let secondary = theme.text_secondary();
        let no_wrap = iced::widget::text::Wrapping::None;
        let reading = self.plan_limits.file.get(provider);
        let mut header = Row::new()
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .push(
                text(provider.label())
                    .size(font)
                    .color(theme.text_primary()),
            )
            .push(iced::widget::Space::new().width(Length::Fill));
        let source = match provider {
            Provider::Claude => "From GitTerm Claude chats; updates when a turn ends.",
            Provider::Codex => "From the newest Codex rollout; read every minute.",
        };
        let report = plan_limits::evaluate_policy(&self.plan_limits.file, now);
        let provider_report = match provider {
            Provider::Claude => &report.report.claude,
            Provider::Codex => &report.report.codex,
        };
        let pin_row = |label: String, pin: Option<plan_limits::RoutingPin>| {
            button(text(label).size(font_tiny).color(secondary))
                .padding([3, 7])
                .on_press(Event::PlanLimits(PlanLimitsEvent::PinChanged(pin)))
        };
        let mut pin_controls = Row::new().spacing(5).align_y(iced::Alignment::Center).push(
            text(format!("Send work to {} until", provider.label()))
                .size(font_tiny)
                .color(muted),
        );
        for (label, duration) in [("1h", 3600), ("4h", 4 * 3600), ("24h", 24 * 3600)] {
            pin_controls = pin_controls.push(pin_row(
                label.into(),
                Some(plan_limits::RoutingPin {
                    provider,
                    until: now + duration,
                }),
            ));
        }
        if let Some(pin) = plan_limits::hot_reset_pin(provider, provider_report, now) {
            pin_controls = pin_controls.push(pin_row(
                format!("Send work to {} until reset", pin.provider.label()),
                Some(pin),
            ));
        }
        if self
            .plan_limits
            .file
            .pin
            .as_ref()
            .is_some_and(|pin| pin.provider == provider)
        {
            pin_controls = pin_controls.push(pin_row("clear".into(), None));
        }
        let Some(reading) = reading else {
            let empty = match provider {
                Provider::Claude => {
                    "No reading yet: the next turn of a GitTerm Claude chat brings one."
                }
                Provider::Codex => {
                    "No reading yet: no Codex rollout with rate limits under ~/.codex/sessions."
                }
            };
            return Column::new()
                .spacing(8)
                .push(header)
                .push(text(empty).size(font_small).color(secondary))
                .push(pin_controls)
                .push(text(source).size(font_tiny).color(muted))
                .into();
        };
        let stale = reading.is_stale(now);
        let freshness = plan_limits::freshness(reading, now, &chrono::Local);
        header = header.push(
            text(if stale {
                format!("{freshness}; may be higher now")
            } else {
                freshness
            })
            .size(font_tiny)
            .color(if stale { theme.warning() } else { muted }),
        );
        let mut col = Column::new().spacing(8).push(header);
        let danger = theme.danger();
        for alert in reading.active_alerts(now) {
            col = col.push(
                container(text(alert.text()).size(font_small).color(danger))
                    .padding([3, 10])
                    .style(move |_| container::Style {
                        border: iced::Border {
                            color: danger,
                            width: 1.0,
                            radius: 10.0.into(),
                        },
                        ..Default::default()
                    }),
            );
        }
        let track = theme.surface0();
        for window in &reading.windows {
            let figure =
                plan_limits::window_figure(reading, window, &self.plan_limits.file.history, now);
            let percent = figure.map(|(p, _)| p);
            let (figure, fill_color) = match figure {
                Some((p, mark)) => (
                    format!("{mark}{}%", p.round()),
                    self.plan_level_color(plan_limits::level(p), theme.accent()),
                ),
                None if window.has_reset(now) => ("–".to_string(), track),
                None => ("unknown".to_string(), track),
            };
            let filled = (percent.unwrap_or(0.0).clamp(0.0, 100.0) * 10.0).round() as u16;
            let bar = container(
                row![
                    container(iced::widget::Space::new())
                        .width(Length::FillPortion(filled))
                        .height(Length::Fill)
                        .style(move |_| container::Style {
                            background: Some(fill_color.into()),
                            border: iced::Border {
                                radius: 3.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }),
                    iced::widget::Space::new().width(Length::FillPortion(1000 - filled)),
                ]
                .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fixed(8.0))
            .style(move |_| container::Style {
                background: Some(track.into()),
                border: iced::Border {
                    radius: 3.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });
            let reset = match window.resets_at {
                Some(at) if window.has_reset(now) => {
                    format!(
                        "{}; no reading since",
                        plan_limits::format_reset(at, now, &chrono::Local)
                    )
                }
                Some(at) => plan_limits::format_reset(at, now, &chrono::Local),
                None => String::new(),
            };
            col = col.push(
                row![
                    text(window.label.as_str())
                        .size(font_small)
                        .color(secondary)
                        .wrapping(no_wrap)
                        .width(Length::Fixed(56.0)),
                    bar,
                    text(figure)
                        .size(font_small)
                        .color(if percent.is_some() { fill_color } else { muted })
                        .wrapping(no_wrap)
                        .width(Length::Fixed(60.0))
                        .align_x(iced::alignment::Horizontal::Right),
                    text(reset)
                        .size(font_tiny)
                        .color(muted)
                        .wrapping(no_wrap)
                        .width(Length::Fixed(190.0)),
                ]
                .spacing(10)
                .align_y(iced::Alignment::Center),
            );
            let history = self.plan_limits.file.history.iter().find(|history| {
                history.provider == provider
                    && history.label == window.label
                    && history.resets_at == window.resets_at
            });
            if let Some(history) = history {
                let bars = Row::new()
                    .spacing(3)
                    .align_y(iced::Alignment::End)
                    .push(text("by day").size(font_tiny).color(muted));
                let daily = daily_usage(&history.samples, now);
                let bars = daily
                    .into_iter()
                    .enumerate()
                    .fold(bars, |row, (day, value)| {
                        let height = value
                            .map(|value| (value.clamp(0.0, 20.0) * 1.2).max(2.0) as f32)
                            .unwrap_or(2.0);
                        let color = if value.is_some() {
                            theme.accent()
                        } else {
                            track
                        };
                        let bar = container(iced::widget::Space::new())
                            .width(Length::Fill)
                            .height(Length::Fixed(height))
                            .style(move |_| container::Style {
                                background: Some(color.into()),
                                border: iced::Border {
                                    radius: 2.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            });
                        let bar = if let Some(value) = value {
                            let weekday = chrono::DateTime::from_timestamp(now, 0)
                                .and_then(|at| {
                                    at.with_timezone(&chrono::Local)
                                        .date_naive()
                                        .checked_sub_days(chrono::Days::new((6 - day) as u64))
                                })
                                .map(|date| date.format("%a").to_string())
                                .unwrap_or_else(|| "Day".into());
                            iced::widget::tooltip(
                                bar,
                                text(format!("{weekday}: +{value:.0}%"))
                                    .size(font_tiny)
                                    .color(theme.text_primary()),
                                iced::widget::tooltip::Position::Top,
                            )
                        } else {
                            iced::widget::tooltip(
                                bar,
                                text("").size(font_tiny),
                                iced::widget::tooltip::Position::Top,
                            )
                        };
                        row.push(container(bar).width(Length::FillPortion(1)))
                    });
                col = col.push(container(bars).height(Length::Fixed(34.0)));
            }
        }
        let almost_out = provider_report.windows.iter().any(|window| {
            window.label == "7d" && window.state == Some(plan_limits::WeeklyState::AlmostOut)
        });
        if self
            .plan_limits
            .file
            .pin
            .as_ref()
            .is_some_and(|pin| pin.provider == provider && pin.until > now)
            && almost_out
        {
            col = col.push(
                text("Pin overridden: this plan is almost out; new work is held until reset.")
                    .size(font_small)
                    .color(theme.warning()),
            );
        }
        col = col.push(pin_controls);
        let mut facts: Vec<String> = Vec::new();
        if let Some(plan) = &reading.plan_type {
            facts.push(format!("Plan: {plan}"));
        }
        if let Some(credits) = &reading.credits {
            facts.push(format!("Credits: {credits}"));
        }
        if !facts.is_empty() {
            col = col.push(text(facts.join("  ·  ")).size(font_small).color(secondary));
        }
        col.push(text(source).size(font_tiny).color(muted)).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_existing_local_tab_can_supply_local_allowances() {
        let mut ws = Workspace::new("work".into(), PathBuf::from("/repo"), WorkspaceColor::Blue);
        ws.tabs.push(TabState::new(1, PathBuf::from("/repo")));
        assert!(local_limit_source(std::slice::from_ref(&ws), 1));
        assert!(!local_limit_source(std::slice::from_ref(&ws), 99));
        ws.location = WorkspaceLocation::RemoteAgent {
            remote_id: "remote".into(),
            workspace_id: "work".into(),
            root: "/repo".into(),
        };
        assert!(!local_limit_source(&[ws], 1));
    }

    #[test]
    fn notices_are_deduplicated_per_chat_grouped_and_held_until_idle() {
        let mut state = PlanLimitsState::default();
        let chats = vec!["first".into(), "second".into()];
        let lines = vec!["hot".into(), "routing changed".into()];
        state.queue_notices(&chats, &lines);
        state.queue_notices(&chats, &lines);
        assert!(state.notice("first", true).is_none());
        let second = state.notice("second", false).unwrap();
        assert_eq!(second.matches("hot").count(), 1);
        assert!(second.contains("routing changed"));
        state.finish_notice("second", false);
        assert!(state.notice("second", false).is_some());
        state.finish_notice("second", true);
        assert!(state.notice("second", false).is_none());
        assert!(state.notice("first", false).is_some());
        state.finish_notice("first", true);
        assert!(state.notice("first", false).is_none());
        state.queue_notices(&chats, &["Routing: hold_claude".into()]);
        state.queue_notices(&chats, &["Routing: balanced".into()]);
        let prompt = state.notice("first", false).unwrap();
        assert!(!prompt.contains("hold_claude"));
        assert!(prompt.contains("Routing: balanced"));
    }

    #[test]
    fn setting_and_clearing_a_pin_queue_a_routing_nudge() {
        let mut state = PlanLimitsState::default();
        let chats = vec!["coordinator".into()];
        let pin = plan_limits::RoutingPin {
            provider: Provider::Codex,
            until: now_unix() + 3600,
        };
        state.queue_notices(
            &chats,
            &[plan_limits::pin_change_notice(
                Some(&pin),
                now_unix(),
                &plan_limits::evaluate_policy(&state.file, now_unix())
                    .report
                    .routing,
            )],
        );
        assert!(state
            .notice("coordinator", false)
            .unwrap()
            .contains("pin to Codex set"));
        state.queue_notices(
            &chats,
            &[plan_limits::pin_change_notice(
                None,
                now_unix(),
                &plan_limits::evaluate_policy(&state.file, now_unix())
                    .report
                    .routing,
            )],
        );
        let notice = state.notice("coordinator", false).unwrap();
        assert!(notice.contains("pin cleared."));
        assert!(!notice.contains("pin to Codex set"));
    }

    fn claude_reading(used: f64, now: i64) -> PlanLimits {
        let mut reading = plan_limits::claude_limits(
            &serde_json::json!({
                "unifiedWindows": {"seven_day": {"utilization": used / 100.0, "resetsAt": now + 5 * 86400}}
            }),
            now,
        );
        reading.local = Some(true);
        reading
    }

    #[test]
    fn pending_hot_and_hold_notices_survive_restart_and_acknowledgement() {
        for used in [35.0, 95.0] {
            let now = now_unix();
            let dir = tempfile::tempdir().unwrap();
            let path = PlanLimitsFile::path_in(dir.path());
            let mut state = PlanLimitsState::default();
            let chats = vec!["coordinator".into(), "other".into()];
            state.file.absorb(claude_reading(used, now));
            state.evaluate(&chats, now);
            assert!(state.notice("coordinator", true).is_none());
            let expected = state.notice("coordinator", false).unwrap();
            state.file.save(&path).unwrap();
            let mut resumed = PlanLimitsState::load(dir.path());
            resumed.evaluate(&chats, now);
            assert_eq!(resumed.notice("coordinator", false), Some(expected.clone()));
            resumed.finish_notice("coordinator", false);
            assert_eq!(resumed.notice("coordinator", false), Some(expected.clone()));
            resumed.finish_notice("coordinator", true);
            resumed.file.save(&path).unwrap();
            let mut acknowledged = PlanLimitsState::load(dir.path());
            acknowledged.evaluate(&chats, now);
            assert!(acknowledged.notice("coordinator", false).is_none());
            assert_eq!(acknowledged.notice("other", false), Some(expected));
        }
    }

    #[test]
    fn recipients_are_coordinator_roots_across_backends_not_resumed_children() {
        let mut root = TabState::new(1, PathBuf::from("/repo"));
        root.kind = TabKind::Agent(tab::AgentSession::new(tab::AgentBackendConfig::Acp {
            agent: gitterm::harness::acp::AcpAgent::Gemini,
            model: String::new(),
            mode: None,
            effort: None,
        }));
        root.chat_session_id = Some("root".into());
        let mut child = TabState::new(2, PathBuf::from("/repo"));
        child.kind = TabKind::Agent(tab::AgentSession::new(tab::AgentBackendConfig::Claude {
            model: "default".into(),
            permission_mode: None,
            effort: None,
        }));
        child.chat_session_id = Some("worker-conversation".into());
        let children = HashSet::from(["worker-conversation".into()]);
        assert!(is_plan_limit_coordinator(&root, &children));
        assert!(!is_plan_limit_coordinator(&child, &children));
        assert!(!is_plan_limit_coordinator(
            &TabState::new(3, PathBuf::from("/repo")),
            &children
        ));
    }

    #[test]
    fn pin_under_hold_keeps_the_effective_checkpoint_and_handoff_notice() {
        let now = now_unix();
        let chats = vec!["coordinator".into()];
        let mut state = PlanLimitsState::default();
        state.file.absorb(claude_reading(95.0, now));
        state.evaluate(&chats, now);
        state.file.pin = Some(plan_limits::RoutingPin {
            provider: Provider::Claude,
            until: now + 3600,
        });
        state.evaluate(&chats, now);
        state.queue_notices(
            &chats,
            &[plan_limits::pin_change_notice(
                state.file.pin.as_ref(),
                now,
                &plan_limits::evaluate_policy(&state.file, now)
                    .report
                    .routing,
            )],
        );
        let prompt = state.notice("coordinator", false).unwrap();
        assert!(prompt.contains("start nothing new on Claude"));
        assert!(prompt.contains("writes checkpoint and handoff"));
        state.finish_notice("coordinator", true);
        state.file.pin = None;
        state.evaluate(&chats, now);
        state.queue_notices(
            &chats,
            &[plan_limits::pin_change_notice(
                None,
                now,
                &plan_limits::evaluate_policy(&state.file, now)
                    .report
                    .routing,
            )],
        );
        let cleared = state.notice("coordinator", false).unwrap();
        assert!(cleared.contains("pin cleared."));
        assert!(cleared.contains("writes checkpoint and handoff"));
    }

    #[test]
    fn busy_count_is_shared_by_provider_and_tracks_same_batch_worker_launches() {
        let mut ws = Workspace::new("work".into(), PathBuf::from("/repo"), WorkspaceColor::Blue);
        let mut codex = TabState::new(1, PathBuf::from("/repo"));
        codex.set_startup_command(Some("codex -m gpt-6-luna".into()));
        codex.task_live_state = Some(TaskSessionLiveState::Working);
        ws.tabs.push(codex);
        assert_eq!(
            running_plan_limit_workers(std::slice::from_ref(&ws), Provider::Codex, None),
            1
        );
        assert_eq!(
            running_plan_limit_workers(std::slice::from_ref(&ws), Provider::Claude, None),
            0
        );
        ws.tabs[0].task_live_state = Some(TaskSessionLiveState::AwaitingInput);
        assert_eq!(
            running_plan_limit_workers(std::slice::from_ref(&ws), Provider::Codex, None),
            0
        );
    }

    #[test]
    fn notice_changes_during_an_in_flight_save_remain_dirty() {
        let chats = vec!["coordinator".into()];
        let mut state = PlanLimitsState::default();
        state.queue_notices(&chats, &["Routing: hold_codex".into()]);
        let snapshot = state.file.clone();
        state.dirty = false;
        state.save_in_flight = true;
        state.finish_notice("coordinator", true);
        let dir = tempfile::tempdir().unwrap();
        let path = PlanLimitsFile::path_in(dir.path());
        snapshot.save(&path).unwrap();
        assert!(state.finish_save(Ok(()), &path));
        assert!(state.dirty);
        state.file.save(&path).unwrap();
        assert!(PlanLimitsState::load(dir.path())
            .notice("coordinator", false)
            .is_none());
        state.queue_notices(&chats, &["Routing: balanced".into()]);
        state.dirty = false;
        state.queue_notices(&[], &[]);
        assert!(state.dirty);
        assert!(state.file.pending_notices.is_empty());
    }

    #[test]
    fn failed_save_restores_dirty_state_for_a_later_tick() {
        let mut state = PlanLimitsState {
            dirty: false,
            save_in_flight: true,
            ..PlanLimitsState::default()
        };

        assert!(!state.finish_save(Err("disk full".into()), Path::new("plan-limits.json")));
        assert!(state.dirty);
        assert!(!state.save_in_flight);
    }

    #[test]
    fn daily_usage_places_monday_evening_before_tuesday_afternoon() {
        use chrono::TimeZone;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 11, 10, 16, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        let monday = chrono::Local
            .with_ymd_and_hms(2026, 11, 9, 20, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        let samples = [plan_limits::UsageSample {
            observed_at: monday,
            used_percent: 12.0,
        }];
        let daily = daily_usage(&samples, now);
        assert_eq!(daily[5], Some(12.0));
        assert_eq!(daily[6], None);
    }

    #[test]
    fn daily_usage_uses_day_deltas_and_respects_missing_days_and_window_start() {
        use plan_limits::UsageSample;

        let now = 10 * 86_400;
        let samples = [
            UsageSample {
                observed_at: now - 6 * 86_400,
                used_percent: 10.0,
            },
            UsageSample {
                observed_at: now - 5 * 86_400,
                used_percent: 15.0,
            },
            UsageSample {
                observed_at: now - 3 * 86_400,
                used_percent: 23.0,
            },
        ];
        let daily = daily_usage(&samples, now);
        assert_eq!(daily[0], Some(10.0));
        assert_eq!(daily[1], Some(5.0));
        assert_eq!(daily[2], None);
        assert_eq!(daily[3], Some(8.0));
        assert_eq!(daily[4], None);

        let first_day_after_reset = [UsageSample {
            observed_at: now - 6 * 86_400,
            used_percent: 3.0,
        }];
        assert_eq!(daily_usage(&first_day_after_reset, now)[0], Some(3.0));

        let reset_drop = [
            UsageSample {
                observed_at: now - 6 * 86_400,
                used_percent: 90.0,
            },
            UsageSample {
                observed_at: now - 5 * 86_400,
                used_percent: 3.0,
            },
        ];
        assert_eq!(daily_usage(&reset_drop, now)[1], Some(0.0));

        let first_day_with_prior = [
            UsageSample {
                observed_at: now - 7 * 86_400,
                used_percent: 8.0,
            },
            UsageSample {
                observed_at: now - 6 * 86_400,
                used_percent: 10.0,
            },
        ];
        assert_eq!(daily_usage(&first_day_with_prior, now)[0], Some(2.0));
    }
}
