//! Native transcript copy parity: HTML 1841–1859, 4320–4450, and
//! d7fe92b:agent_chat.html 1796/1880 (whole-reply source/text).
use super::*;
use gpui_component::menu::PopupMenu;

const COPIED_FOR: Duration = Duration::from_millis(1200);

pub(super) fn markdown_body(row: &Row) -> String {
    let source = display_body(row);
    if row.kind == RowKind::Assistant && row.scope.is_empty() {
        crate::markdown::assistant_source(&source)
    } else {
        source
    }
}

fn inline_code_style(appearance: ChatAppearance, assistant: bool) -> StyleRefinement {
    let style = StyleRefinement::default()
        .font_family("Menlo")
        .text_size(px(appearance.size(12.5)))
        .bg(rgb(appearance.palette().block));
    if assistant {
        style.px(px(4.)).py(px(1.)).rounded(px(4.))
    } else {
        style
    }
}

pub(super) fn assistant_style(
    appearance: ChatAppearance,
    rem_size: gpui::Pixels,
) -> gpui_component::text::TextViewStyle {
    let palette = appearance.palette();
    let mut style = gpui_component::text::TextViewStyle::default()
        .paragraph_gap(gpui::rems(8. / f32::from(rem_size)))
        .heading_font_size(|level, body| {
            body * match level {
                1 => 1.3,
                2 => 1.18,
                3 => 1.06,
                4 => 1.,
                5 => 0.83,
                _ => 0.67,
            }
        })
        .code_block({
            let mut block = StyleRefinement::default()
                .px(px(12.))
                .py(px(10.))
                .rounded(px(6.))
                .bg(rgb(0x11111b))
                .border_color(rgb(palette.border))
                .font_family("Menlo")
                .text_size(px(appearance.size(12.5)))
                .line_height(gpui::relative(1.45));
            block.overflow.x = Some(gpui::Overflow::Scroll);
            block
        })
        .table(
            StyleRefinement::default()
                .rounded(px(0.))
                .border_color(rgb(palette.border)),
        )
        .table_head(StyleRefinement::default().bg(gpui::transparent_black()))
        .table_cell(
            StyleRefinement::default()
                .px(px(8.))
                .py(px(3.))
                .border_color(rgb(palette.border)),
        );
    style.heading_base_font_size = px(appearance.font_size);
    style
}

pub(super) fn assistant_quote_style(appearance: ChatAppearance) -> StyleRefinement {
    StyleRefinement::default()
        .pl(px(10.))
        .border_l(px(3.))
        .border_color(rgb(appearance.palette().strong))
        .text_color(rgb(appearance.palette().muted))
}

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
                std::iter::once(row.read(cx).text.clone()).chain(row.read(cx).edit_text.clone())
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
                std::iter::once(row.read(cx).text.clone()).chain(row.read(cx).edit_text.clone())
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
        let view = TextView::new(self.edit_text.as_ref().unwrap_or(&self.text))
            .on_link_click(super::transcript::open_link)
            .font_family(".SystemUIFont")
            .text_size(px(
                // Iced's cards (`.delivery`, `.dlg`) set 13/14.
                if self.row.kind == RowKind::Thinking
                    || self.row.delivery.is_some()
                    || self.row.delegation.is_some()
                {
                    self.appearance.size(13.)
                } else {
                    self.appearance.font_size
                },
            ))
            .line_height(gpui::relative(1.5))
            .inline_code_style(inline_code_style(
                self.appearance,
                self.row.kind == RowKind::Assistant && self.row.scope.is_empty(),
            ))
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

    fn reply_source(
        chat: &Entity<ChatView>,
        source: &str,
        cx: &mut VisualTestContext,
    ) -> Entity<RowView> {
        let row = row(chat, cx);
        row.update_in(cx, |view, _, cx| {
            let mut value = view.row.clone();
            value.body = source.into();
            view.set_row(value, cx);
        });
        draw(cx);
        row
    }

    #[gpui::test]
    fn assistant_policy_preserves_scoped_and_other_row_sources(cx: &mut TestAppContext) {
        let (chat, cx) = setup(cx);
        let source = "one\ntwo <b>raw</b> ![](https://example.invalid/a)";
        row(&chat, cx).read_with(cx, |view, _| {
            for kind in [
                RowKind::User,
                RowKind::Thinking,
                RowKind::Tool,
                RowKind::Assistant,
            ] {
                let mut row = view.row.clone();
                row.kind = kind;
                row.body = source.into();
                row.scope = "placeholder-worker".into();
                assert_eq!(markdown_body(&row), source);
                assert!(!row.find_body().contains("https://example.invalid/a"));
            }
        });
    }

    #[gpui::test]
    fn assistant_single_newlines_are_hard_breaks(cx: &mut TestAppContext) {
        let (chat, cx) = setup(cx);
        let row = reply_source(
            &chat,
            "first é\nsecond\r\nthird\rfourth\n\n`inline`\n\n```text\na\nb\n```",
            cx,
        );
        let text = row.read_with(cx, |row, cx| {
            row.text.read(cx).rendered_text().as_str().to_string()
        });
        assert!(text.contains("first é\nsecond\nthird\nfourth"), "{text:?}");
        assert!(text.contains("inline"), "{text:?}");
        assert!(text.contains("a\nb"), "fences preserve code: {text:?}");
    }

    #[gpui::test]
    fn assistant_raw_html_is_literal_in_blocks_and_inlines(cx: &mut TestAppContext) {
        let (chat, cx) = setup(cx);
        let source = "before <b title=\"x\">bold</b> after\n\n<div>\n**literal** &amp;\n</div>\n\n`<i>code</i>`";
        let row = reply_source(&chat, source, cx);
        let text = row.read_with(cx, |row, cx| {
            row.text.read(cx).rendered_text().as_str().to_string()
        });
        for expected in [
            "<b title=\"x\">bold</b>",
            "<div>",
            "**literal** &amp;",
            "</div>",
            "<i>code</i>",
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text:?}");
        }
    }

    #[gpui::test]
    fn assistant_images_are_links_or_alt_text_without_image_nodes(cx: &mut TestAppContext) {
        let (chat, cx) = setup(cx);
        let row = row(&chat, cx);
        let source = "![remote](https://example.invalid/a.png) ![](HTTPS://example.invalid/b.png) ![local](file:///placeholder.png) ![inline](data:image/png;base64,placeholder) ![reference][pic]\n\n[pic]: http://example.invalid/c.png";
        // Inspect the parser input without drawing an unadapted image (or fetching it).
        let prepared = row.read_with(cx, |row, _| {
            let mut value = row.row.clone();
            value.body = source.into();
            markdown_body(&value)
        });
        fn walk(node: &markdown::mdast::Node, links: &mut Vec<String>, text: &mut String) {
            use markdown::mdast::Node;
            assert!(
                !matches!(node, Node::Image(_) | Node::ImageReference(_)),
                "{node:?}"
            );
            if let Node::Link(link) = node {
                links.push(link.url.clone());
            }
            if let Node::Text(value) = node {
                text.push_str(&value.value);
            }
            if let Some(children) = node.children() {
                for child in children {
                    walk(child, links, text);
                }
            }
        }
        let ast = markdown::to_mdast(&prepared, &markdown::ParseOptions::gfm()).unwrap();
        let mut links = vec![];
        let mut text = String::new();
        walk(&ast, &mut links, &mut text);
        assert_eq!(
            links,
            [
                "https://example.invalid/a.png",
                "HTTPS://example.invalid/b.png",
                "http://example.invalid/c.png"
            ]
        );
        assert_eq!(
            text,
            "remote HTTPS://example.invalid/b.png local inline reference"
        );
        let row = reply_source(&chat, source, cx);
        row.read_with(cx, |row, cx| {
            let rendered = row.text.read(cx).rendered_text();
            assert!(rendered.as_str().contains("HTTPS://example.invalid/b.png"));
            assert!(
                row.row
                    .find_body()
                    .contains("HTTPS://example.invalid/b.png")
            );
        });
        row.update_in(cx, |row, window, cx| {
            row.text.update(cx, |text, cx| {
                text.focus_handle().focus(window, cx);
                text.select_all(cx);
            });
        });
        draw(cx);
        cx.simulate_keystrokes("cmd-shift-c");
        draw(cx);
        let copied = clipboard(cx);
        assert!(
            copied.contains("[remote](<https://example.invalid/a.png>)"),
            "{copied:?}"
        );
        assert!(!copied.contains("!["), "{copied:?}");
    }

    #[gpui::test]
    fn assistant_inline_code_has_iced_insets_and_radius(_cx: &mut TestAppContext) {
        let style = inline_code_style(ChatAppearance::default(), true);
        assert_eq!(style.padding.top, Some(px(1.).into()));
        assert_eq!(style.padding.bottom, Some(px(1.).into()));
        assert_eq!(style.padding.left, Some(px(4.).into()));
        assert_eq!(style.padding.right, Some(px(4.).into()));
        assert_eq!(style.corner_radii.top_left, Some(px(4.).into()));
        assert_eq!(style.corner_radii.bottom_right, Some(px(4.).into()));
    }

    #[gpui::test]
    fn assistant_exposed_markdown_styles_use_iced_css_values(_cx: &mut TestAppContext) {
        for font_size in [11., 15., 22.] {
            let appearance = ChatAppearance {
                font_size,
                light: false,
            };
            for rem_size in [16., 20.] {
                let style = assistant_style(appearance, px(rem_size));
                assert_eq!(style.paragraph_gap.to_pixels(px(rem_size)), px(8.));
                let heading = style.heading_font_size.as_ref().unwrap();
                for (level, factor) in [
                    (1, 1.3),
                    (2, 1.18),
                    (3, 1.06),
                    (4, 1.),
                    (5, 0.83),
                    (6, 0.67),
                ] {
                    assert_eq!(
                        heading(level, style.heading_base_font_size),
                        px(font_size * factor)
                    );
                }
                let block = style.code_block;
                assert_eq!(block.padding.top, Some(px(10.).into()));
                assert_eq!(block.padding.left, Some(px(12.).into()));
                assert_eq!(block.corner_radii.top_left, Some(px(6.).into()));
                assert_eq!(block.background, Some(rgb(0x11111b).into()));
                assert_eq!(block.text.font_size, Some(px(appearance.size(12.5)).into()));
                assert_eq!(block.text.line_height, Some(gpui::relative(1.45)));
                assert_eq!(block.overflow.x, Some(gpui::Overflow::Scroll));
                assert_eq!(
                    style.table_head.background,
                    Some(gpui::transparent_black().into())
                );
                assert_eq!(style.table.corner_radii.top_left, Some(px(0.).into()));
            }
            let quote = assistant_quote_style(appearance);
            assert_eq!(quote.padding.left, Some(px(10.).into()));
            assert_eq!(quote.border_widths.left, Some(px(3.).into()));
            assert_eq!(
                quote.border_color,
                Some(rgb(appearance.palette().strong).into())
            );
        }
        // Other row types keep their existing component insets.
        assert_eq!(
            inline_code_style(ChatAppearance::default(), false)
                .padding
                .top,
            None
        );
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
