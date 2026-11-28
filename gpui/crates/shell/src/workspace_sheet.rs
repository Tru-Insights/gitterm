//! Iced's Workspace sheet, without catalog mutations or filesystem effects.
//!
//! The host seeds `NewWorkSheet::recent` with core `new_work_recent_repos` and
//! supplies core `existing_work` in catalog order. Retain this entity while a
//! folder picker is above it; return its result through `folder_picker_finished`.
//! Subscribe to `WorkspaceSheetOutcome` before showing the sheet. On Create,
//! create the named workspace using `start`, then open its chat; on OpenExisting,
//! dispatch the core Switch/Resume/Reopen identity. Remove the sheet on success
//! or Cancel, and use `show_error` to retain the draft after a host failure.

use std::path::{Path, PathBuf};

use gitterm_core::{
    checkout_name::repo_name,
    work::catalog::{
        ExistingWork, ExistingWorkAction, NewWorkSheet, WorkStart, existing_work_matches,
        existing_work_named,
    },
};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px, rgb, rgba,
};
use gpui_component::{
    Disableable as _, IndexPath, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    select::{Select, SelectEvent, SelectItem, SelectState},
    tooltip::Tooltip,
};

use crate::theme::{self, Palette};

/// Why the folder picker is open; decides what "Use this folder" does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderPickerPurpose {
    /// A machine's "+" in the Repos menu.
    NewWorkspace,
    /// Add a repo to an existing workspace (the host retains its identity).
    AddRepo(u64),
    /// "Open a repo…".
    OpenFolder,
    /// This sheet's Add repo…: the folder comes back to the sheet.
    NewWork,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceSheetOutcome {
    Create {
        name: String,
        start: WorkStart,
    },
    /// A core identity: Switch(uid), Resume(key), or Reopen(key).
    OpenExisting {
        id: ExistingWorkAction,
    },
    Cancel,
    PickFolder {
        purpose: FolderPickerPurpose,
    },
}

#[derive(Clone)]
struct DecisionsChoice {
    dir: Option<PathBuf>,
    label: SharedString,
}

impl SelectItem for DecisionsChoice {
    type Value = Option<PathBuf>;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.dir
    }
}

pub struct WorkspaceSheet {
    sheet: NewWorkSheet,
    existing: Vec<ExistingWork>,
    home: Option<PathBuf>,
    name: Entity<InputState>,
    decisions: Entity<SelectState<Vec<DecisionsChoice>>>,
    picking_folder: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<WorkspaceSheetOutcome> for WorkspaceSheet {}

impl WorkspaceSheet {
    pub fn new(
        sheet: NewWorkSheet,
        existing: Vec<ExistingWork>,
        home: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Name, e.g. Cloud migration");
            input.set_value(sheet.name.clone(), window, cx);
            input.focus(window, cx);
            input
        });
        let choices = decisions_choices(&sheet, home.as_deref());
        let selected = choices
            .iter()
            .position(|choice| choice.dir == sheet.decisions_repo);
        let decisions = cx.new(|cx| {
            SelectState::new(
                choices,
                selected.map(|row| IndexPath::default().row(row)),
                window,
                cx,
            )
        });
        let subscriptions = vec![
            cx.subscribe_in(&name, window, |this, input, event, _, cx| match event {
                InputEvent::Change => {
                    let name = input.read(cx).value().to_string();
                    // GPUI can emit Change after Enter without changing the text.
                    // Keep a validation error until the user actually edits it.
                    if this.sheet.name != name {
                        this.sheet.name = name;
                        this.sheet.error = None;
                        cx.notify();
                    }
                }
                InputEvent::PressEnter { .. } => this.submit(cx),
                _ => {}
            }),
            cx.subscribe(&decisions, |this, _, event, cx| {
                let SelectEvent::Confirm(dir) = event;
                this.sheet.decisions_repo = dir.clone().flatten();
                this.sheet.error = None;
                cx.notify();
            }),
        ];
        Self {
            sheet,
            existing,
            home,
            name,
            decisions,
            picking_folder: false,
            _subscriptions: subscriptions,
        }
    }

    /// Read-only draft for the host; the emitted Create also carries its name.
    pub fn draft(&self) -> &NewWorkSheet {
        &self.sheet
    }

    pub fn show_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.sheet.error = Some(error);
        cx.notify();
    }

    /// Call with None when the picker is cancelled. The original draft survives.
    pub fn folder_picker_finished(
        &mut self,
        dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picking_folder = false;
        if let Some(dir) = dir {
            self.add_repo(dir, window, cx);
        }
        self.name.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn sync_decisions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let choices = decisions_choices(&self.sheet, self.home.as_deref());
        self.decisions.update(cx, |select, cx| {
            select.set_items(choices, window, cx);
            select.set_selected_value(&self.sheet.decisions_repo, window, cx);
        });
    }

    fn add_repo(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.sheet.use_picked_folder(dir);
        self.sync_decisions(window, cx);
        cx.notify();
    }

    fn remove_repo(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.sheet.remove_repo(dir);
        self.sync_decisions(window, cx);
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        // Enter belongs to the folder picker while it is above this sheet.
        if self.picking_folder {
            return;
        }
        self.sheet.name = self.name.read(cx).value().to_string();
        match self.sheet.validate() {
            Ok((name, start)) => cx.emit(WorkspaceSheetOutcome::Create { name, start }),
            Err(error) => self.show_error(error, cx),
        }
    }

    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
        if !self.picking_folder {
            cx.emit(WorkspaceSheetOutcome::Cancel);
        }
    }

    fn browse(&mut self, cx: &mut Context<Self>) {
        if !self.picking_folder {
            self.picking_folder = true;
            cx.emit(WorkspaceSheetOutcome::PickFolder {
                purpose: FolderPickerPurpose::NewWork,
            });
            cx.notify();
        }
    }

    fn open_existing(&mut self, id: ExistingWorkAction, cx: &mut Context<Self>) {
        cx.emit(WorkspaceSheetOutcome::OpenExisting { id });
    }

    fn create_label(&self) -> &'static str {
        if existing_work_named(&self.existing, &self.sheet.name) {
            "Create another"
        } else {
            "Create workspace"
        }
    }

    fn matching_work(&self) -> Vec<&ExistingWork> {
        existing_work_matches(&self.existing, &self.sheet.name)
    }
}

impl Focusable for WorkspaceSheet {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for WorkspaceSheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let font = theme::font_sizes(cx).ui;
        let small = font - 1.;
        let muted = if p.base == 0x1e1e2e {
            0x9399b2
        } else {
            0x7c7f93
        };
        let button_border = if p.base == 0x1e1e2e {
            0x6c7086
        } else {
            0x9ca0b0
        };
        let border = if p.base == 0x1e1e2e {
            0x45475a
        } else {
            0xccd0da
        };
        let secondary = |text: &str| {
            div()
                .text_size(px(small))
                .text_color(rgb(p.muted))
                .child(text.to_owned())
        };
        let hint = |text: &str| {
            div()
                .text_size(px(small))
                .text_color(rgb(muted))
                .child(text.to_owned())
        };
        let mut card = div()
            .debug_selector(|| "workspace-sheet-card".into())
            .flex()
            .flex_col()
            .gap(px(10.))
            .w(px(568.))
            // GPUI includes the border in layout; Iced draws it inside.
            .p(px(19.))
            .px(px(23.))
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(border))
            .bg(rgb(p.mantle))
            .text_size(px(small))
            .line_height(gpui::relative(1.3))
            .text_color(rgb(p.text))
            .child(
                div()
                    .debug_selector(|| "workspace-sheet-title".into())
                    .text_size(px(font))
                    .child("Workspace"),
            )
            .child(secondary("Open existing work or start new work."))
            .child(
                div()
                    .debug_selector(|| "workspace-sheet-name".into())
                    .child(
                        Input::new(&self.name)
                            .small()
                            .text_size(px(small))
                            .line_height(gpui::relative(1.3))
                            .h(px(28.))
                            .px(px(7.))
                            .py(px(5.))
                            .bg(rgb(p.base))
                            .disabled(self.picking_folder),
                    ),
            );

        let matches = self.matching_work();
        if !matches.is_empty() {
            let heading = if self.sheet.name.trim().is_empty() {
                "Recent work"
            } else {
                "Matching work"
            };
            let mut rows = div().flex().flex_col().gap(px(4.)).child(hint(heading));
            for (index, item) in matches.into_iter().enumerate() {
                let mut row = div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .py(px(4.))
                    .px(px(8.))
                    .bg(rgb(p.base))
                    .rounded(px(6.))
                    .child(div().flex_1().child(item.name.clone()))
                    .child(
                        div()
                            .text_size(px(small - 1.))
                            .text_color(rgb(muted))
                            .child(item.status.label()),
                    );
                if let Some(id) = &item.action {
                    let label = match id {
                        ExistingWorkAction::Switch(_) => "Switch",
                        ExistingWorkAction::Resume(_) => "Resume",
                        ExistingWorkAction::Reopen(_) => "Reopen",
                    };
                    let id = id.clone();
                    row = row.child(
                        sheet_button(("existing-work", index), label, small)
                            .debug_selector(|| format!("workspace-sheet-existing-{index}"))
                            .h(px(20.))
                            .px(px(10.))
                            .outline()
                            .border_color(rgb(button_border))
                            .disabled(self.picking_folder)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_existing(id.clone(), cx)
                            })),
                    );
                }
                rows = rows.child(row);
            }
            card = card.child(rows);
        }
        if existing_work_named(&self.existing, &self.sheet.name) {
            card = card.child(hint(
                "Work with this name exists — open it above, or create another.",
            ));
        }

        let mut repos = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(secondary("Repos (optional)"));
        if !self.sheet.repos.is_empty() {
            let mut chips = div().flex().flex_wrap().gap(px(6.));
            for (index, dir) in self.sheet.repos.iter().enumerate() {
                let path = dir.clone();
                let tooltip = shown(dir, self.home.as_deref());
                chips = chips.child(
                    div()
                        .id(("workspace-sheet-repo-chip", index))
                        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .pl(px(10.))
                        .pr(px(2.))
                        .py(px(2.))
                        .bg(rgb(p.base))
                        .border_1()
                        .border_color(rgb(border))
                        .rounded(px(12.))
                        .child(repo_name(dir))
                        .child(
                            sheet_button(("remove-repo", index), "×", small)
                                .debug_selector(|| format!("workspace-sheet-remove-{index}"))
                                .ghost()
                                .h(px(16.))
                                .px(px(4.))
                                .disabled(self.picking_folder)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.remove_repo(&path, window, cx)
                                })),
                        ),
                );
            }
            repos = repos.child(chips);
        }
        repos = repos.child(
            div().child(
                sheet_button("add-repo", "Add repo…", small)
                    .debug_selector(|| "workspace-sheet-add-repo".into())
                    .h(px(24.))
                    .px(px(10.))
                    .outline()
                    .border_color(rgb(button_border))
                    .disabled(self.picking_folder)
                    .on_click(cx.listener(|this, _, _, cx| this.browse(cx))),
            ),
        );
        let shortcuts = self.sheet.recent_shortcuts();
        if !shortcuts.is_empty() {
            let mut recent = div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(4.))
                .child(hint("Recent:"));
            for (index, dir) in shortcuts.into_iter().enumerate() {
                let dir = dir.clone();
                recent = recent.child(
                    sheet_button(("recent-repo", index), repo_name(&dir), small)
                        .debug_selector(|| format!("workspace-sheet-recent-{index}"))
                        .ghost()
                        .h(px(20.))
                        .px(px(6.))
                        .text_size(px(small))
                        .tooltip(shown(&dir, self.home.as_deref()))
                        .disabled(self.picking_folder)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.add_repo(dir.clone(), window, cx)
                        })),
                );
            }
            repos = repos.child(recent);
        }
        card = card.child(repos);
        if !self.sheet.repos.is_empty() {
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(secondary("Team decisions go to:"))
                    .child(
                        div()
                            .debug_selector(|| "workspace-sheet-decisions".into())
                            .flex_1()
                            .min_w_0()
                            .child(
                                Select::new(&self.decisions)
                                    .accessibility_label("Team decisions go to:")
                                    .small()
                                    .text_size(px(small))
                                    .w_full()
                                    .disabled(self.picking_folder),
                            ),
                    ),
            );
        }
        if let Some(error) = &self.sheet.error {
            card = card.child(div().text_color(rgb(p.red)).child(error.clone()));
        }
        card = card.child(
            div()
                .flex()
                .justify_end()
                .items_center()
                .gap(px(8.))
                .child(
                    sheet_button("cancel-workspace", "Cancel", small)
                        .debug_selector(|| "workspace-sheet-cancel".into())
                        .outline()
                        .border_color(rgb(button_border))
                        .disabled(self.picking_folder)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                )
                .child(
                    sheet_button("create-workspace", self.create_label(), small)
                        .debug_selector(|| "workspace-sheet-create".into())
                        .primary()
                        .disabled(self.sheet.name.trim().is_empty() || self.picking_folder)
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                ),
        );

        div()
            .id("workspace-sheet")
            .occlude()
            .track_focus(&self.focus_handle(cx))
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(if p.base == 0x1e1e2e {
                0x11111bcc
            } else {
                0xdce0e8cc
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && !this.picking_folder {
                    this.cancel(cx);
                    cx.stop_propagation();
                }
            }))
            .child(card)
    }
}

fn sheet_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    small: f32,
) -> Button {
    let label = label.into();
    // Native button labels have their own rem-based size; Iced uses ui_font_small.
    Button::new(id)
        .accessibility_label(label.clone())
        .child(div().text_size(px(small)).child(label))
        .small()
        .h(px(28.))
        .px(px(12.))
        .rounded(px(6.))
}

fn decisions_choices(sheet: &NewWorkSheet, home: Option<&Path>) -> Vec<DecisionsChoice> {
    let mut choices = vec![DecisionsChoice {
        dir: None,
        label: "None".into(),
    }];
    choices.extend(sheet.repos.iter().map(|dir| DecisionsChoice {
        dir: Some(dir.clone()),
        label: format!("{} · {}", repo_name(dir), shown(dir, home)).into(),
    }));
    choices
}

fn shown(dir: &Path, home: Option<&Path>) -> String {
    match home
        .filter(|home| !home.as_os_str().is_empty())
        .and_then(|home| dir.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    }
}

#[cfg(test)]
#[path = "workspace_sheet_tests.rs"]
mod tests;
