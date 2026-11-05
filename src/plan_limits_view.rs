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
    /// One combined prompt per chat, held until its turn finishes.
    pending_notices: HashMap<String, Vec<String>>,
}

impl PlanLimitsState {
    fn queue_notices(&mut self, chats: &[String], lines: &[String]) {
        self.pending_notices.retain(|chat, _| chats.contains(chat));
        if lines.is_empty() {
            return;
        }
        for chat in chats {
            let pending = self.pending_notices.entry(chat.clone()).or_default();
            if lines.iter().any(|line| line.starts_with("Routing:")) {
                pending.retain(|line| !line.starts_with("Routing:"));
            }
            for line in lines {
                if !pending.contains(line) {
                    pending.push(line.clone());
                }
            }
        }
    }

    fn notice(&self, chat: &str, busy: bool) -> Option<String> {
        if busy {
            return None;
        }
        self.pending_notices.get(chat).map(|lines| {
            format!("GitTerm plan limits: {}\nRelay this heads-up to the user and follow `plan_limits` routing for new work. An explicit user model choice wins; never restart running workers to change routing.", lines.join("\n"))
        })
    }

    fn finish_notice(&mut self, chat: &str, accepted: bool) {
        if accepted {
            self.pending_notices.remove(chat);
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

impl App {
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
        self.workspaces
            .iter()
            .filter(|ws| matches!(ws.location, WorkspaceLocation::Local { .. }))
            .flat_map(|ws| &ws.tabs)
            .filter_map(|tab| {
                let session = tab.agent_session()?;
                (session.backend() == tab::AgentBackend::Claude).then(|| {
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
        let now = now_unix();
        if self.plan_limits.file.prune_history(now) {
            self.plan_limits.dirty = true;
        }
        let evaluation = plan_limits::evaluate_policy(&self.plan_limits.file, now);
        if self.plan_limits.file.pin != evaluation.pin {
            self.plan_limits.file.pin = evaluation.pin.clone();
            self.plan_limits.dirty = true;
        }
        if self.plan_limits.file.policy.as_ref() != Some(&evaluation.policy) {
            self.plan_limits.file.policy = Some(evaluation.policy);
            self.plan_limits.dirty = true;
        }
        let chats = self
            .plan_limit_chats()
            .into_iter()
            .map(|(_, uid, _)| uid)
            .collect::<Vec<_>>();
        self.plan_limits.queue_notices(&chats, &evaluation.notices);
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
                self.plan_limits.queue_notices(
                    &chats,
                    &[plan_limits::pin_change_notice(pin.as_ref(), now_unix())],
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
        let badge = plan_limits::badge(self.plan_limits.file.readings(), now_unix());
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
            lines.extend(plan_limits::tooltip_lines(reading, now, &chrono::Local));
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
        if let Some(until) = plan_limits::hot_pin_until_reset(provider_report, now) {
            pin_controls = pin_controls.push(pin_row(
                format!("Send work to {} until reset", provider.label()),
                Some(plan_limits::RoutingPin { provider, until }),
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
            let percent = window.percent_at(now);
            let (figure, fill_color) = match percent {
                Some(p) => (
                    format!("{}{}%", if stale { "≥" } else { "" }, p.round()),
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
                let mut daily = [0.0_f64; 7];
                let mut seen = [false; 7];
                for sample in &history.samples {
                    let age = (now - sample.observed_at).div_euclid(86_400);
                    if (0..7).contains(&age) {
                        let day = 6 - age as usize;
                        daily[day] = sample.used_percent;
                        seen[day] = true;
                    }
                }
                let bars = Row::new()
                    .spacing(3)
                    .align_y(iced::Alignment::End)
                    .push(text("7d").size(font_tiny).color(muted));
                let bars = daily
                    .into_iter()
                    .zip(seen)
                    .fold(bars, |row, (value, seen)| {
                        let height = if seen {
                            (value.clamp(0.0, 100.0) * 0.32).max(2.0) as f32
                        } else {
                            2.0
                        };
                        let color = if seen {
                            self.plan_level_color(plan_limits::level(value), theme.accent())
                        } else {
                            track
                        };
                        row.push(
                            container(iced::widget::Space::new())
                                .width(Length::FillPortion(1))
                                .height(Length::Fixed(height))
                                .style(move |_| container::Style {
                                    background: Some(color.into()),
                                    border: iced::Border {
                                        radius: 2.0.into(),
                                        ..Default::default()
                                    },
                                    ..Default::default()
                                }),
                        )
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
            &[plan_limits::pin_change_notice(Some(&pin), now_unix())],
        );
        assert!(state
            .notice("coordinator", false)
            .unwrap()
            .contains("pin to Codex set"));
        state.queue_notices(&chats, &[plan_limits::pin_change_notice(None, now_unix())]);
        let notice = state.notice("coordinator", false).unwrap();
        assert!(notice.contains("Routing: pin cleared."));
        assert!(!notice.contains("pin to Codex set"));
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
}
