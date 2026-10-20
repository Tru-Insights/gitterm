//! Selected-conversation summary (TRU-155): the pure parts.
//!
//! A conversation picked in the Chats panel opens on a summary above its
//! raw transcript: what the human last asked, Claude's plan, the files the
//! chat touched, what waits on the human and the latest reply. This module
//! derives those once per change from a conversation's events (an open
//! chat tab's buffer, or a transcript read back off the UI thread) or, when
//! only the preview tail is known (Codex, Pi, a remote without history),
//! from its messages. The header and the agents come from the Chats
//! overview's [`ChatSnapshot`]. No iced types, no app state.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use gitterm::chats::{ChatPreviewMessage, ChatRef};
use gitterm::harness::transcript::is_agent_tool;
use gitterm::harness::{HarnessEvent, ItemKind, MessageState, RuntimeRequestKind};
use serde_json::Value;

use crate::chat_overview::{
    first_line, first_line_raw, one_line, short_path, ChatGist, ChatSnapshot, ChildActivity,
    OverviewSnapshot, ToolCall,
};
use crate::tab::AgentEvent;

/// Lines of the latest ask shown before "more".
pub(crate) const ASK_PREVIEW_LINES: usize = 4;
/// Lines of the latest reply shown before "more".
pub(crate) const REPLY_PREVIEW_LINES: usize = 16;
/// Files listed before "+N more".
pub(crate) const FILES_SHOWN: usize = 8;
/// Characters a clipped line may run to; a minified blob is one "line".
const CLIP_LINE_CHARS: usize = 200;
/// Longest reply line or question on an overview row.
const ROW_LINE_CHARS: usize = 140;
/// Longest command or detail quoted in an overview row's ask.
const ASK_DETAIL_CHARS: usize = 60;
/// Claude's tool for asking the human structured questions.
const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    /// OpenCode's `todowrite` can cancel an item.
    Cancelled,
}

/// One item of the chat's latest todo list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TodoItem {
    pub(crate) text: String,
    pub(crate) status: TodoStatus,
}

/// A prompt waiting on the human, as it asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WaitingAsk {
    /// A tool call waits for approval: the tool, and the command, file or
    /// what else it would act on.
    Permission {
        tool: String,
        detail: Option<String>,
    },
    /// The model asked questions (AskUserQuestion): their text.
    Question { questions: Vec<String> },
}

/// What a conversation says about itself, for the summary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatDigest {
    /// The human's latest request: not a tool result, a CLI injection, a
    /// compaction summary or a delegation result sent in as a message.
    pub(crate) latest_ask: Option<String>,
    /// The latest TodoWrite (`todowrite`) list. Empty when there is none
    /// or it was cleared.
    pub(crate) todos: Vec<TodoItem>,
    /// Distinct paths the chat (and its subagents) edited or wrote, most
    /// recent first, as the tool calls name them.
    pub(crate) files_touched: Vec<String>,
    /// The last assistant message: the final text segment of the latest
    /// turn that wrote any.
    pub(crate) latest_reply: Option<String>,
    /// Prompts still open in the conversation, oldest first.
    pub(crate) waiting: Vec<WaitingAsk>,
}

/// Whether a user message is the human's own request.
pub(crate) fn is_human_request(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty()
        // CLI injections: <task-notification>, <system-reminder>, command
        // wrappers and caveats.
        || text.starts_with('<')
        || text.starts_with("[Request interrupted")
        || text.starts_with("This session is being continued from a previous conversation")
    {
        return false;
    }
    // A delegation result sent in as a user message ends with its pointer
    // (`delegations::compose_send_message`).
    let last = text.lines().last().unwrap_or("").trim();
    !last.starts_with("Full record: delegation_get ")
}

/// Whether a tool call writes the todo list (Claude's TodoWrite, OpenCode's
/// `todowrite`).
fn is_todo_tool(name: &str) -> bool {
    name.eq_ignore_ascii_case("TodoWrite")
}

/// The list a todo call writes, when its input carries one.
fn todo_list(input: &Value) -> Option<Vec<TodoItem>> {
    let items = input.get("todos")?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(|item| {
                let text = item
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())?;
                let status = match item.get("status").and_then(Value::as_str) {
                    Some("completed") => TodoStatus::Completed,
                    Some("in_progress") => TodoStatus::InProgress,
                    Some("cancelled") => TodoStatus::Cancelled,
                    _ => TodoStatus::Pending,
                };
                Some(TodoItem {
                    text: text.to_string(),
                    status,
                })
            })
            .collect(),
    )
}

fn str_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The paths an editing tool call names. Claude's Edit, MultiEdit, Write
/// and NotebookEdit carry `file_path` (`notebook_path`); OpenCode's edit
/// tools send their raw input (`filePath`); Gemini CLI sends only the
/// call's kind (`edit`) and the `paths` it touches.
fn edited_paths(name: &str, input: &Value) -> Vec<String> {
    match name {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => str_field(input, "file_path")
            .or_else(|| str_field(input, "notebook_path"))
            .map(str::to_string)
            .into_iter()
            .collect(),
        _ => {
            let acp_edit_tool = matches!(
                name.to_ascii_lowercase().as_str(),
                "edit" | "write" | "multiedit"
            );
            let edit_kind = str_field(input, "kind") == Some("edit");
            if !acp_edit_tool && !edit_kind {
                return Vec::new();
            }
            let mut paths: Vec<String> = ["filePath", "file_path", "path"]
                .iter()
                .find_map(|key| str_field(input, key))
                .map(str::to_string)
                .into_iter()
                .collect();
            for path in input
                .get("paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !paths.iter().any(|known| known == path) {
                    paths.push(path.to_string());
                }
            }
            paths
        }
    }
}

/// What a permission prompt would act on: its command, file, paths, URL
/// or pattern, else the call's own title (ACP).
fn permission_detail(input: &Value) -> Option<String> {
    for key in ["command", "file_path", "notebook_path", "filePath", "path"] {
        if let Some(value) = str_field(input, key) {
            return Some(value.to_string());
        }
    }
    let paths: Vec<&str> = input
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !paths.is_empty() {
        return Some(paths.join(", "));
    }
    ["url", "pattern", "query", "title"]
        .iter()
        .find_map(|key| str_field(input, key))
        .map(str::to_string)
}

fn waiting_ask(kind: &RuntimeRequestKind) -> WaitingAsk {
    match kind {
        RuntimeRequestKind::Permission {
            tool_name, input, ..
        } => WaitingAsk::Permission {
            tool: tool_name.clone(),
            // An ACP call named by its title says it once.
            detail: permission_detail(input).filter(|detail| detail != tool_name),
        },
        RuntimeRequestKind::Question { questions } => WaitingAsk::Question {
            questions: questions
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|question| str_field(question, "question"))
                .map(str::to_string)
                .collect(),
        },
    }
}

/// A tool call as the digest tracks it.
struct Call<'a> {
    call: ToolCall<'a>,
    /// The chat's own call, not a subagent's.
    top_level: bool,
    failed: bool,
}

/// One walk over a conversation.
#[derive(Default)]
struct Walk<'a> {
    /// (message id, text) of every prompt the human sent, in order.
    prompts: Vec<(Option<&'a str>, &'a str)>,
    /// Queued prompts withdrawn before Claude picked them up.
    cancelled: HashSet<&'a str>,
    calls: Vec<Call<'a>>,
    /// (request id, ask) of the prompts still open.
    requests: Vec<(&'a str, WaitingAsk)>,
    segment: String,
    /// The next reply text starts a new segment (a tool call, a prompt or
    /// a turn end came since the last).
    segment_break: bool,
    reply: Option<String>,
}

impl<'a> Walk<'a> {
    fn end_segment(&mut self) {
        if !self.segment.trim().is_empty() {
            self.reply = Some(self.segment.trim().to_string());
        }
        self.segment.clear();
    }

    fn prompt(&mut self, value: &'a Value) {
        if let Some(text) = value.get("text").and_then(Value::as_str) {
            self.prompts
                .push((value.get("id").and_then(Value::as_str), text));
        }
        self.segment_break = true;
    }

    fn event(&mut self, event: &'a HarnessEvent, top_level: bool) {
        match event {
            HarnessEvent::ItemStarted {
                id,
                kind: ItemKind::ToolCall { name, input },
            } => {
                self.calls.push(Call {
                    call: ToolCall::new(id, name, input),
                    top_level,
                    failed: false,
                });
                if top_level {
                    self.segment_break = true;
                }
            }
            HarnessEvent::ItemInputDelta { id, partial_json } => {
                if let Some(call) = self.calls.iter_mut().rev().find(|c| c.call.id == id) {
                    call.call.deltas.push(partial_json);
                }
            }
            HarnessEvent::ItemCompleted { id, is_error, .. } => {
                if let Some(call) = self.calls.iter_mut().rev().find(|c| c.call.id == id) {
                    call.failed = *is_error;
                }
            }
            HarnessEvent::TextDelta(text) if top_level => {
                if self.segment_break {
                    self.end_segment();
                    self.segment_break = false;
                }
                self.segment.push_str(text);
            }
            HarnessEvent::RuntimeRequest {
                request_id, kind, ..
            } => self.requests.push((request_id, waiting_ask(kind))),
            HarnessEvent::RuntimeRequestResolved { request_id } => {
                self.requests.retain(|(id, _)| id != request_id);
            }
            HarnessEvent::TurnCompleted { .. } if top_level => {
                self.requests.clear();
                self.segment_break = true;
            }
            HarnessEvent::ProcessExited { .. } => self.requests.clear(),
            HarnessEvent::SubagentEvent { event, .. } => self.event(event, false),
            HarnessEvent::MessageLifecycle {
                id,
                state: MessageState::Cancelled,
            } => {
                self.cancelled.insert(id);
            }
            _ => {}
        }
    }

    fn finish(mut self) -> ChatDigest {
        self.end_segment();
        let latest_ask = self
            .prompts
            .iter()
            .rev()
            .filter(|(id, _)| id.is_none_or(|id| !self.cancelled.contains(id)))
            .map(|(_, text)| *text)
            .find(|text| is_human_request(text))
            .map(|text| text.trim().to_string());
        // A list still streaming in does not parse yet: the one before it
        // stands until it does.
        let todos = self
            .calls
            .iter()
            .rev()
            .filter(|c| c.top_level && !c.failed && is_todo_tool(c.call.name))
            .find_map(|c| todo_list(&c.call.input()))
            .unwrap_or_default();
        let mut seen = HashSet::new();
        let mut files_touched = Vec::new();
        for c in self.calls.iter().rev().filter(|c| !c.failed) {
            for path in edited_paths(c.call.name, &c.call.input()) {
                if seen.insert(path.clone()) {
                    files_touched.push(path);
                }
            }
        }
        ChatDigest {
            latest_ask,
            todos,
            files_touched,
            latest_reply: self.reply,
            waiting: self.requests.into_iter().map(|(_, ask)| ask).collect(),
        }
    }
}

/// Derive the summary from a conversation's events: an open chat tab's
/// buffer, or a transcript's timeline read back as events.
pub(crate) fn digest(conversation: &[AgentEvent]) -> ChatDigest {
    let mut walk = Walk::default();
    for event in conversation {
        match event {
            AgentEvent::Harness(event) => walk.event(event, true),
            AgentEvent::Other(value)
                if value.get("type").and_then(Value::as_str) == Some("user_prompt") =>
            {
                walk.prompt(value);
            }
            _ => {}
        }
    }
    walk.finish()
}

/// What a preview tail alone tells: the latest ask and the latest reply.
/// For conversations whose events GitTerm cannot read back (Codex, Pi, a
/// remote without history).
pub(crate) fn digest_messages(messages: &[ChatPreviewMessage]) -> ChatDigest {
    ChatDigest {
        latest_ask: messages
            .iter()
            .rev()
            .filter(|message| message.is_user)
            .map(|message| message.text.as_str())
            .find(|text| is_human_request(text))
            .map(|text| text.trim().to_string()),
        latest_reply: messages
            .iter()
            .rev()
            .find(|message| !message.is_user && !message.text.trim().is_empty())
            .map(|message| message.text.trim().to_string()),
        ..ChatDigest::default()
    }
}

/// A digest in a few words, for a Chats overview row (TRU-154): the ask on
/// one line, the reply's first line, the plan's progress (cancelled items
/// left out) and the count of files touched.
pub(crate) fn gist(digest: &ChatDigest) -> ChatGist {
    gist_of(
        digest.latest_ask.as_deref(),
        digest.latest_reply.as_deref(),
        &digest.todos,
        digest.files_touched.len(),
    )
}

fn gist_of(asked: Option<&str>, reply: Option<&str>, todos: &[TodoItem], files: usize) -> ChatGist {
    let planned = todos
        .iter()
        .filter(|todo| todo.status != TodoStatus::Cancelled);
    ChatGist {
        asked: asked
            .map(|ask| one_line(ask, ROW_LINE_CHARS))
            .filter(|ask| !ask.is_empty()),
        reply: reply
            .map(first_line)
            .filter(|line| !line.is_empty())
            .map(|line| one_line(line, ROW_LINE_CHARS)),
        todos_done: planned
            .clone()
            .filter(|todo| todo.status == TodoStatus::Completed)
            .count(),
        todos_total: planned.count(),
        files,
    }
}

/// Todo lists a [`TranscriptTracker`] keeps, newest last: the latest
/// stands until a result says it failed, then the one before it.
const TRACKED_TODO_LISTS: usize = 4;

/// A Claude CLI transcript's gist and open ask, kept current by feeding it
/// only what was appended since the last read (TRU-154): a long session's
/// transcript runs to a hundred MB and more, too much to read back whole
/// every few seconds. Holds no events, only the state [`digest`]'s walk
/// would end with, under the same rules (a parity test holds it to them).
/// Transcript events carry whole tool inputs (no streamed deltas), and
/// their turn ends are the read-back's own markers, so neither matters.
#[derive(Debug, Clone, Default)]
pub(crate) struct TranscriptTracker {
    /// The human's latest request.
    asked: Option<String>,
    /// (call id, list) of the latest todo calls that have not failed.
    todo_lists: Vec<(String, Vec<TodoItem>)>,
    /// Path -> edit calls naming it that have not failed.
    touched: HashMap<String, usize>,
    /// Edit calls still without a result: id -> the paths they name.
    unresolved_edits: HashMap<String, Vec<String>>,
    /// The latest finished reply segment, and the one being written.
    reply: Option<String>,
    segment: String,
    /// The next reply text starts a new segment.
    segment_break: bool,
    /// Top-level calls with no result since the last prompt, oldest first:
    /// (id, the ask; `None` for an Agent call, which waits on its
    /// subagent).
    open: Vec<(String, Option<WaitingAsk>)>,
    /// Results written ahead of their call (Claude does, now and then).
    early_results: HashSet<String>,
}

impl TranscriptTracker {
    /// Take in the next events of the transcript, in order.
    pub(crate) fn feed(&mut self, events: &[AgentEvent]) {
        for event in events {
            match event {
                AgentEvent::Harness(event) => self.event(event, true),
                AgentEvent::Other(value)
                    if value.get("type").and_then(Value::as_str) == Some("user_prompt") =>
                {
                    let text = value.get("text").and_then(Value::as_str).unwrap_or("");
                    if is_human_request(text) {
                        self.asked = Some(text.trim().to_string());
                    }
                    self.segment_break = true;
                    // A new prompt abandons whatever the last turn left
                    // open; a call cut off like that never gets a result.
                    self.open.clear();
                    self.early_results.clear();
                    self.unresolved_edits.clear();
                }
                _ => {}
            }
        }
    }

    fn event(&mut self, event: &HarnessEvent, top_level: bool) {
        match event {
            HarnessEvent::ItemStarted {
                id,
                kind: ItemKind::ToolCall { name, input },
            } => {
                let answered = top_level && self.early_results.remove(id);
                let paths = edited_paths(name, input);
                if !paths.is_empty() {
                    for path in &paths {
                        *self.touched.entry(path.clone()).or_default() += 1;
                    }
                    self.unresolved_edits.insert(id.clone(), paths);
                }
                if !top_level {
                    return;
                }
                if is_todo_tool(name) {
                    if let Some(list) = todo_list(input) {
                        self.todo_lists.push((id.clone(), list));
                        let excess = self.todo_lists.len().saturating_sub(TRACKED_TODO_LISTS);
                        self.todo_lists.drain(..excess);
                    }
                }
                self.segment_break = true;
                if !answered {
                    let ask = (!is_agent_tool(name)).then(|| call_ask(name, input));
                    self.open.push((id.clone(), ask));
                }
            }
            HarnessEvent::ItemCompleted { id, is_error, .. } => {
                if let Some(paths) = self.unresolved_edits.remove(id) {
                    if *is_error {
                        for path in paths {
                            if let Some(count) = self.touched.get_mut(&path) {
                                *count -= 1;
                                if *count == 0 {
                                    self.touched.remove(&path);
                                }
                            }
                        }
                    }
                }
                if *is_error {
                    self.todo_lists.retain(|(call, _)| call != id);
                }
                if !top_level {
                    return;
                }
                match self.open.iter().position(|(call, _)| call == id) {
                    Some(index) => {
                        self.open.remove(index);
                    }
                    None => {
                        self.early_results.insert(id.clone());
                    }
                }
            }
            HarnessEvent::TextDelta(text) if top_level => {
                if self.segment_break {
                    if !self.segment.trim().is_empty() {
                        self.reply = Some(self.segment.trim().to_string());
                    }
                    self.segment.clear();
                    self.segment_break = false;
                }
                self.segment.push_str(text);
            }
            HarnessEvent::TurnCompleted { .. } if top_level => self.segment_break = true,
            HarnessEvent::SubagentEvent { event, .. } => self.event(event, false),
            _ => {}
        }
    }

    /// The transcript so far in a few words.
    pub(crate) fn gist(&self) -> ChatGist {
        let reply = if self.segment.trim().is_empty() {
            self.reply.as_deref()
        } else {
            Some(self.segment.trim())
        };
        let todos = self
            .todo_lists
            .last()
            .map(|(_, list)| list.as_slice())
            .unwrap_or(&[]);
        gist_of(self.asked.as_deref(), reply, todos, self.touched.len())
    }

    /// The ask the transcript leaves open: its oldest top-level call that
    /// has no result yet, after the last prompt. A CLI writes the call
    /// before it asks for approval, so while its terminal says it needs
    /// the human this is what it asks. `None` when nothing is open or the
    /// oldest open call is an Agent call (its subagent's calls are not in
    /// the transcript).
    pub(crate) fn pending(&self) -> Option<WaitingAsk> {
        self.open.first().and_then(|(_, ask)| ask.clone())
    }
}

/// What a tool call would ask the human: AskUserQuestion's questions, or
/// approval for the call and what it acts on.
fn call_ask(name: &str, input: &Value) -> WaitingAsk {
    if name == ASK_USER_QUESTION_TOOL {
        WaitingAsk::Question {
            questions: input
                .get("questions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|question| str_field(question, "question"))
                .map(str::to_string)
                .collect(),
        }
    } else {
        WaitingAsk::Permission {
            tool: name.to_string(),
            detail: permission_detail(input).filter(|detail| detail != name),
        }
    }
}

/// An open ask in one line for an overview row: "Allow Bash: `cargo
/// publish`?", "Edit src/main.rs?", the question itself. `cwd` shortens
/// paths.
pub(crate) fn ask_line(ask: &WaitingAsk, cwd: Option<&Path>) -> String {
    match ask {
        WaitingAsk::Question { questions } => match questions.split_first() {
            Some((first, [])) => one_line(first, ROW_LINE_CHARS),
            Some((first, more)) => {
                format!("{} (+{} more)", one_line(first, ROW_LINE_CHARS), more.len())
            }
            None => "A question for you".to_string(),
        },
        WaitingAsk::Permission { tool, detail } => {
            let detail = detail.as_deref();
            match (tool.as_str(), detail) {
                ("Bash", Some(command)) => format!(
                    "Allow Bash: `{}`?",
                    one_line(first_line_raw(command), ASK_DETAIL_CHARS)
                ),
                ("Edit" | "MultiEdit" | "NotebookEdit", Some(path)) => {
                    format!("Edit {}?", short_path(path, cwd))
                }
                ("Write", Some(path)) => format!("Write {}?", short_path(path, cwd)),
                ("ExitPlanMode", _) => "Approve the plan?".to_string(),
                (tool, detail) => {
                    // MCP tools are `mcp__<server>__<tool>`.
                    let tool = tool
                        .strip_prefix("mcp__")
                        .map(|rest| rest.split_once("__").map_or(rest, |(_, tool)| tool))
                        .unwrap_or(tool);
                    let tool = one_line(tool, ASK_DETAIL_CHARS);
                    match detail {
                        Some(detail) => format!(
                            "Allow {tool}: {}?",
                            one_line(first_line_raw(detail), ASK_DETAIL_CHARS)
                        ),
                        None => format!("Allow {tool}?"),
                    }
                }
            }
        }
    }
}

/// The chat as the Chats overview holds it, and the agents working for it:
/// its snapshot's children, then delegations whose requesting chat is this
/// one but no open tab (their parent tab was closed).
pub(crate) fn chat_in_overview<'a>(
    overview: &'a OverviewSnapshot,
    chat: &ChatRef,
) -> (Option<&'a ChatSnapshot>, Vec<&'a ChildActivity>) {
    let snapshot = overview
        .chats
        .iter()
        .find(|snapshot| snapshot.chat.as_ref() == Some(chat));
    let agents = snapshot
        .into_iter()
        .flat_map(|snapshot| snapshot.children.iter())
        .chain(
            overview
                .detached
                .iter()
                .filter(|detached| detached.parent_chat.as_ref() == Some(chat))
                .map(|detached| &detached.child),
        )
        .collect();
    (snapshot, agents)
}

/// The first `max_lines` lines of `text` (each cut to a sane width), and
/// whether anything was left out.
pub(crate) fn clip_lines(text: &str, max_lines: usize) -> (String, bool) {
    let mut cut = false;
    let mut lines = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if index == max_lines {
            cut = true;
            break;
        }
        if line.chars().count() > CLIP_LINE_CHARS {
            let mut short: String = line.chars().take(CLIP_LINE_CHARS - 1).collect();
            short.push('…');
            lines.push(short);
            cut = true;
        } else {
            lines.push(line.to_string());
        }
    }
    (lines.join("\n"), cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_overview::{
        ChatState, ChildKind, ChildStatus, DetachedActivity, Harness, NavTarget,
    };
    use gitterm::harness::transcript::parse_claude_transcript;
    use gitterm::harness::TurnStatus;
    use serde_json::json;

    fn h(event: HarnessEvent) -> AgentEvent {
        AgentEvent::Harness(event)
    }

    fn prompt(text: &str) -> AgentEvent {
        AgentEvent::user_prompt(text)
    }

    fn tool(id: &str, name: &str, input: Value) -> AgentEvent {
        h(HarnessEvent::ItemStarted {
            id: id.into(),
            kind: ItemKind::ToolCall {
                name: name.into(),
                input,
            },
        })
    }

    fn result(id: &str, is_error: bool) -> AgentEvent {
        h(HarnessEvent::ItemCompleted {
            id: id.into(),
            output: String::new(),
            is_error,
        })
    }

    fn text(t: &str) -> AgentEvent {
        h(HarnessEvent::TextDelta(t.into()))
    }

    fn turn_done() -> AgentEvent {
        h(HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Value::Null,
            cost_usd: None,
        })
    }

    #[test]
    fn latest_ask_skips_injections_results_and_withdrawn_prompts() {
        let conversation = vec![
            prompt("Port the parser to the new API"),
            tool("t1", "Bash", json!({"command": "ls"})),
            result("t1", false),
            text("Done."),
            turn_done(),
            prompt("<task-notification><task-id>x</task-id></task-notification>"),
            prompt("Codex review d-7: verdict correct, no findings.\nFull record: delegation_get d-7"),
            prompt("This session is being continued from a previous conversation that ran out of context."),
            prompt("[Request interrupted by user]"),
            AgentEvent::Other(json!({"type": "user_prompt", "text": "never mind", "id": "u9", "queued": true})),
            h(HarnessEvent::MessageLifecycle {
                id: "u9".into(),
                state: MessageState::Cancelled,
            }),
        ];
        let digest = digest(&conversation);
        assert_eq!(
            digest.latest_ask.as_deref(),
            Some("Port the parser to the new API")
        );
        assert!(is_human_request("Explain the <div> tag"));
        assert!(!is_human_request("  "));
    }

    #[test]
    fn latest_reply_is_the_last_segment_after_the_last_tool_call() {
        let conversation = vec![
            prompt("fix it"),
            text("Let me look."),
            tool("t1", "Read", json!({"file_path": "/r/a.rs"})),
            result("t1", false),
            text("Fixed: "),
            text("the bounds check was off by one."),
            // A subagent's text never becomes the chat's reply.
            h(HarnessEvent::SubagentEvent {
                parent_tool_use_id: "t9".into(),
                event: Box::new(HarnessEvent::TextDelta("sub text".into())),
            }),
            turn_done(),
            // A new turn with no text yet: the last reply stands.
            prompt("and the tests?"),
            tool("t2", "Bash", json!({"command": "cargo test"})),
        ];
        assert_eq!(
            digest(&conversation).latest_reply.as_deref(),
            Some("Fixed: the bounds check was off by one.")
        );
    }

    #[test]
    fn todos_come_from_the_latest_list_including_one_streamed_in() {
        let conversation = vec![
            tool(
                "w1",
                "TodoWrite",
                json!({"todos": [{"content": "old", "status": "pending", "activeForm": "Old"}]}),
            ),
            result("w1", false),
            // Streamed: announced empty, filled by deltas.
            tool("w2", "TodoWrite", json!({})),
            h(HarnessEvent::ItemInputDelta {
                id: "w2".into(),
                partial_json: r#"{"todos":[{"content":"Read the code","status":"completed"},"#
                    .into(),
            }),
            h(HarnessEvent::ItemInputDelta {
                id: "w2".into(),
                partial_json: r#"{"content":"Write the fix","status":"in_progress"},{"content":"Run tests","status":"pending"}]}"#.into(),
            }),
        ];
        let todos = digest(&conversation).todos;
        assert_eq!(
            todos,
            vec![
                TodoItem {
                    text: "Read the code".into(),
                    status: TodoStatus::Completed
                },
                TodoItem {
                    text: "Write the fix".into(),
                    status: TodoStatus::InProgress
                },
                TodoItem {
                    text: "Run tests".into(),
                    status: TodoStatus::Pending
                },
            ]
        );
        // Mid-stream (not yet parseable) the previous list stands.
        let partial = &conversation[..4];
        assert_eq!(digest(partial).todos[0].text, "old");
        // A subagent's list is not the chat's plan.
        let sub_only = vec![h(HarnessEvent::SubagentEvent {
            parent_tool_use_id: "a1".into(),
            event: Box::new(HarnessEvent::ItemStarted {
                id: "s1".into(),
                kind: ItemKind::ToolCall {
                    name: "TodoWrite".into(),
                    input: json!({"todos": [{"content": "sub", "status": "pending"}]}),
                },
            }),
        })];
        assert!(digest(&sub_only).todos.is_empty());
    }

    #[test]
    fn opencode_todowrite_and_cancelled_items() {
        let conversation = vec![tool(
            "call_1",
            "todowrite",
            json!({"todos": [
                {"content": "Plan", "status": "completed", "priority": "high", "id": "1"},
                {"content": "Drop it", "status": "cancelled", "priority": "low", "id": "2"}
            ]}),
        )];
        let todos = digest(&conversation).todos;
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[1].status, TodoStatus::Cancelled);
    }

    #[test]
    fn files_touched_are_distinct_most_recent_first_across_harness_shapes() {
        let conversation = vec![
            tool("e1", "Edit", json!({"file_path": "/r/src/a.rs"})),
            result("e1", false),
            tool("e2", "Write", json!({"file_path": "/r/src/b.rs"})),
            result("e2", false),
            tool("e3", "NotebookEdit", json!({"notebook_path": "/r/n.ipynb"})),
            // A failed write touched nothing.
            tool("e4", "Write", json!({"file_path": "/r/denied.rs"})),
            result("e4", true),
            // Reads are not edits.
            tool("r1", "Read", json!({"file_path": "/r/src/c.rs"})),
            // A subagent's edit counts.
            h(HarnessEvent::SubagentEvent {
                parent_tool_use_id: "a1".into(),
                event: Box::new(HarnessEvent::ItemStarted {
                    id: "s1".into(),
                    kind: ItemKind::ToolCall {
                        name: "MultiEdit".into(),
                        input: json!({"file_path": "/r/src/d.rs"}),
                    },
                }),
            }),
            // Gemini CLI (ACP): named by title, kind and paths only.
            tool(
                "write_file-1",
                "Writing to a.txt",
                json!({"title": "Writing to a.txt", "kind": "edit", "paths": ["/tmp/a.txt"]}),
            ),
            // OpenCode (ACP): its raw input arrives in a later update.
            tool("call_2", "edit", json!({"kind": "edit"})),
            h(HarnessEvent::ItemInputDelta {
                id: "call_2".into(),
                partial_json:
                    json!({"filePath": "/r/src/e.rs", "oldString": "a", "newString": "b"})
                        .to_string(),
            }),
            // An ACP shell call is not an edit.
            tool(
                "call_3",
                "shell",
                json!({"command": "touch x", "cwd": "/tmp"}),
            ),
            // Edited again: moves to the front.
            tool("e5", "Edit", json!({"file_path": "/r/src/a.rs"})),
        ];
        assert_eq!(
            digest(&conversation).files_touched,
            vec![
                "/r/src/a.rs",
                "/r/src/e.rs",
                "/tmp/a.txt",
                "/r/src/d.rs",
                "/r/n.ipynb",
                "/r/src/b.rs",
            ]
        );
    }

    #[test]
    fn waiting_names_the_tool_and_what_it_acts_on_until_resolved() {
        let permission = |request_id: &str, tool_name: &str, input: Value| {
            h(HarnessEvent::RuntimeRequest {
                request_id: request_id.into(),
                tool_use_id: format!("tu-{request_id}"),
                kind: RuntimeRequestKind::Permission {
                    tool_name: tool_name.into(),
                    input,
                    suggestions: Vec::new(),
                    decision_reason: None,
                },
            })
        };
        let conversation = vec![
            permission("r1", "Bash", json!({"command": "rm -rf target"})),
            permission(
                "r2",
                "Write",
                json!({"file_path": "/r/a.rs", "content": "x"}),
            ),
            // Gemini CLI: the tool is its title; the detail is not repeated.
            permission(
                "r3",
                "echo hi",
                json!({"title": "echo hi", "kind": "execute"}),
            ),
            h(HarnessEvent::RuntimeRequest {
                request_id: "r4".into(),
                tool_use_id: "tu-r4".into(),
                kind: RuntimeRequestKind::Question {
                    questions: json!([
                        {"question": "Which database?", "options": []},
                        {"question": "Keep the old API?"}
                    ]),
                },
            }),
            h(HarnessEvent::RuntimeRequestResolved {
                request_id: "r2".into(),
            }),
        ];
        assert_eq!(
            digest(&conversation).waiting,
            vec![
                WaitingAsk::Permission {
                    tool: "Bash".into(),
                    detail: Some("rm -rf target".into())
                },
                WaitingAsk::Permission {
                    tool: "echo hi".into(),
                    detail: None
                },
                WaitingAsk::Question {
                    questions: vec!["Which database?".into(), "Keep the old API?".into()]
                },
            ]
        );
        let mut ended = conversation.clone();
        ended.push(turn_done());
        assert!(digest(&ended).waiting.is_empty());
    }

    #[test]
    fn a_claude_transcript_reads_back_into_the_same_digest() {
        let lines = [
            json!({"type": "user", "message": {"role": "user", "content": "Add a retry to the uploader"}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "text", "text": "I'll plan it."},
                {"type": "tool_use", "id": "tw1", "name": "TodoWrite", "input": {"todos": [
                    {"content": "Find the uploader", "status": "completed", "activeForm": "Finding"},
                    {"content": "Add the retry", "status": "in_progress", "activeForm": "Adding"}
                ]}}
            ]}}),
            json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tw1", "content": "ok"}
            ]}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "ed1", "name": "Edit", "input": {"file_path": "/r/src/upload.rs", "old_string": "a", "new_string": "b"}}
            ]}}),
            json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "ed1", "content": "edited"}
            ]}}),
            json!({"type": "user", "message": {"role": "user", "content": "<task-notification><task-id>b1</task-id><status>completed</status></task-notification>"}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "text", "text": "Added a retry with backoff."}
            ]}}),
        ]
        .map(|line| line.to_string())
        .join("\n");
        let events: Vec<AgentEvent> = parse_claude_transcript(&lines)
            .into_iter()
            .map(AgentEvent::from)
            .collect();
        let digest = digest(&events);
        assert_eq!(
            digest.latest_ask.as_deref(),
            Some("Add a retry to the uploader")
        );
        assert_eq!(digest.todos.len(), 2);
        assert_eq!(digest.todos[1].status, TodoStatus::InProgress);
        assert_eq!(digest.files_touched, vec!["/r/src/upload.rs"]);
        assert_eq!(
            digest.latest_reply.as_deref(),
            Some("Added a retry with backoff.")
        );
        assert!(digest.waiting.is_empty());
    }

    #[test]
    fn preview_messages_give_the_ask_and_the_reply() {
        let message = |is_user: bool, text: &str| ChatPreviewMessage {
            is_user,
            text: text.into(),
        };
        let digest = digest_messages(&[
            message(true, "Rename the crate"),
            message(false, "Renamed."),
            message(
                true,
                "Codex consult d-1:\nSplit it.\nFull record: delegation_get d-1",
            ),
            message(false, "Thanks, noted."),
        ]);
        assert_eq!(digest.latest_ask.as_deref(), Some("Rename the crate"));
        assert_eq!(digest.latest_reply.as_deref(), Some("Thanks, noted."));
        assert!(digest.todos.is_empty() && digest.files_touched.is_empty());
    }

    #[test]
    fn agents_are_the_snapshot_children_and_detached_delegations_of_the_chat() {
        let chat = ChatRef::local("c1");
        let child = |label: &str| ChildActivity {
            kind: ChildKind::Worker,
            label: label.into(),
            status: ChildStatus::Running,
            step: None,
            started_at: None,
            finished_at: None,
            result: None,
            target: NavTarget::Nowhere,
        };
        let snapshot = ChatSnapshot {
            chat: Some(chat.clone()),
            title: "t".into(),
            machine: "This Mac".into(),
            repo: "r".into(),
            branch: None,
            harness: Harness::Claude,
            model: None,
            state: ChatState::Idle,
            step: None,
            turn_started_at: None,
            cost_usd: None,
            children: vec![child("sub")],
            last_finished: None,
            gist: None,
            last_activity: None,
            target: NavTarget::Tab(1),
        };
        let detached = |label: &str, parent_chat: Option<ChatRef>| DetachedActivity {
            parent: "p".into(),
            parent_chat,
            machine: "This Mac".into(),
            child: child(label),
        };
        let overview = OverviewSnapshot {
            chats: vec![snapshot],
            detached: vec![
                detached("worker", Some(chat.clone())),
                detached("other", Some(ChatRef::local("c2"))),
                detached("unknown", None),
            ],
        };
        let (found, agents) = chat_in_overview(&overview, &chat);
        assert_eq!(found.map(|s| s.target.clone()), Some(NavTarget::Tab(1)));
        let labels: Vec<_> = agents.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, vec!["sub", "worker"]);
        let (none, agents) = chat_in_overview(&overview, &ChatRef::local("c2"));
        assert!(none.is_none());
        assert_eq!(agents.len(), 1);
    }

    /// The ask a whole transcript leaves open.
    fn pending_tool_ask(conversation: &[AgentEvent]) -> Option<WaitingAsk> {
        let mut tracker = TranscriptTracker::default();
        tracker.feed(conversation);
        tracker.pending()
    }

    fn transcript(lines: &[Value]) -> Vec<AgentEvent> {
        let text = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        parse_claude_transcript(&text)
            .into_iter()
            .map(AgentEvent::from)
            .collect()
    }

    fn assistant_tool(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": name, "input": input}
        ]}})
    }

    fn tool_result(id: &str) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": "ok"}
        ]}})
    }

    #[test]
    fn a_transcript_ending_in_an_unanswered_bash_call_asks_to_allow_it() {
        let events = transcript(&[
            json!({"type": "user", "message": {"role": "user", "content": "Publish the crate"}}),
            assistant_tool("b1", "Bash", json!({"command": "cargo test"})),
            tool_result("b1"),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "text", "text": "Tests pass. Publishing."}
            ]}}),
            assistant_tool(
                "b2",
                "Bash",
                json!({"command": "cargo publish\n--allow-dirty", "description": "Publish"}),
            ),
        ]);
        let ask = pending_tool_ask(&events).expect("an open call");
        assert_eq!(
            ask,
            WaitingAsk::Permission {
                tool: "Bash".into(),
                detail: Some("cargo publish\n--allow-dirty".into())
            }
        );
        assert_eq!(ask_line(&ask, None), "Allow Bash: `cargo publish`?");

        // Answered (allowed or denied): nothing is open.
        let mut answered = events.clone();
        answered.extend(transcript(&[tool_result("b2")]));
        assert_eq!(pending_tool_ask(&answered), None);
        // A new prompt abandons the call.
        let mut moved_on = events;
        moved_on.push(AgentEvent::user_prompt("never mind"));
        assert_eq!(pending_tool_ask(&moved_on), None);
    }

    #[test]
    fn a_transcript_ending_in_ask_user_question_shows_the_question() {
        let events = transcript(&[
            json!({"type": "user", "message": {"role": "user", "content": "Set up the database"}}),
            assistant_tool(
                "q1",
                "AskUserQuestion",
                json!({"questions": [
                    {"question": "Which database should I use?", "header": "DB",
                     "options": [{"label": "Postgres"}, {"label": "SQLite"}], "multiSelect": false},
                    {"question": "Keep the old schema?", "header": "Schema", "options": []}
                ]}),
            ),
        ]);
        let ask = pending_tool_ask(&events).expect("an open question");
        assert_eq!(
            ask,
            WaitingAsk::Question {
                questions: vec![
                    "Which database should I use?".into(),
                    "Keep the old schema?".into()
                ]
            }
        );
        assert_eq!(
            ask_line(&ask, None),
            "Which database should I use? (+1 more)"
        );
    }

    #[test]
    fn an_open_agent_call_waits_on_its_subagent_not_the_human() {
        let events = transcript(&[
            json!({"type": "user", "message": {"role": "user", "content": "Find the callers"}}),
            assistant_tool("a1", "Agent", json!({"description": "find callers"})),
        ]);
        assert_eq!(pending_tool_ask(&events), None);
        // Parallel calls: the oldest open one is the ask.
        let events = transcript(&[
            json!({"type": "user", "message": {"role": "user", "content": "Fix it"}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "r1", "name": "Read", "input": {"file_path": "/r/a.rs"}},
                {"type": "tool_use", "id": "e1", "name": "Edit", "input": {"file_path": "/r/src/main.rs"}},
                {"type": "tool_use", "id": "w1", "name": "Write", "input": {"file_path": "/r/b.rs"}}
            ]}}),
            tool_result("r1"),
        ]);
        let ask = pending_tool_ask(&events).expect("an open edit");
        assert_eq!(ask_line(&ask, Some(Path::new("/r"))), "Edit src/main.rs?");
    }

    #[test]
    fn the_tracker_fed_in_pieces_matches_the_digest_of_the_whole_transcript() {
        let lines = [
            json!({"type": "user", "message": {"role": "user", "content": "Add a retry to the uploader"}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "text", "text": "I'll plan it."},
                {"type": "tool_use", "id": "tw1", "name": "TodoWrite", "input": {"todos": [
                    {"content": "Find the uploader", "status": "completed", "activeForm": "Finding"},
                    {"content": "Add the retry", "status": "in_progress", "activeForm": "Adding"},
                    {"content": "Skip this", "status": "cancelled", "activeForm": "Skipping"}
                ]}}
            ]}}),
            tool_result("tw1"),
            assistant_tool("ed1", "Edit", json!({"file_path": "/r/src/upload.rs"})),
            tool_result("ed1"),
            // A denied write touched nothing; a failed todo list is not the plan.
            assistant_tool(
                "w1",
                "Write",
                json!({"file_path": "/r/denied.rs", "content": "x"}),
            ),
            json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "w1", "content": "denied", "is_error": true}
            ]}}),
            assistant_tool(
                "tw2",
                "TodoWrite",
                json!({"todos": [
                    {"content": "Broken", "status": "pending"}
                ]}),
            ),
            json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tw2", "content": "bad", "is_error": true}
            ]}}),
            assistant_tool("ed2", "MultiEdit", json!({"file_path": "/r/src/upload.rs"})),
            tool_result("ed2"),
            assistant_tool(
                "ed3",
                "Write",
                json!({"file_path": "/r/tests/retry.rs", "content": "t"}),
            ),
            tool_result("ed3"),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "text", "text": "Added a retry with backoff.\n\nDetails below."}
            ]}}),
            json!({"type": "user", "message": {"role": "user", "content": "<system-reminder>x</system-reminder>"}}),
            json!({"type": "user", "message": {"role": "user", "content": "Now publish it"}}),
            assistant_tool("b1", "Bash", json!({"command": "cargo publish"})),
        ];
        let whole = transcript(&lines);
        let expected = gist(&digest(&whole));
        assert_eq!(
            expected,
            ChatGist {
                asked: Some("Now publish it".into()),
                reply: Some("Added a retry with backoff.".into()),
                todos_done: 1,
                todos_total: 2,
                files: 2,
            }
        );
        // Every split point: the file read in two pieces, each parsed on
        // its own as the overview reads appended lines.
        for split in 0..=lines.len() {
            let mut tracker = TranscriptTracker::default();
            tracker.feed(&transcript(&lines[..split]));
            tracker.feed(&transcript(&lines[split..]));
            assert_eq!(tracker.gist(), expected, "split at {split}");
            assert_eq!(
                tracker.pending(),
                Some(WaitingAsk::Permission {
                    tool: "Bash".into(),
                    detail: Some("cargo publish".into())
                }),
                "split at {split}"
            );
        }
        assert_eq!(
            TranscriptTracker::default().gist(),
            gist(&ChatDigest::default())
        );
    }

    #[test]
    fn ask_lines_name_the_tool_and_what_it_acts_on() {
        let permission = |tool: &str, detail: Option<&str>| WaitingAsk::Permission {
            tool: tool.into(),
            detail: detail.map(str::to_string),
        };
        let cwd = Some(Path::new("/repo"));
        assert_eq!(
            ask_line(&permission("Write", Some("/repo/src/new.rs")), cwd),
            "Write src/new.rs?"
        );
        assert_eq!(
            ask_line(&permission("ExitPlanMode", None), cwd),
            "Approve the plan?"
        );
        assert_eq!(
            ask_line(&permission("WebFetch", Some("https://docs.rs/iced")), cwd),
            "Allow WebFetch: https://docs.rs/iced?"
        );
        assert_eq!(
            ask_line(&permission("mcp__github__create_issue", None), cwd),
            "Allow create_issue?"
        );
        assert_eq!(
            ask_line(&WaitingAsk::Question { questions: vec![] }, cwd),
            "A question for you"
        );
    }

    #[test]
    fn gist_trims_the_ask_takes_the_reply_line_and_counts_progress() {
        let todo = |text: &str, status| TodoItem {
            text: text.into(),
            status,
        };
        let digest = ChatDigest {
            latest_ask: Some("  Port the parser\n  to the new API  ".into()),
            todos: vec![
                todo("a", TodoStatus::Completed),
                todo("b", TodoStatus::Completed),
                todo("c", TodoStatus::InProgress),
                todo("d", TodoStatus::Cancelled),
            ],
            files_touched: vec!["/r/a.rs".into(), "/r/b.rs".into()],
            latest_reply: Some("\n## Ported\n\nAll tests pass.".into()),
            waiting: Vec::new(),
        };
        assert_eq!(
            gist(&digest),
            ChatGist {
                asked: Some("Port the parser to the new API".into()),
                reply: Some("Ported".into()),
                todos_done: 2,
                todos_total: 3,
                files: 2,
            }
        );
        assert_eq!(gist(&ChatDigest::default()), ChatGist::default());
    }

    #[test]
    fn clip_lines_cuts_lines_and_long_lines() {
        assert_eq!(clip_lines("a\nb", 4), ("a\nb".to_string(), false));
        assert_eq!(clip_lines("a\nb\nc", 2), ("a\nb".to_string(), true));
        let (long, cut) = clip_lines(&"x".repeat(500), 2);
        assert!(cut);
        assert_eq!(long.chars().count(), CLIP_LINE_CHARS);
    }
}
