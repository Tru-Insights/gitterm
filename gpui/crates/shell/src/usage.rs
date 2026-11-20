//! Native presentation for the shared core usage report. This module does not
//! scan transcripts or calculate accounting values.

use gitterm_core::{
    chats::ChatBackend,
    usage::{Bucket, UsageReport, UsageWindow, format_cost, format_tokens},
};
use gpui::{
    Context, EventEmitter, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window, div, rgb,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageIntent {
    SelectWindow(UsageWindow),
}

#[derive(Clone, Debug, PartialEq)]
pub enum UsageState {
    Scanning,
    Error(String),
    Report(Box<UsageReport>),
}

/// A fixture-ready leaf view. The host may subscribe to [`UsageIntent`] later;
/// selecting a window currently emits the intent without starting a scan.
pub struct UsagePage {
    state: UsageState,
}

impl UsagePage {
    pub fn new(state: UsageState) -> Self {
        Self { state }
    }

    pub fn from_report(report: UsageReport) -> Self {
        Self::new(UsageState::Report(Box::new(report)))
    }

    pub fn window_intent(window: UsageWindow) -> UsageIntent {
        UsageIntent::SelectWindow(window)
    }

    /// The text rows rendered by this page, kept available for deterministic
    /// fixture tests without creating a native window.
    pub fn display_lines(&self) -> Vec<String> {
        let mut lines = vec!["Usage".to_string()];
        if let UsageState::Report(report) = &self.state {
            lines.push(format!("Window: {}", report.window.label()));
        }
        match &self.state {
            UsageState::Scanning => lines.push("Reading transcripts…".into()),
            UsageState::Error(error) => lines.push(format!("Usage scan failed: {error}")),
            UsageState::Report(report) => {
                let view = &report.all;
                let totals = &view.totals;
                lines.push(format!("Estimated cost: {}", format_cost(totals.cost)));
                lines.push(format!(
                    "Processed: {}",
                    format_tokens(totals.tokens.processed())
                ));
                lines.push(format!(
                    "Cached input: {}",
                    format_tokens(totals.tokens.cache_read)
                ));
                lines.push(format!(
                    "Uncached input: {}",
                    format_tokens(totals.tokens.uncached_input)
                ));
                lines.push(format!(
                    "Cache writes: {}",
                    format_tokens(totals.tokens.cache_write)
                ));
                lines.push(format!("Output: {}", format_tokens(totals.tokens.output)));
                if totals.unpriced_tokens > 0 {
                    lines.push(format!(
                        "+ {} tokens on unpriced models",
                        format_tokens(totals.unpriced_tokens)
                    ));
                }

                lines.push("By harness".into());
                for backend in ChatBackend::ALL {
                    let bucket = &view.by_harness[gitterm_core::usage::harness_index(backend)];
                    lines.push(bucket_line(backend.label(), bucket));
                }

                lines.push("By model".into());
                if view.models.is_empty() {
                    lines.push("No usage in this window.".into());
                }
                for model in &view.models {
                    let label = if model.priced {
                        model.model.clone()
                    } else {
                        format!("{} (unpriced)", model.model)
                    };
                    lines.push(format!(
                        "{label} · {} · {}",
                        model.harness.label(),
                        bucket_line_values(&model.usage)
                    ));
                }

                lines.push("By day".into());
                for day in &view.days {
                    let buckets = ChatBackend::ALL
                        .iter()
                        .map(|backend| {
                            let bucket =
                                &day.by_harness[gitterm_core::usage::harness_index(*backend)];
                            format!("{} {}", backend.label(), bucket_line_values(bucket))
                        })
                        .collect::<Vec<_>>()
                        .join(" · ");
                    lines.push(format!("{} · {buckets}", day.date));
                }

                lines.push("By repo".into());
                if view.repos.is_empty() {
                    lines.push("No usage in this window.".into());
                }
                for repo in &view.repos {
                    let name = if repo.resolved {
                        repo.name.clone()
                    } else {
                        format!("{} (no repo)", repo.name)
                    };
                    lines.push(format!("{name} · {}", bucket_line_values(&repo.usage)));
                }

                lines.push(format!(
                    "{} transcripts · {} responses · scanned in {:.1}s",
                    report.stats.files_seen - report.stats.files_outside_window,
                    format_tokens(report.stats.responses),
                    report.stats.elapsed.as_secs_f64()
                ));
                if report.headless_responses > 0 {
                    lines.push(format!(
                        "{} headless responses (SDK, exec, harness subagents) included",
                        format_tokens(report.headless_responses)
                    ));
                }
                if report.stats.files_unreadable > 0 {
                    lines.push(format!(
                        "{} transcripts could not be read (see log)",
                        report.stats.files_unreadable
                    ));
                }
            }
        }
        lines
    }
}

impl EventEmitter<UsageIntent> for UsagePage {}

impl Render for UsagePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::Palette::current(cx);
        let mut body = div()
            .id("usage-page")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .bg(rgb(p.base))
            .text_color(rgb(p.text));
        let selected = match &self.state {
            UsageState::Report(report) => report.window,
            _ => UsageWindow::default(),
        };
        let mut selector = div().flex().flex_row().gap_2();
        for window in UsageWindow::ALL {
            let active = selected == window;
            selector = selector.child(
                div()
                    .id(format!("usage-window-{}", window.label()))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(if active { p.surface } else { p.mantle }))
                    .text_color(rgb(if active { p.text } else { p.muted }))
                    .cursor_pointer()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(UsageIntent::SelectWindow(window));
                    }))
                    .child(window.label()),
            );
        }
        body = body.child(
            div()
                .text_xl()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child("Usage"),
        );
        body = body.child(selector);
        for line in self.display_lines().into_iter().skip(1) {
            body = body.child(div().child(line));
        }
        body
    }
}

fn bucket_line(label: &str, bucket: &Bucket) -> String {
    format!("{label} · {}", bucket_line_values(bucket))
}

fn bucket_line_values(bucket: &Bucket) -> String {
    let cost = if bucket.cost == 0.0 && bucket.unpriced_tokens > 0 {
        "unpriced".to_string()
    } else {
        format_cost(bucket.cost)
    };
    format!(
        "{cost} · {} tokens",
        format_tokens(bucket.tokens.processed())
    )
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
