//! Plan limits in the Usage view and on the rail (TRU-211): the Claude Code
//! and Codex windows `/usage` shows. The reading, the rules and the words
//! live in `gitterm::plan_limits`; shared state and jobs live in core limits.
//! This adapter selects local sources and renders Usage status; it never prompts chats.
//!
//! Claude readings arrive on `TurnCompleted.rate_limit` from any chat tab,
//! local only, and are taken in `update()` (a cheap in-memory merge).
//! The Codex reading is a tail read of the newest rollout, run off the UI
//! thread from `Tick` every [`CODEX_READ_EVERY`], and at once when the
//! Usage view opens or refreshes. Changes are written to
//! `plan-limits.json` off the UI thread, one write at a time.

use super::*;

use gitterm::harness::RateLimitReading;
use gitterm::plan_limits::{self, Level, PlanLimitsFile, Provider};

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

fn weekly_status(report: &plan_limits::ProviderReport, now: i64) -> String {
    let weekly = report.windows.iter().find(|window| window.label == "7d");
    let Some(weekly) = weekly else {
        return "Weekly usage unknown; days left unknown; run-out unknown; pace unknown; no reading".into();
    };
    let used = weekly
        .estimated_percent
        .or(weekly.used_percent)
        .map(|used| format!("{used:.0}% used"))
        .unwrap_or_else(|| "usage unknown".into());
    let left = weekly
        .resets_in
        .filter(|left| *left > 0)
        .map(|left| format!("{:.1} days until reset", left as f64 / 86400.0))
        .unwrap_or_else(|| {
            if weekly.resets_at.is_some_and(|at| at <= now) {
                "window reset; no reading since".into()
            } else {
                "reset unknown".into()
            }
        });
    let runout = weekly
        .days_left
        .and_then(|days| {
            chrono::DateTime::from_timestamp(now.saturating_add((days * 86400.0) as i64), 0)
        })
        .map(|at| format!("run-out {} UTC", at.format("%Y-%m-%d %H:%M")))
        .unwrap_or_else(|| "run-out unknown".into());
    let pace = weekly
        .pace
        .map(|pace| format!("pace {pace:.0} (100 uses the full weekly allowance)"))
        .unwrap_or_else(|| "pace unknown".into());
    let status = match weekly.state {
        Some(plan_limits::WeeklyState::Hot) => "running hot",
        Some(plan_limits::WeeklyState::AlmostOut) => "almost out",
        Some(plan_limits::WeeklyState::OnTrack) => "on track",
        None => "unknown",
    };
    let reading = if weekly.estimated {
        "estimated reading"
    } else if report.observed_at.is_none() {
        "no reading"
    } else if report.stale {
        "stale reading"
    } else {
        "observed reading"
    };
    format!("Weekly {status}: {used} · {left} · {runout} · {pace} · {reading}")
}

pub use gitterm_core::limits::{PlanLimitsEvent, PlanLimitsState};

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

pub(super) fn local_limit_source(workspaces: &[Workspace], tab_id: usize) -> bool {
    workspaces.iter().any(|ws| {
        matches!(ws.location, WorkspaceLocation::Local { .. })
            && ws.tabs.iter().any(|tab| tab.id == tab_id)
    })
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
        running_plan_limit_workers(&self.workspaces, provider, self.task_host.store().as_ref())
    }

    pub(super) fn plan_limits_routing(&self) -> plan_limits::RoutingReport {
        plan_limits::evaluate_policy(self.plan_limits.file(), now_unix())
            .report
            .routing
    }

    pub(super) fn plan_limits_snapshot(&self) -> Result<serde_json::Value, String> {
        serde_json::to_value(
            plan_limits::evaluate_policy(self.plan_limits.file(), now_unix()).report,
        )
        .map_err(|error| format!("cannot encode plan_limits result: {error}"))
    }

    fn evaluate_plan_limits(&mut self) {
        self.plan_limits.evaluate(&[], now_unix());
    }

    /// A Claude chat's turn reported its plan limits: the latest wins.
    pub(super) fn observe_claude_rate_limit(&mut self, reading: &RateLimitReading) {
        if self.plan_limits.observe_claude_rate_limit(reading) {
            self.evaluate_plan_limits();
        }
    }

    /// Tick: a Codex read when one is due, and a save when anything
    /// changed.
    pub(super) fn poll_plan_limits(&mut self) -> Task<Event> {
        self.evaluate_plan_limits();
        let read = self
            .plan_limits
            .begin_scheduled_codex_read(Instant::now())
            .map_or_else(Task::none, |job| {
                shutdown_task("read_codex_limits", job.run(), Event::PlanLimits)
            });
        Task::batch([
            read,
            self.save_plan_limits(),
            self.start_queued_delegations(),
        ])
    }

    /// Read the newest Codex rollout's limits now, unless a read runs.
    pub(super) fn read_codex_limits(&mut self) -> Task<Event> {
        if self.plan_limits.codex_read_in_flight() {
            return Task::none();
        }
        let sessions = gitterm_core::limits::local_codex_sessions();
        let Some(job) = self.plan_limits.begin_codex_read(sessions, Instant::now()) else {
            return Task::none();
        };
        shutdown_task("read_codex_limits", job.run(), Event::PlanLimits)
    }

    /// Writes the readings off the UI thread, one write at a time.
    pub(super) fn save_plan_limits(&mut self) -> Task<Event> {
        let path = PlanLimitsFile::path_in(&config::global_config_dir());
        let Some(job) = self.plan_limits.begin_save(path) else {
            return Task::none();
        };
        shutdown_task("save_plan_limits", job.run(), Event::PlanLimits)
    }

    pub(super) fn update_plan_limits(&mut self, event: PlanLimitsEvent) -> Task<Event> {
        let path = PlanLimitsFile::path_in(&config::global_config_dir());
        if self.plan_limits.complete_event(event, &path, now_unix()) {
            self.save_plan_limits()
        } else {
            Task::none()
        }
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
        let (label, color, height) = match self.plan_limits.file().usage_badge.level {
            plan_limits::UsageBadgeLevel::Hot => ("hot", self.theme.warning(), BADGE_HEIGHT),
            plan_limits::UsageBadgeLevel::Urgent => ("urgent", self.theme.danger(), BADGE_HEIGHT),
            plan_limits::UsageBadgeLevel::OnTrack => ("", self.theme.subtext0(), 0.0),
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
        match self.plan_limits.file().usage_badge.level {
            plan_limits::UsageBadgeLevel::Hot => lines.push("Plan usage is running hot".into()),
            plan_limits::UsageBadgeLevel::Urgent => lines.push("Plan usage is urgent".into()),
            plan_limits::UsageBadgeLevel::OnTrack => {}
        }
        for reading in self.plan_limits.file().readings() {
            lines.extend(plan_limits::tooltip_lines(
                reading,
                &self.plan_limits.file().history,
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
            .push(
                text(self.plan_limits_routing().reason)
                    .size(font_small)
                    .color(theme.text_secondary()),
            )
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
        let reading = self.plan_limits.file().get(provider);
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
        let report = plan_limits::evaluate_policy(self.plan_limits.file(), now);
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
            .file()
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
                .push(
                    text(weekly_status(provider_report, now))
                        .size(font_small)
                        .color(secondary),
                )
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
        let mut col = Column::new().spacing(8).push(header).push(
            text(weekly_status(provider_report, now))
                .size(font_small)
                .color(secondary),
        );
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
                plan_limits::window_figure(reading, window, &self.plan_limits.file().history, now);
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
            let history = self.plan_limits.file().history.iter().find(|history| {
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
            .file()
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
    fn weekly_status_shows_projection_estimate_and_unknown_reset() {
        let now = 1_800_000_000;
        let mut file = PlanLimitsFile::default();
        let mut reading = plan_limits::claude_limits(
            &serde_json::json!({
                "unifiedWindows": {"seven_day": {"utilization": 0.35, "resetsAt": now + 5 * 86400}}
            }),
            now,
        );
        reading.local = Some(true);
        file.absorb(reading);
        let report = plan_limits::evaluate_policy(&file, now);
        let status = weekly_status(&report.report.claude, now);
        for fact in [
            "running hot",
            "35% used",
            "5.0 days until reset",
            "run-out 20",
            "UTC",
            "pace",
            "observed reading",
        ] {
            assert!(status.contains(fact), "missing {fact}: {status}");
        }
        let mut tiny = plan_limits::evaluate_policy(&file, now);
        tiny.report
            .claude
            .windows
            .iter_mut()
            .find(|w| w.label == "7d")
            .unwrap()
            .days_left = Some(f64::MAX);
        assert!(weekly_status(&tiny.report.claude, now).contains("run-out unknown"));
        let later = now + plan_limits::STALE_AFTER_SECS + 1;
        let estimated = plan_limits::evaluate_policy(&file, later);
        assert!(weekly_status(&estimated.report.claude, later).contains("estimated reading"));
        let reset_at = now + 5 * 86400;
        let reset = plan_limits::evaluate_policy(&file, reset_at);
        let status = weekly_status(&reset.report.claude, reset_at);
        assert!(status.contains("usage unknown"));
        assert!(status.contains("window reset; no reading since"));
        assert!(status.contains("run-out unknown"));
    }

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
