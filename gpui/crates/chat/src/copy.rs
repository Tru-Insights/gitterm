//! Native transcript copy parity: HTML 1841–1859, 4320–4450, and
//! d7fe92b:agent_chat.html 1796/1880 (whole-reply source/text).
use super::*;
use gpui_component::menu::PopupMenu;

const COPIED_FOR: Duration = Duration::from_millis(1200);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum CopyTarget {
    Code(usize),
    #[cfg(test)]
    Reply,
}

impl ChatView {
    pub(super) fn copy(&mut self, markdown: bool, window: &mut Window, cx: &mut Context<Self>) {
        // Let the find input keep the platform's normal editing clipboard behavior.
        if window.has_focused_input(cx) {
            cx.propagate();
            return;
        }
        self.copy_selection(markdown, window, cx);
    }
    pub(super) fn copy_selection(
        &mut self,
        markdown: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.selected_payload(markdown, window, cx);
        self.copy_selected_payload(text, cx);
    }

    fn copy_selected_payload(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.selection_copied = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            _ = this.update(cx, |view, cx| {
                view.selection_copied = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn selected_payload(&self, markdown: bool, window: &mut Window, cx: &mut App) -> String {
        // Source reconstructs selected formatting (bold, lists, fences, links),
        // rather than treating rendered byte offsets as offsets into Markdown.
        for row in self.row_views.iter().flatten() {
            for state in
                std::iter::once(row.read(cx).text.clone()).chain(row.read(cx).output_text.clone())
            {
                state.update(cx, |text, cx| {
                    text.set_selection_format(
                        if markdown {
                            SelectionFormat::Source
                        } else {
                            SelectionFormat::Plain
                        },
                        cx,
                    );
                });
            }
        }
        let text = TextSelection::selected_text(window, cx);
        for row in self.row_views.iter().flatten() {
            for state in
                std::iter::once(row.read(cx).text.clone()).chain(row.read(cx).output_text.clone())
            {
                state.update(cx, |text, cx| {
                    text.set_selection_format(SelectionFormat::Plain, cx)
                });
            }
        }
        if markdown {
            text
        } else {
            text.trim().to_owned()
        }
    }

    pub(super) fn selection_menu(
        owner: gpui::WeakEntity<Self>,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut App,
    ) -> PopupMenu {
        // HTML captures on contextmenu before focus moves into the popup.
        let (plain, markdown) = owner
            .update(cx, |view, cx| {
                (
                    view.selected_payload(false, window, cx),
                    view.selected_payload(true, window, cx),
                )
            })
            .unwrap_or_default();
        let plain_owner = owner.clone();
        menu.item(
            PopupMenuItem::new("Copy")
                .disabled(plain.is_empty())
                .on_click(move |_, _, cx| {
                    _ = plain_owner
                        .update(cx, |view, cx| view.copy_selected_payload(plain.clone(), cx));
                }),
        )
        .item(
            PopupMenuItem::new("Copy as Markdown")
                .disabled(markdown.is_empty())
                .on_click(move |_, _, cx| {
                    _ = owner.update(cx, |view, cx| {
                        view.copy_selected_payload(markdown.clone(), cx)
                    });
                }),
        )
    }
}

impl RowView {
    fn copied_payload(&mut self, target: CopyTarget, payload: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(payload));
        // Replacing the task cancels the earlier expiry on repeated copy.
        let expiry = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            _ = this.update(cx, |view, cx| {
                view.copied.remove(&target);
                cx.notify();
            });
        });
        self.copied.insert(target, expiry);
        cx.notify();
    }

    pub(super) fn copy_code(&mut self, key: usize, code: String, cx: &mut Context<Self>) {
        self.copied_payload(CopyTarget::Code(key), code, cx);
    }

    #[cfg(test)]
    fn copy_reply(&mut self, markdown: bool, cx: &mut Context<Self>) {
        // Row source is synchronized with the model, independently of selection,
        // asynchronous text layout, or a prior rich-row entity's lifetime.
        let payload = if markdown {
            self.row.body.clone()
        } else {
            plain_markdown(&self.row.body).trim_matches('\n').to_owned()
        };
        self.copied_payload(CopyTarget::Reply, payload, cx);
    }

    pub(super) fn copyable_text(&self, cx: &mut Context<Self>) -> TextView {
        let owner = cx.entity().downgrade();
        let copied: HashSet<_> = self.copied.keys().copied().collect();
        let view = TextView::new(self.output_text.as_ref().unwrap_or(&self.text))
            .on_link_click(super::transcript::open_link)
            // Iced's `.error-block` and legacy `pre` are monospace.
            .font_family(match self.row.kind {
                RowKind::Error | RowKind::Legacy(_) => "Menlo",
                _ => ".SystemUIFont",
            })
            .text_size(px(
                // Iced's cards (`.delivery`, `.dlg`) set 13/14.
                if self.row.kind == RowKind::Thinking
                    || matches!(self.row.kind, RowKind::Legacy(_))
                    || self.row.delivery.is_some()
                    || self.row.delegation.is_some()
                {
                    self.appearance.size(13.)
                } else if self.row.kind == RowKind::Error {
                    self.appearance.size(12.5)
                } else if matches!(self.row.kind, RowKind::Status | RowKind::Warning) {
                    self.appearance.size(12.)
                } else {
                    self.appearance.font_size
                },
            ))
            .line_height(gpui::relative(1.5))
            .inline_code_style(
                StyleRefinement::default()
                    .font_family("Menlo")
                    .text_size(px(self.appearance.size(12.5)))
                    .bg(rgb(self.appearance.palette().block)),
            )
            .selectable(true)
            .scrollable(false)
            .stream_fade(true)
            .style(
                gpui_component::text::TextViewStyle::default()
                    .table(
                        StyleRefinement::default()
                            .border_color(rgb(self.appearance.palette().border)),
                    )
                    .table_head(StyleRefinement::default().bg(rgb(self.appearance.palette().alt)))
                    .table_cell(
                        StyleRefinement::default()
                            .px(px(8.))
                            .py(px(3.))
                            .border_color(rgb(self.appearance.palette().border)),
                    ),
            )
            .code_block_actions(move |block, _, _| {
                let mut hash = std::hash::DefaultHasher::new();
                block
                    .span
                    .map(|span| (span.start, span.end))
                    .hash(&mut hash);
                block.code().hash(&mut hash);
                block.lang().hash(&mut hash);
                let key = hash.finish() as usize;
                let code = block.code().to_string();
                let owner = owner.clone();
                div()
                    .id(("copy-code-hover", key))
                    .opacity(if copied.contains(&CopyTarget::Code(key)) {
                        1.
                    } else {
                        0.
                    })
                    .group_hover("chat-row", |style| style.opacity(1.))
                    .child(
                        Button::new(("copy-code", key))
                            .debug_selector({
                                let owner = owner.clone();
                                move || format!("copy-code-{}", owner.entity_id())
                            })
                            .small()
                            .label(if copied.contains(&CopyTarget::Code(key)) {
                                "Copied"
                            } else {
                                "Copy"
                            })
                            .on_click(move |_, _, cx| {
                                _ = owner
                                    .update(cx, |view, cx| view.copy_code(key, code.clone(), cx));
                            }),
                    )
            });
        #[cfg(test)]
        let view = view.table_actions(|_, _, _| {
            div()
                .debug_selector(|| "chat-markdown-table".into())
                .size(px(1.))
        });
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    const REPLY: &str = "**Bold é** and [a link](https://example.com)\n\n- first\n- second `inline`\n\n```rust\n  let x = 1;  \n\t// é\n```";

    fn setup(cx: &mut TestAppContext) -> (Entity<ChatView>, &mut VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
        });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let chat = cx.new(|cx| {
                ChatView::new(
                    vec![Record {
                        at_ms: None,
                        entry: Entry::Harness(HarnessEvent::TextDelta(REPLY.into())),
                    }],
                    window,
                    cx,
                )
            });
            Root::new(chat, window, cx)
        });
        let chat = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<ChatView>().unwrap()
        });
        draw(cx);
        (chat, cx)
    }
    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        cx.run_until_parked();
    }
    fn clipboard(cx: &mut VisualTestContext) -> String {
        // TestAppContext installs GPUI's in-memory test platform clipboard.
        cx.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap())
    }
    fn row(chat: &Entity<ChatView>, cx: &mut VisualTestContext) -> Entity<RowView> {
        chat.read_with(cx, |view, _| view.row_views[0].as_ref().unwrap().clone())
    }
    fn click(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        draw(cx);
    }

    #[gpui::test]
    fn no_per_reply_buttons_and_copy_source_remains_independent_of_selection(
        cx: &mut TestAppContext,
    ) {
        let (chat, cx) = setup(cx);
        let row = row(&chat, cx);
        let text = row.read_with(cx, |row, _| row.text.clone());
        text.update_in(cx, |text, _, cx| text.select_all(cx));
        draw(cx);
        let selected = text.read_with(cx, |text, _| text.selected_source_range());
        row.update_in(cx, |row, _, cx| row.copy_reply(true, cx));
        assert_eq!(
            text.read_with(cx, |text, _| text.selected_source_range()),
            selected
        );
        assert!(cx.debug_bounds("copy-reply-markdown").is_none());
        assert!(cx.debug_bounds("copy-reply-text").is_none());
        row.update_in(cx, |row, _, cx| row.copy_reply(true, cx));
        assert_eq!(clipboard(cx), REPLY);
        assert!(row.read_with(cx, |row, _| row.copied.contains_key(&CopyTarget::Reply)));
        row.update_in(cx, |row, _, cx| row.copy_reply(false, cx));
        assert_eq!(
            clipboard(cx),
            "Bold é and a link\nfirst\n\nsecond inline\n\n  let x = 1;  \n\t// é"
        );
        cx.executor().advance_clock(COPIED_FOR);
        draw(cx);
        assert!(!row.read_with(cx, |row, _| row.copied.contains_key(&CopyTarget::Reply)));
    }

    #[gpui::test]
    fn selection_plain_and_markdown_keep_bold_lists_code_and_links(cx: &mut TestAppContext) {
        let (chat, cx) = setup(cx);
        let text = row(&chat, cx).read_with(cx, |row, _| row.text.clone());
        text.update_in(cx, |text, window, cx| {
            text.focus_handle().focus(window, cx);
            text.select_all(cx);
        });
        draw(cx);
        cx.simulate_keystrokes("cmd-shift-c");
        draw(cx);
        let markdown = clipboard(cx);
        for expected in [
            "**Bold é**",
            "[a link](https://example.com)",
            "- first",
            "- second `inline`",
            "```rust",
            "  let x = 1;  ",
        ] {
            assert!(
                markdown.contains(expected),
                "missing {expected:?} in {markdown:?}"
            );
        }
        cx.simulate_keystrokes("cmd-c");
        draw(cx);
        let plain = clipboard(cx);
        assert!(
            plain.contains("Bold é and a link") && plain.contains("second inline"),
            "{plain:?}"
        );
        for markup in ["**", "https://", "```", "`inline`", "- first"] {
            assert!(!plain.contains(markup), "{plain:?}");
        }
        assert!(chat.read_with(cx, |view, _| view.selection_copied.is_some()));
        // Source mode must have been restored to Plain, without clearing selection.
        assert!(
            !text
                .read_with(cx, |text, _| text.selected_text())
                .contains("**")
        );
        cx.executor().advance_clock(COPIED_FOR);
        draw(cx);
        assert!(chat.read_with(cx, |view, _| view.selection_copied.is_none()));
    }

    #[gpui::test]
    fn code_button_copies_exact_whitespace_and_repeated_copy_extends_feedback(
        cx: &mut TestAppContext,
    ) {
        let (chat, cx) = setup(cx);
        let row = row(&chat, cx);
        let selector = Box::leak(format!("copy-code-{}", row.entity_id()).into_boxed_str());
        click(selector, cx);
        assert_eq!(clipboard(cx), "  let x = 1;  \n\t// é");
        assert_eq!(row.read_with(cx, |row, _| row.copied.len()), 1);
        cx.executor().advance_clock(Duration::from_millis(800));
        draw(cx);
        click(selector, cx);
        cx.executor().advance_clock(Duration::from_millis(500));
        draw(cx);
        assert_eq!(row.read_with(cx, |row, _| row.copied.len()), 1);
        cx.executor().advance_clock(Duration::from_millis(700));
        draw(cx);
        assert!(row.read_with(cx, |row, _| row.copied.is_empty()));
    }

    #[gpui::test]
    fn reply_source_survives_virtual_row_eviction_and_stream_updates(cx: &mut TestAppContext) {
        let (chat, cx) = setup(cx);
        let original = row(&chat, cx).downgrade();
        original
            .update(cx, |row, cx| {
                row.text.update(cx, |text, cx| text.select_all(cx))
            })
            .unwrap();
        draw(cx);
        chat.update_in(cx, |view, window, cx| {
            for n in 0..110 {
                view.append_record(
                    &Record {
                        at_ms: None,
                        entry: Entry::UserPrompt(format!("row {n}")),
                    },
                    cx,
                );
                view.row(view.model.rows.len() - 1, window, cx);
            }
            assert!(view.row_views[0].is_none());
            assert!(original.upgrade().is_none());
            let restored = view.row(0, window, cx);
            restored.update(cx, |row, cx| {
                assert!(
                    row.text.read(cx).selected_source_range().is_none(),
                    "eviction releases selection"
                );
                row.copy_reply(true, cx);
            });
        });
        assert_eq!(clipboard(cx), REPLY);
        chat.update_in(cx, |view, window, cx| {
            view.append_event(HarnessEvent::TextDelta("**streamed**".into()), cx);
            let last = view.row(view.model.rows.len() - 1, window, cx);
            view.append_event(HarnessEvent::TextDelta(" more".into()), cx);
            last.update(cx, |row, cx| row.copy_reply(true, cx));
        });
        assert_eq!(clipboard(cx), "**streamed** more");
    }
    #[gpui::test]
    fn context_menu_copies_plain_and_markdown_snapshots(cx: &mut TestAppContext) {
        let (chat, cx) = setup(cx);
        let text = row(&chat, cx).read_with(cx, |row, _| row.text.clone());
        for (markdown, keys) in [(false, "down enter"), (true, "down down enter")] {
            text.update_in(cx, |text, window, cx| {
                text.focus_handle().focus(window, cx);
                text.select_all(cx);
            });
            draw(cx);
            let bounds = text.read_with(cx, |text, _| text.bounds());
            cx.simulate_mouse_down(
                bounds.center(),
                gpui::MouseButton::Right,
                gpui::Modifiers::none(),
            );
            cx.simulate_mouse_up(
                bounds.center(),
                gpui::MouseButton::Right,
                gpui::Modifiers::none(),
            );
            draw(cx);
            // Menu payload must survive selection changes after opening.
            cx.update(TextSelection::clear);
            cx.simulate_keystrokes(keys);
            draw(cx);
            let copied = clipboard(cx);
            if markdown {
                assert!(
                    copied.contains("**Bold é**")
                        && copied.contains("[a link](https://example.com)"),
                    "{copied:?}"
                );
            } else {
                assert!(
                    copied.contains("Bold é and a link")
                        && !copied.contains("**")
                        && !copied.contains("https://"),
                    "{copied:?}"
                );
            }
        }
    }
}
