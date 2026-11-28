//! The single host adapter for Iced-compatible terminal settings and UI effects.
use gitterm_terminal::{ColorPalette, TerminalConfig, TerminalLaunch, TerminalView};
use gpui::{App, ClipboardItem, Context, Edges, Global, px};
use gpui_component::Theme;

pub(crate) struct Scrollback(pub usize);
impl Global for Scrollback {}

#[derive(Clone, Copy)]
pub(crate) enum Placement {
    Main,
    Bottom,
}

pub(crate) fn config(placement: Placement, cx: &App) -> TerminalConfig {
    let theme = Theme::global(cx);
    TerminalConfig {
        font_family: theme.mono_font_family.to_string(),
        font_size: theme.mono_font_size,
        colors: ColorPalette::for_theme(theme.mode.is_dark()),
        scrollback: cx
            .try_global::<Scrollback>()
            .map_or(30_000, |value| value.0),
        // Iced view_terminal / view_bottom_panel wrap the terminal with these insets.
        padding: padding(placement),
        ..Default::default()
    }
}

fn padding(placement: Placement) -> Edges<gpui::Pixels> {
    match placement {
        Placement::Main => Edges::all(px(4.)),
        Placement::Bottom => Edges {
            top: px(2.),
            bottom: px(2.),
            left: px(0.),
            right: px(0.),
        },
    }
}

/// Dock placement, rather than pane kind, determines the inset. New, restored
/// and moved bottom terminals can all use the same pane kind as main terminals.
pub(crate) fn sync_workspace_insets(state: &crate::WorkspaceDock, cx: &mut App) {
    let bottom = crate::bottom_pane_ids(state, cx);
    for (id, pane) in &state.panes {
        let view = match &pane.read(cx).content {
            crate::pane::PaneContent::Terminal(view) => view.clone(),
            _ => continue,
        };
        let inset = padding(if bottom.contains(id) {
            Placement::Bottom
        } else {
            Placement::Main
        });
        if view.read(cx).config().padding != inset {
            let mut config = view.read(cx).config().clone();
            config.padding = inset;
            view.update(cx, |view, cx| view.update_config(config, cx));
        }
    }
}

pub(crate) fn launch(
    launch: TerminalLaunch,
    placement: Placement,
    cx: &mut Context<TerminalView>,
) -> TerminalView {
    TerminalView::launch(launch, config(placement, cx), cx)
        // The backend detects the same URL schemes as iced_term; the platform opens them.
        .with_app_url_opener(|url, cx| {
            if gitterm_core::terminal::link_scheme_allowed(url) {
                cx.open_url(url);
            }
        })
        // As in Iced's ClipboardStore handlers, forward Alacritty's decoded UTF-8 text.
        .with_clipboard_store_callback(|_, cx, text| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        })
}

/// Preview's synthetic cwd tab is a new shell, rather than a read-only view
/// of a terminal imported from Iced. Retain its single pane and session id.
pub(crate) fn start_initial_terminal(
    shell: &mut crate::Shell,
    window: &mut gpui::Window,
    cx: &mut Context<crate::Shell>,
) {
    let state = shell.active(cx);
    let session = state.active_session.expect("initial cwd session");
    let pane = state.panes[&crate::session_pane_id(session)].clone();
    let workspace = state.workspace.id;
    let cwd = shell.data.terminal_cwd(workspace, cx).expect("initial cwd");
    let data = shell.data.clone();
    pane.update(cx, |pane, cx| {
        *pane = crate::pane::Pane::new(
            pane.id.clone(),
            pane.title.clone(),
            crate::pane::PaneKind::CenterTerminal(cwd, None),
            workspace,
            data,
            window,
            cx,
        );
    });
}

#[cfg(test)]
mod tests;
