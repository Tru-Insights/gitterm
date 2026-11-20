use crate::events::*;
use serde_json::Value;
use std::{collections::HashMap, ops::Range};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    User,
    Assistant,
    Thinking,
    Tool,
    Permission,
    Question,
    Worker,
    Delegation,
    Error,
    Status,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditDiff {
    pub path: String,
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    pub input: Value,
    pub partial_input: String,
    pub output: Option<String>,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub id: String,
    pub owner: Vec<String>,
    pub order: u64,
    pub identity: Option<gitterm_core::chat::state::RequestIdentity>,
    pub kind: RuntimeRequestKind,
    pub resolved: bool,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub id: u64,
    pub kind: RowKind,
    pub scope: String,
    pub title: String,
    pub body: String,
    pub tool: Option<Tool>,
    pub decision: Option<DecisionState>,
    pub request: Option<Request>,
}
impl Row {
    pub fn collapsible(&self) -> bool {
        matches!(
            self.kind,
            RowKind::Thinking | RowKind::Tool | RowKind::Worker | RowKind::Delegation
        )
    }
    pub fn edits(&self) -> Vec<EditDiff> {
        let Some(tool) = &self.tool else {
            return vec![];
        };
        let input = &tool.input;
        let path = input["file_path"]
            .as_str()
            .unwrap_or("(path not provided)")
            .to_owned();
        let edit = |value: &Value| -> Option<EditDiff> {
            Some(EditDiff {
                path: path.clone(),
                old: value["old_string"].as_str()?.into(),
                new: value["new_string"].as_str()?.into(),
            })
        };
        match tool.name.as_str() {
            "Edit" => edit(input).into_iter().collect(),
            "MultiEdit" => input["edits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(edit)
                .collect(),
            "Write" => input["content"]
                .as_str()
                .map(|new| EditDiff {
                    path,
                    old: String::new(),
                    new: new.into(),
                })
                .into_iter()
                .collect(),
            _ => vec![],
        }
    }
    pub fn search_text(&self) -> String {
        format!("{}\n{}", self.title, plain_markdown(&self.body))
    }
}

#[derive(Default)]
pub struct ChatModel {
    pub rows: Vec<Row>,
    streams: HashMap<String, (RowKind, usize)>,
    items: HashMap<(String, String), usize>,
    requests: HashMap<(Vec<String>, String), usize>,
    request_owner: Vec<String>,
    prompts: HashMap<String, usize>,
    workers: HashMap<String, usize>,
    turn_start: HashMap<String, u64>,
    next_id: u64,
    recorded_time: Option<u64>,
}
impl ChatModel {
    pub fn pending_rows(&self) -> Vec<usize> {
        let mut ordered: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let request = row.request.as_ref()?;
                (!request.resolved && row.decision.is_none()).then_some((request.order, index))
            })
            .collect();
        ordered.sort_by_key(|(order, _)| *order);
        ordered.into_iter().map(|(_, index)| index).collect()
    }
    pub fn from_records(records: &[Record]) -> Self {
        let mut model = Self::default();
        for record in records {
            model.apply(record);
        }
        model
    }
    /// Rows are a disposable projection of core State::conversation.
    pub fn from_conversation(events: &[gitterm_core::chat::state::AgentEvent]) -> Self {
        let mut model = Self::default();
        for event in events {
            model.apply_agent(event);
        }
        model
    }
    pub fn apply_agent(&mut self, event: &gitterm_core::chat::state::AgentEvent) -> Vec<usize> {
        use gitterm_core::chat::state::AgentEvent;
        let entry = match event {
            AgentEvent::Harness(event) => Entry::Harness(event.clone()),
            AgentEvent::Other(value) if value["kind"] == "turn_timing" => {
                if let Some(start) = value["start_ms"].as_u64() {
                    self.turn_start.insert(String::new(), start);
                }
                self.recorded_time = value["end_ms"].as_u64();
                return vec![];
            }
            AgentEvent::Other(value) if value["kind"] == "subagent_started_at" => {
                if let (Some(id), Some(at)) = (value["id"].as_str(), value["at_ms"].as_u64()) {
                    self.turn_start.insert(id.into(), at);
                }
                return vec![];
            }
            AgentEvent::Other(value) if value["type"] == "user_prompt" => {
                let Some(text) = value["text"].as_str() else {
                    return self.apply(&Record {
                        at_ms: None,
                        entry: Entry::Harness(HarnessEvent::Error(
                            "Invalid user prompt event: missing text".into(),
                        )),
                    });
                };
                let delivery = match value.get("delivery").filter(|v| !v.is_null()) {
                    Some(value) => match serde_json::from_value(value.clone()) {
                        Ok(delivery) => Some(delivery),
                        Err(error) => {
                            return self.apply(&Record {
                                at_ms: None,
                                entry: Entry::Harness(HarnessEvent::Error(format!(
                                    "Invalid prompt delivery card: {error}"
                                ))),
                            });
                        }
                    },
                    None => None,
                };
                Entry::Harness(HarnessEvent::UserMessageEcho {
                    id: value["id"].as_str().map(str::to_owned),
                    text: text.into(),
                    image_count: value["images"].as_array().map_or_else(
                        || value["image_count"].as_u64().unwrap_or(0) as usize,
                        Vec::len,
                    ),
                    delivery,
                })
            }
            _ => return vec![],
        };
        let at_ms = self.recorded_time.take();
        self.apply(&Record { at_ms, entry })
    }
    fn push(&mut self, kind: RowKind, scope: &str, title: String, body: String) -> usize {
        let index = self.rows.len();
        self.rows.push(Row {
            id: self.next_id,
            kind,
            scope: scope.into(),
            title,
            body,
            tool: None,
            decision: None,
            request: None,
        });
        self.next_id += 1;
        index
    }
    /// Returns changed row indexes so UI text state can update without scanning history.
    pub fn apply(&mut self, record: &Record) -> Vec<usize> {
        let mut changed = vec![];
        self.entry(&record.entry, record.at_ms, "", &mut changed);
        changed.sort_unstable();
        changed.dedup();
        changed
    }
    fn entry(&mut self, entry: &Entry, at: Option<u64>, scope: &str, changed: &mut Vec<usize>) {
        match entry {
            Entry::UserPrompt(text) => {
                self.streams.remove(scope);
                changed.push(self.push(RowKind::User, scope, "You".into(), literal(text)));
            }
            Entry::Harness(event) => self.event(event, at, scope, changed),
            Entry::DisplayDecision {
                request_id,
                state,
                detail,
            } => {
                if let Some(&index) = self
                    .requests
                    .get(&(self.request_owner.clone(), request_id.clone()))
                {
                    let row = &mut self.rows[index];
                    row.decision = Some(*state);
                    row.title = format!(
                        "{} · {} · {request_id}",
                        if row.kind == RowKind::Question {
                            "Question"
                        } else {
                            "Permission"
                        },
                        match state {
                            DecisionState::Answered => "Answered",
                            DecisionState::Denied => "Denied",
                        }
                    );
                    row.body.push_str(&format!("\n\n{}", literal(detail)));
                    changed.push(index);
                } else {
                    changed.push(self.push(
                        RowKind::Error,
                        scope,
                        "Unmatched display decision".into(),
                        literal(request_id),
                    ));
                }
            }
            Entry::DisplayDelegation {
                id,
                title,
                state,
                body,
            } => {
                changed.push(self.push(
                    RowKind::Delegation,
                    scope,
                    format!("Delegation {id} · {title} · {state}"),
                    literal(body),
                ));
            }
        }
    }
    fn event(
        &mut self,
        event: &HarnessEvent,
        at: Option<u64>,
        scope: &str,
        changed: &mut Vec<usize>,
    ) {
        if matches!(
            event,
            HarnessEvent::TurnCompleted { .. }
                | HarnessEvent::ProcessExited { .. }
                | HarnessEvent::ConversationReset { .. }
        ) {
            for ((owner, _), &index) in &self.requests {
                if owner.starts_with(&self.request_owner) {
                    self.rows[index].request.as_mut().unwrap().resolved = true;
                    changed.push(index);
                }
            }
        }
        match event {
            HarnessEvent::TextDelta(text) | HarnessEvent::ThinkingDelta(text) => {
                let kind = if matches!(event, HarnessEvent::TextDelta(_)) {
                    RowKind::Assistant
                } else {
                    RowKind::Thinking
                };
                let index = match self.streams.get(scope).copied() {
                    Some((previous, index)) if previous == kind => index,
                    _ => {
                        let title = if kind == RowKind::Thinking {
                            "Thinking"
                        } else {
                            "Assistant"
                        };
                        let index = self.push(kind, scope, title.into(), String::new());
                        self.streams.insert(scope.into(), (kind, index));
                        index
                    }
                };
                self.rows[index].body.push_str(text);
                changed.push(index);
            }
            HarnessEvent::TurnStarted { model, .. } => {
                self.streams.remove(scope);
                if let Some(at) = at {
                    self.turn_start.insert(scope.into(), at);
                } else {
                    self.turn_start.remove(scope);
                }
                if !scope.is_empty() {
                    let index = self.push(
                        RowKind::Worker,
                        scope,
                        format!("Worker {scope} · Running"),
                        literal(model.as_deref().unwrap_or("Model not reported")),
                    );
                    self.workers.insert(scope.into(), index);
                    changed.push(index);
                }
            }
            HarnessEvent::ItemStarted {
                id,
                kind: ItemKind::ToolCall { name, input },
            } => {
                self.streams.remove(scope);
                let index = self.push(
                    RowKind::Tool,
                    scope,
                    format!("{name} · Running"),
                    String::new(),
                );
                self.rows[index].tool = Some(Tool {
                    name: name.clone(),
                    input: input.clone(),
                    partial_input: String::new(),
                    output: None,
                    is_error: false,
                });
                self.items.insert((scope.into(), id.clone()), index);
                self.refresh_tool(index);
                changed.push(index);
            }
            HarnessEvent::ItemInputDelta { id, partial_json } => {
                if let Some(&index) = self.items.get(&(scope.into(), id.clone())) {
                    let tool = self.rows[index].tool.as_mut().expect("tool row");
                    tool.partial_input.push_str(partial_json);
                    if let Ok(input) = serde_json::from_str(&tool.partial_input) {
                        tool.input = input;
                    }
                    self.refresh_tool(index);
                    changed.push(index);
                } else {
                    self.orphan(scope, id, "input", changed);
                }
            }
            HarnessEvent::ItemCompleted {
                id,
                output,
                is_error,
            } => {
                if let Some(&index) = self.items.get(&(scope.into(), id.clone())) {
                    let tool = self.rows[index].tool.as_mut().expect("tool row");
                    tool.output = Some(output.clone());
                    tool.is_error = *is_error;
                    self.refresh_tool(index);
                    changed.push(index);
                } else {
                    self.orphan(scope, id, "result", changed);
                }
            }
            HarnessEvent::RuntimeRequest {
                request_id, kind, ..
            } => {
                self.streams.remove(scope);
                let (row_kind, body) = match kind {
                    RuntimeRequestKind::Permission {
                        tool_name,
                        input,
                        suggestions,
                        decision_reason,
                    } => (
                        RowKind::Permission,
                        format!(
                            "**{}**\n\n{}\n\n{}\n\nOffered rules/options:\n{}",
                            literal(tool_name),
                            code(&pretty(input), "json"),
                            literal(decision_reason.as_deref().unwrap_or("Awaiting a decision")),
                            code(&pretty(&Value::Array(suggestions.clone())), "json")
                        ),
                    ),
                    RuntimeRequestKind::Question { questions } => {
                        (RowKind::Question, question_body(questions))
                    }
                };
                let index = self.push(
                    row_kind,
                    scope,
                    format!(
                        "{} · Pending · {request_id}",
                        if row_kind == RowKind::Question {
                            "Question"
                        } else {
                            "Permission"
                        }
                    ),
                    body,
                );
                self.rows[index].request = Some(Request {
                    id: request_id.clone(),
                    order: self.rows[index].id,
                    owner: self.request_owner.clone(),
                    identity: None,
                    kind: kind.clone(),
                    resolved: false,
                });
                if let Some(old) = self.requests.insert((self.request_owner.clone(), request_id.clone()), index) {
                    if !self.rows[old].request.as_ref().unwrap().resolved {
                        self.rows[index].request.as_mut().unwrap().order = self.rows[old].request.as_ref().unwrap().order;
                    }
                    self.rows[old].request.as_mut().unwrap().resolved = true;
                    changed.push(old);
                }
                changed.push(index);
            }
            HarnessEvent::RuntimeRequestResolved { request_id } => {
                if let Some(&index) = self.requests.get(&(self.request_owner.clone(), request_id.clone())) {
                    let row = &mut self.rows[index];
                    if let Some(request) = &mut row.request {
                        request.resolved = true;
                    }
                    if row.decision.is_none() {
                        row.title = row
                            .title
                            .replace("Pending", "Resolved (decision not recorded)");
                    }
                    changed.push(index);
                } else {
                    self.orphan(scope, request_id, "request resolution", changed);
                }
            }
            HarnessEvent::TurnCompleted {
                status,
                usage,
                cost_usd,
                ..
            } => {
                self.streams.remove(scope);
                let duration = self
                    .turn_start
                    .remove(scope)
                    .zip(at)
                    .map(|(start, at)| format!("{:.2}s", at.saturating_sub(start) as f64 / 1000.))
                    .unwrap_or_else(|| "Time not recorded".into());
                let status = match status {
                    TurnStatus::Completed => "Done".into(),
                    TurnStatus::Interrupted => "Interrupted".into(),
                    TurnStatus::Failed(reason) => format!("Failed: {reason}"),
                };
                let input = usage["input_tokens"].as_u64();
                let output = usage["output_tokens"].as_u64();
                let tokens = if input.is_some() || output.is_some() {
                    format!(
                        "{} in / {} out tokens",
                        input.map_or_else(|| "unreported".into(), |value| value.to_string()),
                        output.map_or_else(|| "unreported".into(), |value| value.to_string())
                    )
                } else {
                    "Tokens not reported".into()
                };
                let title = format!(
                    "{status} · {tokens} · {duration}{}",
                    cost_usd
                        .map(|cost| format!(" · ${cost:.4}"))
                        .unwrap_or_default()
                );
                if let Some(&index) = self.workers.get(scope) {
                    self.rows[index].title = format!("Worker {scope} · {status}");
                    changed.push(index);
                }
                changed.push(self.push(
                    RowKind::Status,
                    scope,
                    title,
                    code(&pretty(usage), "json"),
                ));
            }
            HarnessEvent::SubagentEvent {
                parent_tool_use_id,
                event,
            } => {
                let child_scope = if scope.is_empty() {
                    parent_tool_use_id.clone()
                } else {
                    format!("{scope}/{parent_tool_use_id}")
                };
                self.request_owner.push(parent_tool_use_id.clone());
                self.event(event, at, &child_scope, changed);
                self.request_owner.pop();
            }
            HarnessEvent::UserMessageEcho {
                id,
                text,
                image_count,
                delivery,
            } => {
                if id.as_ref().is_some_and(|id| self.prompts.contains_key(id)) {
                    return;
                }
                self.streams.remove(scope);
                let kind = if delivery.is_some() {
                    RowKind::Delegation
                } else {
                    RowKind::User
                };
                let mut body = literal(text);
                if *image_count > 0 {
                    body.push_str(&format!(
                        "\n\n{image_count} image attachment(s) (not relayed)"
                    ));
                }
                if let Some(delivery) = delivery {
                    body.push_str(&format!(
                        "\n\n{}",
                        code(
                            &pretty(&serde_json::to_value(delivery).expect("delivery DTO")),
                            "json"
                        )
                    ));
                }
                let index = self.push(kind, scope, "You".into(), body);
                if let Some(id) = id {
                    self.prompts.insert(id.clone(), index);
                }
                changed.push(index);
            }
            HarnessEvent::MessageLifecycle { id, state } => {
                if let Some(&index) = self.prompts.get(id) {
                    self.rows[index].title = format!("You · {state:?}");
                    changed.push(index);
                }
            }
            HarnessEvent::HistoryReplayed { entries } => {
                for entry in entries {
                    let entry = match entry {
                        gitterm_core::harness::transcript::TranscriptEntry::UserPrompt(text) => {
                            Entry::UserPrompt(text.clone())
                        }
                        gitterm_core::harness::transcript::TranscriptEntry::Harness(event) => {
                            Entry::Harness(event.clone())
                        }
                    };
                    self.entry(&entry, at, scope, changed);
                }
            }
            HarnessEvent::ConversationReset { trigger } => {
                // Keep prior rows as history; new item/request IDs cannot target the old turn.
                self.streams.clear();
                self.items.clear();
                for ((owner, _), &index) in &self.requests {
                    if !owner.starts_with(&self.request_owner) { continue; }
                    let row = &mut self.rows[index];
                    if let Some(request) = &mut row.request {
                        request.resolved = true;
                    }
                    if row.decision.is_none() {
                        row.title = row.title.replace("Pending", "Closed by reset");
                    }
                    changed.push(index);
                }
                self.requests.retain(|(owner, _), _| !owner.starts_with(&self.request_owner));
                self.prompts.clear();
                self.workers.clear();
                self.turn_start.clear();
                changed.push(self.push(
                    RowKind::Status,
                    scope,
                    "Conversation cleared".into(),
                    literal(trigger.as_deref().unwrap_or("Harness reset")),
                ));
            }
            HarnessEvent::Error(error) => {
                self.streams.remove(scope);
                changed.push(self.push(RowKind::Error, scope, "Error".into(), literal(error)));
            }
            HarnessEvent::MessageWithdrawRefused { id, reason } => {
                changed.push(self.push(
                    RowKind::Error,
                    scope,
                    format!("Withdrawal refused · {id}"),
                    literal(reason),
                ));
            }
            HarnessEvent::ProcessExited { code: exit } => {
                changed.push(self.push(
                    RowKind::Status,
                    scope,
                    format!("Process exited · {exit:?}"),
                    String::new(),
                ));
            }
            HarnessEvent::RemoteDisconnected { machine, error } => {
                changed.push(self.push(
                    RowKind::Error,
                    scope,
                    format!("Reconnecting · {machine}"),
                    literal(error),
                ));
            }
            HarnessEvent::RemoteReconnected {
                machine,
                history_gap,
            } => {
                changed.push(self.push(
                    RowKind::Status,
                    scope,
                    format!("Reconnected · {machine}"),
                    if *history_gap {
                        "History gap: reload required".into()
                    } else {
                        String::new()
                    },
                ));
            }
            HarnessEvent::Ready { .. }
            | HarnessEvent::PermissionModeChanged(_)
            | HarnessEvent::ModelChanged(_)
            | HarnessEvent::EffortChanged { .. }
            // The unchanged Error that follows carries the visible text.
            | HarnessEvent::SettingRejected { .. } => {}
        }
    }
    fn orphan(&mut self, scope: &str, id: &str, operation: &str, changed: &mut Vec<usize>) {
        changed.push(self.push(
            RowKind::Error,
            scope,
            "Incomplete fixture history".into(),
            literal(&format!("{operation} references unknown id {id}")),
        ));
    }
    fn refresh_tool(&mut self, index: usize) {
        let row = &mut self.rows[index];
        let tool = row.tool.as_ref().expect("tool row");
        let state = if tool.is_error {
            "Error"
        } else if tool.output.is_some() {
            "Done"
        } else {
            "Running"
        };
        row.title = format!("{} · {state}", tool.name);
        let input = if tool.partial_input.is_empty() {
            pretty(&tool.input)
        } else {
            tool.partial_input.clone()
        };
        row.body = format!(
            "**Input{}**\n\n{}",
            if tool.output.is_none() && !tool.partial_input.is_empty() {
                " (streaming / partial JSON)"
            } else {
                ""
            },
            code(&input, "json")
        );
        if let Some(output) = &tool.output {
            row.body.push_str(&format!(
                "\n\n**{}**\n\n{}",
                if tool.is_error { "Error" } else { "Result" },
                code(output, "text")
            ));
        }
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("JSON Value serializes")
}
pub fn literal(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if ch.is_ascii_punctuation() {
                vec!['\\', ch]
            } else {
                vec![ch]
            }
        })
        .collect::<String>()
        .replace('\n', "  \n")
}
pub fn code(text: &str, language: &str) -> String {
    let longest = text.split(|ch| ch != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(3.max(longest + 1));
    format!("{fence}{language}\n{text}\n{fence}")
}
fn question_body(questions: &Value) -> String {
    let Some(questions) = questions.as_array() else {
        return code(&pretty(questions), "json");
    };
    questions
        .iter()
        .map(|q| {
            let mut body = format!(
                "**{}**{}\n\n",
                literal(
                    q["question"]
                        .as_str()
                        .unwrap_or("Question text not provided")
                ),
                if q["multiSelect"] == true {
                    " (select multiple)"
                } else {
                    ""
                }
            );
            if let Some(options) = q["options"].as_array() {
                for option in options {
                    body.push_str(&format!(
                        "- {} — {}\n",
                        literal(option["label"].as_str().unwrap_or("Option")),
                        literal(option["description"].as_str().unwrap_or(""))
                    ));
                }
            }
            body.push_str("\nOther: free text allowed\n");
            body
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn plain_markdown(source: &str) -> String {
    fn walk(node: &markdown::mdast::Node, out: &mut String) {
        use markdown::mdast::Node;
        if let Some(children) = node.children() {
            for child in children {
                walk(child, out);
            }
        } else {
            out.push_str(&node.to_string());
        }
        match node {
            Node::Paragraph(_)
            | Node::Heading(_)
            | Node::Code(_)
            | Node::ListItem(_)
            | Node::TableRow(_)
            | Node::Break(_) => out.push('\n'),
            Node::TableCell(_) => out.push('\t'),
            _ => {}
        }
    }
    let mut out = String::new();
    // GFM parse cannot fail without MDX constructs enabled.
    let ast = markdown::to_mdast(source, &markdown::ParseOptions::gfm()).expect("GFM parse");
    walk(&ast, &mut out);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub row: usize,
    pub occurrence: usize,
    pub range: Range<usize>,
}
#[derive(Default)]
pub struct FindState {
    pub query: String,
    pub matches: Vec<SearchMatch>,
    pub current: Option<usize>,
}
impl FindState {
    pub fn search(&mut self, model: &ChatModel, query: String) {
        let anchor = self
            .current
            .and_then(|index| self.matches.get(index))
            .cloned();
        self.query = query;
        self.matches = model
            .rows
            .iter()
            .enumerate()
            .flat_map(|(row, value)| {
                find_ranges(&value.search_text(), &self.query)
                    .into_iter()
                    .enumerate()
                    .map(move |(occurrence, range)| SearchMatch {
                        row,
                        occurrence,
                        range,
                    })
            })
            .collect();
        self.current = anchor
            .and_then(|old| self.matches.iter().position(|new| new == &old))
            .or(if self.matches.is_empty() {
                None
            } else {
                Some(0)
            });
    }
    /// Re-search only rows changed by the stream, retaining the active occurrence.
    pub fn refresh(&mut self, model: &ChatModel, changed: &[usize]) {
        if self.query.is_empty() {
            return;
        }
        let anchor = self.active().cloned();
        self.matches.retain(|found| !changed.contains(&found.row));
        for &row in changed {
            self.matches.extend(
                find_ranges(&model.rows[row].search_text(), &self.query)
                    .into_iter()
                    .enumerate()
                    .map(|(occurrence, range)| SearchMatch {
                        row,
                        occurrence,
                        range,
                    }),
            );
        }
        self.matches
            .sort_by_key(|found| (found.row, found.occurrence));
        self.current = anchor
            .and_then(|old| self.matches.iter().position(|new| new == &old))
            .or(if self.matches.is_empty() {
                None
            } else {
                Some(0)
            });
    }
    pub fn step(&mut self, backwards: bool) -> Option<&SearchMatch> {
        if self.matches.is_empty() {
            return None;
        }
        let current = self.current.unwrap_or(0);
        self.current = Some(if backwards {
            (current + self.matches.len() - 1) % self.matches.len()
        } else {
            (current + 1) % self.matches.len()
        });
        self.active()
    }
    pub fn active(&self) -> Option<&SearchMatch> {
        self.current.and_then(|index| self.matches.get(index))
    }
}

/// Lowercase with a byte map so Unicode case expansion never produces invalid ranges.
pub fn find_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return vec![];
    }
    let mut lower = String::new();
    let mut starts = vec![];
    let mut ends = vec![];
    for (offset, ch) in text.char_indices() {
        let folded = ch.to_lowercase().to_string();
        starts.extend(std::iter::repeat_n(offset, folded.len()));
        ends.extend(std::iter::repeat_n(offset + ch.len_utf8(), folded.len()));
        lower.push_str(&folded);
    }
    lower
        .match_indices(&query.to_lowercase())
        .map(|(index, value)| starts[index]..ends[index + value.len() - 1])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn record(event: HarnessEvent) -> Record {
        Record {
            at_ms: None,
            entry: Entry::Harness(event),
        }
    }
    #[test]
    fn fixtures_build_stable_rows_and_do_not_merge_workers_into_parent() {
        let records = parse_fixture(include_str!("../fixtures/showcase.jsonl")).unwrap();
        let a = ChatModel::from_records(&records);
        let b = ChatModel::from_records(&records);
        assert_eq!(
            a.rows
                .iter()
                .map(|row| (row.id, &row.body))
                .collect::<Vec<_>>(),
            b.rows
                .iter()
                .map(|row| (row.id, &row.body))
                .collect::<Vec<_>>()
        );
        assert!(a.rows.iter().any(|row| row.kind == RowKind::Assistant
            && row.scope == "worker"
            && row.body.contains("cap prevents")));
        assert!(
            !a.rows
                .iter()
                .any(|row| row.scope.is_empty() && row.body.contains("cap prevents"))
        );
        assert_eq!(
            a.rows
                .iter()
                .filter(|row| row.kind == RowKind::Permission)
                .count(),
            3
        );
        assert_eq!(
            a.rows
                .iter()
                .filter(|row| row.kind == RowKind::Question)
                .count(),
            3
        );
        assert_eq!(a.rows.iter().flat_map(Row::edits).count(), 4);
        assert!(a.rows.iter().any(
            |row| row.title.contains("456 in / 196 out tokens") && row.title.contains("4.32s")
        ));
        assert!(a.rows.iter().any(|row| {
            row.tool
                .as_ref()
                .is_some_and(|tool| tool.partial_input == "{\"command\":\"cargo")
        }));
        assert_eq!(
            a.rows
                .iter()
                .filter(|row| row.decision == Some(DecisionState::Denied))
                .count(),
            2
        );
    }
    #[test]
    fn normalized_wire_events_partial_input_and_errors() {
        let records = parse_fixture(r#"{"type":"item_started","data":{"id":"a","kind":{"type":"tool_call","name":"Bash","input":{}}}}
{"type":"item_input_delta","data":{"id":"a","partial_json":"{\"command\":"}}
{"type":"item_input_delta","data":{"id":"a","partial_json":"\"echo hi\"}"}}
{"type":"item_completed","data":{"id":"a","output":"failed","is_error":true}}"#).unwrap();
        let model = ChatModel::from_records(&records);
        assert_eq!(model.rows.len(), 1);
        assert_eq!(
            model.rows[0].tool.as_ref().unwrap().input,
            json!({"command":"echo hi"})
        );
        assert!(model.rows[0].title.contains("Error"));
        assert!(
            parse_fixture("\n{\"type\":\"unknown_event\",\"data\":{}}")
                .unwrap_err()
                .to_string()
                .contains("line 2")
        );
    }
    #[test]
    fn reset_does_not_retarget_old_tools_and_echoes_deduplicate() {
        let mut model = ChatModel::default();
        let echo = record(HarnessEvent::UserMessageEcho {
            id: Some("user-1".into()),
            text: "hello".into(),
            image_count: 0,
            delivery: None,
        });
        model.apply(&echo);
        model.apply(&echo);
        assert_eq!(model.rows.len(), 1);
        model.apply(&record(HarnessEvent::RuntimeRequest {
            request_id: "before-reset".into(),
            tool_use_id: "tool".into(),
            kind: RuntimeRequestKind::Question {
                questions: serde_json::json!([]),
            },
        }));
        let request_index = model.rows.len() - 1;
        let changed = model.apply(&record(HarnessEvent::ConversationReset { trigger: None }));
        assert!(changed.contains(&request_index));
        assert!(model.rows[request_index].request.as_ref().unwrap().resolved);
        model.apply(&record(HarnessEvent::ItemCompleted {
            id: "missing".into(),
            output: "result".into(),
            is_error: false,
        }));
        assert_eq!(model.rows.last().unwrap().kind, RowKind::Error);
    }
    #[test]
    fn find_searches_all_5000_rows_and_survives_incremental_replay() {
        let records = parse_fixture(include_str!("../fixtures/long-5000.jsonl")).unwrap();
        let model = ChatModel::from_records(&records);
        assert_eq!(model.rows.len(), 5000);
        let mut find = FindState::default();
        find.search(&model, "row-0001".into());
        assert_eq!(find.active().unwrap().row, 1);
        find.search(&model, "bounded".into());
        assert_eq!(find.matches.len(), 5000);
        assert_eq!(find.step(true).unwrap().row, 4999);
        assert_eq!(find.step(false).unwrap().row, 0);
        let mut replay = ChatModel::default();
        find.search(&replay, "row-0001".into());
        for record in &records[..4] {
            let changed = replay.apply(record);
            find.refresh(&replay, &changed);
        }
        assert_eq!(find.active().unwrap().row, 1);
    }
    #[test]
    fn untimed_turns_do_not_invent_duration_or_missing_usage() {
        let records = parse_fixture(r#"{"type":"turn_started","data":{"session_id":null,"model":null}}
{"type":"turn_completed","data":{"status":{"state":"completed"},"usage":{"output_tokens":12},"cost_usd":null}}"#).unwrap();
        let model = ChatModel::from_records(&records);
        assert!(model.rows[0].title.contains("Time not recorded"));
        assert!(model.rows[0].title.contains("unreported in / 12 out"));
        assert_eq!(
            plain_markdown(&literal("1. raw **text**\nsecond line")).trim(),
            "1. raw **text**\nsecond line"
        );
    }
    #[test]
    fn unicode_case_folding_and_markdown_are_searchable() {
        let text = "İstanbul **café** and CAFÉ 🦀";
        let ranges = find_ranges(text, "café");
        assert_eq!(ranges.len(), 2);
        assert_eq!(&text[ranges[1].clone()], "CAFÉ");
        assert_eq!(find_ranges("İx", "x"), vec![2..3]);
        assert!(
            plain_markdown("a **bold** phrase\n\n```rust\nlet x = 1;\n```")
                .contains("a bold phrase")
        );
        assert!(code("```\ntext", "text").starts_with("````text"));
    }
}
