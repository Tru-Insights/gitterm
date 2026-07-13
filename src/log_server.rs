use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use warp::Filter;

const LOG_SERVER_BASE_PORT: u16 = 13_030;
const LOG_SERVER_PORTS_PER_INSTANCE: u16 = 10;

/// Snapshot of terminal content for a single tab
#[derive(Clone)]
pub struct TerminalSnapshot {
    pub tab_id: usize,
    pub tab_name: String,
    pub content: String,
}

/// Snapshot of a file being viewed
#[derive(Clone)]
pub struct FileSnapshot {
    pub file_path: String,
    pub content: String,
}

/// Shared state between the Iced app and HTTP server
#[derive(Clone)]
pub struct ServerState {
    pub terminals: Arc<RwLock<HashMap<usize, TerminalSnapshot>>>,
    pub files: Arc<RwLock<HashMap<usize, FileSnapshot>>>,
    pub shutdown: Arc<tokio::sync::Notify>,
    pub bound_port: Arc<std::sync::Mutex<Option<u16>>>,
    /// Absolute paths to the active workspace's markdown document directories,
    /// pushed by the app whenever the active workspace changes. `None` means no
    /// workspace is selected or its directory is unknown, so viewers show an
    /// empty list rather than guessing.
    pub plans_dir: Arc<std::sync::RwLock<Option<PathBuf>>>,
    pub docs_dir: Arc<std::sync::RwLock<Option<PathBuf>>>,
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            terminals: Arc::new(RwLock::new(HashMap::new())),
            files: Arc::new(RwLock::new(HashMap::new())),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            bound_port: Arc::new(std::sync::Mutex::new(None)),
            plans_dir: Arc::new(std::sync::RwLock::new(None)),
            docs_dir: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Get the base URL of the log server, if it's running
    pub fn base_url(&self) -> Option<String> {
        self.bound_port
            .lock()
            .ok()
            .and_then(|port| port.map(|p| format!("http://localhost:{}", p)))
    }
}

/// Find an available port, trying instance-specific range first, then OS-assigned
fn find_available_port() -> Option<u16> {
    // Get instance-specific port range: base + (instance_id % 100) * 10
    let instance_id: u16 = crate::config::instance_id()
        .parse::<u32>()
        .unwrap_or(0)
        .rem_euclid(100) as u16; // Limit to 100 to avoid going too high

    let base_port = LOG_SERVER_BASE_PORT + (instance_id * LOG_SERVER_PORTS_PER_INSTANCE);
    let port_range = base_port..(base_port + LOG_SERVER_PORTS_PER_INSTANCE);

    // Try instance-specific range first
    for port in port_range {
        if port < 65535 && std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }

    // Fallback: let OS assign a port
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|listener| listener.local_addr().ok().map(|addr| addr.port()))
}

/// Start the HTTP log server with graceful shutdown
pub async fn start_server(state: ServerState) {
    let shutdown = state.shutdown.clone();
    let bound_port = state.bound_port.clone();
    let plans_state = state.clone();
    let state_filter = warp::any().map(move || state.clone());

    // Route: GET / - List all tabs
    let index = warp::path::end()
        .and(state_filter.clone())
        .and_then(handle_index);

    // Route: GET /tab/{id} - Show terminal content for a specific tab
    let tab = warp::path!("tab" / usize)
        .and(state_filter.clone())
        .and_then(handle_tab);

    // Route: GET /file/{id} - Show file content for a specific tab
    let file = warp::path!("file" / usize)
        .and(state_filter.clone())
        .and_then(handle_file);

    let routes = index
        .or(tab)
        .or(file)
        .or(crate::plans_viewer::routes(plans_state));

    let Some(port) = find_available_port() else {
        eprintln!("Log server disabled: unable to bind any localhost port");
        if let Ok(mut p) = bound_port.lock() {
            *p = None;
        }
        return;
    };

    let (_addr, server) =
        warp::serve(routes).bind_with_graceful_shutdown(([127, 0, 0, 1], port), async move {
            shutdown.notified().await;
        });

    // Store the actual port so the app can reference it
    if let Ok(mut p) = bound_port.lock() {
        *p = Some(port);
    }

    println!("Log server started at http://localhost:{}", port);
    server.await;
    if let Ok(mut p) = bound_port.lock() {
        *p = None;
    }
    println!("Log server shut down");
}

/// Handler for index page - lists all tabs
async fn handle_index(state: ServerState) -> Result<impl warp::Reply, warp::Rejection> {
    let snapshots = state.terminals.read().await;

    let mut html = String::from(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>GitTerm V4 Log Viewer</title>
    <style>
        body {
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
            max-width: 1200px;
            margin: 40px auto;
            padding: 20px;
            background: #1e1e1e;
            color: #d4d4d4;
        }
        h1 {
            color: #4ec9b0;
            border-bottom: 2px solid #4ec9b0;
            padding-bottom: 10px;
        }
        .tab-list {
            list-style: none;
            padding: 0;
        }
        .tab-item {
            background: #252526;
            margin: 10px 0;
            padding: 15px 20px;
            border-radius: 5px;
            border-left: 4px solid #4ec9b0;
        }
        .tab-item a {
            color: #4ec9b0;
            text-decoration: none;
            font-size: 18px;
            font-weight: 500;
        }
        .tab-item a:hover {
            text-decoration: underline;
        }
        .tab-id {
            color: #858585;
            font-size: 14px;
            margin-left: 10px;
        }
    </style>
</head>
<body>
    <h1>GitTerm V4 Log Viewer</h1>
    <p>Select a tab to view its terminal output:</p>
    <ul class="tab-list">
"#,
    );

    let mut tabs: Vec<_> = snapshots.values().collect();
    tabs.sort_by_key(|t| t.tab_id);

    for snapshot in tabs {
        let safe_tab_name = html_escape(&snapshot.tab_name);
        html.push_str(&format!(
            r#"        <li class="tab-item">
            <a href="/tab/{}">{}</a>
            <span class="tab-id">Tab #{}</span>
        </li>
"#,
            snapshot.tab_id, safe_tab_name, snapshot.tab_id
        ));
    }

    html.push_str(
        r#"    </ul>
</body>
</html>"#,
    );

    Ok(warp::reply::html(html))
}

/// Handler for tab page - shows terminal content
async fn handle_tab(
    tab_id: usize,
    state: ServerState,
) -> Result<impl warp::Reply, warp::Rejection> {
    let snapshots = state.terminals.read().await;

    if let Some(snapshot) = snapshots.get(&tab_id) {
        let safe_tab_name = html_escape(&snapshot.tab_name);
        let html = format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>{} - GitTerm V4 Logs</title>
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
            font-size: 20px;
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
            white-space: pre-wrap;
            word-wrap: break-word;
            line-height: 1.5;
            font-size: 13px;
        }}
        .search-box {{
            display: inline-block;
            margin-left: 10px;
        }}
        .search-box input {{
            background: #3c3c3c;
            border: 1px solid #555;
            color: #d4d4d4;
            padding: 6px 12px;
            border-radius: 3px;
            font-size: 14px;
            width: 300px;
        }}
        .search-box input:focus {{
            outline: none;
            border-color: #4ec9b0;
        }}
        mark {{
            background: #ffd700;
            color: #000;
            padding: 2px;
        }}
    </style>
    <script>
        function copyToClipboard() {{
            const pre = document.getElementById('terminal-content');
            const text = pre.textContent;
            navigator.clipboard.writeText(text).then(() => {{
                const btn = document.getElementById('copy-btn');
                const oldText = btn.textContent;
                btn.textContent = 'Copied!';
                setTimeout(() => {{ btn.textContent = oldText; }}, 2000);
            }});
        }}

        function searchText() {{
            const query = document.getElementById('search-input').value;
            const content = document.getElementById('terminal-content');
            const text = content.getAttribute('data-original') || content.textContent;

            if (!content.hasAttribute('data-original')) {{
                content.setAttribute('data-original', text);
            }}

            if (query === '') {{
                content.textContent = text;
                return;
            }}

            // Escape HTML and highlight matches
            const escaped = text.replace(/[&<>"']/g, m => ({{
                '&': '&amp;',
                '<': '&lt;',
                '>': '&gt;',
                '"': '&quot;',
                "'": '&#39;'
            }})[m]);

            const regex = new RegExp(query.replace(/[.*+?^${{}}()|[\]\\]/g, '\\$&'), 'gi');
            const highlighted = escaped.replace(regex, match => `<mark>${{match}}</mark>`);

            content.innerHTML = highlighted;
        }}

        function refreshPage() {{
            location.reload();
        }}
    </script>
</head>
<body>
    <div class="header">
        <h1>{} (Tab #{})</h1>
        <a href="/">← Back to all tabs</a>
    </div>
    <div class="actions">
        <button id="copy-btn" onclick="copyToClipboard()">📋 Copy All</button>
        <button onclick="refreshPage()">🔄 Refresh</button>
        <div class="search-box">
            <input type="text" id="search-input" placeholder="Search in output..." onkeyup="searchText()">
        </div>
    </div>
    <pre id="terminal-content">{}</pre>
</body>
</html>"#,
            safe_tab_name,
            safe_tab_name,
            tab_id,
            html_escape(&snapshot.content)
        );

        Ok(warp::reply::html(html))
    } else {
        Ok(warp::reply::html(format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Tab Not Found</title>
    <style>
        body {{
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
            max-width: 800px;
            margin: 40px auto;
            padding: 20px;
            background: #1e1e1e;
            color: #d4d4d4;
            text-align: center;
        }}
        h1 {{ color: #f48771; }}
        a {{
            color: #4ec9b0;
            text-decoration: none;
        }}
        a:hover {{ text-decoration: underline; }}
    </style>
</head>
<body>
    <h1>Tab #{} Not Found</h1>
    <p><a href="/">← Back to all tabs</a></p>
</body>
</html>"#,
            tab_id
        )))
    }
}

/// Handler for file page - shows file content
async fn handle_file(
    tab_id: usize,
    state: ServerState,
) -> Result<impl warp::Reply, warp::Rejection> {
    let files = state.files.read().await;

    if let Some(file_snapshot) = files.get(&tab_id) {
        let safe_file_path = html_escape(&file_snapshot.file_path);
        let html = format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>{} - GitTerm V4 File Viewer</title>
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
            add_line_numbers(&html_escape(&file_snapshot.content))
        );

        Ok(warp::reply::html(html))
    } else {
        Ok(warp::reply::html(format!(
            r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>File Not Found</title>
    <style>
        body {{
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
            max-width: 800px;
            margin: 40px auto;
            padding: 20px;
            background: #1e1e1e;
            color: #d4d4d4;
            text-align: center;
        }}
        h1 {{ color: #f48771; }}
        a {{
            color: #4ec9b0;
            text-decoration: none;
        }}
        a:hover {{ text-decoration: underline; }}
    </style>
</head>
<body>
    <h1>No file currently viewed in Tab #{}</h1>
    <p><a href="/">← Back to all tabs</a></p>
</body>
</html>"#,
            tab_id
        )))
    }
}

/// Add line numbers to content
fn add_line_numbers(content: &str) -> String {
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

/// Escape HTML special characters
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_server_port_range_does_not_overlap_v3() {
        let v4_first = LOG_SERVER_BASE_PORT;
        let v4_last = LOG_SERVER_BASE_PORT + (99 * LOG_SERVER_PORTS_PER_INSTANCE) + 9;
        let v3_last = 3_030 + (99 * 10) + 9;
        assert!(v4_first > v3_last);
        assert_eq!(v4_last, 14_029);
    }

    // === html_escape ===

    #[test]
    fn html_escape_all_special_chars() {
        assert_eq!(
            html_escape(r#"<div class="a" data='b'>&</div>"#),
            "&lt;div class=&quot;a&quot; data=&#39;b&#39;&gt;&amp;&lt;/div&gt;"
        );
    }

    #[test]
    fn html_escape_no_special_chars() {
        assert_eq!(html_escape("hello world 123"), "hello world 123");
    }

    #[test]
    fn html_escape_empty() {
        assert_eq!(html_escape(""), "");
    }

    // === add_line_numbers ===

    #[test]
    fn add_line_numbers_single_line() {
        let result = add_line_numbers("hello");
        assert!(result.contains("1"));
        assert!(result.contains("hello"));
    }

    #[test]
    fn add_line_numbers_multi_line() {
        let result = add_line_numbers("line1\nline2\nline3");
        assert!(result.contains("1"));
        assert!(result.contains("2"));
        assert!(result.contains("3"));
        assert!(result.contains("line1"));
        assert!(result.contains("line3"));
    }

    #[test]
    fn add_line_numbers_empty() {
        let result = add_line_numbers("");
        // Empty string produces no lines from .lines(), so result is empty
        assert_eq!(result, "");
    }
}
