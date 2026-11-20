use gpui::*;
use gpui_component::{
    WindowExt,
    button::{Button, ButtonVariants},
    dialog::{DialogEntrance, DialogFooter},
    input::{Input, InputState, Textarea, TextareaState},
    menu::{DropdownMenu, PopupMenuItem},
    notification::Notification,
};

pub fn notice(message: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    window.push_notification(Notification::new().message(message), cx);
}
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
                    .child("⌥/       Keyboard shortcuts")
                    .child("Drag pane tabs to move or group; drag dividers to resize.")
                    .child("Reopen closed panes from More → Show pane."),
            )
    });
}
pub fn new_task(repository: String, window: &mut Window, cx: &mut App) {
    let title = cx.new(|cx| InputState::new(window, cx).placeholder("Short outcome"));
    let objective = cx.new(|cx| {
        TextareaState::new(window, cx)
            .placeholder("What should the agent accomplish?")
            .auto_grow(3, 8)
    });
    let issue = cx.new(|cx| InputState::new(window, cx).placeholder("TRU-280 (optional)"));
    let base = cx.new(|cx| InputState::new(window, cx).default_value("HEAD"));
    let boundary = cx.new(|_| String::from("Tests pass"));
    window.open_dialog(cx, move |dialog, _, cx| {
        let base_input = base.clone();
        let boundary_input = boundary.clone();
        let title_value = title.clone();
        let field = |label: &'static str, input: &Entity<InputState>| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(label)
                .child(Input::new(input))
        };
        dialog
            .entrance(DialogEntrance::None)
            .title("New task")
            .w(px(620.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(format!("{repository} · This Mac · sample task"))
                    .child(field("TITLE", &title))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child("OBJECTIVE")
                            .child(Textarea::new(&objective)),
                    )
                    .child(field("LINEAR ISSUE", &issue))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(field("BASE", &base))
                            .child(
                                Button::new("choose-base")
                                    .label("Choose base…")
                                    .dropdown_menu(move |menu, _, _| {
                                        ["HEAD", "main", "develop"].into_iter().fold(
                                            menu,
                                            |menu, value| {
                                                let input = base_input.clone();
                                                menu.item(PopupMenuItem::new(value).on_click(
                                                    move |_, window, cx| {
                                                        input.update(cx, |s, cx| {
                                                            s.set_value(value, window, cx)
                                                        })
                                                    },
                                                ))
                                            },
                                        )
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child("STOPPING BOUNDARY")
                            .child(
                                Button::new("boundary")
                                    .label(format!("{} ▾", boundary.read(cx)))
                                    .dropdown_menu(move |menu, _, _| {
                                        ["Plan only", "Tests pass", "Draft PR"].into_iter().fold(
                                            menu,
                                            |menu, value| {
                                                let input = boundary_input.clone();
                                                menu.item(PopupMenuItem::new(value).on_click(
                                                    move |_, window, cx| {
                                                        input.update(cx, |s, cx| {
                                                            *s = value.into();
                                                            cx.notify();
                                                        });
                                                        window.refresh();
                                                    },
                                                ))
                                            },
                                        )
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .p_2()
                            .child("BRANCH / WORKTREE · preview only; nothing will be created"),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("cancel-task")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("create-task")
                            .primary()
                            .label("Create sample task")
                            .on_click(move |_, window, cx| {
                                let title = title_value.read(cx).value();
                                if title.trim().is_empty() {
                                    notice("Enter a task title", window, cx);
                                    return;
                                }
                                window.close_dialog(cx);
                                notice(
                                    format!("Sample task: {title} · no task or worktree created"),
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
            .on_cancel(|_, _, _| true)
    });
}
