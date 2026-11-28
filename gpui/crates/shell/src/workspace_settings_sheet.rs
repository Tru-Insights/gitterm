//! Iced's workspace settings sheet (A02–A10). Live writes use DesktopStore;
//! Preview keeps every write control read-only with its existing notice.

use std::{collections::HashMap, path::Path};

use gitterm_core::{
    checkout_name::repo_name,
    settings::WorkspaceColor,
    workspace_config::{RepoConfig, WorkspaceConfig, WorkspaceTabConfig, WorkspaceTarget},
    workspace_settings::{Edit, EnvEdit},
};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px, rgb, rgba,
};
use gpui_component::{
    Disableable as _, IndexPath, Sizable as _,
    button::Button,
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
    select::{Select, SelectEvent, SelectItem, SelectState},
};

use crate::theme::Palette;

pub const ALL_COLORS: [WorkspaceColor; 8] = [
    WorkspaceColor::Lavender,
    WorkspaceColor::Blue,
    WorkspaceColor::Green,
    WorkspaceColor::Peach,
    WorkspaceColor::Pink,
    WorkspaceColor::Yellow,
    WorkspaceColor::Red,
    WorkspaceColor::Teal,
];

const GH_ACCOUNT_ENV_KEY: &str = "GITTERM_GH_ACCOUNT";
const MONO: &str = "Menlo";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Close,
    EditGuidelines,
    Rename(String),
    Edit(Edit),
    /// A control that would write; the label names it for the notice.
    Unavailable(String),
}

/// What the sheet shows, read from Iced's config.
#[derive(Clone, Debug, PartialEq)]
pub struct SheetModel {
    pub name: String,
    pub color: WorkspaceColor,
    /// `None`: no profiles.json. Otherwise sorted names and whether active.
    pub profiles: Option<Vec<(String, bool)>>,
    /// Sorted by key.
    pub env: Vec<(String, String)>,
    pub guidelines_enabled: bool,
    pub guidelines_workspace: bool,
    /// The active tab's repo and whether the guidelines are on for it.
    pub guidelines_repo: Option<(String, bool)>,
    pub chrome: bool,
    pub decisions: String,
    pub decisions_choices: Vec<(Option<String>, String)>,
}

impl SheetModel {
    /// Reads like Iced's `view_workspace_settings_modal`. `repos` is the
    /// catalog's repo list (for repo env); `config_dir` is Iced's config dir.
    pub fn read(
        ws: &WorkspaceConfig,
        repos: &[RepoConfig],
        config_dir: &Path,
        current_tab: Option<&WorkspaceTabConfig>,
    ) -> Self {
        let home = ws.home_dir();
        // A legacy remote workspace keeps its settings in its own fields.
        let base: HashMap<String, String> = if ws.is_legacy_remote() {
            ws.env.clone()
        } else {
            let key = ws.repo_key(home);
            repos
                .iter()
                .find(|repo| Some(repo.key()) == key)
                .map(|repo| repo.env.clone())
                .unwrap_or_default()
        };
        let env_map = gitterm_core::workspace_config::layered_env(&base, &ws.env_overrides);
        let mut env: Vec<_> = env_map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        env.sort_by(|a, b| a.0.cmp(&b.0));
        let guidelines = gitterm_core::guidelines::load_settings(config_dir);
        let local = matches!(
            ws.location,
            None | Some(gitterm_core::remote::config::WorkspaceLocationConfig::Local { .. })
        ) && !ws.is_legacy_remote();
        let guidelines_repo = local
            .then(|| {
                let tab = current_tab?;
                let repo = gitterm_core::guidelines::repo_key(Path::new(
                    tab.repo_dir.as_deref().unwrap_or(&tab.dir),
                ));
                let off = guidelines.repo_is_off(&repo);
                Some((repo.display().to_string(), !off))
            })
            .flatten();
        Self {
            name: ws.name.clone(),
            color: ws.color,
            profiles: load_profiles(config_dir, &env_map),
            env,
            guidelines_enabled: guidelines.enabled,
            guidelines_workspace: !ws.guidelines_off,
            guidelines_repo,
            chrome: ws.claude_in_chrome,
            decisions: decisions_label(ws),
            decisions_choices: {
                let mut choices = vec![(None, "None".into())];
                if local {
                    choices.extend(ws.repos.iter().map(|dir| {
                        (
                            Some(dir.clone()),
                            format!("{} · {}", repo_name(Path::new(dir)), dir),
                        )
                    }));
                }
                if let Some(dir) = &ws.decisions_repo
                    && !choices
                        .iter()
                        .any(|(choice, _)| choice.as_ref() == Some(dir))
                {
                    choices.push((Some(dir.clone()), decisions_label(ws)));
                }
                choices
            },
        }
    }
}

fn decisions_label(ws: &WorkspaceConfig) -> String {
    let Some(dir) = &ws.decisions_repo else {
        return "None".into();
    };
    let label = format!("{} \u{00B7} {}", repo_name(Path::new(dir)), dir);
    let local = !ws.is_legacy_remote()
        && matches!(
            ws.location,
            None | Some(gitterm_core::remote::config::WorkspaceLocationConfig::Local { .. })
        );
    // Iced lists the workspace's local repos; a saved choice outside them is
    // still shown, and told.
    if local && ws.repos.iter().any(|repo| repo == dir) {
        label
    } else {
        format!("{label} (not listed)")
    }
}

fn load_profiles(config_dir: &Path, env: &HashMap<String, String>) -> Option<Vec<(String, bool)>> {
    let profiles = gitterm_core::workspace_settings::load_profiles(config_dir)?;
    // The first profile (by Iced's map order) whose keys all match is current;
    // with sorted display only one may be marked.
    let mut names: Vec<_> = profiles.keys().cloned().collect();
    names.sort();
    let current = names.iter().find(|name| {
        let p = &profiles[*name];
        !p.is_empty() && p.iter().all(|(k, v)| env.get(k) == Some(v))
    });
    let current = current.cloned();
    Some(
        names
            .into_iter()
            .map(|name| {
                let active = current.as_ref() == Some(&name);
                (name, active)
            })
            .collect(),
    )
}

#[derive(Clone)]
struct DecisionsChoice {
    dir: Option<String>,
    label: SharedString,
}
impl SelectItem for DecisionsChoice {
    type Value = Option<String>;
    fn title(&self) -> SharedString {
        self.label.clone()
    }
    fn value(&self) -> &Self::Value {
        &self.dir
    }
}

pub struct WorkspaceSettingsSheet {
    pub(crate) workspace_id: Option<crate::WorkspaceId>,
    model: SheetModel,
    focus: FocusHandle,
    key: Entity<InputState>,
    value: Entity<InputState>,
    rename: Option<Entity<InputState>>,
    notice: Option<String>,
    save_pending: bool,
    decisions: Entity<SelectState<Vec<DecisionsChoice>>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<Outcome> for WorkspaceSettingsSheet {}

impl Focusable for WorkspaceSettingsSheet {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl WorkspaceSettingsSheet {
    /// Escape cancels an inline rename before closing. Also the shell's
    /// Escape while a terminal behind the sheet holds focus.
    pub(crate) fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename.take().is_some() {
            self.focus.focus(window, cx);
            cx.notify();
        } else {
            cx.emit(Outcome::Close);
        }
    }
    pub fn new(model: SheetModel, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let key = cx.new(|cx| InputState::new(window, cx).placeholder("KEY"));
        let value = cx.new(|cx| InputState::new(window, cx).placeholder("VALUE"));
        let mut subscriptions: Vec<_> = [&key, &value]
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, event, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.add_env(window, cx);
                    }
                })
            })
            .into();
        let choices: Vec<_> = model
            .decisions_choices
            .iter()
            .map(|(dir, label)| DecisionsChoice {
                dir: dir.clone(),
                label: label.clone().into(),
            })
            .collect();
        let selected = choices
            .iter()
            .position(|choice| choice.label.as_ref() == model.decisions);
        let decisions = cx.new(|cx| {
            SelectState::new(
                choices,
                selected.map(|row| IndexPath::default().row(row)),
                window,
                cx,
            )
        });
        subscriptions.push(cx.subscribe(&decisions, |this, _, event, cx| {
            let SelectEvent::Confirm(dir) = event;
            this.edit("Decisions repo", Edit::Decisions(dir.clone().flatten()), cx);
        }));
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            workspace_id: None,
            model,
            focus,
            key,
            value,
            rename: None,
            notice: None,
            save_pending: false,
            decisions,
            _subscriptions: subscriptions,
        }
    }

    pub fn model(&self) -> &SheetModel {
        &self.model
    }
    pub(crate) fn saved_name(&mut self, name: String, cx: &mut Context<Self>) {
        self.model.name = name;
        cx.notify();
    }
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    fn unavailable(&mut self, label: &str, cx: &mut Context<Self>) {
        let notice = format!("{label} \u{00B7} not available in preview");
        self.notice = Some(notice.clone());
        cx.emit(Outcome::Unavailable(label.to_owned()));
        cx.notify();
    }

    fn edit(&mut self, label: &str, edit: Edit, cx: &mut Context<Self>) {
        if self.save_pending {
            return;
        }
        if cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .is_none()
        {
            self.unavailable(label, cx);
        } else {
            cx.emit(Outcome::Edit(edit));
        }
    }

    fn add_env(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.save_pending {
            return;
        }
        if cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .is_none()
        {
            self.unavailable("Add environment variable", cx);
            return;
        }
        if let Some(edit) = gitterm_core::workspace_settings::env_add(
            self.key.read(cx).value().as_ref(),
            self.value.read(cx).value().as_ref(),
        ) {
            self.edit("Add environment variable", Edit::Env(edit), cx);
        }
    }

    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.save_pending {
            return;
        }
        let name = self.model.name.clone();
        let input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Workspace name");
            input.set_value(name, window, cx);
            input.focus(window, cx);
            input
        });
        let sub = cx.subscribe(&input, |this, input, event, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) && !this.save_pending {
                let draft = input.read(cx).value().to_string();
                this.rename = None;
                if cx
                    .try_global::<crate::workspace_config::LiveWorkspaces>()
                    .is_some()
                {
                    cx.emit(Outcome::Rename(draft));
                    cx.notify();
                } else {
                    this.unavailable("Rename", cx);
                }
            }
        });
        self._subscriptions.push(sub);
        self.rename = Some(input);
        cx.notify();
    }
}

fn mono(label: impl Into<SharedString>) -> gpui::Div {
    div().font_family(MONO).child(label.into())
}

impl Render for WorkspaceSettingsSheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let dark = p.base == 0x1e1e2e;
        let accent = p.blue;
        let model = self.model.clone();
        let saving = self.save_pending;
        let spacer = |h: f32| div().h(px(h));

        let title = match &self.rename {
            Some(input) => div()
                .w(px(260.))
                .child(Input::new(input).small().disabled(saving))
                .into_any_element(),
            None => div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .debug_selector(|| "settings-title".into())
                        .text_size(px(16.))
                        .text_color(rgb(p.text))
                        .child(format!("Workspace: {}", model.name)),
                )
                .child(
                    div()
                        .id("settings-rename")
                        .debug_selector(|| "settings-rename".into())
                        .px(px(6.))
                        .py(px(2.))
                        .text_size(px(12.))
                        .text_color(rgb(accent))
                        .opacity(if saving { 0.5 } else { 1.0 })
                        .child("\u{270E} Rename")
                        .when(!saving, |control| {
                            control.cursor_pointer().on_click(
                                cx.listener(|this, _, window, cx| this.start_rename(window, cx)),
                            )
                        }),
                )
                .into_any_element(),
        };

        let mut colors = div().flex().items_center().gap(px(6.)).child(
            mono("Color:")
                .text_size(px(12.))
                .text_color(rgb(p.subtext0)),
        );
        for color in ALL_COLORS {
            let selected = model.color == color;
            colors = colors.child(
                div()
                    .id(("settings-color", color as usize))
                    .debug_selector(move || format!("settings-color-{color:?}"))
                    .size(px(18.))
                    .rounded_full()
                    .bg(rgb(crate::session_picker::preset_color(color, dark)))
                    .border_2()
                    .border_color(if selected { rgb(accent) } else { rgba(0) })
                    .opacity(if saving { 0.5 } else { 1.0 })
                    .when(!saving, |control| {
                        control
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.edit("Color", Edit::Color(color), cx)
                            }))
                    }),
            );
        }

        let mut profiles = div().flex().items_center().gap(px(6.)).child(
            mono("Profile:")
                .text_size(px(12.))
                .text_color(rgb(p.subtext0)),
        );
        match &model.profiles {
            Some(list) => {
                for (index, (name, active)) in list.iter().enumerate() {
                    let label = format!("Profile {name}");
                    let name_to_apply = name.clone();
                    profiles = profiles.child(
                        mono(name.clone())
                            .id(("settings-profile", index))
                            .debug_selector({
                                let name = name.clone();
                                move || format!("settings-profile-{name}")
                            })
                            .px(px(10.))
                            .py(px(3.))
                            .text_size(px(11.))
                            .rounded(px(3.))
                            .border_1()
                            .border_color(rgb(if *active { accent } else { p.border }))
                            .bg(if *active { rgb(accent) } else { rgba(0) })
                            .text_color(rgb(if *active { p.base } else { p.text }))
                            .opacity(if saving { 0.5 } else { 1.0 })
                            .when(!saving, |control| {
                                control.cursor_pointer().on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.edit(&label, Edit::Profile(name_to_apply.clone()), cx)
                                    },
                                ))
                            }),
                    );
                }
            }
            None => {
                profiles = profiles.child(
                    mono("No profiles.json found")
                        .text_size(px(11.))
                        .text_color(rgb(p.muted)),
                );
            }
        }

        let check = |id: &'static str,
                     checked: bool,
                     label: String,
                     notice: &'static str,
                     edit: Edit,
                     cx: &mut Context<Self>| {
            div().debug_selector(move || id.into()).child(
                Checkbox::new(id)
                    .label(label)
                    .checked(checked)
                    .disabled(saving)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.edit(notice, edit.clone(), cx)),
                    ),
            )
        };
        let mut guidelines = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        mono("Change Guidelines")
                            .text_size(px(12.))
                            .text_color(rgb(accent)),
                    )
                    .child(
                        mono("Edit guidelines")
                            .id("settings-edit-guidelines")
                            .debug_selector(|| "settings-edit-guidelines".into())
                            .px(px(6.))
                            .py(px(2.))
                            .text_size(px(11.))
                            .text_color(rgb(accent))
                            .cursor_pointer()
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(Outcome::EditGuidelines))),
                    ),
            )
            .child(check(
                "settings-guidelines-all",
                model.guidelines_enabled,
                "Add them to every agent GitTerm starts (all workspaces)".into(),
                "Guidelines for all workspaces",
                Edit::GuidelinesEnabled(!model.guidelines_enabled),
                cx,
            ))
            .child(check(
                "settings-guidelines-workspace",
                model.guidelines_workspace,
                "Use them in this workspace".into(),
                "Guidelines for this workspace",
                Edit::GuidelinesWorkspace(!model.guidelines_workspace),
                cx,
            ));
        if let Some((repo, on)) = &model.guidelines_repo {
            guidelines = guidelines.child(check(
                "settings-guidelines-repo",
                *on,
                format!("Use them in {repo}"),
                "Guidelines for this repo",
                Edit::GuidelinesRepo {
                    repo: repo.into(),
                    enabled: !on,
                },
                cx,
            ));
        }

        let chrome = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(mono("Claude in Chrome").text_size(px(12.)).text_color(rgb(accent)))
            .child(check(
                "settings-chrome",
                model.chrome,
                "Start Claude sessions in this workspace with Claude in Chrome".into(),
                "Claude in Chrome",
                Edit::Chrome(!model.chrome),
                cx,
            ))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child("Off: agents use GitTerm's browser. Applies to Claude sessions started from now on; a running chat keeps its setting until it restarts."),
            );

        let decisions = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(mono("Decisions repo:").text_size(px(12.)).text_color(rgb(p.subtext0)))
                    .child(
                        div().debug_selector(|| "settings-decisions".into()).flex_1().min_w_0()
                            .child(if cx.try_global::<crate::workspace_config::LiveWorkspaces>().is_some() {
                                Select::new(&self.decisions).disabled(saving).accessibility_label("Decisions repo").small().text_size(px(12.)).w_full().into_any_element()
                            } else {
                                mono(model.decisions.clone()).id("settings-decisions-preview")
                                    .px(px(8.)).py(px(4.)).text_size(px(12.)).text_color(rgb(p.text))
                                    .bg(rgb(p.base)).border_1().border_color(rgb(p.border)).rounded(px(3.)).cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| this.unavailable("Decisions repo", cx))).into_any_element()
                            })
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child("Agents started in this work are told its docs folder for working notes and this repo for decisions the team should see (through a PR)."),
            );

        let mut env = div()
            .id("settings-env")
            .flex()
            .flex_col()
            .gap(px(4.))
            .h(px(200.))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(mono("KEY").w(px(180.)))
                    .child(mono("VALUE")),
            );
        for (index, (key, value)) in model.env.iter().enumerate() {
            let label = format!("Remove {key}");
            let remove_key = key.clone();
            env = env.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        mono(key.clone())
                            .w(px(180.))
                            .text_size(px(12.))
                            .text_color(rgb(p.text)),
                    )
                    .child(
                        mono(value.clone())
                            .text_size(px(12.))
                            .text_color(rgb(p.subtext0)),
                    )
                    .child(div().flex_1())
                    .child(
                        mono("\u{2715}")
                            .id(("settings-env-remove", index))
                            .debug_selector({
                                let key = key.clone();
                                move || format!("settings-env-remove-{key}")
                            })
                            .px(px(4.))
                            .py(px(1.))
                            .text_size(px(10.))
                            .text_color(rgb(p.red))
                            .opacity(if saving { 0.5 } else { 1.0 })
                            .when(!saving, |control| {
                                control.cursor_pointer().on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.edit(
                                            &label,
                                            Edit::Env(EnvEdit::Set {
                                                key: remove_key.clone(),
                                                value: None,
                                            }),
                                            cx,
                                        )
                                    },
                                ))
                            }),
                    ),
            );
        }
        env = env.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .debug_selector(|| "settings-key".into())
                        .w(px(174.))
                        .child(Input::new(&self.key).small()),
                )
                .child(
                    div()
                        .debug_selector(|| "settings-value".into())
                        .w(px(200.))
                        .child(Input::new(&self.value).small()),
                )
                .child(
                    mono("Add")
                        .id("settings-env-add")
                        .debug_selector(|| "settings-env-add".into())
                        .px(px(10.))
                        .py(px(4.))
                        .text_size(px(12.))
                        .text_color(rgb(accent))
                        .border_1()
                        .border_color(rgb(accent))
                        .rounded(px(3.))
                        .opacity(if saving { 0.5 } else { 1.0 })
                        .when(!saving, |control| {
                            control.cursor_pointer().on_click(
                                cx.listener(|this, _, window, cx| this.add_env(window, cx)),
                            )
                        }),
                ),
        );

        let footer = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .debug_selector(move || {
                        if saving {
                            "settings-saving"
                        } else {
                            "settings-save-status"
                        }
                        .into()
                    })
                    .child(if saving {
                        "Saving… Write controls are disabled"
                    } else {
                        "Changes apply to new terminal sessions"
                    }),
            )
            .child(
                Button::new("settings-close")
                    .debug_selector(|| "settings-close".into())
                    .small()
                    .outline()
                    .label("Close")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(Outcome::Close))),
            );

        let mut card = div()
            .debug_selector(|| "settings-card".into())
            .flex()
            .flex_col()
            .w(px(560.))
            .py(px(23.))
            .px(px(27.))
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.mantle))
            .text_size(px(12.))
            .text_color(rgb(p.text))
            .child(title)
            .child(spacer(12.))
            .child(colors)
            .child(spacer(12.))
            .child(profiles)
            .child(spacer(16.))
            .child(guidelines)
            .child(spacer(16.))
            .child(chrome)
            .child(spacer(16.))
            .child(decisions)
            .child(spacer(16.))
            .child(mono("Environment Variables").text_size(px(12.)).text_color(rgb(accent)))
            .child(
                mono(format!(
                    "{GH_ACCOUNT_ENV_KEY}=<login> pins the gh account for task pushes and PRs; otherwise the logged-in account with access to the repo is used."
                ))
                .text_size(px(11.))
                .text_color(rgb(p.muted)),
            )
            .child(spacer(6.))
            .child(env)
            .child(spacer(12.))
            .child(footer);
        if let Some(notice) = &self.notice {
            card = card.child(
                div()
                    .debug_selector(|| "settings-notice".into())
                    .pt(px(8.))
                    .text_size(px(11.))
                    .text_color(rgb(p.red))
                    .child(notice.clone()),
            );
        }

        div()
            .id("workspace-settings-overlay")
            .occlude()
            .track_focus(&self.focus)
            .key_context("IcedWorkspaceSettings")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(if dark { 0x11111bcc } else { 0xdce0e8cc }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.escape(window, cx);
                }
            }))
            .child(card)
    }
}

impl crate::Shell {
    fn settings_sheet_model(&self, id: crate::WorkspaceId, cx: &App) -> Option<SheetModel> {
        let owner = self.owner.read(cx);
        let ws = owner.iced_workspace(id)?;
        let catalog = owner.iced_catalog()?;
        let current_tab = self
            .workspaces
            .iter()
            .find(|state| state.workspace.id == id)
            .and_then(|state| state.active_session)
            .and_then(|session| self.session_tab(session, cx));
        Some(SheetModel::read(
            ws,
            &catalog.repos,
            &catalog.source,
            current_tab,
        ))
    }

    pub(crate) fn sync_settings_sheet_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(sheet) = &self.settings_sheet {
            let pending = self.owner.read(cx).workspace_save_pending;
            sheet.update(cx, |sheet, cx| {
                if sheet.save_pending != pending {
                    sheet.save_pending = pending;
                    cx.notify();
                }
            });
        }
    }

    pub(crate) fn open_settings_sheet(
        &mut self,
        id: crate::data::WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_sheet.is_some() {
            return;
        }
        let model = self.settings_sheet_model(id, cx);
        let Some(model) = model else {
            // Not a workspace from Iced's config: there are no settings to show.
            self.chrome_unavailable("Workspace settings", cx);
            return;
        };
        let previous_focus = window.focused(cx).unwrap_or_else(|| self.focus.clone());
        self.session_picker = None;
        self.picker_subscription = None;
        self.park_sheet = None;
        self.park_subscription = None;
        self.new_work_sheet = None;
        self.new_work_subscription = None;
        self.bar_menu = None;
        let sheet = cx.new(|cx| {
            let mut sheet = WorkspaceSettingsSheet::new(model, window, cx);
            sheet.workspace_id = Some(id);
            sheet.save_pending = self.owner.read(cx).workspace_save_pending;
            sheet
        });
        self.settings_subscription = Some(cx.subscribe_in(
            &sheet,
            window,
            move |shell, _, outcome: &Outcome, window, cx| match outcome {
                Outcome::Close => {
                    shell.close_settings_sheet(window, cx);
                    previous_focus.focus(window, cx);
                }
                Outcome::Unavailable(label) => shell.chrome_unavailable(label, cx),
                Outcome::Rename(name) => shell.repo_outcome(
                    &crate::work_sidebar::WorkSidebarOutcome::Rename {
                        workspace: id.0,
                        name: name.clone(),
                    },
                    window,
                    cx,
                ),
                Outcome::EditGuidelines => shell.edit_guidelines(id, window, cx),
                Outcome::Edit(edit) => shell.save_sheet_edit(id, edit.clone(), window, cx),
            },
        ));
        self.settings_sheet = Some(sheet);
        self.cover_sheet(true, window, cx);
        cx.notify();
    }

    fn save_sheet_edit(
        &mut self,
        id: crate::WorkspaceId,
        edit: Edit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = cx
            .try_global::<crate::workspace_config::LiveWorkspaces>()
            .cloned()
        else {
            return;
        };
        if self.owner.read(cx).workspace_save_pending {
            return;
        }
        let Some(saved) = self.owner.read(cx).iced_workspace(id) else {
            return;
        };
        let target = WorkspaceTarget::of(saved);
        let draft = matches!(edit, Edit::Env(EnvEdit::Set { value: Some(_), .. }))
            .then(|| {
                self.settings_sheet.as_ref().map(|sheet| {
                    let sheet = sheet.read(cx);
                    (
                        sheet.key.read(cx).value().to_string(),
                        sheet.value.read(cx).value().to_string(),
                    )
                })
            })
            .flatten();
        self.owner.update(cx, |owner, cx| {
            owner.workspace_save_pending = true;
            cx.notify();
        });
        self.sync_settings_sheet_pending(cx);
        let root = store.0.root().to_path_buf();
        let result = cx
            .background_executor()
            .spawn(async move { store.0.edit_workspace_settings(target, edit).wait() });
        cx.spawn_in(window, async move |shell, cx| {
            let result = result.await;
            _ = shell.update_in(cx, |shell, window, cx| {
                shell.owner.update(cx, |owner, cx| {
                    owner.workspace_save_pending = false;
                    cx.notify();
                });
                match result {
                    Ok(file) => {
                        shell.repo_document_saved(&root, file, cx);
                        let model = shell.settings_sheet_model(id, cx);
                        if let Some(sheet) = &shell.settings_sheet
                            && sheet.read(cx).workspace_id == Some(id)
                        {
                            sheet.update(cx, |sheet, cx| {
                                if let Some(model) = model {
                                    let choices: Vec<_> = model
                                        .decisions_choices
                                        .iter()
                                        .map(|(dir, label)| DecisionsChoice {
                                            dir: dir.clone(),
                                            label: label.clone().into(),
                                        })
                                        .collect();
                                    let selected = choices
                                        .iter()
                                        .find(|choice| choice.label.as_ref() == model.decisions)
                                        .and_then(|choice| choice.dir.clone());
                                    sheet.decisions.update(cx, |select, cx| {
                                        select.set_items(choices, window, cx);
                                        select.set_selected_value(&selected, window, cx);
                                    });
                                    sheet.model = model;
                                }
                                if let Some((key, value)) = &draft
                                    && sheet.key.read(cx).value().as_ref() == key
                                    && sheet.value.read(cx).value().as_ref() == value
                                {
                                    sheet
                                        .key
                                        .update(cx, |input, cx| input.set_value("", window, cx));
                                    sheet
                                        .value
                                        .update(cx, |input, cx| input.set_value("", window, cx));
                                }
                                sheet.notice = None;
                                cx.notify();
                            });
                        }
                        shell.owner.update(cx, |owner, cx| {
                            owner.error = None;
                            cx.notify();
                        });
                    }
                    Err(error) => {
                        if let Some(sheet) = &shell.settings_sheet
                            && sheet.read(cx).workspace_id == Some(id)
                        {
                            sheet.update(cx, |sheet, cx| {
                                sheet.notice = Some(error.clone());
                                let selected = sheet
                                    .model
                                    .decisions_choices
                                    .iter()
                                    .find(|(_, label)| label == &sheet.model.decisions)
                                    .and_then(|(dir, _)| dir.clone());
                                sheet.decisions.update(cx, |select, cx| {
                                    select.set_selected_value(&selected, window, cx)
                                });
                                cx.notify();
                            });
                        }
                        shell.owner.update(cx, |owner, cx| {
                            owner.error = Some(error);
                            cx.notify();
                        });
                    }
                }
                shell.sync_settings_sheet_pending(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn close_settings_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_sheet = None;
        self.settings_subscription = None;
        self.cover_sheet(false, window, cx);
        cx.notify();
    }

    /// Iced creates a missing guidelines file; this preview never writes, so a
    /// missing file is a notice and an existing one opens read-only.
    fn edit_guidelines(
        &mut self,
        workspace: crate::data::WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = self
            .owner
            .read(cx)
            .iced_catalog()
            .map(|catalog| gitterm_core::guidelines::guidelines_path(&catalog.source));
        let Some(path) = path.filter(|path| path.is_file()) else {
            if let Some(sheet) = &self.settings_sheet {
                sheet.update(cx, |sheet, cx| sheet.unavailable("Edit guidelines", cx));
            }
            return;
        };
        self.close_settings_sheet(window, cx);
        self.open_file_viewer(
            &crate::owner::FileOpen {
                workspace,
                path,
                changed: None,
                from_files: true,
            },
            window,
            cx,
        );
    }
}

#[cfg(test)]
#[path = "workspace_settings_sheet_tests.rs"]
mod tests;
