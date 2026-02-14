use pulldown_cmark::{html, Options, Parser};

/// Theme colors for HTML generation (mirrors AppTheme colors)
#[allow(dead_code)]
pub struct ThemeColors {
    pub bg_base: String,
    pub bg_surface: String,
    pub bg_overlay: String,
    pub text_primary: String,
    pub text_secondary: String,
    pub text_muted: String,
    pub accent: String,
    pub border: String,
    pub success: String,
    pub warning: String,
    pub danger: String,
    pub code_bg: String,
}

impl ThemeColors {
    pub fn dark() -> Self {
        Self {
            bg_base: "#1e1e2e".to_string(),
            bg_surface: "#181825".to_string(),
            bg_overlay: "#313244".to_string(),
            text_primary: "#cdd6f4".to_string(),
            text_secondary: "#6c7086".to_string(),
            text_muted: "#45475a".to_string(),
            accent: "#89b4fa".to_string(),
            border: "#45475a".to_string(),
            success: "#a6e3a1".to_string(),
            warning: "#f9e2af".to_string(),
            danger: "#f38ba8".to_string(),
            code_bg: "#11111b".to_string(),
        }
    }

    pub fn light() -> Self {
        Self {
            bg_base: "#eff1f5".to_string(),
            bg_surface: "#e6e9ef".to_string(),
            bg_overlay: "#dce0e8".to_string(),
            text_primary: "#4c4f69".to_string(),
            text_secondary: "#8c8fa1".to_string(),
            text_muted: "#bcc0cc".to_string(),
            accent: "#1e66f5".to_string(),
            border: "#ccd0da".to_string(),
            success: "#40a02b".to_string(),
            warning: "#df8e1d".to_string(),
            danger: "#d20f39".to_string(),
            code_bg: "#dce0e8".to_string(),
        }
    }
}

/// Render markdown content to a complete HTML document with theme styling and Mermaid support
pub fn render_markdown_to_html(content: &str, is_dark_theme: bool) -> String {
    let theme = if is_dark_theme {
        ThemeColors::dark()
    } else {
        ThemeColors::light()
    };

    // Process markdown and extract mermaid blocks
    let (processed_content, has_mermaid) = process_mermaid_blocks(content);

    // Parse markdown with GFM extensions
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;

    let parser = Parser::new_ext(&processed_content, options);

    // Convert to HTML
    let mut html_content = String::new();
    html::push_html(&mut html_content, parser);

    // Build the complete HTML document
    build_html_document(&html_content, &theme, has_mermaid, is_dark_theme)
}

/// Process content to convert ```mermaid code blocks to <pre class="mermaid">
fn process_mermaid_blocks(content: &str) -> (String, bool) {
    let mut result = String::new();
    let mut has_mermaid = false;
    let mut in_mermaid_block = false;
    let mut mermaid_content = String::new();

    for line in content.lines() {
        if line.trim() == "```mermaid" {
            in_mermaid_block = true;
            has_mermaid = true;
            mermaid_content.clear();
        } else if in_mermaid_block && line.trim() == "```" {
            in_mermaid_block = false;
            // Output as HTML that won't be escaped by pulldown-cmark
            result.push_str("\n<pre class=\"mermaid\">\n");
            result.push_str(&mermaid_content);
            result.push_str("</pre>\n\n");
        } else if in_mermaid_block {
            mermaid_content.push_str(line);
            mermaid_content.push('\n');
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }

    (result, has_mermaid)
}

/// Build a complete HTML document with styling
fn build_html_document(
    content: &str,
    theme: &ThemeColors,
    has_mermaid: bool,
    is_dark_theme: bool,
) -> String {
    let mermaid_theme = if is_dark_theme { "dark" } else { "default" };

    let mermaid_script = if has_mermaid {
        format!(
            r#"
    <script src="https://cdn.jsdelivr.net/npm/mermaid@10/dist/mermaid.min.js"></script>
    <script>
        mermaid.initialize({{
            startOnLoad: true,
            theme: '{mermaid_theme}',
            themeVariables: {{
                primaryColor: '{accent}',
                primaryTextColor: '{text_primary}',
                primaryBorderColor: '{border}',
                lineColor: '{text_secondary}',
                secondaryColor: '{bg_overlay}',
                tertiaryColor: '{bg_surface}'
            }}
        }});
    </script>"#,
            mermaid_theme = mermaid_theme,
            accent = theme.accent,
            text_primary = theme.text_primary,
            border = theme.border,
            text_secondary = theme.text_secondary,
            bg_overlay = theme.bg_overlay,
            bg_surface = theme.bg_surface,
        )
    } else {
        String::new()
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <style>
        * {{
            box-sizing: border-box;
        }}

        body {{
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            font-size: 14px;
            line-height: 1.6;
            color: {text_primary};
            background-color: {bg_base};
            margin: 0;
            padding: 8px 24px 16px 24px;
            max-width: 100%;
            overflow-x: hidden;
        }}

        h1, h2, h3, h4, h5, h6 {{
            color: {text_primary};
            margin-top: 24px;
            margin-bottom: 16px;
            font-weight: 600;
            line-height: 1.25;
        }}

        body > *:first-child {{
            margin-top: 0;
        }}

        h1 {{
            font-size: 2em;
            border-bottom: 1px solid {border};
            padding-bottom: 0.3em;
        }}

        h2 {{
            font-size: 1.5em;
            border-bottom: 1px solid {border};
            padding-bottom: 0.3em;
        }}

        h3 {{
            font-size: 1.25em;
        }}

        p {{
            margin-top: 0;
            margin-bottom: 16px;
        }}

        a {{
            color: {accent};
            text-decoration: none;
        }}

        a:hover {{
            text-decoration: underline;
        }}

        code {{
            font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Monaco, Consolas, monospace;
            font-size: 85%;
            background-color: {code_bg};
            padding: 0.2em 0.4em;
            border-radius: 4px;
        }}

        pre {{
            font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Monaco, Consolas, monospace;
            font-size: 85%;
            background-color: {code_bg};
            padding: 16px;
            border-radius: 6px;
            overflow-x: auto;
            line-height: 1.45;
        }}

        pre code {{
            background-color: transparent;
            padding: 0;
            font-size: 100%;
        }}

        pre.mermaid {{
            background-color: transparent;
            text-align: center;
            padding: 16px 0;
        }}

        blockquote {{
            margin: 0 0 16px 0;
            padding: 0 16px;
            color: {text_secondary};
            border-left: 4px solid {border};
        }}

        ul, ol {{
            margin-top: 0;
            margin-bottom: 16px;
            padding-left: 2em;
        }}

        li {{
            margin-bottom: 4px;
        }}

        li > p {{
            margin-bottom: 4px;
        }}

        /* Task lists */
        ul.contains-task-list {{
            list-style: none;
            padding-left: 0;
        }}

        li.task-list-item {{
            display: flex;
            align-items: flex-start;
            gap: 8px;
        }}

        input[type="checkbox"] {{
            margin-top: 4px;
            accent-color: {accent};
        }}

        table {{
            border-collapse: collapse;
            margin-bottom: 16px;
            width: auto;
            max-width: 100%;
            overflow-x: auto;
            display: block;
        }}

        th, td {{
            padding: 8px 16px;
            border: 1px solid {border};
        }}

        th {{
            background-color: {bg_surface};
            font-weight: 600;
        }}

        tr:nth-child(even) {{
            background-color: {bg_surface};
        }}

        hr {{
            border: none;
            border-top: 1px solid {border};
            margin: 24px 0;
        }}

        img {{
            max-width: 100%;
            height: auto;
        }}

        /* Strikethrough */
        del {{
            color: {text_secondary};
        }}

        /* Footnotes */
        .footnote-definition {{
            font-size: 85%;
            color: {text_secondary};
            margin-top: 16px;
            padding-top: 16px;
            border-top: 1px solid {border};
        }}
    </style>
    {mermaid_script}
</head>
<body>
{content}
</body>
</html>"#,
        text_primary = theme.text_primary,
        bg_base = theme.bg_base,
        border = theme.border,
        accent = theme.accent,
        code_bg = theme.code_bg,
        text_secondary = theme.text_secondary,
        bg_surface = theme.bg_surface,
        mermaid_script = mermaid_script,
        content = content,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_markdown() {
        let content = "# Hello\n\nThis is a test.";
        let html = render_markdown_to_html(content, true);
        assert!(html.contains("<h1>Hello</h1>"));
        assert!(html.contains("<p>This is a test.</p>"));
    }

    #[test]
    fn test_mermaid_detection() {
        let content = "# Test\n\n```mermaid\ngraph TD\nA --> B\n```\n";
        let (_, has_mermaid) = process_mermaid_blocks(content);
        assert!(has_mermaid);
    }

    #[test]
    fn test_no_mermaid() {
        let content = "# Test\n\n```rust\nfn main() {}\n```\n";
        let (_, has_mermaid) = process_mermaid_blocks(content);
        assert!(!has_mermaid);
    }
}
