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

use std::path::Path;

use serde_json::Value;

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

/// Read and parse a transcript file.
pub fn load_claude_history(path: &Path) -> std::io::Result<Vec<TranscriptEntry>> {
    let text = std::fs::read_to_string(path)?;
    Ok(parse_claude_transcript(&text))
}

/// Parse transcript lines into timeline entries. Unparseable lines are
/// skipped: the file is append-only and the last line may be partial.
pub fn parse_claude_transcript(text: &str) -> Vec<TranscriptEntry> {
    let mut out = Vec::new();
    // Whether assistant activity has been emitted since the last turn end;
    // the transcript has no turn markers, so a new user prompt (or the end
    // of the file) closes the turn.
    let mut turn_open = false;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["isMeta"].as_bool().unwrap_or(false) || v["isSidechain"].as_bool().unwrap_or(false) {
            continue;
        }
        match v["type"].as_str() {
            Some("user") => {
                for entry in user_entries(&v["message"]["content"]) {
                    match &entry {
                        TranscriptEntry::UserPrompt(_) => {
                            if turn_open {
                                out.push(TranscriptEntry::Harness(turn_completed()));
                                turn_open = false;
                            }
                        }
                        TranscriptEntry::Harness(_) => turn_open = true,
                    }
                    out.push(entry);
                }
            }
            Some("assistant") => {
                let events = assistant_events(&v["message"]["content"]);
                if !events.is_empty() {
                    turn_open = true;
                }
                out.extend(events.into_iter().map(TranscriptEntry::Harness));
            }
            _ => {}
        }
    }
    if turn_open {
        out.push(TranscriptEntry::Harness(turn_completed()));
    }
    out
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
}
