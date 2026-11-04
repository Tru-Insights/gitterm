//! Plan limits in the Usage view and on the rail (TRU-211): the Claude Code
//! and Codex windows `/usage` shows. The reading, the rules and the words
//! live in `gitterm::plan_limits`; this holds the state, the background
//! work and the widgets.
//!
//! Claude readings arrive on `TurnCompleted.rate_limit` from any chat tab,
//! local or remote, and are taken in `update()` (a cheap in-memory merge).
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
    /// The persisted readings from the config dir (a small read).
    pub fn load(config_dir: &Path) -> Self {
        Self {
            file: PlanLimitsFile::load(&PlanLimitsFile::path_in(config_dir)),
            ..Self::default()
        }
    }
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

impl App {
    /// A Claude chat's turn reported its plan limits: the latest wins.
    pub(super) fn observe_claude_rate_limit(&mut self, reading: &RateLimitReading) {
        let limits = plan_limits::claude_limits(&reading.info, reading.observed_at);
        if self.plan_limits.file.absorb(limits) {
            self.plan_limits.dirty = true;
        }
    }

    /// Tick: a Codex read when one is due, and a save when anything
    /// changed.
    pub(super) fn poll_plan_limits(&mut self) -> Task<Event> {
        let due = self
            .plan_limits
            .codex_read_at
            .is_none_or(|at| at.elapsed() >= CODEX_READ_EVERY);
        let read = if due {
            self.read_codex_limits()
        } else {
            Task::none()
        };
        Task::batch([read, self.save_plan_limits()])
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
                        return self.save_plan_limits();
                    }
                }
            }
            PlanLimitsEvent::Saved(result) => {
                self.plan_limits.save_in_flight = false;
                if let Err(error) = result {
                    eprintln!("[plan-limits] not saved: {error}");
                }
                return self.save_plan_limits();
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
        }
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
