use gpui::*;
use gpui_component::{WindowExt, dialog::DialogEntrance};

pub fn shortcuts(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, _, _| {
        // Keep modal visibility and the root's single dim layer in the same frame
        // as native content restoration; active-dialog excludes exit animations.
        dialog
            .entrance(DialogEntrance::None)
            .title("Keyboard shortcuts")
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child("⌃1–9    Switch workspace")
                    .child("⌘1–9    Switch session")
                    .child("⌥/       Keyboard shortcuts"),
            )
    });
}
