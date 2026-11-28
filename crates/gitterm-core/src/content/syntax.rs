//! Iced's source viewer highlighter, shared without a frontend color type.
use super::perf_enabled;
use std::{path::Path, sync::OnceLock};
use syntect::{
    easy::HighlightLines,
    highlighting::ThemeSet,
    parsing::{SyntaxReference, SyntaxSet},
    util::LinesWithEndings,
};

#[derive(Debug, Clone)]
pub struct SyntaxHighlightSegment {
    pub text: String,
    pub color: [u8; 3],
}
#[derive(Debug, Clone)]
pub struct SyntaxHighlightLine {
    pub segments: Vec<SyntaxHighlightSegment>,
}
const MAX_SYNTAX_HIGHLIGHT_BYTES: usize = 96 * 1024;
const MAX_SYNTAX_HIGHLIGHT_LINES: usize = 1200;
const MAX_SYNTAX_HIGHLIGHT_SEGMENTS: usize = 8000;
pub const MAX_FILE_VIEW_RENDER_LINES: usize = 1200;

pub fn syntect_syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

pub fn syntect_theme_set() -> &'static ThemeSet {
    static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

pub fn syntect_theme_for(is_dark_theme: bool) -> &'static syntect::highlighting::Theme {
    let theme_set = syntect_theme_set();
    let preferred = if is_dark_theme {
        [
            "base16-eighties.dark",
            "base16-ocean.dark",
            "Solarized (dark)",
        ]
    } else {
        ["InspiredGitHub", "base16-ocean.light", "Solarized (light)"]
    };

    for name in preferred {
        if let Some(theme) = theme_set.themes.get(name) {
            return theme;
        }
    }

    theme_set
        .themes
        .values()
        .next()
        .expect("syntect default themes should not be empty")
}

pub fn syntect_syntax_for_path(path: &Path) -> &'static SyntaxReference {
    let syntax_set = syntect_syntax_set();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if let Some(syntax) = syntax_set.find_syntax_by_extension(&ext) {
            return syntax;
        }

        // Some packaged syntax sets do not register TS/TSX extensions.
        // Fall back to close JavaScript/TypeScript aliases before plain text.
        let alias_extensions: &[&str] = match ext.as_str() {
            "ts" => &["tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
            "tsx" => &["ts", "jsx", "js", "mjs", "cjs"],
            "mts" | "cts" => &["ts", "js", "mjs", "cjs"],
            "jsx" => &["js", "tsx", "ts", "mjs", "cjs"],
            "mjs" | "cjs" => &["js", "jsx", "ts", "tsx"],
            _ => &[],
        };
        for alias in alias_extensions {
            if let Some(syntax) = syntax_set.find_syntax_by_extension(alias) {
                return syntax;
            }
        }

        let alias_tokens: &[&str] = match ext.as_str() {
            "ts" | "tsx" | "mts" | "cts" => &["typescript", "ts", "tsx", "javascript", "js", "jsx"],
            "js" | "jsx" | "mjs" | "cjs" => &["javascript", "js", "jsx", "typescript", "ts", "tsx"],
            _ => &[],
        };
        for token in alias_tokens {
            if let Some(syntax) = syntax_set.find_syntax_by_token(token) {
                return syntax;
            }
        }

        let alias_names: &[&str] = match ext.as_str() {
            "ts" | "tsx" | "mts" | "cts" => &[
                "TypeScript",
                "TypeScript React",
                "JavaScript",
                "JavaScript (Babel)",
            ],
            "js" | "jsx" | "mjs" | "cjs" => &["JavaScript", "JavaScript (Babel)", "TypeScript"],
            _ => &[],
        };
        for name in alias_names {
            if let Some(syntax) = syntax_set.find_syntax_by_name(name) {
                return syntax;
            }
        }
    }

    syntax_set.find_syntax_plain_text()
}

pub fn build_syntax_highlight_lines(
    path: &Path,
    content: &str,
    is_dark_theme: bool,
) -> (Option<Vec<SyntaxHighlightLine>>, Option<String>) {
    if content.is_empty() {
        return (Some(Vec::new()), None);
    }
    let total_line_count = LinesWithEndings::from(content).count();
    let total_bytes = content.len();

    let syntax_set = syntect_syntax_set();
    let syntax = syntect_syntax_for_path(path);
    let theme = syntect_theme_for(is_dark_theme);
    let mut highlighter = HighlightLines::new(syntax, theme);
    let fallback_color = if is_dark_theme {
        [0xcd, 0xd6, 0xf4]
    } else {
        [0x4c, 0x4f, 0x69]
    };

    let mut highlighted_lines =
        Vec::with_capacity(total_line_count.min(MAX_SYNTAX_HIGHLIGHT_LINES));
    let mut parse_errors = 0usize;
    let mut highlighted_line_count = 0usize;
    let mut highlighted_bytes = 0usize;
    let mut highlighted_segment_count = 0usize;
    let mut truncation_notice: Option<String> = None;

    for line_with_ending in LinesWithEndings::from(content) {
        let line_bytes = line_with_ending.len();
        if highlighted_line_count >= MAX_SYNTAX_HIGHLIGHT_LINES {
            truncation_notice = Some(format!(
                "Syntax highlighting limited to first {} lines for performance.",
                MAX_SYNTAX_HIGHLIGHT_LINES
            ));
            break;
        }
        if highlighted_bytes + line_bytes > MAX_SYNTAX_HIGHLIGHT_BYTES {
            truncation_notice = Some(format!(
                "Syntax highlighting limited to first ~{} KB for performance.",
                MAX_SYNTAX_HIGHLIGHT_BYTES / 1024
            ));
            break;
        }

        let line = line_with_ending.trim_end_matches(['\r', '\n']);
        let mut segments = Vec::new();
        if let Ok(ranges) = highlighter.highlight_line(line_with_ending, syntax_set) {
            for (style, token) in ranges {
                let token = token.trim_end_matches(['\r', '\n']);
                if token.is_empty() {
                    continue;
                }
                segments.push(SyntaxHighlightSegment {
                    text: token.to_string(),
                    color: [style.foreground.r, style.foreground.g, style.foreground.b],
                });
            }
        } else {
            parse_errors += 1;
        }

        highlighted_segment_count += segments.len();
        if highlighted_segment_count > MAX_SYNTAX_HIGHLIGHT_SEGMENTS {
            truncation_notice = Some(format!(
                "Syntax highlighting limited to first {} tokens for performance.",
                MAX_SYNTAX_HIGHLIGHT_SEGMENTS
            ));
            break;
        }

        if segments.is_empty() {
            let fallback_text = if line.is_empty() { " " } else { line };
            segments.push(SyntaxHighlightSegment {
                text: fallback_text.to_string(),
                color: fallback_color,
            });
        }

        highlighted_line_count += 1;
        highlighted_bytes += line_bytes;
        highlighted_lines.push(SyntaxHighlightLine { segments });
    }

    perf_log!(
        "syntect path={} syntax={} lines={} highlighted_lines={} bytes={} highlighted_bytes={} parse_errors={} truncated={}",
        path.display(),
        syntax.name,
        total_line_count,
        highlighted_lines.len(),
        total_bytes,
        highlighted_bytes,
        parse_errors,
        truncation_notice.is_some()
    );
    (Some(highlighted_lines), truncation_notice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_iced_source_highlighter_preserves_aliases_colors_blank_lines_and_budgets() {
        for name in ["example.ts", "example.tsx", "example.mts", "example.cts"] {
            assert_ne!(syntect_syntax_for_path(Path::new(name)).name, "Plain Text");
        }
        for dark in [true, false] {
            let (lines, notice) =
                build_syntax_highlight_lines(Path::new("source.rs"), "fn main() {}\r\n\r\n", dark);
            assert!(notice.is_none());
            let lines = lines.unwrap();
            assert_eq!(lines.len(), 2);
            assert_eq!(
                lines[0]
                    .segments
                    .iter()
                    .map(|segment| segment.text.as_str())
                    .collect::<String>(),
                "fn main() {}"
            );
            assert_eq!(lines[1].segments[0].text, " ");
            let expected = if dark {
                [0xcd, 0xd6, 0xf4]
            } else {
                [0x4c, 0x4f, 0x69]
            };
            assert_eq!(lines[1].segments[0].color, expected);
        }
        let (lines, notice) = build_syntax_highlight_lines(
            Path::new("plain.txt"),
            &"x".repeat(MAX_SYNTAX_HIGHLIGHT_BYTES + 1),
            true,
        );
        assert!(lines.unwrap().is_empty());
        assert_eq!(
            notice.as_deref(),
            Some("Syntax highlighting limited to first ~96 KB for performance.")
        );
        let (lines, notice) = build_syntax_highlight_lines(
            Path::new("plain.txt"),
            &"line\n".repeat(MAX_SYNTAX_HIGHLIGHT_LINES + 1),
            true,
        );
        assert_eq!(lines.unwrap().len(), MAX_SYNTAX_HIGHLIGHT_LINES);
        assert_eq!(
            notice.as_deref(),
            Some("Syntax highlighting limited to first 1200 lines for performance.")
        );
    }
}
