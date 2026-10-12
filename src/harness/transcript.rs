//! Rebuild a chat timeline from a Claude Code session transcript.
//!
//! Claude Code appends one JSON object per line to
//! `~/.claude/projects/<cwd-slug>/<session-id>.jsonl`. GitTerm reads it
//! once, when a chat tab that resumes an existing session is first shown
//! after a restart, so the timeline is not empty until the next turn. The
//! output is the same event stream the live parser produces, minus timing,
//! usage and cost (the transcript does not carry per-turn usage).
//!
//! Only `user` and `assistant` lines matter. Everything else the CLI writes
//! (`mode`, `file-history-*`, `cost-state`, hook summaries, sidechains) is
//! bookkeeping and is skipped.
//!
//! Subagents get the lifecycle the live stream gives them: a
//! `SubagentEvent` start after each Agent call, and an end with the
//! subagent's tokens, tool uses and duration. A foreground subagent ends at
//! its Agent result; a background one (result status `async_launched`) ends
//! at the `<task-notification>` the CLI later injects, or, when the file
//! ends first, as stopped (its process is gone).

use std::collections::HashSet;
use std::path::Path;

use serde_json::{json, Value};

use super::{HarnessEvent, ItemKind, TurnStatus};

/// Longest tool result carried into the timeline. The chat page shows at
/// most this much of an output; keeping more only bloats the replay payload
/// (long sessions have thousands of tool results).
pub const MAX_RESULT_CHARS: usize = 20_000;

/// One timeline entry recovered from a transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEntry {
    /// Something the human typed (or a compaction summary standing in for
    /// the earlier conversation).
    UserPrompt(String),
    Harness(HarnessEvent),
}

/// A timeline entry and when its transcript line was written (the line's
/// RFC 3339 `timestamp`, when it has one). The time places delegation
/// cards, which are not in the transcript, back where they were (TRU-142).
#[derive(Debug, Clone, PartialEq)]
pub struct TimedEntry {
    pub at: Option<String>,
    pub entry: TranscriptEntry,
}

/// Read and parse a transcript file.
pub fn load_claude_history(path: &Path) -> std::io::Result<Vec<TimedEntry>> {
    let text = std::fs::read_to_string(path)?;
    Ok(parse_claude_transcript_timed(&text))
}

/// Parse transcript lines into timeline entries. Unparseable lines are
/// skipped: the file is append-only and the last line may be partial.
pub fn parse_claude_transcript(text: &str) -> Vec<TranscriptEntry> {
    parse_claude_transcript_timed(text)
        .into_iter()
        .map(|timed| timed.entry)
        .collect()
}

/// `parse_claude_transcript`, keeping each entry's line time. The turn end
/// closed by a new prompt takes that prompt's time; the one closing the
/// file has none.
pub fn parse_claude_transcript_timed(text: &str) -> Vec<TimedEntry> {
    let mut out = Vec::new();
    // Whether assistant activity has been emitted since the last turn end;
    // the transcript has no turn markers, so a new user prompt (or the end
    // of the file) closes the turn.
    let mut turn_open = false;
    // Agent calls seen, and the background ones still awaiting their
    // completion notice.
    let mut agent_calls: HashSet<String> = HashSet::new();
    let mut background: Vec<String> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["isMeta"].as_bool().unwrap_or(false) || v["isSidechain"].as_bool().unwrap_or(false) {
            continue;
        }
        let at = v["timestamp"].as_str().map(str::to_string);
        let timed = |entry| TimedEntry {
            at: at.clone(),
            entry,
        };
        match v["type"].as_str() {
            // A notice that arrived mid-turn is recorded as a queued
            // command attachment rather than a user message.
            Some("attachment") if v["attachment"]["type"] == "queued_command" => {
                if let Some(end) = notice_end(&mut background, &v["attachment"]["prompt"]) {
                    out.push(timed(end));
                }
            }
            Some("user") => {
                if task_notification(&v["message"]["content"]).is_some() {
                    if let Some(end) = notice_end(&mut background, &v["message"]["content"]) {
                        out.push(timed(end));
                    }
                    continue;
                }
                for entry in user_entries(&v["message"]["content"]) {
                    let agent_result = match &entry {
                        TranscriptEntry::Harness(HarnessEvent::ItemCompleted {
                            id,
                            is_error,
                            ..
                        }) if agent_calls.contains(id) => Some((id.clone(), *is_error)),
                        _ => None,
                    };
                    match &entry {
                        TranscriptEntry::UserPrompt(_) => {
                            if turn_open {
                                out.push(timed(TranscriptEntry::Harness(turn_completed())));
                                turn_open = false;
                            }
                        }
                        TranscriptEntry::Harness(_) => turn_open = true,
                    }
                    out.push(timed(entry));
                    let Some((id, is_error)) = agent_result else {
                        continue;
                    };
                    let result = &v["toolUseResult"];
                    if !is_error && result["status"] == "async_launched" {
                        background.push(id);
                    } else {
                        let end = foreground_end(result, is_error);
                        out.push(timed(TranscriptEntry::Harness(subagent(&id, end))));
                    }
                }
            }
            Some("assistant") => {
                let events = assistant_events(&v["message"]["content"]);
                if !events.is_empty() {
                    turn_open = true;
                }
                for event in events {
                    let started_agent = match &event {
                        HarnessEvent::ItemStarted {
                            id,
                            kind: ItemKind::ToolCall { name, .. },
                        } if is_agent_tool(name) => Some(id.clone()),
                        _ => None,
                    };
                    out.push(timed(TranscriptEntry::Harness(event)));
                    if let Some(id) = started_agent {
                        let start = HarnessEvent::TurnStarted {
                            session_id: None,
                            model: None,
                        };
                        out.push(timed(TranscriptEntry::Harness(subagent(&id, start))));
                        agent_calls.insert(id);
                    }
                }
            }
            _ => {}
        }
    }
    // Background subagents with no notice: the process that ran them is
    // gone, so they stopped.
    for parent in background {
        let end = HarnessEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            usage: Value::Null,
            cost_usd: None,
        };
        out.push(TimedEntry {
            at: None,
            entry: TranscriptEntry::Harness(subagent(&parent, end)),
        });
    }
    if turn_open {
        out.push(TimedEntry {
            at: None,
            entry: TranscriptEntry::Harness(turn_completed()),
        });
    }
    out
}

/// The subagent end a `<task-notification>` reports, when it is for a
/// background subagent still awaiting one (each is taken once).
fn notice_end(background: &mut Vec<String>, content: &Value) -> Option<TranscriptEntry> {
    let (parent, end) = task_notification(content)?;
    let pos = background.iter().position(|id| *id == parent)?;
    background.remove(pos);
    Some(TranscriptEntry::Harness(subagent(&parent, end)))
}

/// Claude's subagent tool (`Task` in older CLIs).
pub fn is_agent_tool(name: &str) -> bool {
    matches!(name, "Agent" | "Task")
}

fn subagent(parent: &str, event: HarnessEvent) -> HarnessEvent {
    HarnessEvent::SubagentEvent {
        parent_tool_use_id: parent.to_string(),
        event: Box::new(event),
    }
}

/// The usage shape the live `task_notification` carries.
fn subagent_usage(tokens: Option<u64>, tool_uses: Option<u64>, duration_ms: Option<u64>) -> Value {
    if tokens.is_none() && tool_uses.is_none() && duration_ms.is_none() {
        return Value::Null;
    }
    json!({ "total_tokens": tokens, "tool_uses": tool_uses, "duration_ms": duration_ms })
}

/// A foreground subagent's end, from its Agent result's `toolUseResult`.
fn foreground_end(result: &Value, is_error: bool) -> HarnessEvent {
    HarnessEvent::TurnCompleted {
        status: if is_error {
            TurnStatus::Failed("the subagent failed".to_string())
        } else {
            TurnStatus::Completed
        },
        usage: subagent_usage(
            result["totalTokens"].as_u64(),
            result["totalToolUseCount"].as_u64(),
            result["totalDurationMs"].as_u64(),
        ),
        cost_usd: None,
    }
}

/// A `<task-notification>` user message as (Agent call id, the subagent's
/// end). `None` for any other message.
fn task_notification(content: &Value) -> Option<(String, HarnessEvent)> {
    let text = content.as_str()?.trim_start();
    if !text.starts_with("<task-notification>") {
        return None;
    }
    // The tags before <summary> are the CLI's own; the usage block comes
    // last, after the free-text result.
    let parent = tag(text, "tool-use-id")?;
    let status = match tag(text, "status").unwrap_or_default().as_str() {
        "completed" => TurnStatus::Completed,
        "failed" | "error" => TurnStatus::Failed(
            tag(text, "summary")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "the subagent failed".to_string()),
        ),
        _ => TurnStatus::Interrupted,
    };
    let usage = text
        .rfind("<usage>")
        .map(|start| &text[start..])
        .map(|block| {
            let num = |name| tag(block, name).and_then(|n| n.trim().parse::<u64>().ok());
            subagent_usage(num("subagent_tokens"), num("tool_uses"), num("duration_ms"))
        })
        .unwrap_or(Value::Null);
    Some((
        parent,
        HarnessEvent::TurnCompleted {
            status,
            usage,
            cost_usd: None,
        },
    ))
}

/// The text of the first `<name>…</name>` in `text`.
fn tag(text: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim().to_string())
}

fn turn_completed() -> HarnessEvent {
    HarnessEvent::TurnCompleted {
        status: TurnStatus::Completed,
        usage: Value::Null,
        cost_usd: None,
    }
}

/// Prompts and tool results carried by one `user` line. The CLI records
/// its own injected messages (`<local-command-caveat>`, `<system-reminder>`
/// wrappers, ...) as user text starting with `<`; those are not the
/// human's words and are dropped.
/// What an image block reads as in restored text.
const IMAGE_MARKER: &str = "[image]";

fn user_entries(content: &Value) -> Vec<TranscriptEntry> {
    match content {
        Value::String(text) => human_prompt(text).into_iter().collect(),
        Value::Array(blocks) => {
            let mut out = Vec::new();
            let mut text = String::new();
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = block["text"].as_str() {
                            if !is_cli_injected(t) {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                        }
                    }
                    // A pasted or dropped image (TRU-140) shows as a
                    // marker in the restored prompt.
                    Some("image") => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(IMAGE_MARKER);
                    }
                    Some("tool_result") => {
                        if let Some(id) = block["tool_use_id"].as_str() {
                            out.push(TranscriptEntry::Harness(HarnessEvent::ItemCompleted {
                                id: id.to_string(),
                                output: result_text(&block["content"]),
                                is_error: block["is_error"].as_bool().unwrap_or(false),
                            }));
                        }
                    }
                    _ => {}
                }
            }
            out.extend(human_prompt(&text));
            out
        }
        _ => Vec::new(),
    }
}

fn human_prompt(text: &str) -> Option<TranscriptEntry> {
    let trimmed = text.trim();
    (!trimmed.is_empty() && !is_cli_injected(trimmed))
        .then(|| TranscriptEntry::UserPrompt(text.to_string()))
}

fn is_cli_injected(text: &str) -> bool {
    text.trim_start().starts_with('<')
}

/// A tool result's content is a string or a list of text/image blocks,
/// cut to `MAX_RESULT_CHARS` with a note of how much was dropped.
fn result_text(content: &Value) -> String {
    let text = result_text_full(content);
    let total = text.chars().count();
    if total <= MAX_RESULT_CHARS {
        return text;
    }
    let mut cut: String = text.chars().take(MAX_RESULT_CHARS).collect();
    cut.push_str(&format!(
        "\n… ({} more characters in the transcript)",
        total - MAX_RESULT_CHARS
    ));
    cut
}

fn result_text_full(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().unwrap_or("").to_string(),
                Some("image") => IMAGE_MARKER.to_string(),
                _ => String::new(),
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Text, thinking and tool-use blocks of one `assistant` line, as the
/// events the live stream would have produced for them.
fn assistant_events(content: &Value) -> Vec<HarnessEvent> {
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| match block["type"].as_str() {
            Some("text") => {
                let text = block["text"].as_str()?;
                (!text.is_empty()).then(|| HarnessEvent::TextDelta(text.to_string()))
            }
            // Redacted thinking has an empty string here, which the chat
            // page already treats as "thinking happened, nothing to show".
            Some("thinking") => Some(HarnessEvent::ThinkingDelta(
                block["thinking"].as_str().unwrap_or("").to_string(),
            )),
            Some("tool_use") => Some(HarnessEvent::ItemStarted {
                id: block["id"].as_str()?.to_string(),
                kind: ItemKind::ToolCall {
                    name: block["name"].as_str().unwrap_or("").to_string(),
                    input: block.get("input").cloned().unwrap_or(Value::Null),
                },
            }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(v: Value) -> String {
        v.to_string()
    }

    fn user_text(text: &str) -> String {
        line(json!({"type":"user","isSidechain":false,"message":{"role":"user","content":text}}))
    }

    fn assistant(blocks: Value) -> String {
        line(
            json!({"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":blocks}}),
        )
    }

    #[test]
    fn two_turns_with_a_tool_call() {
        let lines = [
            line(json!({"type":"mode","mode":"normal"})),
            line(
                json!({"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>x</local-command-caveat>"}}),
            ),
            user_text("list the files"),
            assistant(json!([{"type":"thinking","thinking":""}])),
            assistant(
                json!([{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]),
            ),
            line(
                json!({"type":"user","isSidechain":false,"message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":"a\nb","is_error":false}]}}),
            ),
            assistant(json!([{"type":"text","text":"Two files."}])),
            line(json!({"type":"cost-state","totalCostUSD":1.0})),
            user_text("thanks"),
            assistant(json!([{"type":"text","text":"Welcome."}])),
        ];
        let entries = parse_claude_transcript(&lines.join("\n"));
        use HarnessEvent as H;
        use TranscriptEntry::{Harness, UserPrompt};
        assert_eq!(
            entries,
            vec![
                UserPrompt("list the files".into()),
                Harness(H::ThinkingDelta(String::new())),
                Harness(H::ItemStarted {
                    id: "toolu_1".into(),
                    kind: ItemKind::ToolCall {
                        name: "Bash".into(),
                        input: json!({"command":"ls"}),
                    },
                }),
                Harness(H::ItemCompleted {
                    id: "toolu_1".into(),
                    output: "a\nb".into(),
                    is_error: false,
                }),
                Harness(H::TextDelta("Two files.".into())),
                Harness(turn_completed()),
                UserPrompt("thanks".into()),
                Harness(H::TextDelta("Welcome.".into())),
                Harness(turn_completed()),
            ]
        );
    }

    #[test]
    fn sidechain_and_injected_text_are_dropped() {
        let lines = [
            line(
                json!({"type":"user","isSidechain":true,"message":{"role":"user","content":"subagent prompt"}}),
            ),
            line(
                json!({"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"subagent reply"}]}}),
            ),
            line(
                json!({"type":"user","isSidechain":false,"message":{"role":"user","content":[
                {"type":"text","text":"<system-reminder>hidden</system-reminder>"},
                {"type":"text","text":"real question"}]}}),
            ),
            "{not json".to_string(),
        ];
        let entries = parse_claude_transcript(&lines.join("\n"));
        assert_eq!(
            entries,
            vec![TranscriptEntry::UserPrompt("real question".into())]
        );
    }

    #[test]
    fn tool_result_blocks_join_text_and_mark_images() {
        let content = json!([{"type":"text","text":"one"},{"type":"image","source":{}},{"type":"text","text":"two"}]);
        assert_eq!(result_text(&content), "one\n[image]\ntwo");
        assert_eq!(result_text(&json!("plain")), "plain");
        assert_eq!(result_text(&Value::Null), "");
    }

    #[test]
    fn user_image_blocks_restore_as_an_image_marker() {
        let lines = [
            line(json!({"type":"user","isSidechain":false,"message":{"role":"user","content":[
                {"type":"text","text":"What colour is this?"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}},
                {"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"BBBB"}}
            ]}})),
            line(json!({"type":"user","isSidechain":false,"message":{"role":"user","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}
            ]}})),
        ]
        .join("\n");
        assert_eq!(
            parse_claude_transcript(&lines),
            vec![
                TranscriptEntry::UserPrompt("What colour is this?\n[image]\n[image]".into()),
                TranscriptEntry::UserPrompt("[image]".into()),
            ]
        );
    }

    #[test]
    fn long_tool_results_are_cut_with_a_note() {
        let long = "x".repeat(MAX_RESULT_CHARS + 5);
        let cut = result_text(&json!(long));
        assert!(cut.starts_with(&"x".repeat(MAX_RESULT_CHARS)));
        assert!(cut.ends_with("(5 more characters in the transcript)"));
        assert_eq!(result_text(&json!("short")), "short");
    }

    #[test]
    fn empty_transcript_yields_nothing() {
        assert!(parse_claude_transcript("").is_empty());
    }

    fn agent_call(id: &str) -> String {
        assistant(json!([{"type":"tool_use","id":id,"name":"Agent","input":{"description":"d"}}]))
    }

    fn agent_result(id: &str, result: Value) -> String {
        line(json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":id,"content":"ok","is_error":false}]},
            "toolUseResult": result}))
    }

    fn sub_events(entries: &[TranscriptEntry]) -> Vec<(String, String)> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Harness(HarnessEvent::SubagentEvent {
                    parent_tool_use_id,
                    event,
                }) => Some((
                    parent_tool_use_id.clone(),
                    match &**event {
                        HarnessEvent::TurnStarted { .. } => "start".to_string(),
                        HarnessEvent::TurnCompleted { status, usage, .. } => {
                            format!("{status:?} {usage}")
                        }
                        other => format!("{other:?}"),
                    },
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn subagents_start_and_end_with_their_usage() {
        let notice = "<task-notification>\n<task-id>t</task-id>\n<tool-use-id>bg</tool-use-id>\n<status>completed</status>\n<summary>s</summary>\n<result>a <usage> word</result>\n<usage><subagent_tokens>2000</subagent_tokens><tool_uses>7</tool_uses><duration_ms>9000</duration_ms></usage>\n</task-notification>";
        let text = [
            user_text("go"),
            agent_call("fg"),
            agent_result(
                "fg",
                json!({"status":"completed","totalTokens":500,"totalToolUseCount":3,"totalDurationMs":4000}),
            ),
            agent_call("bg"),
            agent_result("bg", json!({"status":"async_launched"})),
            agent_call("lost"),
            agent_result("lost", json!({"status":"async_launched"})),
            agent_call("mid"),
            agent_result("mid", json!({"status":"async_launched"})),
            user_text(notice),
            line(json!({"type":"attachment","isSidechain":false,"attachment":{
                "type":"queued_command","commandMode":"task-notification",
                "prompt": notice.replace("<tool-use-id>bg<", "<tool-use-id>mid<")}})),
        ]
        .join("\n");
        let entries = parse_claude_transcript(&text);
        assert_eq!(
            sub_events(&entries),
            vec![
                ("fg".into(), "start".into()),
                (
                    "fg".into(),
                    r#"Completed {"duration_ms":4000,"tool_uses":3,"total_tokens":500}"#.into()
                ),
                ("bg".into(), "start".into()),
                ("lost".into(), "start".into()),
                ("mid".into(), "start".into()),
                (
                    "bg".into(),
                    r#"Completed {"duration_ms":9000,"tool_uses":7,"total_tokens":2000}"#.into()
                ),
                (
                    "mid".into(),
                    r#"Completed {"duration_ms":9000,"tool_uses":7,"total_tokens":2000}"#.into()
                ),
                ("lost".into(), "Interrupted null".into()),
            ]
        );
        // The notice is not shown as something the human typed.
        assert!(!entries.iter().any(
            |e| matches!(e, TranscriptEntry::UserPrompt(t) if t.contains("task-notification"))
        ));
    }
}
