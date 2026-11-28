//! Shared V6 view preferences; Preview retains its isolated layout behavior.
use super::*;

gpui::actions!(gitterm_iced, [SearchPrev]);

#[derive(Default)]
struct LiveRegions(HashSet<WorkspaceId>);
impl gpui::Global for LiveRegions {}

impl Shell {
    pub(super) fn live_preferences(cx: &App) -> bool {
        cx.try_global::<workspace_config::LiveWorkspaces>()
            .is_some()
    }

    pub(super) fn save_view_preferences(
        &self,
        edit: impl FnOnce(&mut gitterm_core::settings::DesktopSettings) + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = cx.try_global::<workspace_config::LiveWorkspaces>() else {
            return;
        };
        let fonts = theme::font_sizes(cx);
        let receipt = store.0.edit_settings(move |settings| {
            edit(settings);
            // Iced clears the legacy combined size when saving any preference.
            settings.font_size = None;
            settings.ui_font_size = fonts.ui;
            settings.terminal_font_size = fonts.terminal;
        });
        let save = cx
            .background_executor()
            .spawn(async move { receipt.wait() });
        cx.spawn(async move |shell, cx| {
            if let Err(error) = save.await {
                let _ = shell.update(cx, |shell, cx| {
                    shell.owner.update(cx, |owner, cx| {
                        owner.error = Some(format!("Save V6 view preferences: {error}"));
                        cx.notify();
                    });
                });
            }
        })
        .detach();
    }

    pub(super) fn restore_view_preferences(
        workspaces: &mut [WorkspaceDock],
        settings: &workspace_config::IcedSettings,
        bottom_height: f32,
        cx: &mut App,
    ) -> f32 {
        let live = Self::live_preferences(cx);
        // Preview has no V6 writer. Its most recently saved divider wins over
        // the read-only Iced seed, even when another workspace was dragged.
        let restored = if live {
            None
        } else {
            workspaces
                .iter()
                .filter_map(|state| {
                    let modified = std::fs::metadata(&state.store.path).ok()?.modified().ok()?;
                    Some((
                        modified,
                        &state.store.path,
                        state.store.regions.sidebar_width,
                    ))
                })
                .max_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)))
                .map(|(_, _, width)| width)
        };
        let width = layout::clamp_sidebar(restored.unwrap_or(settings.sidebar_width));
        for state in workspaces {
            let regions = &mut state.store.regions;
            if live {
                cx.default_global::<LiveRegions>()
                    .0
                    .insert(state.workspace.id);
                // Iced's settings.json is authoritative over older GPUI layouts.
                regions.fonts = None;
                regions.chat_font_size = None;
                regions.chat_width = None;
                regions.bottom_height = bottom_height;
                regions.bottom_visible = settings.console_expanded;
            }
            regions.resize_sidebar(width);
        }
        width
    }

    pub(super) fn toggle_view_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = !gpui_component::Theme::global(cx).mode.is_dark();
        theme::apply(dark, Some(window), cx);
        self.save_view_preferences(
            move |settings| {
                settings.theme = if dark { "dark" } else { "light" }.into();
            },
            cx,
        );
        self.refresh_view_appearance(cx);
        cx.notify();
    }

    pub(super) fn refresh_view_appearance(&self, cx: &mut Context<Self>) {
        let colors = terminal_palette(cx);
        for state in &self.workspaces {
            for pane in state.panes.values() {
                pane.update(cx, |pane, cx| {
                    match &pane.content {
                        pane::PaneContent::Chat(view) => view.update(cx, |view, cx| {
                            view.set_appearance(theme::chat_appearance(cx), cx)
                        }),
                        pane::PaneContent::Terminal(view) => {
                            // Keep the PTY/session alive while applying the new palette.
                            if view.read(cx).config().colors.background() != colors.background() {
                                let mut config = view.read(cx).config().clone();
                                config.colors = colors.clone();
                                view.update(cx, |view, cx| view.update_config(config, cx));
                            }
                        }
                        _ => {}
                    }
                });
            }
        }
    }

    pub(super) fn toggle_log_mirroring(&mut self, cx: &mut Context<Self>) {
        if !Self::live_preferences(cx) {
            self.preview_unavailable("Terminal log mirroring", cx);
            return;
        }
        let enabled = !self.bottom.read(cx).input.mirroring;
        self.bottom.update(cx, |bottom, cx| {
            let mut input = bottom.input.clone();
            input.mirroring = enabled;
            bottom.set_input(input, cx);
        });
        self.save_view_preferences(
            move |settings| {
                settings.terminal_log_mirroring_enabled = enabled;
            },
            cx,
        );
        cx.notify();
    }

    pub(super) fn save_global_geometry(&mut self, cx: &mut Context<Self>) {
        if !Self::live_preferences(cx) || self.active_workspace(cx).is_none() {
            return;
        }
        let regions = &self.active(cx).store.regions;
        let (height, expanded) = (regions.bottom_height, regions.bottom_visible);
        let width = self.sidebar_width;
        for state in &mut self.workspaces {
            state.store.regions.bottom_height = height;
            state.store.regions.bottom_visible = expanded;
            state.store.regions.resize_sidebar(width);
        }
        self.save_view_preferences(
            move |settings| {
                settings.sidebar_width = width;
                settings.console_height = height;
                settings.console_expanded = expanded;
            },
            cx,
        );
    }

    pub(super) fn refresh_global_geometry(&mut self, window_height: f32, cx: &mut Context<Self>) {
        if !Self::live_preferences(cx) {
            return;
        }
        let input = self.bottom.read(cx).input.clone();
        cx.default_global::<LiveRegions>().0.retain(|id| {
            self.workspaces
                .iter()
                .any(|state| state.workspace.id == *id)
        });
        for state in &mut self.workspaces {
            if cx
                .default_global::<LiveRegions>()
                .0
                .insert(state.workspace.id)
            {
                state.store.regions.bottom_height = input.height;
                state.store.regions.bottom_visible = input.expanded;
                state.store.regions.resize_sidebar(self.sidebar_width);
            }
        }
        let active = self.active_workspace(cx);
        let Some(state) = self
            .workspaces
            .iter()
            .find(|state| Some(state.workspace.id) == active)
        else {
            return;
        };
        let height = bottom_panel::clamp_height(state.store.regions.bottom_height, window_height);
        let resized_window = input.window_height != window_height;
        if resized_window {
            for state in &mut self.workspaces {
                state.store.regions.bottom_height = height;
            }
        }
        let state = self
            .workspaces
            .iter()
            .find(|state| Some(state.workspace.id) == active)
            .unwrap();
        let regions = &state.store.regions;
        if self.resize.is_none()
            && ((!resized_window && regions.bottom_height != input.height)
                || regions.bottom_visible != input.expanded)
        {
            self.save_global_geometry(cx);
        }
    }

    pub(super) fn finish_preference_resize(&mut self, cx: &mut Context<Self>) {
        if self.resize.is_some() {
            self.save_global_geometry(cx);
        }
    }
}

#[cfg(test)]
#[path = "view_preferences_tests.rs"]
mod tests;

// shortcut: the terminal renderer derives dim ANSI shades; use Iced's explicit
// dim colors when the renderer exposes custom dim palette entries.
fn terminal_palette(cx: &App) -> gitterm_terminal::ColorPalette {
    let dark = gpui_component::Theme::global(cx).mode.is_dark();
    let p = Palette::for_mode(dark);
    macro_rules! color {
        ($builder:expr, $method:ident, $color:expr) => {{
            let value: u32 = $color;
            $builder.$method((value >> 16) as u8, (value >> 8) as u8, value as u8)
        }};
    }
    let palette = gitterm_terminal::ColorPalette::builder();
    let palette = color!(palette, background, p.base);
    let palette = color!(palette, foreground, p.text);
    let palette = color!(palette, cursor, p.text);
    let palette = color!(palette, black, if dark { 0x45475a } else { 0x5c5f77 });
    let palette = color!(palette, red, p.red);
    let palette = color!(palette, green, p.green);
    let palette = color!(palette, yellow, p.yellow);
    let palette = color!(palette, blue, p.blue);
    let palette = color!(palette, magenta, p.pink);
    let palette = color!(palette, cyan, p.teal);
    let palette = color!(palette, white, if dark { 0xbac2de } else { 0xacb0be });
    let palette = color!(
        palette,
        bright_black,
        if dark { 0x585b70 } else { 0x6c6f85 }
    );
    let palette = color!(palette, bright_red, p.red);
    let palette = color!(palette, bright_green, p.green);
    let palette = color!(palette, bright_yellow, p.yellow);
    let palette = color!(palette, bright_blue, p.blue);
    let palette = color!(palette, bright_magenta, p.pink);
    let palette = color!(palette, bright_cyan, p.teal);
    color!(
        palette,
        bright_white,
        if dark { 0xa6adc8 } else { 0xbcc0cc }
    )
    .build()
}
