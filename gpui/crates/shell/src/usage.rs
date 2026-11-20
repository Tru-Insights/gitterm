//! Iced's Usage dashboard over host-owned data. No paths are discovered and no
//! files are written here. The host runs core `usage::scan_usage_in` off-thread
//! with explicit UsageSources, local date/timezone and configured Pricing, then
//! replaces this input. Plan limits come from `PlanLimitsFile::load` at the
//! host's explicit config path and subsequent local limit observations.
//! Subscribe to UsageIntent: SelectWindow/Refresh request scans; filters and
//! chart mode update locally; PinChanged goes to the host's existing limits
//! authority. The host preserves its latest report while a scan is running.

use gitterm_core::{
    chats::ChatBackend,
    plan_limits::{self, PlanLimitsFile, Provider, RoutingPin, UsageProviderSnapshot},
    usage::{Bucket, RepoKey, UsageReport, UsageWindow, format_cost, format_tokens, harness_index},
};
use gpui::{
    Context, EventEmitter, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, div, px, relative, rgb,
};
use gpui_component::{ActiveTheme, tooltip::Tooltip};

#[derive(Clone, Debug, PartialEq)]
pub enum UsageIntent {
    SelectWindow(UsageWindow),
    SelectRepo(Option<(RepoKey, String)>),
    ChartTokens(bool),
    ToggleHeadless,
    Refresh,
    PinChanged(Option<RoutingPin>),
}

pub(crate) fn follow_up_window(
    requested: UsageWindow,
    current: UsageWindow,
) -> Option<UsageWindow> {
    (requested != current).then_some(current)
}

/// Compatibility entry points for hosts that only have the scan state yet.
#[derive(Clone, Debug, PartialEq)]
pub enum UsageState {
    Scanning,
    Error(String),
    Report(Box<UsageReport>),
}

/// Mirrors Iced's usage fields. A refresh keeps the previous report visible;
/// plan limits render even before the first transcript scan completes.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageInput {
    pub report: Option<Box<UsageReport>>,
    pub window: UsageWindow,
    pub repo: Option<(RepoKey, String)>,
    pub chart_tokens: bool,
    pub show_headless: bool,
    pub scanning: bool,
    pub error: Option<String>,
    pub plan_limits: PlanLimitsFile,
    pub now: i64,
}

impl Default for UsageInput {
    fn default() -> Self {
        Self {
            report: None,
            window: UsageWindow::default(),
            repo: None,
            chart_tokens: false,
            show_headless: true,
            scanning: false,
            error: None,
            plan_limits: PlanLimitsFile::default(),
            now: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        }
    }
}

pub struct UsagePage {
    input: UsageInput,
}

impl UsagePage {
    pub fn new(state: UsageState) -> Self {
        let mut input = UsageInput::default();
        match state {
            UsageState::Scanning => input.scanning = true,
            UsageState::Error(error) => input.error = Some(error),
            UsageState::Report(report) => {
                input.window = report.window;
                input.report = Some(report);
            }
        }
        Self::from_input(input)
    }

    pub fn from_input(input: UsageInput) -> Self {
        Self { input }
    }

    /// Host calls inside Entity::update and then notifies the context.
    pub fn set_input(&mut self, input: UsageInput) {
        self.input = input;
    }

    pub fn from_report(report: UsageReport) -> Self {
        Self::new(UsageState::Report(Box::new(report)))
    }

    pub fn window_intent(window: UsageWindow) -> UsageIntent {
        UsageIntent::SelectWindow(window)
    }

    fn select(&mut self, intent: &UsageIntent) {
        match intent {
            UsageIntent::SelectWindow(window) => self.input.window = *window,
            UsageIntent::SelectRepo(repo) => self.input.repo = repo.clone(),
            UsageIntent::ChartTokens(tokens) => self.input.chart_tokens = *tokens,
            UsageIntent::ToggleHeadless => self.input.show_headless = !self.input.show_headless,
            UsageIntent::Refresh | UsageIntent::PinChanged(_) => {}
        }
    }

    fn repo_intent(&self, key: RepoKey, label: String) -> UsageIntent {
        UsageIntent::SelectRepo(
            if self.input.repo.as_ref().map(|(key, _)| key) == Some(&key) {
                None
            } else {
                Some((key, label))
            },
        )
    }

    fn control(
        &self,
        id: String,
        label: String,
        active: bool,
        intent: UsageIntent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = crate::theme::Palette::current(cx);
        div()
            .id(id)
            .px(px(10.))
            .py(px(4.))
            .rounded(px(4.))
            .bg(rgb(if active { p.surface } else { p.base }))
            .text_color(rgb(if active { p.text } else { p.subtext0 }))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select(&intent);
                cx.emit(intent.clone());
                cx.notify();
            }))
            .child(label)
    }

    /// Deterministic accessibility/test transcript of the same dashboard data.
    pub fn display_lines(&self) -> Vec<String> {
        let i = &self.input;
        let mut lines = vec![
            "Usage".into(),
            i.repo
                .as_ref()
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| "All repos".into()),
        ];
        lines.extend(UsageWindow::ALL.map(|w| w.label().into()));
        lines.extend([
            "Cost".into(),
            "Tokens".into(),
            headless_label(i).into(),
            refresh_label(i).into(),
        ]);
        if let Some(error) = &i.error {
            lines.push(format!("Usage scan failed: {error}"));
        }
        lines.push("Plan limits".into());
        lines.push(
            plan_limits::evaluate_policy(&i.plan_limits, i.now)
                .report
                .routing
                .reason,
        );
        for snapshot in i.plan_limits.usage_snapshot(i.now) {
            lines.push(snapshot.provider.label().into());
            if let Some(freshness) = &snapshot.freshness {
                lines.push(freshness.clone());
            }
            lines.push(snapshot.weekly_status.clone());
            if let Some(reading) = i.plan_limits.get(snapshot.provider) {
                lines.extend(reading.active_alerts(i.now).iter().map(|a| a.text()));
                for window in snapshot.windows {
                    lines.push(format!(
                        "{} · {} · {}",
                        window.label, window.figure, window.reset
                    ));
                    if window.daily.is_some() {
                        lines.push("by day".into());
                    }
                }
            } else {
                lines.push(empty_limits(snapshot.provider).into());
            }
            if pin_overridden(i, snapshot.provider) {
                lines.push(PIN_OVERRIDDEN.into());
            }
            lines.push(format!("Send work to {} until", snapshot.provider.label()));
            lines.extend(["1h".into(), "4h".into(), "24h".into()]);
            if let Some(pin) = reset_pin(i, snapshot.provider) {
                lines.push(format!("Send work to {} until reset", pin.provider.label()));
            }
            if i.plan_limits
                .pin
                .as_ref()
                .is_some_and(|pin| pin.provider == snapshot.provider)
            {
                lines.push("clear".into());
            }
            if let Some(facts) = plan_facts(i, snapshot.provider) {
                lines.push(facts);
            }
            lines.push(limit_source(snapshot.provider).into());
        }
        let Some(report) = &i.report else {
            lines.push("Reading transcripts…".into());
            return lines;
        };
        let view = report.view_for(i.show_headless, i.repo.as_ref().map(|(key, _)| key));
        for (label, value) in total_values(&view.totals) {
            lines.push(format!("{label}: {value}"));
        }
        lines.push(format!(
            "{} days at full API list rates",
            report.window.days()
        ));
        if view.totals.unpriced_tokens > 0 {
            lines.push(unpriced_note(&view.totals));
        }
        lines.push(chart_title(i).into());
        let peak = chart_peak(&view.days, i.chart_tokens);
        lines.push(peak_label(peak, i.chart_tokens));
        lines.push(report.start.format("%b %-d").to_string());
        lines.push(report.end.format("%b %-d").to_string());
        for backend in ChatBackend::ALL {
            let bucket = &view.by_harness[harness_index(backend)];
            lines.push(format!(
                "{} {}",
                backend.label(),
                if i.chart_tokens {
                    format_tokens(bucket.tokens.processed())
                } else {
                    cost_label(bucket)
                }
            ));
        }
        lines.extend(["By model".into(), "Model · Cost · Share · Tokens".into()]);
        for model in &view.models {
            lines.push(table_line(
                &model.model,
                &model.usage,
                &view.totals,
                i.chart_tokens,
            ));
        }
        if view.models.is_empty() {
            lines.push(EMPTY.into());
        }
        let all = report.view(i.show_headless);
        lines.extend(["By repo".into(), "Repo · Cost · Share · Tokens".into()]);
        for repo in &all.repos {
            lines.push(table_line(
                &repo_name(repo),
                &repo.usage,
                &all.totals,
                i.chart_tokens,
            ));
        }
        if all.repos.is_empty() {
            lines.push(EMPTY.into());
        }
        lines.extend(footer(report, i.show_headless));
        lines
    }

    fn limits_block(&self, snapshot: UsageProviderSnapshot, cx: &mut Context<Self>) -> gpui::Div {
        let p = crate::theme::Palette::current(cx);
        let font: f32 = cx.theme().font_size.into();
        let i = &self.input;
        let provider = snapshot.provider;
        let mut header = row()
            .justify_between()
            .child(div().text_size(px(font)).child(provider.label()));
        if let Some(freshness) = snapshot.freshness {
            let stale = i
                .plan_limits
                .get(provider)
                .is_some_and(|r| r.is_stale(i.now));
            header = header.child(
                div()
                    .text_size(px(font - 3.))
                    .text_color(rgb(if stale { p.yellow } else { p.muted }))
                    .child(freshness),
            );
        }
        let mut block = card(p).flex_1().min_w_0().gap(px(8.)).child(header).child(
            div()
                .text_color(rgb(p.subtext0))
                .child(snapshot.weekly_status),
        );
        if let Some(reading) = i.plan_limits.get(provider) {
            for alert in reading.active_alerts(i.now) {
                block = block.child(
                    div()
                        .border_1()
                        .border_color(rgb(p.red))
                        .rounded_full()
                        .px(px(10.))
                        .py(px(3.))
                        .text_color(rgb(p.red))
                        .child(alert.text()),
                );
            }
            for window in snapshot.windows {
                let color = match window.percent.map(plan_limits::level) {
                    Some(plan_limits::Level::Danger) => p.red,
                    Some(plan_limits::Level::Warning) => p.yellow,
                    Some(plan_limits::Level::Normal) => p.blue,
                    None => p.surface,
                };
                block = block.child(
                    row()
                        .gap(px(10.))
                        .child(div().w(px(56.)).flex_shrink_0().child(window.label.clone()))
                        .child(
                            div()
                                .flex_1()
                                .h(px(8.))
                                .rounded(px(3.))
                                .bg(rgb(p.surface))
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(
                                            (window.percent.unwrap_or(0.).clamp(0., 100.) / 100.)
                                                as f32,
                                        ))
                                        .rounded(px(3.))
                                        .bg(rgb(color)),
                                ),
                        )
                        .child(
                            div()
                                .w(px(60.))
                                .flex_shrink_0()
                                .text_right()
                                .text_color(rgb(if window.percent.is_some() {
                                    color
                                } else {
                                    p.muted
                                }))
                                .child(window.figure),
                        )
                        .child(
                            div()
                                .w(px(190.))
                                .flex_shrink_0()
                                .text_size(px(font - 3.))
                                .text_color(rgb(p.muted))
                                .child(window.reset),
                        ),
                );
                if let Some(daily) = window.daily {
                    let mut bars = row().h(px(34.)).items_end().gap(px(3.)).child(
                        div()
                            .text_size(px(font - 3.))
                            .text_color(rgb(p.muted))
                            .child("by day"),
                    );
                    for (day, value) in daily.into_iter().enumerate() {
                        let hint = value
                            .map(|v| format!("{}: +{v:.0}%", window.weekdays[day]))
                            .unwrap_or_default();
                        bars = bars.child(
                            div()
                                .id(format!("limit-day-{provider:?}-{}-{day}", window.label))
                                .flex_1()
                                .h(px(value
                                    .map(|v| (v.clamp(0., 20.) * 1.2).max(2.) as f32)
                                    .unwrap_or(2.)))
                                .bg(rgb(if value.is_some() { p.blue } else { p.surface }))
                                .rounded(px(2.))
                                .tooltip(move |window, cx| {
                                    Tooltip::new(hint.clone()).build(window, cx)
                                }),
                        );
                    }
                    block = block.child(bars);
                }
            }
        } else {
            block = block.child(
                div()
                    .text_color(rgb(p.subtext0))
                    .child(empty_limits(provider)),
            );
        }
        if pin_overridden(i, provider) {
            block = block.child(div().text_color(rgb(p.yellow)).child(PIN_OVERRIDDEN));
        }
        let mut pins = row().gap(px(5.)).text_size(px(font - 3.)).child(
            div()
                .text_color(rgb(p.muted))
                .child(format!("Send work to {} until", provider.label())),
        );
        for (label, duration) in [("1h", 3600), ("4h", 4 * 3600), ("24h", 24 * 3600)] {
            pins = pins.child(self.control(
                format!("pin-{provider:?}-{label}"),
                label.into(),
                false,
                UsageIntent::PinChanged(Some(RoutingPin {
                    provider,
                    until: i.now + duration,
                })),
                cx,
            ));
        }
        if let Some(pin) = reset_pin(i, provider) {
            pins = pins.child(self.control(
                format!("pin-{provider:?}-reset"),
                format!("Send work to {} until reset", pin.provider.label()),
                false,
                UsageIntent::PinChanged(Some(pin)),
                cx,
            ));
        }
        if i.plan_limits
            .pin
            .as_ref()
            .is_some_and(|pin| pin.provider == provider)
        {
            pins = pins.child(self.control(
                format!("pin-{provider:?}-clear"),
                "clear".into(),
                false,
                UsageIntent::PinChanged(None),
                cx,
            ));
        }
        block = block.child(pins);
        if let Some(facts) = plan_facts(i, provider) {
            block = block.child(div().text_color(rgb(p.subtext0)).child(facts));
        }
        block.child(
            div()
                .text_size(px(font - 3.))
                .text_color(rgb(p.muted))
                .child(limit_source(provider)),
        )
    }
}

impl EventEmitter<UsageIntent> for UsagePage {}

impl Render for UsagePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::current(cx);
        let font: f32 = cx.theme().font_size.into();
        let i = &self.input;
        let mut body = div()
            .id("usage-page")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(24.))
            .bg(rgb(p.base))
            .text_color(rgb(p.text))
            .text_size(px(font - 2.));
        let mut header = row()
            .gap(px(4.))
            .child(div().text_size(px(font + 6.)).mr(px(12.)).child("Usage"));
        header = if let Some((_, name)) = &i.repo {
            header.child(
                row()
                    .gap(px(6.))
                    .px(px(10.))
                    .py(px(3.))
                    .rounded_full()
                    .bg(rgb(p.surface))
                    .child(name.clone())
                    .child(self.control(
                        "usage-clear-repo".into(),
                        "×".into(),
                        true,
                        UsageIntent::SelectRepo(None),
                        cx,
                    )),
            )
        } else {
            header.child(div().text_color(rgb(p.muted)).child("All repos"))
        };
        header = header.child(div().flex_1());
        for window in UsageWindow::ALL {
            header = header.child(self.control(
                format!("usage-window-{}", window.label()),
                window.label().into(),
                i.window == window,
                UsageIntent::SelectWindow(window),
                cx,
            ));
        }
        header = header.child(div().w(px(12.)));
        for (label, tokens) in [("Cost", false), ("Tokens", true)] {
            header = header.child(self.control(
                format!("usage-mode-{label}"),
                label.into(),
                i.chart_tokens == tokens,
                UsageIntent::ChartTokens(tokens),
                cx,
            ));
        }
        header = header
            .child(div().w(px(12.)))
            .child(self.control(
                "usage-headless".into(),
                headless_label(i).into(),
                i.show_headless,
                UsageIntent::ToggleHeadless,
                cx,
            ))
            .child(self.control(
                "usage-refresh".into(),
                refresh_label(i).into(),
                false,
                UsageIntent::Refresh,
                cx,
            ));
        body = body.child(header);
        if let Some(error) = &i.error {
            body = body.child(
                div()
                    .text_color(rgb(p.red))
                    .child(format!("Usage scan failed: {error}")),
            );
        }
        let mut limits = row().gap(px(12.)).items_start();
        for snapshot in i.plan_limits.usage_snapshot(i.now) {
            limits = limits.child(self.limits_block(snapshot, cx));
        }
        body = body.child(
            column()
                .gap(px(8.))
                .child(div().text_size(px(font)).child("Plan limits"))
                .child(
                    div().text_color(rgb(p.subtext0)).child(
                        plan_limits::evaluate_policy(&i.plan_limits, i.now)
                            .report
                            .routing
                            .reason,
                    ),
                )
                .child(limits),
        );
        let Some(report) = &i.report else {
            return body.child(
                div()
                    .text_size(px(font))
                    .text_color(rgb(p.subtext0))
                    .child("Reading transcripts…"),
            );
        };
        let view = report.view_for(i.show_headless, i.repo.as_ref().map(|(key, _)| key));
        let mut totals = row().gap(px(12.)).items_stretch();
        for (index, (label, value)) in total_values(&view.totals).into_iter().enumerate() {
            let mut tile = card(p)
                .flex_1()
                .flex_grow(if index == 0 { 3. } else { 2. })
                .min_w_0()
                .gap(px(4.))
                .child(
                    div()
                        .text_size(px(font - 3.))
                        .text_color(rgb(p.muted))
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(font + if index == 0 { 10. } else { 6. }))
                        .child(value),
                );
            if index == 0 {
                tile = tile.child(
                    div()
                        .text_size(px(font - 3.))
                        .text_color(rgb(p.muted))
                        .child(format!(
                            "{} days at full API list rates",
                            report.window.days()
                        )),
                );
                if view.totals.unpriced_tokens > 0 {
                    tile = tile.child(
                        div()
                            .text_size(px(font - 3.))
                            .text_color(rgb(p.yellow))
                            .child(unpriced_note(&view.totals)),
                    );
                }
            }
            totals = totals.child(tile);
        }
        body = body.child(totals);
        let peak = chart_peak(&view.days, i.chart_tokens);
        let mut bars = row()
            .h(px(180.))
            .items_end()
            .gap(px(if view.days.len() > 30 { 1. } else { 3. }));
        for day in &view.days {
            let mut stack = column().flex_1().min_w_0();
            for backend in ChatBackend::ALL.iter().rev() {
                let v = chart_value(&day.by_harness[harness_index(*backend)], i.chart_tokens);
                let height = if peak > 0. {
                    (v / peak * 180.) as f32
                } else {
                    0.
                };
                if height >= 0.5 {
                    stack = stack.child(
                        div()
                            .h(px(height))
                            .w_full()
                            .bg(rgb(backend_color(*backend, p))),
                    );
                }
            }
            bars = bars.child(stack);
        }
        let mut legend = row().gap(px(16.));
        for backend in ChatBackend::ALL {
            let bucket = &view.by_harness[harness_index(backend)];
            let figure = if i.chart_tokens {
                format_tokens(bucket.tokens.processed())
            } else {
                cost_label(bucket)
            };
            legend = legend.child(
                row()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(9.))
                            .text_color(rgb(backend_color(backend, p)))
                            .child("●"),
                    )
                    .child(
                        div()
                            .text_color(rgb(p.subtext0))
                            .child(format!("{} {figure}", backend.label())),
                    ),
            );
        }
        body = body.child(
            card(p)
                .gap(px(8.))
                .child(
                    row()
                        .justify_between()
                        .child(div().text_size(px(font)).child(chart_title(i)))
                        .child(
                            div()
                                .text_color(rgb(p.muted))
                                .child(peak_label(peak, i.chart_tokens)),
                        ),
                )
                .child(
                    column()
                        .child(bars)
                        .child(div().w_full().h(px(1.)).bg(rgb(surface1(p)))),
                )
                .child(
                    row()
                        .justify_between()
                        .text_size(px(font - 3.))
                        .text_color(rgb(p.muted))
                        .child(report.start.format("%b %-d").to_string())
                        .child(report.end.format("%b %-d").to_string()),
                )
                .child(legend),
        );
        let mut models = card(p)
            .flex_1()
            .min_w_0()
            .gap(px(6.))
            .child(div().text_size(px(font)).child("By model"))
            .child(table_header("Model", p, font));
        for model in &view.models {
            models = models.child(
                row()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(9.))
                            .text_color(rgb(backend_color(model.harness, p)))
                            .child("●"),
                    )
                    .child(table_row(
                        &model.model,
                        &model.usage,
                        &view.totals,
                        i.chart_tokens,
                        p,
                    )),
            );
        }
        if view.models.is_empty() {
            models = models.child(div().text_color(rgb(p.muted)).child(EMPTY));
        }
        let all = report.view(i.show_headless);
        let mut repos = card(p)
            .flex_1()
            .min_w_0()
            .gap(px(2.))
            .child(div().text_size(px(font)).child("By repo"))
            .child(table_header("Repo", p, font).px(px(6.)).py(px(4.)));
        for (index, repo) in all.repos.iter().enumerate() {
            let name = repo_name(repo);
            let intent = self.repo_intent(repo.path.clone(), name.clone());
            let selected = i.repo.as_ref().map(|(key, _)| key) == Some(&repo.path);
            repos = repos.child(
                div()
                    .id(format!("usage-repo-{index}"))
                    .px(px(6.))
                    .py(px(4.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .bg(rgb(if selected { surface1(p) } else { p.mantle }))
                    .hover(|style| style.bg(rgb(if selected { surface1(p) } else { p.surface })))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select(&intent);
                        cx.emit(intent.clone());
                        cx.notify();
                    }))
                    .child(
                        table_row(&name, &repo.usage, &all.totals, i.chart_tokens, p)
                            .text_color(rgb(if repo.resolved { p.text } else { p.subtext0 })),
                    ),
            );
        }
        if all.repos.is_empty() {
            repos = repos.child(div().text_color(rgb(p.muted)).child(EMPTY));
        }
        body = body.child(row().gap(px(16.)).items_start().child(models).child(repos));
        let mut foot = column()
            .gap(px(2.))
            .text_size(px(font - 3.))
            .text_color(rgb(p.muted));
        let footer_lines = footer(report, i.show_headless);
        for (index, line) in footer_lines.iter().enumerate() {
            let warning = report.stats.files_unreadable > 0 && index + 1 == footer_lines.len();
            foot = foot.child(
                div()
                    .text_color(rgb(if warning { p.yellow } else { p.muted }))
                    .child(line.clone()),
            );
        }
        body.child(foot)
    }
}

const EMPTY: &str = "No usage in this window.";
const PIN_OVERRIDDEN: &str =
    "Pin overridden: this plan is almost out; new work is held until reset.";
fn row() -> gpui::Div {
    div().flex().flex_row().items_center()
}
fn column() -> gpui::Div {
    div().flex().flex_col()
}
fn card(p: crate::theme::Palette) -> gpui::Div {
    column()
        .p(px(14.))
        .bg(rgb(p.mantle))
        .border_1()
        .border_color(rgb(p.surface))
        .rounded(px(6.))
}
fn surface1(p: crate::theme::Palette) -> u32 {
    if p.base == 0x1e1e2e {
        0x45475a
    } else {
        0xbcc0cc
    }
}
fn backend_color(backend: ChatBackend, p: crate::theme::Palette) -> u32 {
    match backend {
        ChatBackend::Claude => p.peach,
        ChatBackend::Codex => p.blue,
        ChatBackend::Pi => {
            if p.base == 0x1e1e2e {
                0xcba6f7
            } else {
                0x8839ef
            }
        }
    }
}
fn headless_label(i: &UsageInput) -> &'static str {
    if i.show_headless {
        "headless shown"
    } else {
        "headless hidden"
    }
}
fn refresh_label(i: &UsageInput) -> &'static str {
    if i.scanning { "scanning…" } else { "refresh" }
}
fn chart_title(i: &UsageInput) -> &'static str {
    if i.chart_tokens {
        "Daily tokens"
    } else {
        "Daily cost"
    }
}
fn chart_value(b: &Bucket, tokens: bool) -> f64 {
    if tokens {
        b.tokens.processed() as f64
    } else {
        b.cost
    }
}
fn chart_peak(days: &[gitterm_core::usage::DayRow], tokens: bool) -> f64 {
    days.iter()
        .map(|d| {
            d.by_harness
                .iter()
                .map(|b| chart_value(b, tokens))
                .sum::<f64>()
        })
        .fold(0., f64::max)
}
fn peak_label(peak: f64, tokens: bool) -> String {
    format!(
        "peak {} / day",
        if tokens {
            format_tokens(peak as u64)
        } else {
            format_cost(peak)
        }
    )
}
fn total_values(b: &Bucket) -> [(&'static str, String); 6] {
    [
        ("Estimated cost", format_cost(b.cost)),
        ("Processed", format_tokens(b.tokens.processed())),
        ("Cached input", format_tokens(b.tokens.cache_read)),
        ("Uncached input", format_tokens(b.tokens.uncached_input)),
        ("Cache writes", format_tokens(b.tokens.cache_write)),
        ("Output", format_tokens(b.tokens.output)),
    ]
}
fn unpriced_note(b: &Bucket) -> String {
    format!(
        "+ {} tokens on unpriced models",
        format_tokens(b.unpriced_tokens)
    )
}
fn cost_label(b: &Bucket) -> String {
    if b.cost == 0. && b.unpriced_tokens > 0 {
        "unpriced".into()
    } else {
        format_cost(b.cost)
    }
}
fn share(b: &Bucket, whole: &Bucket, tokens: bool) -> String {
    let (part, whole) = (chart_value(b, tokens), chart_value(whole, tokens));
    if whole > 0. && part > 0. {
        format!("{:.0}%", part / whole * 100.)
    } else {
        "—".into()
    }
}
fn table_line(name: &str, b: &Bucket, whole: &Bucket, tokens: bool) -> String {
    format!(
        "{name} · {} · {} · {}",
        cost_label(b),
        share(b, whole, tokens),
        format_tokens(b.tokens.processed())
    )
}
fn cell(value: String, width: f32) -> gpui::Div {
    div()
        .w(px(width))
        .flex_shrink_0()
        .text_right()
        .whitespace_nowrap()
        .child(value)
}
fn table_header(name: &'static str, p: crate::theme::Palette, font: f32) -> gpui::Div {
    row()
        .gap(px(8.))
        .text_size(px(font - 3.))
        .text_color(rgb(p.muted))
        .child(div().flex_1().child(name))
        .child(cell("Cost".into(), 80.))
        .child(cell("Share".into(), 50.))
        .child(cell("Tokens".into(), 70.))
}
fn table_row(
    name: &str,
    b: &Bucket,
    whole: &Bucket,
    tokens: bool,
    p: crate::theme::Palette,
) -> gpui::Div {
    row()
        .flex_1()
        .min_w_0()
        .gap(px(8.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_nowrap()
                .child(name.to_owned()),
        )
        .child(cell(cost_label(b), 80.).text_color(rgb(p.subtext0)))
        .child(cell(share(b, whole, tokens), 50.).text_color(rgb(p.subtext0)))
        .child(cell(format_tokens(b.tokens.processed()), 70.).text_color(rgb(p.subtext0)))
}
fn repo_name(repo: &gitterm_core::usage::RepoRow) -> String {
    if repo.resolved {
        repo.name.clone()
    } else {
        format!("{} (no repo)", repo.name)
    }
}
fn empty_limits(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "No reading yet: the next turn of a GitTerm Claude chat brings one.",
        Provider::Codex => {
            "No reading yet: no Codex rollout with rate limits under ~/.codex/sessions."
        }
    }
}
fn limit_source(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "From GitTerm Claude chats; updates when a turn ends.",
        Provider::Codex => "From the newest Codex rollout; read every minute.",
    }
}
fn reset_pin(i: &UsageInput, provider: Provider) -> Option<RoutingPin> {
    let evaluation = plan_limits::evaluate_policy(&i.plan_limits, i.now);
    plan_limits::hot_reset_pin(
        provider,
        match provider {
            Provider::Claude => &evaluation.report.claude,
            Provider::Codex => &evaluation.report.codex,
        },
        i.now,
    )
}
fn pin_overridden(i: &UsageInput, provider: Provider) -> bool {
    let evaluation = plan_limits::evaluate_policy(&i.plan_limits, i.now);
    let report = match provider {
        Provider::Claude => evaluation.report.claude,
        Provider::Codex => evaluation.report.codex,
    };
    i.plan_limits.get(provider).is_some()
        && i.plan_limits
            .pin
            .as_ref()
            .is_some_and(|pin| pin.provider == provider && pin.until > i.now)
        && report
            .windows
            .iter()
            .any(|w| w.label == "7d" && w.state == Some(plan_limits::WeeklyState::AlmostOut))
}
fn plan_facts(i: &UsageInput, provider: Provider) -> Option<String> {
    let reading = i.plan_limits.get(provider)?;
    let mut facts = vec![];
    if let Some(plan) = &reading.plan_type {
        facts.push(format!("Plan: {plan}"));
    }
    if let Some(credits) = &reading.credits {
        facts.push(format!("Credits: {credits}"));
    }
    (!facts.is_empty()).then(|| facts.join("  ·  "))
}
fn footer(report: &UsageReport, headless: bool) -> Vec<String> {
    let stats = &report.stats;
    let mut lines = vec![format!(
        "{} transcripts · {} responses · scanned in {:.1}s",
        stats.files_seen - stats.files_outside_window,
        format_tokens(stats.responses),
        stats.elapsed.as_secs_f64()
    )];
    if report.headless_responses > 0 {
        lines.push(format!(
            "{} headless responses (SDK, exec, harness subagents) {}",
            format_tokens(report.headless_responses),
            if headless { "included" } else { "hidden" }
        ));
    }
    if stats.files_unreadable > 0 {
        lines.push(format!(
            "{} transcripts could not be read (see log)",
            stats.files_unreadable
        ));
    }
    lines
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
