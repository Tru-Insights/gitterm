//! The standalone file page: the HTML GitTerm's content server serves at
//! `/file/{id}`, also written to a temp file by hosts without a server.

/// Escape HTML special characters.
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Add line numbers to (already escaped) content.
pub fn add_line_numbers(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let max_line = lines.len();
    let width = max_line.to_string().len();

    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            format!(
                "<span class=\"line-numbers\">{:width$}</span>{}",
                i + 1,
                line,
                width = width
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The page for `content` of the file at `file_path`.
pub fn file_page_html(file_path: &str, content: &str) -> String {
    let safe_file_path = html_escape(file_path);
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>{} - GitTerm V6 File Viewer</title>
<style>
    body {{
        font-family: 'Monaco', 'Menlo', 'Courier New', monospace;
        margin: 0;
        padding: 20px;
        background: #1e1e1e;
        color: #d4d4d4;
    }}
    .header {{
        position: sticky;
        top: 0;
        background: #252526;
        padding: 15px 20px;
        margin: -20px -20px 20px -20px;
        border-bottom: 2px solid #4ec9b0;
        display: flex;
        justify-content: space-between;
        align-items: center;
    }}
    .header h1 {{
        margin: 0;
        color: #4ec9b0;
        font-size: 18px;
        font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
    }}
    .header a {{
        color: #4ec9b0;
        text-decoration: none;
        font-size: 14px;
    }}
    .header a:hover {{
        text-decoration: underline;
    }}
    .actions {{
        margin-bottom: 20px;
        background: #252526;
        padding: 10px 15px;
        border-radius: 5px;
    }}
    .actions button {{
        background: #4ec9b0;
        color: #1e1e1e;
        border: none;
        padding: 8px 16px;
        border-radius: 3px;
        cursor: pointer;
        font-size: 14px;
        margin-right: 10px;
    }}
    .actions button:hover {{
        background: #5fd9c0;
    }}
    pre {{
        background: #252526;
        padding: 20px;
        border-radius: 5px;
        overflow-x: auto;
        white-space: pre;
        line-height: 1.5;
        font-size: 13px;
    }}
    .line-numbers {{
        display: inline-block;
        color: #858585;
        padding-right: 15px;
        border-right: 1px solid #3c3c3c;
        margin-right: 15px;
        user-select: none;
    }}
</style>
<script>
    function copyToClipboard() {{
        const pre = document.getElementById('file-content');
        const text = pre.textContent;
        navigator.clipboard.writeText(text).then(() => {{
            const btn = document.getElementById('copy-btn');
            const oldText = btn.textContent;
            btn.textContent = 'Copied!';
            setTimeout(() => {{ btn.textContent = oldText; }}, 2000);
        }});
    }}
</script>
</head>
<body>
<div class="header">
    <h1>{}</h1>
    <a href="/">← Back to all tabs</a>
</div>
<div class="actions">
    <button id="copy-btn" onclick="copyToClipboard()">📋 Copy All</button>
</div>
<pre id="file-content">{}</pre>
</body>
</html>"#,
        safe_file_path,
        safe_file_path,
        add_line_numbers(&html_escape(content))
    )
}
