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

use std::collections::HashSet;

use gitterm::chats::{ChatPreviewMessage, ChatRef};
use gitterm::harness::{HarnessEvent, ItemKind, MessageState, RuntimeRequestKind};
use serde_json::Value;

use crate::chat_overview::{ChatSnapshot, ChildActivity, OverviewSnapshot, ToolCall};
use crate::tab::AgentEvent;

/// Lines of the latest ask shown before "more".
pub(crate) const ASK_PREVIEW_LINES: usize = 4;
/// Lines of the latest reply shown before "more".
pub(crate) const REPLY_PREVIEW_LINES: usize = 16;
/// Files listed before "+N more".
pub(crate) const FILES_SHOWN: usize = 8;
/// Characters a clipped line may run to; a minified blob is one "line".
const CLIP_LINE_CHARS: usize = 200;

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

    #[test]
    fn clip_lines_cuts_lines_and_long_lines() {
        assert_eq!(clip_lines("a\nb", 4), ("a\nb".to_string(), false));
        assert_eq!(clip_lines("a\nb\nc", 2), ("a\nb".to_string(), true));
        let (long, cut) = clip_lines(&"x".repeat(500), 2);
        assert!(cut);
        assert_eq!(long.chars().count(), CLIP_LINE_CHARS);
    }
}
