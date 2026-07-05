use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Truncate a string to at most `max_bytes` bytes at a valid UTF-8 char boundary.
fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapture {
    pub version: u32,
    #[serde(default)]
    pub source: Option<String>,
    pub commit_hash: String,
    #[serde(default)]
    pub commit_hash_full: Option<String>,
    pub branch: String,
    pub repo: String,
    #[serde(default)]
    pub message: Option<String>,
    pub session_ref: Option<SessionRef>,
    pub models: HashMap<String, ModelUsage>,
    pub files: FileActivity,
    #[serde(default)]
    pub diff_stats: Option<DiffStats>,
    pub tokens: TokenUsage,
    pub estimated_api_cost_usd: f64,
    pub billing: String,
    pub turns: u32,
    pub user_prompts: UserPrompts,
    pub errors: ErrorInfo,
    pub compactions: u32,
    pub duration: AgentDuration,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRef {
    pub file: String,
    pub session_id: String,
    pub entry_range: [String; 2],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsage {
    pub provider: String,
    pub model: String,
    pub turns: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileActivity {
    pub read: Vec<String>,
    pub modified: Vec<String>,
    pub created: Vec<String>,
    #[serde(default)]
    pub deleted: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffStats {
    pub lines_added: u64,
    pub lines_removed: u64,
    pub files_changed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPrompts {
    pub count: u32,
    pub texts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub count: u32,
    pub recovered: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDuration {
    pub wall_clock_seconds: u64,
    pub agent_active_seconds: u64,
}

impl AgentCapture {
    pub fn total_tokens(&self) -> u64 {
        self.tokens.input + self.tokens.output
    }

    pub fn total_files(&self) -> usize {
        self.files.read.len()
            + self.files.modified.len()
            + self.files.created.len()
            + self.files.deleted.len()
    }

    pub fn primary_model(&self) -> Option<(&String, &ModelUsage)> {
        self.models.iter().max_by_key(|(_, usage)| usage.turns)
    }

    pub fn timestamp_parsed(&self) -> Option<SystemTime> {
        chrono::DateTime::parse_from_rfc3339(&self.timestamp)
            .ok()
            .map(|dt| SystemTime::UNIX_EPOCH + Duration::from_secs(dt.timestamp() as u64))
    }

    pub fn short_hash(&self) -> &str {
        &self.commit_hash[..8.min(self.commit_hash.len())]
    }

    pub fn format_cost(&self) -> String {
        if self.estimated_api_cost_usd < 0.01 {
            format!("${:.4}", self.estimated_api_cost_usd)
        } else if self.estimated_api_cost_usd < 1.0 {
            format!("${:.3}", self.estimated_api_cost_usd)
        } else {
            format!("${:.2}", self.estimated_api_cost_usd)
        }
    }

    pub fn format_duration(&self) -> String {
        let seconds = self.duration.wall_clock_seconds;
        if seconds < 60 {
            format!("{}s", seconds)
        } else if seconds < 3600 {
            format!("{}m {}s", seconds / 60, seconds % 60)
        } else {
            format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
        }
    }

    pub fn activity_intensity(&self) -> f32 {
        // Calculate how active the agent was (0.0-1.0)
        let activity_ratio = if self.duration.wall_clock_seconds > 0 {
            self.duration.agent_active_seconds as f32 / self.duration.wall_clock_seconds as f32
        } else {
            0.0
        };
        activity_ratio.min(1.0)
    }

    pub fn is_reconstructed(&self) -> bool {
        self.source.as_deref() == Some("git-reconstruct")
    }

    pub fn commit_message_short(&self) -> Option<&str> {
        self.message
            .as_deref()
            .filter(|m| !m.is_empty())
            .map(|m| m.lines().next().unwrap_or(m))
    }

    /// Best description of what was worked on:
    /// commit message > first user prompt > None
    pub fn description(&self) -> Option<String> {
        // Prefer commit message — it's the outcome
        if let Some(msg) = self.commit_message_short() {
            return Some(msg.to_string());
        }
        // Fall back to first user prompt — it's the intent
        if let Some(first) = self.user_prompts.texts.first() {
            // Clean up: take first line, trim
            let line = first.lines().next().unwrap_or(first).trim();
            if !line.is_empty() {
                return Some(line.to_string());
            }
        }
        None
    }

    pub fn diff_summary(&self) -> String {
        if let Some(stats) = &self.diff_stats {
            let mut parts = Vec::new();
            if stats.lines_added > 0 {
                parts.push(format!("+{}", stats.lines_added));
            }
            if stats.lines_removed > 0 {
                parts.push(format!("-{}", stats.lines_removed));
            }
            if stats.files_changed > 0 {
                parts.push(format!("{}f", stats.files_changed));
            }
            parts.join(" ")
        } else {
            let f = self.total_files();
            if f > 0 {
                format!("{}f", f)
            } else {
                String::new()
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentActivity {
    pub captures: Vec<AgentCapture>,
    pub total_cost: f64,
    pub total_commits: usize,
}

impl AgentActivity {
    pub fn new() -> Self {
        Self {
            captures: Vec::new(),
            total_cost: 0.0,
            total_commits: 0,
        }
    }

    pub fn load_from_repo(repo_path: &Path) -> Result<Self, String> {
        let captures_base = crate::config::global_config_dir().join("captures");

        let repo_name = repo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let log_path = captures_base.join(repo_name).join("log.jsonl");

        if !log_path.exists() {
            return Ok(Self::new());
        }

        let content = fs::read_to_string(&log_path)
            .map_err(|e| format!("Failed to read capture log: {}", e))?;

        let mut captures = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<AgentCapture>(line) {
                Ok(capture) => captures.push(capture),
                Err(e) => eprintln!("Failed to parse capture line: {}", e),
            }
        }

        // Enrich captures missing commit messages by looking them up from git
        Self::enrich_commit_messages(&mut captures, repo_path);

        // Sort by timestamp (newest first)
        captures.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        let total_cost = captures.iter().map(|c| c.estimated_api_cost_usd).sum();
        let total_commits = captures.len();

        Ok(Self {
            captures,
            total_cost,
            total_commits,
        })
    }

    /// Fill in missing commit messages from git log (batch lookup)
    fn enrich_commit_messages(captures: &mut [AgentCapture], repo_path: &Path) {
        // Collect hashes that need messages
        let needs_message: Vec<usize> = captures
            .iter()
            .enumerate()
            .filter(|(_, c)| c.message.as_ref().is_none_or(|m| m.is_empty()))
            .map(|(i, _)| i)
            .collect();

        if needs_message.is_empty() {
            return;
        }

        // Batch: ask git for all commit messages at once using --stdin
        // Format: one hash per line, get back "hash subject"
        let hashes: Vec<&str> = needs_message
            .iter()
            .map(|&i| captures[i].commit_hash.as_str())
            .collect();

        // Use git log with format to get hash + subject for each
        if let Ok(output) = std::process::Command::new("git")
            .args(["log", "--format=%h %s", "--no-walk"])
            .args(&hashes)
            .current_dir(repo_path)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let mut message_map: std::collections::HashMap<&str, &str> =
                    std::collections::HashMap::new();

                for line in stdout.lines() {
                    if let Some(space_pos) = line.find(' ') {
                        let hash = &line[..space_pos];
                        let msg = &line[space_pos + 1..];
                        message_map.insert(hash, msg);
                    }
                }

                // Apply messages back to captures
                for &idx in &needs_message {
                    let short_hash = captures[idx].short_hash();
                    if let Some(&msg) = message_map.get(short_hash) {
                        captures[idx].message = Some(msg.to_string());
                    }
                }
            }
        }
    }

    pub fn recent_captures(&self, limit: usize) -> &[AgentCapture] {
        let end = limit.min(self.captures.len());
        &self.captures[..end]
    }

    pub fn live_capture_count(&self) -> usize {
        self.captures
            .iter()
            .filter(|c| !c.is_reconstructed())
            .count()
    }

    pub fn format_total_cost(&self) -> String {
        if self.total_cost < 1.0 {
            format!("${:.3}", self.total_cost)
        } else {
            format!("${:.2}", self.total_cost)
        }
    }
}

// ── Conversation viewer ──────────────────────────────────────────────

/// A single entry in the conversation thread, distilled for display.
#[derive(Debug, Clone)]
pub enum ConversationEntry {
    /// User prompt text
    User { text: String },
    /// Assistant text response (may be partial — one per text block)
    Assistant { text: String, model: String },
    /// Assistant called a tool
    ToolCall {
        tool: String,
        summary: String, // e.g. "bash: git status" or "edit: src/main.rs"
    },
    /// Tool result
    ToolResult {
        tool: String,
        output: String, // truncated
        is_error: bool,
    },
    /// Compaction summary (context was compressed)
    Compaction { summary: String },
    /// Thinking block
    Thinking { text: String },
}

/// A loaded conversation thread for a capture entry
#[derive(Debug, Clone)]
pub struct Conversation {
    pub entries: Vec<ConversationEntry>,
    pub error: Option<String>,
}

impl Conversation {
    pub fn load_for_capture(capture: &AgentCapture) -> Self {
        let session_ref = match &capture.session_ref {
            Some(r) => r,
            None => {
                return Self {
                    entries: Vec::new(),
                    error: Some("No session reference (git-only commit)".to_string()),
                }
            }
        };

        let session_path = Path::new(&session_ref.file);
        if !session_path.exists() {
            return Self {
                entries: Vec::new(),
                error: Some("Session file no longer exists".to_string()),
            };
        }

        let content = match fs::read_to_string(session_path) {
            Ok(c) => c,
            Err(e) => {
                return Self {
                    entries: Vec::new(),
                    error: Some(format!("Failed to read session: {}", e)),
                }
            }
        };

        let start_id = &session_ref.entry_range[0];
        let end_id = &session_ref.entry_range[1];

        let mut entries = Vec::new();
        let mut in_range = start_id.is_empty();

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }

            let parsed: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let entry_id = parsed.get("id").and_then(|v| v.as_str()).unwrap_or("");

            if !in_range {
                if entry_id == start_id {
                    in_range = true;
                } else {
                    continue;
                }
            }

            let entry_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");

            match entry_type {
                "message" => {
                    if let Some(msg) = parsed.get("message") {
                        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
                        match role {
                            "user" => {
                                let text = extract_text_content(msg);
                                if !text.is_empty() {
                                    entries.push(ConversationEntry::User { text });
                                }
                            }
                            "assistant" => {
                                let model = msg
                                    .get("model")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();

                                if let Some(content) = msg.get("content").and_then(|v| v.as_array())
                                {
                                    for block in content {
                                        let block_type = block
                                            .get("type")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        match block_type {
                                            "text" => {
                                                let text = block
                                                    .get("text")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string();
                                                if !text.trim().is_empty() {
                                                    entries.push(ConversationEntry::Assistant {
                                                        text,
                                                        model: model.clone(),
                                                    });
                                                }
                                            }
                                            "thinking" => {
                                                let text = block
                                                    .get("thinking")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string();
                                                if !text.trim().is_empty() {
                                                    entries
                                                        .push(ConversationEntry::Thinking { text });
                                                }
                                            }
                                            "toolCall" => {
                                                let tool = block
                                                    .get("name")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("?")
                                                    .to_string();
                                                let args = block
                                                    .get("arguments")
                                                    .cloned()
                                                    .unwrap_or(Value::Null);
                                                let summary = summarize_tool_call(&tool, &args);
                                                entries.push(ConversationEntry::ToolCall {
                                                    tool,
                                                    summary,
                                                });
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            "toolResult" => {
                                let tool = msg
                                    .get("toolName")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?")
                                    .to_string();
                                let is_error = msg
                                    .get("isError")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                let output = extract_text_content(msg);
                                // Truncate long output for display
                                let output = if output.len() > 500 {
                                    format!("{}…", truncate_str(&output, 497))
                                } else {
                                    output
                                };
                                entries.push(ConversationEntry::ToolResult {
                                    tool,
                                    output,
                                    is_error,
                                });
                            }
                            _ => {}
                        }
                    }
                }
                "compaction" => {
                    let summary = parsed
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(context compacted)")
                        .to_string();
                    entries.push(ConversationEntry::Compaction { summary });
                }
                _ => {}
            }

            if entry_id == end_id {
                break;
            }
        }

        Self {
            entries,
            error: None,
        }
    }
}

/// Extract text from a message's content (handles string or array of text blocks)
fn extract_text_content(msg: &Value) -> String {
    if let Some(content) = msg.get("content") {
        if let Some(s) = content.as_str() {
            return s.to_string();
        }
        if let Some(arr) = content.as_array() {
            let texts: Vec<&str> = arr
                .iter()
                .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                .collect();
            return texts.join("\n");
        }
    }
    String::new()
}

/// Create a short summary for a tool call
fn summarize_tool_call(tool: &str, args: &Value) -> String {
    match tool.to_lowercase().as_str() {
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let first_line = cmd.lines().next().unwrap_or(cmd);
            if first_line.len() > 80 {
                format!("{}…", truncate_str(first_line, 77))
            } else {
                first_line.to_string()
            }
        }
        "read" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            path.to_string()
        }
        "edit" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            path.to_string()
        }
        "write" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            path.to_string()
        }
        _ => {
            // Generic: show first string arg value
            if let Some(obj) = args.as_object() {
                for (_, v) in obj {
                    if let Some(s) = v.as_str() {
                        let line = s.lines().next().unwrap_or(s);
                        if line.len() > 60 {
                            return format!("{}…", truncate_str(line, 57));
                        }
                        return line.to_string();
                    }
                }
            }
            String::new()
        }
    }
}
