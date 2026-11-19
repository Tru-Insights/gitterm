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

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{json, Value};

use super::types::{DeliveryCard, HarnessEvent, ItemKind, TurnStatus};

/// Longest tool result carried into the timeline. The chat page shows at
/// most this much of an output; keeping more only bloats the replay payload
/// (long sessions have thousands of tool results).
pub const MAX_RESULT_CHARS: usize = 20_000;

pub use crate::harness::transcript::{TimedEntry, TranscriptEntry};

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

/// `parse_claude_transcript`, keeping each entry's line time. A synthetic
/// turn end has no timestamp: the next prompt's time includes idle time.
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
    // Every tool call started so far, and results that arrived before
    // their call: Claude occasionally writes a result line ahead of the
    // assistant line holding its tool_use. Such a result is replayed right
    // after its call starts, so the card and any subagent still close.
    let mut started_tools: HashSet<String> = HashSet::new();
    let mut early_results: HashMap<String, (TranscriptEntry, Value)> = HashMap::new();
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
                    if let TranscriptEntry::Harness(HarnessEvent::ItemCompleted { id, .. }) = &entry
                    {
                        if !started_tools.contains(id) {
                            early_results
                                .insert(id.clone(), (entry.clone(), v["toolUseResult"].clone()));
                        }
                    }
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
                                out.push(TimedEntry {
                                    at: None,
                                    entry: TranscriptEntry::Harness(turn_completed()),
                                });
                                turn_open = false;
                            }
                        }
                        TranscriptEntry::Harness(_) => turn_open = true,
                    }
                    out.push(timed(entry));
                    let Some((id, is_error)) = agent_result else {
                        continue;
                    };
                    if let Some(end) =
                        agent_result_end(&mut background, id, is_error, &v["toolUseResult"])
                    {
                        out.push(timed(end));
                    }
                }
            }
            Some("assistant") => {
                let events = assistant_events(&v["message"]["content"]);
                if !events.is_empty() {
                    turn_open = true;
                }
                for event in events {
                    let started_tool = match &event {
                        HarnessEvent::ItemStarted {
                            id,
                            kind: ItemKind::ToolCall { name, .. },
                        } => Some((id.clone(), is_agent_tool(name))),
                        _ => None,
                    };
                    out.push(timed(TranscriptEntry::Harness(event)));
                    let Some((id, is_agent)) = started_tool else {
                        continue;
                    };
                    started_tools.insert(id.clone());
                    if is_agent {
                        let start = HarnessEvent::TurnStarted {
                            session_id: None,
                            model: None,
                        };
                        out.push(timed(TranscriptEntry::Harness(subagent(&id, start))));
                        agent_calls.insert(id.clone());
                    }
                    if let Some((completed, result)) = early_results.remove(&id) {
                        let is_error = matches!(
                            &completed,
                            TranscriptEntry::Harness(HarnessEvent::ItemCompleted {
                                is_error: true,
                                ..
                            })
                        );
                        out.push(timed(completed));
                        if is_agent {
                            if let Some(end) =
                                agent_result_end(&mut background, id, is_error, &result)
                            {
                                out.push(timed(end));
                            }
                        }
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
            rate_limit: None,
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

/// What an Agent call's result means for its subagent: a background launch
/// waits for its completion notice (`None`); anything else ends it now.
fn agent_result_end(
    background: &mut Vec<String>,
    id: String,
    is_error: bool,
    result: &Value,
) -> Option<TranscriptEntry> {
    if !is_error && result["status"] == "async_launched" {
        background.push(id);
        return None;
    }
    let end = foreground_end(result, is_error);
    Some(TranscriptEntry::Harness(subagent(&id, end)))
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
        rate_limit: None,
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
            rate_limit: None,
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
        rate_limit: None,
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
    if crate::chat::html_render::is_render_output(&text) {
        return text;
    }
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

pub const DELIVERY_SOURCE_LINE: &str = "GitTerm delivery (not typed by the user):";
pub const DELIVERY_FOOTER_PREFIX: &str = "Full record: delegation_get ";

/// Whether text is a prompt GitTerm sent rather than the user: it starts
/// with the source line, or (results delivered before the source line
/// existed) it ends with the full-record footer.
///
/// shortcut: judged from the text alone, so a user who pastes a message
/// starting with the source line (or ending with a copied footer) sees it
/// as a card and it is left out of the chat summary; it is still sent in
/// full. A transcript replay has nothing but the text, so a recorded origin
/// would only help the live view; record one if this ever misleads.
pub fn is_delivery_message(text: &str) -> bool {
    let text = text.trim();
    text.lines().next() == Some(DELIVERY_SOURCE_LINE)
        || text
            .lines()
            .last()
            .is_some_and(|line| line.starts_with(DELIVERY_FOOTER_PREFIX))
}

/// The card for `text`, or `None` when the user wrote it.
pub fn delivery_card(text: &str) -> Option<DeliveryCard> {
    if !is_delivery_message(text) {
        return None;
    }
    let text = text.trim();
    let content = text
        .strip_prefix(DELIVERY_SOURCE_LINE)
        .map(|rest| rest.trim_start_matches('\n'))
        .unwrap_or(text);
    let header = content.lines().next().unwrap_or("GitTerm delivery");
    let mut ids = content
        .lines()
        .filter_map(|line| line.strip_prefix(DELIVERY_FOOTER_PREFIX));
    let delegation_id = match (ids.next(), ids.next()) {
        (Some(id), None) => id.to_string(),
        _ => String::new(),
    };
    Some(DeliveryCard {
        header: header.to_string(),
        delegation_id,
        body: content
            .strip_prefix(header)
            .unwrap_or(content)
            .trim_start()
            .to_string(),
    })
}

/// The timeline marker recorded in the parent tab's conversation where the
/// request was made, so a rebuilt page puts the card back there.
pub fn anchor_payload(delegation_id: &str) -> Value {
    json!({ "kind": "delegation_anchor", "delegation_id": delegation_id })
}

/// The task tools whose result returns a new delegation's id.
const REQUEST_TOOLS: [&str; 3] = ["review_request", "consult_request", "delegate_task"];

/// Where a chat's delegation cards go in a timeline read back from its
/// transcript, as (index to insert the anchor at, delegation id), by
/// index. The anchors recorded live are only in memory, so without this
/// every card lands at the end after a restart. A delegation Claude asked
/// for goes right after the tool result that returned its id; one asked
/// for from the page (Review…, Re-run) goes before the first entry written
/// after its `created_at`. One placed by neither is left out, and its card
/// goes at the end. `delegations` are (id, created_at), oldest first.
pub fn history_anchor_positions(
    entries: &[TimedEntry],
    delegations: &[(String, String)],
) -> Vec<(usize, String)> {
    let mut request_calls: HashSet<&str> = HashSet::new();
    let mut request_results: Vec<(usize, &str)> = Vec::new();
    for (index, timed) in entries.iter().enumerate() {
        match &timed.entry {
            TranscriptEntry::Harness(HarnessEvent::ItemStarted {
                id,
                kind: ItemKind::ToolCall { name, .. },
            }) if is_request_tool(name) => {
                request_calls.insert(id);
            }
            TranscriptEntry::Harness(HarnessEvent::ItemCompleted {
                id,
                output,
                is_error: false,
            }) if request_calls.contains(id.as_str()) => request_results.push((index, output)),
            _ => {}
        }
    }
    let mut positions: Vec<(usize, String)> = delegations
        .iter()
        .filter_map(|(id, created_at)| {
            let after_result = request_results
                .iter()
                .find(|(_, output)| output.contains(id.as_str()))
                .map(|(index, _)| index + 1);
            let by_time = || {
                let created = parse_time(created_at)?;
                Some(
                    entries
                        .iter()
                        .position(|timed| {
                            timed
                                .at
                                .as_deref()
                                .and_then(parse_time)
                                .is_some_and(|at| at > created)
                        })
                        .unwrap_or(entries.len()),
                )
            };
            after_result
                .or_else(by_time)
                .map(|index| (index, id.clone()))
        })
        .collect();
    // Stable: delegations at one index keep their oldest-first order.
    positions.sort_by_key(|(index, _)| *index);
    positions
}

/// A Claude tool name for one of `REQUEST_TOOLS` on GitTerm's task server
/// (MCP tools reach Claude as `mcp__<server>__<tool>`).
fn is_request_tool(name: &str) -> bool {
    name.strip_prefix("mcp__gitterm_tasks__")
        .is_some_and(|tool| REQUEST_TOOLS.contains(&tool))
}

fn parse_time(text: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(text).ok()
}

/// The page's echo of a prompt the human submitted.
pub fn user_prompt_payload(text: &str) -> Value {
    let mut payload = serde_json::json!({"type": "user_prompt", "text": text});
    if let Some(card) = delivery_card(text) {
        payload["delivery"] = serde_json::json!(card);
    }
    payload
}

/// A harness event as the page's `__appendEvent` receives it. `None` (and
/// logged) when the event cannot be encoded.
pub fn harness_payload(event: &HarnessEvent) -> Option<Value> {
    match serde_json::to_value(event) {
        Ok(event) => Some(serde_json::json!({"kind": "harness", "event": event})),
        Err(e) => {
            eprintln!("[agent] could not serialize harness event {event:?}: {e}");
            None
        }
    }
}

/// The Agent call whose subagent `event` starts, if it is a subagent start.
pub fn subagent_started(event: &HarnessEvent) -> Option<&str> {
    match event {
        HarnessEvent::SubagentEvent {
            parent_tool_use_id,
            event,
        } if matches!(**event, HarnessEvent::TurnStarted { .. }) => {
            Some(parent_tool_use_id.as_str())
        }
        _ => None,
    }
}

/// The timeline marker for when Agent call `parent`'s subagent started
/// (Unix ms). The chat page uses it when the start was not seen live.
pub fn subagent_started_at(parent: &str, at_ms: i64) -> Value {
    serde_json::json!({ "kind": "subagent_started_at", "id": parent, "at_ms": at_ms })
}

/// The start and end of a chat turn, when known (TRU-209).
pub fn turn_timing(start_ms: Option<i64>, end_ms: Option<i64>) -> Value {
    let mut payload = serde_json::json!({ "kind": "turn_timing" });
    if let Some(start_ms) = start_ms {
        payload["start_ms"] = serde_json::json!(start_ms);
    }
    if let Some(end_ms) = end_ms {
        payload["end_ms"] = serde_json::json!(end_ms);
    }
    payload
}

/// A transcript read-back as page payloads, each subagent start followed
/// by when it started (from its transcript line). The desktop adds its
/// delegation anchors on top of this; an agent has none.
pub fn history_payloads(entries: Vec<TimedEntry>) -> Vec<Value> {
    let mut payloads = Vec::with_capacity(entries.len());
    for timed in entries {
        let at_ms = timed
            .at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.timestamp_millis());
        match timed.entry {
            TranscriptEntry::UserPrompt(text) => payloads.push(user_prompt_payload(&text)),
            TranscriptEntry::Harness(event) => {
                let started = subagent_started(&event).map(str::to_string);
                if let Some(payload) = harness_payload(&event) {
                    payloads.push(payload);
                }
                if let (Some(parent), Some(at_ms)) = (started, at_ms) {
                    payloads.push(subagent_started_at(&parent, at_ms));
                }
            }
        }
    }
    payloads
}

/// A restored timeline row: a normalized harness event or an existing page marker.
#[derive(Debug, Clone)]
pub enum HistoryEvent {
    Harness(HarnessEvent),
    Other(Value),
}

impl From<TranscriptEntry> for HistoryEvent {
    fn from(entry: TranscriptEntry) -> Self {
        match entry {
            TranscriptEntry::UserPrompt(text) => Self::Other(user_prompt_payload(&text)),
            TranscriptEntry::Harness(ev) => Self::Harness(ev),
        }
    }
}

fn history_subagent_started(ev: &HistoryEvent) -> Option<&str> {
    match ev {
        HistoryEvent::Harness(event) => subagent_started(event),
        _ => None,
    }
}

/// A transcript read-back as a chat timeline, with each delegation card's
/// anchor put back where the card was requested
/// (`delegations::history_anchor_positions`).
pub fn history_timeline(
    entries: Vec<TimedEntry>,
    delegations: &[(String, String)],
) -> Vec<HistoryEvent> {
    let anchors = history_anchor_positions(&entries, delegations);
    let mut anchors = anchors.into_iter().peekable();
    let mut timeline = Vec::with_capacity(entries.len() + delegations.len());
    for (index, timed) in entries.into_iter().enumerate() {
        while let Some((_, id)) = anchors.next_if(|(at, _)| *at == index) {
            timeline.push(HistoryEvent::Other(anchor_payload(&id)));
        }
        let at_ms = timed
            .at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.timestamp_millis());
        let event = HistoryEvent::from(timed.entry);
        let is_prompt = matches!(
            &event,
            HistoryEvent::Other(value)
                if value.get("type").and_then(|value| value.as_str()) == Some("user_prompt")
        );
        let is_turn_end = matches!(
            &event,
            HistoryEvent::Harness(HarnessEvent::TurnCompleted { .. })
        );
        let started = history_subagent_started(&event).map(str::to_string);
        if is_turn_end {
            if let Some(at_ms) = at_ms {
                timeline.push(HistoryEvent::Other(turn_timing(None, Some(at_ms))));
            }
        }
        timeline.push(event);
        if is_prompt {
            if let Some(at_ms) = at_ms {
                timeline.push(HistoryEvent::Other(turn_timing(Some(at_ms), None)));
            }
        }
        if let (Some(parent), Some(at_ms)) = (started, at_ms) {
            timeline.push(HistoryEvent::Other(subagent_started_at(&parent, at_ms)));
        }
    }
    timeline.extend(anchors.map(|(_, id)| HistoryEvent::Other(anchor_payload(&id))));
    timeline
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn visual_reply_survives_transcript_and_serde_without_output_truncation() {
        let html = format!("<h1>{}</h1>", "é".repeat(100_000));
        let output = serde_json::to_string(&crate::chat::html_render::HtmlRenderResult {
            html_render: crate::chat::html_render::HtmlRenderRequest {
                html: html.clone(),
                height: 600,
            },
        })
        .unwrap();
        let transcript = [
            json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"tool_use","id":"visual","name":"mcp__gitterm_tasks__html_render","input":{"html":html,"height":600}}]}}),
            json!({"type":"user", "message":{"role":"user", "content":[{"type":"tool_result","tool_use_id":"visual","content":[{"type":"text","text":output}],"is_error":false}]}}),
        ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        let entries = parse_claude_transcript(&transcript);
        let encoded = serde_json::to_string(&entries).unwrap();
        let restored: Vec<TranscriptEntry> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(entries, restored);
        let Some(TranscriptEntry::Harness(HarnessEvent::ItemCompleted {
            output: restored,
            is_error,
            ..
        })) = restored.iter().find(|entry| {
            matches!(
                entry,
                TranscriptEntry::Harness(HarnessEvent::ItemCompleted { .. })
            )
        })
        else {
            panic!("render completion restored");
        };
        assert!(!is_error);
        assert_eq!(restored, &output);
        assert!(crate::chat::html_render::is_render_output(restored));
    }

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
    fn timed_entries_round_trip_through_json_for_remote_history() {
        let lines = [
            line(
                json!({"type":"user","timestamp":"2026-10-18T10:00:00Z","message":{"role":"user","content":"list the files"}}),
            ),
            assistant(
                json!([{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]),
            ),
            assistant(json!([{"type":"text","text":"Two files."}])),
        ];
        let entries = parse_claude_transcript_timed(&lines.join("\n"));
        assert!(entries.len() >= 3, "{entries:?}");
        assert_eq!(entries[0].at.as_deref(), Some("2026-10-18T10:00:00Z"));
        let json = serde_json::to_string(&entries).unwrap();
        let back: Vec<TimedEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entries);
        assert_eq!(
            serde_json::to_value(&entries[0]).unwrap(),
            json!({"at": "2026-10-18T10:00:00Z",
                   "entry": {"type": "user_prompt", "data": "list the files"}})
        );
    }

    #[test]
    fn synthetic_turn_end_does_not_inherit_idle_time_from_the_next_prompt() {
        let lines = [
            line(
                json!({"type":"user","timestamp":"2026-10-18T10:00:00Z","isSidechain":false,"message":{"role":"user","content":"first"}}),
            ),
            assistant(
                json!([{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"true"}}]),
            ),
            line(
                json!({"type":"user","timestamp":"2026-10-18T10:30:00Z","isSidechain":false,"message":{"role":"user","content":"second"}}),
            ),
        ];

        let entries = parse_claude_transcript_timed(&lines.join("\n"));
        let synthetic_end = entries
            .iter()
            .find(|entry| {
                matches!(
                    &entry.entry,
                    TranscriptEntry::Harness(HarnessEvent::TurnCompleted { .. })
                )
            })
            .expect("the next prompt still closes the preceding turn");
        assert_eq!(synthetic_end.at, None);
        assert_eq!(
            entries.last().unwrap().at.as_deref(),
            Some("2026-10-18T10:30:00Z")
        );
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

    #[test]
    fn a_result_written_before_its_call_still_closes_the_subagent() {
        // Seen in a real transcript: the async_launched result line comes
        // ahead of the assistant line that holds the Agent tool_use.
        let notice = "<task-notification>\n<task-id>t</task-id>\n<tool-use-id>early</tool-use-id>\n<status>completed</status>\n<summary>s</summary>\n<usage><subagent_tokens>10</subagent_tokens><tool_uses>1</tool_uses><duration_ms>20</duration_ms></usage>\n</task-notification>";
        let text = [
            user_text("go"),
            agent_result("early", json!({"status":"async_launched"})),
            agent_call("early"),
            line(
                json!({"type":"attachment","isSidechain":false,"attachment":{
                "type":"queued_command","commandMode":"task-notification",
                "prompt": notice}}),
            ),
        ]
        .join("\n");
        let entries = parse_claude_transcript(&text);
        assert_eq!(
            sub_events(&entries),
            vec![
                ("early".into(), "start".into()),
                (
                    "early".into(),
                    r#"Completed {"duration_ms":20,"tool_uses":1,"total_tokens":10}"#.into()
                ),
            ]
        );
        // The tool card completes after it starts, not only before.
        let started = entries.iter().position(|e| {
            matches!(e, TranscriptEntry::Harness(HarnessEvent::ItemStarted { id, .. }) if id == "early")
        });
        let completed_after = entries.iter().rposition(|e| {
            matches!(e, TranscriptEntry::Harness(HarnessEvent::ItemCompleted { id, .. }) if id == "early")
        });
        assert!(started.unwrap() < completed_after.unwrap());
    }
}
