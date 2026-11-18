use crate::{
    data::{FileNode, SessionKind, ShellData, WorkspaceId},
    theme::{Palette, TERMINAL_FONT_SIZE},
};
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    dock::{BasePanel, Panel, PanelEvent, PanelInfo, PanelState},
    input::{Input, InputState},
};
use std::{collections::HashSet, rc::Rc};

pub const PANEL_NAME: &str = "GitTermShellPane";
#[derive(Clone, Copy)]
pub enum PaneKind {
    Terminal,
    Chat,
    Git,
    Files,
}
impl From<SessionKind> for PaneKind {
    fn from(kind: SessionKind) -> Self {
        match kind {
            SessionKind::Terminal => Self::Terminal,
            SessionKind::Chat => Self::Chat,
        }
    }
}
pub struct Pane {
    pub id: String,
    pub title: String,
    pub kind: PaneKind,
    workspace: WorkspaceId,
    data: Rc<dyn ShellData>,
    focus: FocusHandle,
    pub draft: Option<Entity<InputState>>,
    pub sample_lines: usize,
    expanded: HashSet<String>,
}
impl Pane {
    pub fn new(
        id: String,
        title: String,
        kind: PaneKind,
        workspace: WorkspaceId,
        data: Rc<dyn ShellData>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = matches!(kind, PaneKind::Chat).then(|| {
            cx.new(|cx| {
                InputState::new(window, cx).placeholder("Keep a draft while moving this pane…")
            })
        });
        Self {
            id,
            title,
            kind,
            workspace,
            data,
            focus: cx.focus_handle(),
            draft,
            sample_lines: 0,
            expanded: HashSet::from(["src".into()]),
        }
    }
    fn files(
        &self,
        nodes: Vec<FileNode>,
        parent: &str,
        depth: usize,
        p: Palette,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = vec![];
        for node in nodes {
            let path = if parent.is_empty() {
                node.name.clone()
            } else {
                format!("{parent}/{}", node.name)
            };
            let directory = !node.children.is_empty();
            let expanded = self.expanded.contains(&path);
            let label = format!(
                "{} {}",
                if directory {
                    if expanded { "⌄" } else { "›" }
                } else {
                    "·"
                },
                node.name
            );
            if directory {
                let key = path.clone();
                rows.push(
                    Button::new(SharedString::from(path.clone()))
                        .ghost()
                        .label(label)
                        .w_full()
                        .accessibility_label(format!(
                            "{} directory, {}",
                            node.name,
                            if expanded { "expanded" } else { "collapsed" }
                        ))
                        .pl(px(8. + depth as f32 * 16.))
                        .justify_start()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded.remove(&key) {
                                this.expanded.insert(key.clone());
                            }
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            } else {
                rows.push(
                    div()
                        .pl(px(16. + depth as f32 * 16.))
                        .py_1()
                        .text_color(rgb(p.muted))
                        .child(label)
                        .into_any_element(),
                );
            }
            if directory && expanded {
                rows.extend(self.files(node.children, &path, depth + 1, p, cx));
            }
        }
        rows
    }
}
impl Focusable for Pane {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.draft
            .as_ref()
            .map(|input| input.focus_handle(cx))
            .unwrap_or_else(|| self.focus.clone())
    }
}
impl EventEmitter<PanelEvent> for Pane {}
impl BasePanel for Pane {
    fn panel_name(&self) -> &'static str {
        PANEL_NAME
    }
    fn dump(&self, _: &App) -> PanelState {
        PanelState {
            panel_name: PANEL_NAME.into(),
            children: vec![],
            info: PanelInfo::panel(serde_json::json!(self.id)),
        }
    }
}
impl Panel for Pane {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.title.clone()
    }
    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}
impl Render for Pane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let body = div()
            .id("pane-body")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .bg(rgb(p.base))
            .text_color(rgb(p.text));
        match self.kind {
            PaneKind::Terminal => body
                .font_family("Menlo")
                .text_size(px(TERMINAL_FONT_SIZE))
                .child(
                    div()
                        .text_color(rgb(p.muted))
                        .child("Terminal placeholder · sample output only"),
                )
                .child(format!(
                    "~/{}  ⑂ {}",
                    self.data.status(self.workspace).repository,
                    self.data.status(self.workspace).branch
                ))
                .child(div().text_color(rgb(p.green)).child("$ cargo test"))
                .child("test result: ok. 24 passed; 0 failed")
                .child(format!(
                    "Sample output lines appended: {}",
                    self.sample_lines
                ))
                .child(
                    Button::new("sample-output")
                        .ghost()
                        .label("Append sample output")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sample_lines += 1;
                            cx.notify();
                        })),
                ),
            PaneKind::Chat => body
                .child(
                    div()
                        .text_color(rgb(p.muted))
                        .child("Native chat placeholder · sample conversation"),
                )
                .child(div().mt_3().child("You"))
                .child("Add retry handling for temporary API failures.")
                .child(div().mt_3().text_color(rgb(p.blue)).child("Codex"))
                .child(
                    "I’ll inspect the client, add bounded retries, and verify the failure cases.",
                )
                .child(
                    div()
                        .p_3()
                        .rounded_md()
                        .bg(rgb(p.mantle))
                        .child("Read src/client.rs · complete"),
                )
                .child(div().flex_1())
                .child(Input::new(self.draft.as_ref().expect("chat owns a draft")))
                .child(
                    div()
                        .text_color(rgb(p.muted))
                        .child("Sample only · drafts stay with their session"),
                ),
            PaneKind::Git => {
                let status = self.data.status(self.workspace);
                body.child(status.repository)
                    .child(
                        div()
                            .text_color(rgb(p.muted))
                            .child(format!("⑂ {}", status.branch)),
                    )
                    .child(format!("Changes · {}", status.files.len()))
                    .children(status.files.into_iter().map(|file| {
                        let group = if file.staged {
                            "staged"
                        } else if file.status == "??" {
                            "untracked"
                        } else {
                            "unstaged"
                        };
                        div()
                            .flex()
                            .gap_2()
                            .py_1()
                            .child(
                                div()
                                    .w(px(24.))
                                    .text_color(rgb(if file.staged { p.green } else { p.blue }))
                                    .child(file.status),
                            )
                            .child(div().flex_1().child(file.path))
                            .child(div().text_color(rgb(p.muted)).child(group))
                    }))
            }
            PaneKind::Files => body
                .child(self.data.status(self.workspace).repository)
                .children(self.files(self.data.files(self.workspace), "", 0, p, cx)),
        }
    }
}
