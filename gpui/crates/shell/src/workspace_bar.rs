//! Standalone Iced workspace bar. The caller projects its catalog and handles
//! outcomes; rendering never selects, persists, launches or closes anything.
//!
//! Hookup: replace the plain bottom bar with `render(&input, Colors::new(dark),
//! emit)`. Project the owner roster in catalog order, using
//! `work::catalog::is_work` for BarChip::work. Resolve outcome IDs against the
//! current owner; selection uses the existing selection reducer, NewWork opens
//! the named-work sheet, and OpenReposMenu toggles the host's Repos popup.
//! Native GPUI tooltips replace Iced's delayed overlay hints; their placement
//! and rasterization are framework-specific. No shell hookup is installed here.
use std::rc::Rc;

use gitterm_core::{park::WakeReason, settings::WorkspaceColor, workspace};
use gpui::{App, Div, SharedString, Stateful, Window, div, prelude::*, px, rgb};
use gpui_component::{
    button::{Button, ButtonVariants},
    popover::Popover,
    tooltip::Tooltip,
};

use crate::data::WorkspaceId;

/// A fresh, read-only projection of Iced's saved PR and parked facts. The
/// catalog retains only open workspaces, so closed entries come from its source.
#[derive(Default)]
pub(crate) struct SavedFacts {
    pub ready: usize,
    pub news_count: usize,
    pub parked: usize,
    pub news: std::collections::HashMap<WorkspaceId, News>,
    pub machines: Vec<News>,
}
impl SavedFacts {
    pub fn read(
        catalog: &crate::workspace_config::IcedCatalog,
        roots: &std::collections::HashMap<WorkspaceId, Vec<String>>,
        colors: Colors,
    ) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        use gitterm_core::{chat_facts, chats::ChatBackend, merge_plan, prs};
        use std::collections::{HashMap, HashSet};
        let path = catalog.source.join("workspaces.json");
        let file: gitterm_core::workspace_config::WorkspacesFile = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("parse {}", path.display()))?;
        let watch = prs::WatchFile::load(&prs::WatchFile::path_in(&catalog.source));
        let plans = merge_plan::load_plans(&merge_plan::plans_dir(&catalog.source));
        let briefs: HashMap<_, _> = watch
            .prs
            .iter()
            .filter_map(|(id, pr)| Some((id.clone(), pr.brief()?)))
            .collect();
        let mut result = Self {
            parked: file.workspaces.iter().filter(|w| w.is_parked()).count(),
            ..Default::default()
        };
        result.machines = file
            .remote_agents
            .iter()
            .map(|machine| News {
                label: format!("{} · not connected in preview", machine.name),
                color: colors.muted,
            })
            .collect();
        let homes = catalog
            .workspaces
            .iter()
            .enumerate()
            .map(|(i, w)| (Some(WorkspaceId(i as u64 + 1)), w))
            .chain(
                file.workspaces
                    .iter()
                    .filter(|w| w.is_parked())
                    .map(|w| (None, w)),
            );
        for (id, saved) in homes {
            if saved.is_legacy_remote()
                || matches!(
                    saved.location,
                    Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
                )
            {
                continue;
            }
            let mut home_roots = saved.chats.clone();
            if let Some(extra) = id.and_then(|id| roots.get(&id)) {
                home_roots.extend(extra.iter().cloned());
            }
            home_roots.extend(saved.tabs.iter().filter_map(|t| t.chat_session_id.clone()));
            let mut seen_roots = HashSet::new();
            let mut seen_prs = HashSet::new();
            let mut newest = None;
            for root in home_roots
                .into_iter()
                .filter(|root| seen_roots.insert(root.clone()))
            {
                if let Some(plan) = plans.get(&root) {
                    result.ready += usize::from(
                        merge_plan::next_to_merge(&merge_plan::order_plan(plan, &briefs)).is_some(),
                    );
                }
                let mut keys: Vec<_> = plans
                    .get(&root)
                    .into_iter()
                    .flat_map(|plan| plan.entries.iter().map(|entry| entry.key()))
                    .collect();
                let mut pending = vec![root];
                let mut seen_chats = HashSet::new();
                while let Some(chat) = pending.pop() {
                    if !seen_chats.insert(chat.clone()) {
                        continue;
                    }
                    for backend in ChatBackend::ALL {
                        let path = chat_facts::facts_path(
                            &chat_facts::facts_dir(&catalog.source),
                            backend,
                            &chat,
                        )
                        .map_err(anyhow::Error::msg)?;
                        if let Some(facts) =
                            chat_facts::load_facts(&path).map_err(anyhow::Error::msg)?
                        {
                            pending
                                .extend(facts.children.into_iter().map(|child| child.session_id));
                            keys.extend(facts.prs.iter().map(|pr| pr.key()));
                        }
                    }
                }
                for key in keys.into_iter().filter(|key| seen_prs.insert(key.id())) {
                    if let Some(pr) = watch.prs.get(&key.id()) {
                        for news in pr.open_news() {
                            result.news_count += 1;
                            if newest
                                .as_ref()
                                .is_none_or(|(_, old): &(_, &prs::NewsItem)| news.at > old.at)
                            {
                                newest = Some((pr.key.number, news));
                            }
                        }
                    }
                }
            }
            if let (Some(id), Some((number, news))) = (id, newest) {
                result.news.insert(
                    id,
                    News {
                        label: format!("#{number} {}", news.text),
                        color: match news.kind {
                            prs::NewsKind::ChecksFailed
                            | prs::NewsKind::Conflicting
                            | prs::NewsKind::Closed => colors.red,
                            prs::NewsKind::RequiredChecksPassed | prs::NewsKind::Merged => {
                                colors.green
                            }
                            prs::NewsKind::NewActivity => colors.blue,
                        },
                    },
                );
            }
        }
        Ok(result)
    }
}

pub type Handler<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;

/// Resolved Iced colours, also used by the adjacent strip and spine.
#[derive(Clone, Copy, Debug)]
pub struct Colors {
    pub dark: bool,
    pub base: u32,
    pub crust: u32,
    pub surface0: u32,
    pub surface2: u32,
    pub text: u32,
    pub subtext0: u32,
    pub muted: u32,
    pub overlay0: u32,
    pub peach: u32,
    pub yellow: u32,
    pub red: u32,
    pub green: u32,
    pub blue: u32,
}
impl Colors {
    pub fn new(dark: bool) -> Self {
        let p = crate::theme::Palette::for_mode(dark);
        Self {
            dark,
            base: p.base,
            crust: p.crust,
            surface0: p.surface,
            surface2: p.surface2,
            text: p.text,
            subtext0: p.subtext0,
            muted: p.muted,
            overlay0: p.overlay0,
            peach: p.peach,
            yellow: p.yellow,
            red: p.red,
            green: p.green,
            blue: p.blue,
        }
    }
    pub fn workspace(self, color: WorkspaceColor) -> u32 {
        let p = crate::theme::Palette::for_mode(self.dark);
        match color {
            WorkspaceColor::Lavender => p.lavender,
            WorkspaceColor::Blue => p.blue,
            WorkspaceColor::Green => p.green,
            WorkspaceColor::Peach => p.peach,
            WorkspaceColor::Pink => p.pink,
            WorkspaceColor::Yellow => p.yellow,
            WorkspaceColor::Red => p.red,
            WorkspaceColor::Teal => p.teal,
        }
    }
}

/// Presentation facts for attention still owned by the Iced/root host.
/// The reason is supplied verbatim (including review/consult/worker wording).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attention {
    pub count: usize,
    pub kind: AttentionKind,
    pub reason: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionKind {
    NeedsYou,
    Failed,
    Review,
}
impl AttentionKind {
    pub fn color(self, colors: Colors) -> u32 {
        match self {
            Self::NeedsYou => colors.peach,
            Self::Failed => colors.red,
            Self::Review => colors.green,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct News {
    pub label: String,
    pub color: u32,
}
#[derive(Clone, Debug)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub color: WorkspaceColor,
    /// Use core work::catalog::is_work for `work`, and the host's machine key.
    pub placement: workspace::BarChip,
    pub attention: Option<Attention>,
    pub woke: Option<WakeReason>,
    pub pr_news: Option<News>,
}
impl Workspace {
    pub fn label(&self, active: bool) -> String {
        if active {
            self.name.clone()
        } else {
            workspace::short_label(&self.name)
        }
    }
    pub fn attention(&self) -> Option<&Attention> {
        self.attention.as_ref().filter(|a| a.count > 0)
    }
    pub fn wake_label(&self, active: bool) -> Option<&'static str> {
        self.woke.filter(|_| !active).map(|reason| match reason {
            WakeReason::NeedsYou => "needs you",
            WakeReason::Finished => "finished",
            WakeReason::Failed => "failed",
        })
    }
    pub fn dot_color(&self, active: bool, pulse_bright: bool, c: Colors) -> u32 {
        match self.attention() {
            Some(a) if a.kind != AttentionKind::NeedsYou => a.kind.color(c),
            Some(_) if pulse_bright => c.peach,
            Some(_) => c.yellow,
            None if active => c.workspace(self.color),
            None => c.surface2,
        }
    }
    pub fn hint(&self) -> String {
        let mut parts = vec![self.name.clone()];
        if let Some(a) = self.attention() {
            parts.push(format!("{} · {}", a.count, a.reason));
        }
        if let Some(news) = &self.pr_news {
            parts.push(news.label.clone());
        }
        parts.join(" · ")
    }
}

#[derive(Clone, Debug)]
pub struct Input {
    /// Catalog order, never display-sorted by the host.
    pub workspaces: Vec<Workspace>,
    pub active: Option<WorkspaceId>,
    pub repos_on_bar: bool,
    pub repos_open: bool,
    pub machine_news: Vec<News>,
    pub pulse_bright: bool,
    pub ready_count: usize,
    pub ready_news_count: usize,
    pub parked_count: usize,
    pub terminal_expanded: bool,
    pub terminal_running: bool,
    pub browser: Browser,
    pub more_open: bool,
}
#[derive(Clone, Debug)]
pub struct Browser {
    pub color: u32,
    pub driving: bool,
    pub hint: String,
    /// None while busy or unavailable; otherwise Open or Focus, per host status.
    pub action: Option<BrowserAction>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserAction {
    Open,
    Focus,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    SelectWorkspace(WorkspaceId),
    OpenSettings(WorkspaceId),
    OpenReposMenu,
    NewWork,
    OpenReady,
    OpenParked,
    ToggleTerminal,
    Browser(BrowserAction),
    OpenMoreMenu,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Workspace(usize),
    Repos,
    NewWork,
}
impl Input {
    pub fn layout(&self) -> workspace::BarLayout {
        let chips: Vec<_> = self
            .workspaces
            .iter()
            .map(|w| w.placement.clone())
            .collect();
        let active = self
            .workspaces
            .iter()
            .position(|w| Some(w.id) == self.active)
            .unwrap_or(usize::MAX);
        workspace::bar_layout(&chips, active, self.repos_on_bar)
    }
    pub fn items(&self) -> Vec<Item> {
        let layout = self.layout();
        layout
            .work
            .into_iter()
            .map(Item::Workspace)
            .chain([Item::Repos])
            .chain(layout.after.into_iter().map(Item::Workspace))
            .chain([Item::NewWork])
            .collect()
    }
    pub fn hidden_news(&self) -> bool {
        self.layout().hidden.into_iter().any(|i| {
            let w = &self.workspaces[i];
            w.attention().is_some() || w.pr_news.is_some()
        })
    }
    pub fn terminal_color(&self, c: Colors) -> u32 {
        if self.terminal_expanded {
            c.blue
        } else if self.terminal_running {
            c.green
        } else {
            c.muted
        }
    }
}

pub(crate) fn dot(color: u32, width: f32, height: f32) -> Div {
    div()
        .flex_none()
        .w(px(width))
        .h(px(height))
        .rounded(px(3.))
        .bg(rgb(color))
}
pub(crate) fn control<T: Clone + 'static>(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    color: u32,
    colors: Colors,
    outcome: T,
    emit: Handler<T>,
) -> Stateful<Div> {
    let hint = hint.into();
    div()
        .id(id.into())
        .role(gpui::Role::Button)
        .aria_label(hint.clone())
        .font_family(crate::theme::LABEL_FONT)
        .text_size(px(11.))
        .text_color(rgb(color))
        .px(px(8.))
        .py(px(4.))
        .rounded(px(4.))
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(colors.surface0)).text_color(rgb(colors.text)))
        .tooltip(move |window, cx| {
            Tooltip::new(hint.clone())
                .font_family(crate::theme::LABEL_FONT)
                .build(window, cx)
        })
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            emit(outcome.clone(), window, cx);
        })
        .child(label.into())
}

/// 28px including its top separator; fixed controls stay outside the scroller.
pub fn render(input: &Input, colors: Colors, emit: Handler<Outcome>) -> gpui::Stateful<gpui::Div> {
    let mut chips = div()
        .id("pstrip-workspace-scroll")
        .debug_selector(|| "pstrip-workspace-scroll".into())
        .flex()
        .items_center()
        .flex_1()
        .min_w_0()
        .overflow_x_scroll()
        .px(px(4.));
    for (position, item) in input.items().into_iter().enumerate() {
        if position > 0 && item != Item::NewWork {
            chips = chips.child(
                div()
                    .flex_none()
                    .w(px(1.))
                    .h(px(14.))
                    .bg(rgb(colors.surface0)),
            );
        }
        match item {
            Item::Workspace(index) => {
                let w = &input.workspaces[index];
                let active = input.active == Some(w.id);
                let id = w.id;
                let on_select = emit.clone();
                let hint = w.hint();
                let mut chip = div()
                    .id(SharedString::from(format!("pstrip-workspace-{}", id.0)))
                    .debug_selector(move || format!("pstrip-workspace-{}", id.0))
                    .role(gpui::Role::Tab)
                    .aria_label(w.name.clone())
                    .aria_selected(active)
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(6.))
                    .px(px(12.))
                    .h(px(27.))
                    .border_t_2()
                    .border_color(if active {
                        rgb(colors.workspace(w.color)).into()
                    } else {
                        gpui::transparent_black()
                    })
                    .when(active, |s| s.bg(rgb(colors.base)))
                    .when(!active, |s| s.hover(move |s| s.bg(rgb(colors.surface0))))
                    .cursor_pointer()
                    .tooltip(move |window, cx| {
                        Tooltip::new(hint.clone())
                            .font_family(crate::theme::LABEL_FONT)
                            .build(window, cx)
                    })
                    .on_click(move |_, window, cx| {
                        on_select(Outcome::SelectWorkspace(id), window, cx)
                    })
                    .child(dot(w.dot_color(active, input.pulse_bright, colors), 6., 6.))
                    .child(
                        div()
                            .text_color(rgb(if active {
                                colors.workspace(w.color)
                            } else {
                                colors.subtext0
                            }))
                            .child(w.label(active)),
                    );
                if active {
                    chip = chip.child(
                        control(
                            format!("pstrip-settings-{}", id.0),
                            "⚙",
                            "Workspace settings",
                            colors.muted,
                            colors,
                            Outcome::OpenSettings(id),
                            emit.clone(),
                        )
                        .text_size(px(10.))
                        .px(px(4.))
                        .py_0()
                        .debug_selector(|| "pstrip-settings".into()),
                    );
                }
                if let Some(a) = w.attention() {
                    chip = chip.child(
                        div()
                            .text_size(px(9.))
                            .text_color(rgb(colors.crust))
                            .bg(rgb(a.kind.color(colors)))
                            .rounded(px(6.))
                            .px(px(4.))
                            .child(a.count.to_string())
                            .debug_selector(move || format!("pstrip-attention-{}", id.0)),
                    );
                }
                if let Some(label) = w.wake_label(active) {
                    let color = match w.woke {
                        Some(WakeReason::Failed) => colors.red,
                        Some(WakeReason::Finished) => colors.green,
                        _ => colors.peach,
                    };
                    chip = chip.child(
                        div()
                            .text_size(px(9.))
                            .text_color(rgb(color))
                            .child(label)
                            .debug_selector(move || format!("pstrip-wake-{}", id.0)),
                    );
                }
                if let Some(news) = &w.pr_news {
                    chip = chip.child(
                        dot(news.color, 6., 6.)
                            .debug_selector(move || format!("pstrip-pr-news-{}", id.0)),
                    );
                }
                chips = chips.child(chip);
            }
            Item::Repos => {
                let mut repos = Button::new("pstrip-repos")
                    .ghost()
                    .label("Repos ▾")
                    .tooltip("Repos and machines")
                    .font_family(crate::theme::LABEL_FONT)
                    .text_size(px(11.))
                    .text_color(rgb(colors.muted))
                    .h(px(27.))
                    .px(px(12.))
                    .flex_none()
                    .debug_selector(|| "pstrip-repos".into());
                if input.hidden_news() {
                    repos = repos.child(
                        dot(colors.peach, 6., 6.).debug_selector(|| "pstrip-repos-news".into()),
                    );
                }
                for news in &input.machine_news {
                    repos = repos.child(dot(news.color, 5., 5.));
                }
                let menu_input = input.clone();
                let menu_emit = emit.clone();
                chips = chips.child(
                    Popover::new("pstrip-repos-menu")
                        .anchor(gpui::Anchor::BottomLeft)
                        .trigger(repos)
                        .content(move |_, _, cx| {
                            let mut menu = div()
                                .id("pstrip-repos-list")
                                .debug_selector(|| "pstrip-repos-list".into())
                                .flex()
                                .flex_col()
                                .w(px(260.))
                                .max_h(px(360.))
                                .overflow_y_scroll()
                                .font_family(crate::theme::LABEL_FONT)
                                .text_size(px(11.));
                            for section in menu_input.layout().sections {
                                menu = menu.child(
                                    div()
                                        .px(px(8.))
                                        .py(px(4.))
                                        .text_color(rgb(colors.muted))
                                        .child(if section.key == workspace::LOCAL_GROUP {
                                            "This Mac".to_string()
                                        } else {
                                            section.key
                                        }),
                                );
                                for index in section.workspaces {
                                    let workspace = &menu_input.workspaces[index];
                                    let id = workspace.id;
                                    let weak = cx.entity().downgrade();
                                    let emit = menu_emit.clone();
                                    menu = menu.child(
                                        control(
                                            format!("pstrip-repo-{}", id.0),
                                            workspace.name.clone(),
                                            workspace.hint(),
                                            colors.text,
                                            colors,
                                            Outcome::SelectWorkspace(id),
                                            Rc::new(move |outcome, window, cx| {
                                                let _ = weak.update(cx, |state, cx| {
                                                    state.dismiss(window, cx)
                                                });
                                                emit(outcome, window, cx);
                                            }),
                                        )
                                        .debug_selector(move || format!("pstrip-repo-{}", id.0)),
                                    );
                                }
                            }
                            menu.child(
                                div()
                                    .px(px(8.))
                                    .py(px(6.))
                                    .text_color(rgb(colors.muted))
                                    .child("Manage repos · not available in preview"),
                            )
                        }),
                );
            }
            Item::NewWork => {
                chips = chips.child(
                    control(
                        "pstrip-new-work",
                        "+",
                        "New workspace",
                        colors.muted,
                        colors,
                        Outcome::NewWork,
                        emit.clone(),
                    )
                    .flex_none()
                    .debug_selector(|| "pstrip-new-work".into()),
                )
            }
        }
    }
    let mut right = div().flex().flex_none().items_center();
    if input.ready_count > 0 || input.ready_news_count > 0 {
        right = right.child(
            control(
                "pstrip-ready",
                format!("↑{}", input.ready_count),
                if input.ready_news_count > 0 {
                    format!(
                        "{} to merge · {} new",
                        input.ready_count, input.ready_news_count
                    )
                } else {
                    format!("{} to merge", input.ready_count)
                },
                if input.ready_news_count > 0 {
                    colors.blue
                } else {
                    colors.muted
                },
                colors,
                Outcome::OpenReady,
                emit.clone(),
            )
            .debug_selector(|| "pstrip-ready".into()),
        );
    }
    if input.parked_count > 0 {
        right = right.child(
            control(
                "pstrip-parked",
                format!("⏸{}", input.parked_count),
                format!("{} parked", input.parked_count),
                colors.muted,
                colors,
                Outcome::OpenParked,
                emit.clone(),
            )
            .debug_selector(|| "pstrip-parked".into()),
        );
    }
    right = right
        .child(div().w(px(1.)).h(px(16.)).bg(rgb(colors.surface0)))
        .child(
            control(
                "pstrip-terminal",
                ">_",
                match (input.terminal_expanded, input.terminal_running) {
                    (true, _) => "Fold the terminal panel (⌘J)",
                    (false, true) => "Terminal panel, a command is running (⌘J)",
                    (false, false) => "Terminal panel (⌘J)",
                },
                input.terminal_color(colors),
                colors,
                Outcome::ToggleTerminal,
                emit.clone(),
            )
            .debug_selector(|| "pstrip-terminal".into()),
        );
    let browser = div()
        .w(px(13.))
        .h(px(10.))
        .border_1()
        .border_color(rgb(input.browser.color))
        .rounded(px(2.))
        .when(input.browser.driving, |s| {
            s.bg(gpui::rgba((input.browser.color << 8) | 0x59))
        })
        .child(div().h(px(2.)).w_full().bg(rgb(input.browser.color)));
    let browser_hint = input.browser.hint.clone();
    let mut browser_button = div()
        .id("pstrip-browser")
        .role(gpui::Role::Button)
        .aria_label(browser_hint.clone())
        .debug_selector(|| "pstrip-browser".into())
        .px(px(7.))
        .py(px(7.))
        .rounded(px(4.))
        .hover(move |s| s.bg(rgb(colors.surface0)))
        .tooltip(move |window, cx| Tooltip::new(browser_hint.clone()).build(window, cx))
        .child(browser);
    if let Some(action) = input.browser.action {
        let emit = emit.clone();
        browser_button = browser_button
            .cursor_pointer()
            .on_click(move |_, window, cx| emit(Outcome::Browser(action), window, cx));
    }
    right = right.child(browser_button).pr(px(4.));
    div()
        .id("pstrip-workspace-bar")
        .debug_selector(|| "pstrip-workspace-bar".into())
        .whitespace_nowrap()
        .line_height(gpui::relative(crate::theme::LABEL_LINE_HEIGHT))
        .flex()
        .items_center()
        .w_full()
        .h(px(28.))
        .flex_none()
        .font_family(crate::theme::LABEL_FONT)
        .text_size(px(11.))
        .bg(rgb(colors.crust))
        .border_t_1()
        .border_color(rgb(colors.surface0))
        .child(chips)
        .child(right)
}

#[cfg(test)]
fn right_side_labels(input: &Input) -> Vec<String> {
    let mut labels = Vec::new();
    if input.ready_count > 0 || input.ready_news_count > 0 {
        labels.push(format!("↑{}", input.ready_count));
    }
    if input.parked_count > 0 {
        labels.push(format!("⏸{}", input.parked_count));
    }
    labels.extend([">_".to_string(), "Browser".to_string()]);
    labels
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui::{Context, Render, TestAppContext, VisualTestContext};
    use std::cell::RefCell;

    pub(crate) fn workspace(
        id: u64,
        name: &str,
        group: &str,
        work: bool,
        parked: bool,
    ) -> Workspace {
        Workspace {
            id: WorkspaceId(id),
            name: name.into(),
            color: WorkspaceColor::Lavender,
            placement: workspace::BarChip {
                group: group.into(),
                work,
                parked,
            },
            attention: None,
            woke: None,
            pr_news: None,
        }
    }
    pub(crate) fn input() -> Input {
        Input {
            workspaces: vec![
                workspace(1, "Folder", "local", false, false),
                workspace(2, "Docs and the release", "local", true, false),
                workspace(3, "Remote", "agent:mac", true, false),
                workspace(4, "Parked", "local", true, true),
            ],
            active: Some(WorkspaceId(2)),
            repos_on_bar: false,
            repos_open: false,
            machine_news: vec![],
            pulse_bright: false,
            ready_count: 0,
            ready_news_count: 0,
            parked_count: 0,
            terminal_expanded: false,
            terminal_running: false,
            browser: Browser {
                color: 0x9399b2,
                driving: false,
                hint: "Open browser".into(),
                action: Some(BrowserAction::Open),
            },
            more_open: false,
        }
    }
    #[test]
    fn labels_and_badges_follow_iced_rules() {
        let mut w = workspace(1, "Docs and the release", "local", true, false);
        assert_eq!(w.label(true), "Docs and the release");
        assert_eq!(w.label(false), "Docs release");
        assert_eq!(
            workspace(1, "  api  ", "local", true, false).label(false),
            "api"
        );
        assert_eq!(
            workspace(1, "Internationalisation rollout", "local", true, false).label(false),
            "Internationalisat…"
        );
        w.attention = Some(Attention {
            count: 0,
            kind: AttentionKind::Failed,
            reason: "Agent failed".into(),
        });
        let c = Colors::new(true);
        assert!(w.attention().is_none());
        assert_eq!(w.dot_color(false, false, c), c.surface2);
        w.attention.as_mut().unwrap().count = 2;
        assert_eq!(w.dot_color(false, false, c), c.red);
        w.attention.as_mut().unwrap().kind = AttentionKind::Review;
        assert_eq!(w.dot_color(false, false, c), c.green);
        w.attention.as_mut().unwrap().kind = AttentionKind::NeedsYou;
        assert_eq!(w.dot_color(false, false, c), c.yellow);
        assert_eq!(w.dot_color(false, true, c), c.peach);
        for (reason, label) in [
            (WakeReason::NeedsYou, "needs you"),
            (WakeReason::Finished, "finished"),
            (WakeReason::Failed, "failed"),
        ] {
            w.woke = Some(reason);
            assert_eq!(w.wake_label(false), Some(label));
            assert_eq!(w.wake_label(true), None);
        }
        w.pr_news = Some(News {
            label: "#841 checks failed".into(),
            color: c.red,
        });
        assert!(w.hint().contains("#841 checks failed"));
        assert!(w.hint().contains("Agent failed"));
    }
    #[test]
    fn core_orders_work_repos_folders_and_pinned_remote() {
        let mut i = input();
        assert_eq!(
            i.items(),
            vec![Item::Workspace(1), Item::Repos, Item::NewWork]
        );
        i.repos_on_bar = true;
        i.active = Some(WorkspaceId(3));
        assert_eq!(
            i.items(),
            vec![
                Item::Workspace(1),
                Item::Repos,
                Item::Workspace(0),
                Item::Workspace(2),
                Item::NewWork
            ]
        );
        let indices: Vec<_> = i
            .items()
            .into_iter()
            .filter_map(|i| match i {
                Item::Workspace(index) => Some(index),
                _ => None,
            })
            .collect();
        assert_eq!(indices, workspace::visible_order(&i.layout()));
        i.workspaces[0].pr_news = Some(News {
            label: "PR merged".into(),
            color: 0,
        });
        assert!(!i.hidden_news(), "visible news stays on its chip");
        i.repos_on_bar = false;
        assert!(i.hidden_news());
        i.workspaces[0].pr_news = None;
        assert!(!i.hidden_news());
        i.workspaces[3].attention = Some(Attention {
            count: 1,
            kind: AttentionKind::NeedsYou,
            reason: "Input".into(),
        });
        assert!(!i.hidden_news(), "parked work is omitted entirely");
    }
    #[test]
    fn right_controls_match_iced_order_and_hide_zero_counts() {
        let mut i = input();
        i.ready_count = 2;
        i.parked_count = 16;
        assert_eq!(right_side_labels(&i), ["↑2", "⏸16", ">_", "Browser"]);
        i.ready_count = 0;
        i.parked_count = 0;
        i.ready_news_count = 0;
        assert_eq!(right_side_labels(&i), [">_", "Browser"]);
    }
    pub(crate) fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
    }
    pub(crate) fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing {selector}"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        draw(cx);
    }
    struct View {
        input: Input,
        events: Rc<RefCell<Vec<Outcome>>>,
    }
    impl Render for View {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let events = self.events.clone();
            render(
                &self.input,
                Colors::new(true),
                Rc::new(move |outcome, _, _| events.borrow_mut().push(outcome)),
            )
        }
    }
    #[gpui::test]
    fn overflowing_chips_scroll_without_shrinking_and_right_controls_stay_pinned(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let mut input = input();
        input.workspaces = (1..=30)
            .map(|id| workspace(id, "A long workspace name", "local", true, false))
            .collect();
        input.ready_count = 2;
        input.parked_count = 16;
        let (_, cx) = cx.add_window_view(|_, _| View {
            input,
            events: Rc::new(RefCell::new(vec![])),
        });
        cx.simulate_resize(gpui::size(px(1400.), px(800.)));
        draw(cx);
        let chip_width = cx.debug_bounds("pstrip-workspace-1").unwrap().size.width;
        let ready_width = cx.debug_bounds("pstrip-ready").unwrap().size.width;
        for width in [320., 640., 1400.] {
            cx.simulate_resize(gpui::size(px(width), px(800.)));
            draw(cx);
            let bar = cx.debug_bounds("pstrip-workspace-bar").unwrap();
            let scroll = cx.debug_bounds("pstrip-workspace-scroll").unwrap();
            let ready = cx.debug_bounds("pstrip-ready").unwrap();
            let terminal = cx.debug_bounds("pstrip-terminal").unwrap();
            let browser = cx.debug_bounds("pstrip-browser").unwrap();
            assert_eq!(
                cx.debug_bounds("pstrip-workspace-1").unwrap().size.width,
                chip_width
            );
            assert_eq!(ready.size.width, ready_width);
            assert!(scroll.right() <= ready.left());
            assert!(ready.left() >= bar.left() && browser.right() <= bar.right());
            let terminal_before = terminal;
            cx.simulate_event(gpui::ScrollEvent {
                position: scroll.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-10000.))),
                modifiers: Default::default(),
                touch_phase: gpui::TouchPhase::Moved,
                source: gpui::ScrollSource::Pointer,
            });
            draw(cx);
            let plus = cx.debug_bounds("pstrip-new-work").unwrap();
            assert!(plus.left() >= scroll.left() && plus.right() <= scroll.right());
            assert_eq!(cx.debug_bounds("pstrip-terminal").unwrap(), terminal_before);
            assert!(cx.debug_bounds("pstrip-more").is_none());
        }
    }
    #[gpui::test]
    fn every_bar_control_emits_typed_outcome(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let events = Rc::new(RefCell::new(vec![]));
        let mut input = input();
        input.ready_count = 3;
        input.parked_count = 2;
        input.repos_on_bar = true;
        input.workspaces[0].attention = Some(Attention {
            count: 2,
            kind: AttentionKind::NeedsYou,
            reason: "Input or approval needed".into(),
        });
        input.workspaces[0].woke = Some(WakeReason::Finished);
        input.workspaces[0].pr_news = Some(News {
            label: "#841 merged".into(),
            color: 0xa6e3a1,
        });
        input.workspaces[1].woke = Some(WakeReason::NeedsYou);
        input.workspaces[2].pr_news = Some(News {
            label: "#842 failed".into(),
            color: 0xf38ba8,
        });
        let (view, cx) = cx.add_window_view(|_, _| View {
            input,
            events: events.clone(),
        });
        cx.simulate_resize(gpui::size(px(1400.), px(800.)));
        draw(cx);
        for selector in [
            "pstrip-attention-1",
            "pstrip-wake-1",
            "pstrip-pr-news-1",
            "pstrip-repos-news",
        ] {
            assert!(cx.debug_bounds(selector).is_some(), "missing {selector}");
        }
        assert!(
            cx.debug_bounds("pstrip-wake-2").is_none(),
            "active workspace hides wake reason"
        );
        for selector in [
            "pstrip-workspace-1",
            "pstrip-settings",
            "pstrip-new-work",
            "pstrip-ready",
            "pstrip-parked",
            "pstrip-terminal",
            "pstrip-browser",
        ] {
            click(cx, selector);
        }
        assert_eq!(
            *events.borrow(),
            vec![
                Outcome::SelectWorkspace(WorkspaceId(1)),
                Outcome::OpenSettings(WorkspaceId(2)),
                Outcome::NewWork,
                Outcome::OpenReady,
                Outcome::OpenParked,
                Outcome::ToggleTerminal,
                Outcome::Browser(BrowserAction::Open)
            ]
        );
        assert!(cx.debug_bounds("pstrip-more").is_none());
        click(cx, "pstrip-repos");
        assert!(cx.debug_bounds("pstrip-repos-list").is_some());
        click(cx, "pstrip-repo-1");
        assert_eq!(
            events.borrow().last(),
            Some(&Outcome::SelectWorkspace(WorkspaceId(1)))
        );
        events.borrow_mut().clear();
        view.update(cx, |v, cx| {
            v.input.browser.action = Some(BrowserAction::Focus);
            v.input.workspaces[0].attention.as_mut().unwrap().count = 0;
            v.input.workspaces[0].woke = None;
            v.input.workspaces[0].pr_news = None;
            v.input.workspaces[2].pr_news = None;
            v.input.ready_count = 0;
            v.input.parked_count = 0;
            cx.notify();
        });
        draw(cx);
        for selector in [
            "pstrip-attention-1",
            "pstrip-wake-1",
            "pstrip-pr-news-1",
            "pstrip-repos-news",
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "badge should hide: {selector}"
            );
        }
        assert!(cx.debug_bounds("pstrip-ready").is_none());
        assert!(cx.debug_bounds("pstrip-parked").is_none());
        view.update(cx, |v, cx| {
            v.input.ready_news_count = 1;
            cx.notify();
        });
        draw(cx);
        assert!(
            cx.debug_bounds("pstrip-ready").is_some(),
            "news alone shows ↑0"
        );
        click(cx, "pstrip-browser");
        assert_eq!(*events.borrow(), [Outcome::Browser(BrowserAction::Focus)]);
        assert!(cx.debug_bounds("pstrip-more").is_none());
        click(cx, "pstrip-repos");
        assert!(cx.debug_bounds("pstrip-repos-list").is_some());
        click(cx, "pstrip-repo-1");
        assert_eq!(
            events.borrow().last(),
            Some(&Outcome::SelectWorkspace(WorkspaceId(1)))
        );
        events.borrow_mut().clear();
        view.update(cx, |v, cx| {
            v.input.browser.action = None;
            cx.notify();
        });
        draw(cx);
        click(cx, "pstrip-browser");
        assert!(events.borrow().is_empty());
    }
}
