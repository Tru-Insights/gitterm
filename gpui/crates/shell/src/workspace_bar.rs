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
use gpui_component::tooltip::Tooltip;

use crate::data::WorkspaceId;

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
        if dark {
            Self {
                dark,
                base: 0x1e1e2e,
                crust: 0x11111b,
                surface0: 0x313244,
                surface2: 0x585b70,
                text: 0xcdd6f4,
                subtext0: 0xa6adc8,
                muted: 0x9399b2,
                overlay0: 0x6c7086,
                peach: 0xfab387,
                yellow: 0xf9e2af,
                red: 0xf38ba8,
                green: 0xa6e3a1,
                blue: 0x89b4fa,
            }
        } else {
            Self {
                dark,
                base: 0xeff1f5,
                crust: 0xdce0e8,
                surface0: 0xccd0da,
                surface2: 0xacb0be,
                text: 0x4c4f69,
                subtext0: 0x6c6f85,
                muted: 0x7c7f93,
                overlay0: 0x9ca0b0,
                peach: 0xfe640b,
                yellow: 0xdf8e1d,
                red: 0xd20f39,
                green: 0x40a02b,
                blue: 0x1e66f5,
            }
        }
    }
    pub fn workspace(self, color: WorkspaceColor) -> u32 {
        match color {
            WorkspaceColor::Lavender => {
                if self.dark {
                    0xb4befe
                } else {
                    0x7287fd
                }
            }
            WorkspaceColor::Blue => self.blue,
            WorkspaceColor::Green => self.green,
            WorkspaceColor::Peach => self.peach,
            WorkspaceColor::Pink => {
                if self.dark {
                    0xf5c2e7
                } else {
                    0xea76cb
                }
            }
            WorkspaceColor::Yellow => self.yellow,
            WorkspaceColor::Red => self.red,
            WorkspaceColor::Teal => {
                if self.dark {
                    0x94e2d5
                } else {
                    0x179299
                }
            }
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
        .font_family("Menlo")
        .text_size(px(11.))
        .text_color(rgb(color))
        .px(px(8.))
        .py(px(4.))
        .rounded(px(4.))
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(colors.surface0)).text_color(rgb(colors.text)))
        .tooltip(move |window, cx| {
            Tooltip::new(hint.clone())
                .font_family("Menlo")
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
                            .font_family("Menlo")
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
                let mut repos = control(
                    "pstrip-repos",
                    if input.repos_open {
                        "Repos ▴"
                    } else {
                        "Repos ▾"
                    },
                    std::iter::once("Repos and machines".to_string())
                        .chain(input.machine_news.iter().map(|news| news.label.clone()))
                        .chain(input.hidden_news().then(|| "news inside".to_string()))
                        .collect::<Vec<_>>()
                        .join(" · "),
                    colors.muted,
                    colors,
                    Outcome::OpenReposMenu,
                    emit.clone(),
                )
                .flex()
                .items_center()
                .gap(px(5.))
                .px(px(12.))
                .flex_none()
                .debug_selector(|| "pstrip-repos".into())
                .when(input.repos_open, |s| {
                    s.bg(rgb(colors.surface0)).text_color(rgb(colors.text))
                });
                if input.hidden_news() {
                    repos = repos.child(
                        dot(colors.peach, 6., 6.).debug_selector(|| "pstrip-repos-news".into()),
                    );
                }
                for news in &input.machine_news {
                    repos = repos.child(dot(news.color, 5., 5.));
                }
                chips = chips.child(repos);
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
    right = right
        .child(browser_button)
        .child(
            control(
                "pstrip-more",
                "⋯",
                "Help and browser actions",
                if input.more_open {
                    colors.text
                } else {
                    colors.muted
                },
                colors,
                Outcome::OpenMoreMenu,
                emit,
            )
            .debug_selector(|| "pstrip-more".into()),
        )
        .pr(px(4.));
    div()
        .id("pstrip-workspace-bar")
        .debug_selector(|| "pstrip-workspace-bar".into())
        .whitespace_nowrap()
        .line_height(gpui::relative(1.3))
        .flex()
        .items_center()
        .w_full()
        .h(px(28.))
        .flex_none()
        .font_family("Menlo")
        .text_size(px(11.))
        .bg(rgb(colors.crust))
        .border_t_1()
        .border_color(rgb(colors.surface0))
        .child(chips)
        .child(right)
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
            "pstrip-repos",
            "pstrip-new-work",
            "pstrip-ready",
            "pstrip-parked",
            "pstrip-terminal",
            "pstrip-browser",
            "pstrip-more",
        ] {
            click(cx, selector);
        }
        assert_eq!(
            *events.borrow(),
            vec![
                Outcome::SelectWorkspace(WorkspaceId(1)),
                Outcome::OpenSettings(WorkspaceId(2)),
                Outcome::OpenReposMenu,
                Outcome::NewWork,
                Outcome::OpenReady,
                Outcome::OpenParked,
                Outcome::ToggleTerminal,
                Outcome::Browser(BrowserAction::Open),
                Outcome::OpenMoreMenu
            ]
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
