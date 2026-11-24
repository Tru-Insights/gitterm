//! Iced's marked renderer policy, adapted before the native Markdown parser.
use markdown::mdast::Node;
use std::{collections::HashMap, ops::Range};

fn label_end(source: &str) -> Option<usize> {
    let start = if source.starts_with("![") { 2 } else { 1 };
    let mut depth = 1;
    let mut escaped = false;
    for (offset, ch) in source.char_indices().skip(start) {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '[' {
            depth += 1;
        } else if ch == ']' {
            depth -= 1;
            if depth == 0 {
                return Some(offset);
            }
        }
    }
    None
}

fn without_quote_markers(source: &str, quotes: usize) -> String {
    // Continuation lines include the enclosing quote's source markers, which
    // marked removes before tokenizing inline images.
    source
        .split(['\r', '\n'])
        .enumerate()
        .map(|(line, text)| {
            let mut text = text;
            if line > 0 {
                for _ in 0..quotes {
                    if let Some(rest) = text.trim_start().strip_prefix('>') {
                        text = rest;
                    }
                }
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// marked's image token keeps the label's Markdown punctuation and escapes HTML
// before handing it to Iced's renderer (which escapes it again). mdast::Image's
// alt has already lost that information, so recover it from the source span.
fn image_alt(source: &str, fallback: &str, quotes: usize) -> String {
    let label = label_end(source).map_or(fallback, |end| &source[2..end]);
    let label = without_quote_markers(label, quotes).replace('\n', " ");
    let label = label.replace("\\[", "[").replace("\\]", "]");
    let mut out = String::new();
    for (offset, ch) in label.char_indices() {
        let entity = || {
            let rest = &label[offset + 1..];
            let Some(end) = rest
                .bytes()
                .position(|ch| !ch.is_ascii_alphanumeric() && ch != b'_' && ch != b'#')
            else {
                return false;
            };
            if rest.as_bytes()[end] != b';' {
                return false;
            }
            let name = &rest[..end];
            if let Some(digits) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                (1..=6).contains(&digits.len()) && digits.bytes().all(|ch| ch.is_ascii_hexdigit())
            } else if let Some(digits) = name.strip_prefix('#') {
                (1..=7).contains(&digits.len()) && digits.bytes().all(|ch| ch.is_ascii_digit())
            } else {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
            }
        };
        match ch {
            '&' if !entity() => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\t' | '\x0c' | ' ' => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

fn image_url(source: &str, fallback: &str) -> String {
    let Some(end) = label_end(source) else {
        return fallback.into();
    };
    let after = source[end + 1..].trim_start();
    let Some(after) = after.strip_prefix('(').or_else(|| after.strip_prefix(':')) else {
        return fallback.into();
    };
    let after = after.trim_start();
    let angle = after.starts_with('<');
    let after = if angle { &after[1..] } else { after };
    let mut depth = 0;
    let mut escaped = false;
    let mut end = after.len();
    for (offset, ch) in after.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if (angle && ch == '>')
            || (!angle && (ch.is_ascii_whitespace() || (ch == ')' && depth == 0)))
        {
            end = offset;
            break;
        }
        if !angle {
            if ch == '(' {
                depth += 1;
            }
            if ch == ')' {
                depth -= 1;
            }
        }
    }
    let mut url = String::new();
    let mut chars = after[..end].chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek().is_some_and(char::is_ascii_punctuation) {
            url.push(chars.next().unwrap());
        } else {
            url.push(ch);
        }
    }
    url
}

pub(crate) fn assistant_source(source: &str) -> String {
    match markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        Ok(ast) => prepare(source, &ast),
        Err(_) => crate::model::literal(source),
    }
}

fn prepare(source: &str, ast: &Node) -> String {
    fn definitions<'a>(
        node: &'a Node,
        source: &str,
        out: &mut HashMap<&'a str, String>,
        quotes: usize,
    ) {
        if let Node::Definition(def) = node {
            let raw = def
                .position
                .as_ref()
                .map_or("", |pos| &source[pos.start.offset..pos.end.offset]);
            out.entry(&def.identifier)
                .or_insert_with(|| image_url(&without_quote_markers(raw, quotes), &def.url));
        }
        if let Some(children) = node.children() {
            for child in children {
                definitions(
                    child,
                    source,
                    out,
                    quotes + usize::from(matches!(node, Node::Blockquote(_))),
                );
            }
        }
    }
    fn image(alt: &str, url: &str) -> String {
        let http = url
            .get(..5)
            .is_some_and(|s| s.eq_ignore_ascii_case("http:"))
            || url
                .get(..6)
                .is_some_and(|s| s.eq_ignore_ascii_case("https:"));
        if http {
            // An angle destination preserves spaces; escaping brackets keeps the
            // alt label literal, just like marked's escapeHtml override.
            format!(
                "[{}](<{}>)",
                crate::model::literal(if alt.is_empty() { url } else { alt }),
                url.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('\\', "&#92;")
            )
        } else {
            crate::model::literal(alt)
        }
    }
    fn walk(
        node: &Node,
        source: &str,
        definitions: &HashMap<&str, String>,
        edits: &mut Vec<(Range<usize>, String)>,
        quotes: usize,
    ) {
        let Some(position) = node.position() else {
            return;
        };
        let range = position.start.offset..position.end.offset;
        match node {
            Node::Html(html) => {
                edits.push((range, crate::model::literal(&html.value)));
                return;
            }
            Node::Image(img) => {
                let raw = &source[range.clone()];
                let alt = image_alt(raw, &img.alt, quotes);
                edits.push((range, image(&alt, &image_url(raw, &img.url))));
                return;
            }
            Node::ImageReference(img) => {
                if let Some(url) = definitions.get(img.identifier.as_str()) {
                    let alt = image_alt(&source[range.clone()], &img.alt, quotes);
                    edits.push((range, image(&alt, url)));
                }
                return;
            }
            Node::Text(_) => {
                for (offset, ch) in source[range.clone()].char_indices() {
                    if ch == '\n'
                        || (ch == '\r' && !source[range.start + offset..].starts_with("\r\n"))
                    {
                        let end = range.start + offset;
                        let start = if ch == '\n' && source[..end].ends_with('\r') {
                            end - 1
                        } else {
                            end
                        };
                        edits.push((start..end + 1, "  \n".into()));
                    }
                }
            }
            _ => {}
        }
        if let Some(children) = node.children() {
            for child in children {
                walk(
                    child,
                    source,
                    definitions,
                    edits,
                    quotes + usize::from(matches!(node, Node::Blockquote(_))),
                );
            }
        }
    }
    let mut refs = HashMap::new();
    definitions(ast, source, &mut refs, 0);
    let mut edits = vec![];
    walk(ast, source, &refs, &mut edits, 0);
    let mut result = String::with_capacity(source.len());
    let mut cursor = 0;
    for (range, replacement) in edits {
        result.push_str(&source[cursor..range.start]);
        result.push_str(&replacement);
        cursor = range.end;
    }
    result.push_str(&source[cursor..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_leaves_markdown_and_html_code_samples_unchanged() {
        let source = "```md\n![example](https://example.invalid/a) <b>raw</b>\n```\n\n`![inline](data:placeholder)`";
        assert_eq!(assistant_source(source), source);
    }

    #[test]
    fn image_link_keeps_marked_label_and_url_query() {
        fn link(node: &Node) -> Option<&markdown::mdast::Link> {
            if let Node::Link(link) = node {
                return Some(link);
            }
            node.children()?.iter().find_map(link)
        }
        // Expected labels/destinations checked against assets/vendor/marked.min.js.
        for (source, expected_label, expected_url) in [
            (
                "![a &amp; b *literal*](https://example.invalid/a?x=1&y=2 \"title\")",
                "a &amp; b *literal*",
                "https://example.invalid/a?x=1&y=2",
            ),
            (
                "![a & b <b> \"q\"](https://example.invalid/a)",
                "a &amp; b &lt;b&gt; &quot;q&quot;",
                "https://example.invalid/a",
            ),
            (
                "> ![a\n> b](https://example.invalid/a)",
                "a b",
                "https://example.invalid/a",
            ),
            (
                "> > ![a\n> > b](https://example.invalid/a)",
                "a b",
                "https://example.invalid/a",
            ),
            (
                "![ref &amp; *b*][pic]\n\n[pic]: https://example.invalid/a?x=1&amp;y=2",
                "ref &amp; *b*",
                "https://example.invalid/a?x=1&amp;y=2",
            ),
            (
                "> ![alt][pic]\n>\n> [pic]:\n>   https://example.invalid/a?x=1&amp;y=2",
                "alt",
                "https://example.invalid/a?x=1&amp;y=2",
            ),
            (
                r"![a](https://example.invalid/a\\*)",
                "a",
                r"https://example.invalid/a\*",
            ),
        ] {
            let prepared = assistant_source(source);
            let ast = markdown::to_mdast(&prepared, &markdown::ParseOptions::gfm()).unwrap();
            assert_eq!(link(&ast).expect(source).url, expected_url, "{source:?}");
            assert_eq!(
                crate::model::plain_markdown(&prepared).trim(),
                expected_label,
                "{source:?}"
            );
        }
    }
}
