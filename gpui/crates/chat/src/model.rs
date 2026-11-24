#[path = "roster.rs"]
pub mod roster;

use crate::events::*;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

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
    /// Iced's `.error-block`: the body only, in a red monospace box.
    Error,
    /// Iced's muted `.note`.
    Status,
    /// Iced's yellow `.note.warn`.
    Warning,
    /// A pi block (Iced's `legacyEvent`): its label is the row title.
    Legacy(LegacyBlock),
    /// Iced's `.html-render`: a tool's visual reply, its own block from when
    /// the result arrived. The row carries a copy of that tool.
    Visual,
    /// A block Iced removed: a queued prompt whose withdrawal was confirmed,
    /// or a reconnect notice. The slot stays so row indices remain stable,
    /// but nothing reads it.
    Withdrawn,
}

/// Iced's `.block.<kind>` for pi's legacy events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyBlock {
    Thinking,
    Tool,
    ToolResult,
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
    /// Iced's `.tool-badge` while a request for this call waits.
    pub badge: Option<&'static str>,
    /// Iced's `details.subagent` in an Agent/Task card.
    pub sub: Option<Subagent>,
}

/// Iced's subagent states (`details.subagent.<state>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubState {
    Running,
    Done,
    Failed,
    Stopped,
}

/// One `.sa-tool` chip.
#[derive(Debug, Clone, PartialEq)]
pub struct SubTool {
    pub id: String,
    pub name: String,
    /// Iced's chip `title`: the call's salient argument, else its name.
    pub tip: String,
    /// The chip's `done`/`error` class once its result arrives: `Some(is_error)`.
    pub result: Option<bool>,
}

/// Iced's subagent block, nested in its root Agent/Task card: what the
/// subagent, and every subagent below it, did (`ensureSubagent`).
#[derive(Debug, Clone, PartialEq)]
pub struct Subagent {
    pub state: SubState,
    /// The subagent's own turn was seen, so only its completion ends it.
    pub tracked: bool,
    /// A request from inside the subagent waits.
    pub waiting: bool,
    pub tools: Vec<SubTool>,
    pub text: String,
    pub model: Option<String>,
    /// Running when its turn ended: Iced folds then, so the card stays out.
    pub running_at_turn_end: bool,
}
impl Subagent {
    fn new() -> Self {
        Self {
            state: SubState::Running,
            tracked: false,
            waiting: false,
            tools: vec![],
            text: String::new(),
            model: None,
            running_at_turn_end: false,
        }
    }
    /// Iced's `.sa-desc`: description, type and model.
    pub fn description(&self, input: &Value) -> String {
        let text = |key: &str| input[key].as_str().filter(|s| !s.is_empty());
        [
            text("description"),
            text("subagent_type"),
            text("model").or(self.model.as_deref().filter(|s| !s.is_empty())),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
    }
    /// Iced's `.sa-state`.
    pub fn state_text(&self) -> String {
        let state = match self.state {
            SubState::Running if self.waiting => "waiting for approval",
            SubState::Running => "running",
            SubState::Done => "done",
            SubState::Failed => "failed",
            SubState::Stopped => "stopped",
        };
        match self.tools.len() {
            0 => state.into(),
            1 => format!("{state} · 1 tool"),
            n => format!("{state} · {n} tools"),
        }
    }
    /// Iced's `.sa-latest`: the last nonempty line, cut at 200 characters.
    pub fn latest(&self) -> Option<String> {
        if self.text.is_empty() {
            return None;
        }
        let line = self
            .text
            .split('\n')
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .unwrap_or("");
        // JavaScript length and slice: UTF-16 units.
        if line.encode_utf16().count() <= 200 {
            return Some(line.into());
        }
        let mut units = 0;
        let end = line
            .char_indices()
            .find(|(_, ch)| {
                units += ch.len_utf16();
                units > 200
            })
            .map_or(line.len(), |(index, _)| index);
        Some(format!("{}…", &line[..end]))
    }
}

/// Claude's MCP prefix, Antigravity's FQN, OpenCode's server_tool name, and
/// the unprefixed MCP name, as Iced. Other tool output is never rendered.
const TOOL_NAMES: [&str; 4] = [
    "html_render",
    "mcp__gitterm_tasks__html_render",
    "mcp_gitterm-tasks_html_render",
    "gitterm_tasks_html_render",
];

/// The validated document and height of an html_render result.
pub fn visual_request(tool: &Tool) -> Option<(String, u32)> {
    // Iced calls showHtmlRender only for a result that is not an error.
    if tool.is_error || !TOOL_NAMES.contains(&tool.name.as_str()) {
        return None;
    }
    let result: gitterm_core::chat::html_render::HtmlRenderResult =
        serde_json::from_str(tool.output.as_deref()?).ok()?;
    result.html_render.validate().ok()?;
    Some((result.html_render.html, result.html_render.height))
}

/// Iced's `salient(name, input)`: the argument that says what a call does.
pub fn salient<'a>(name: &str, input: &'a Value) -> Option<&'a str> {
    let keys: &[&str] = match name {
        "Bash" => &["command"],
        "Read" | "Write" | "Edit" | "MultiEdit" => &["file_path"],
        "NotebookEdit" => &["notebook_path"],
        "Grep" | "Glob" => &["pattern"],
        "WebFetch" => &["url"],
        "WebSearch" => &["query"],
        "Agent" | "Task" => &["description"],
        _ => &[
            "command",
            "file_path",
            "filePath",
            "path",
            "url",
            "query",
            "pattern",
            "title",
        ],
    };
    keys.iter()
        .find_map(|key| input[*key].as_str().filter(|s| !s.is_empty()))
}

/// Iced's `MAX_TOOL_OUTPUT`: a result shows this many characters.
pub const MAX_TOOL_OUTPUT: usize = 20_000;
/// Iced's displayed result: `(no output)` for an empty one, and the first
/// `MAX_TOOL_OUTPUT` JavaScript characters (UTF-16 units) of a long one.
pub fn shown_output(output: &str) -> String {
    if output.is_empty() {
        return "(no output)".into();
    }
    let units = output.encode_utf16().count();
    if units <= MAX_TOOL_OUTPUT {
        return output.into();
    }
    let mut kept = 0;
    let end = output
        .char_indices()
        .find(|(_, ch)| {
            kept += ch.len_utf16();
            kept > MAX_TOOL_OUTPUT
        })
        .map_or(output.len(), |(index, _)| index);
    format!(
        "{}\n… ({} more characters)",
        &output[..end],
        units - MAX_TOOL_OUTPUT
    )
}

#[derive(Debug, Clone)]
pub struct Request {
    pub id: String,
    pub owner: Vec<String>,
    pub order: u64,
    pub identity: Option<gitterm_core::chat::state::RequestIdentity>,
    pub kind: RuntimeRequestKind,
    pub resolved: bool,
    /// The top-level tool card this request is for (a subagent's request:
    /// its Agent card), as Iced's `keepsOutOfFold` links them.
    pub tool_row: Option<usize>,
    /// Its turn ended before the human answered. Iced folds a finished turn
    /// before clearing its requests, so this card and its tool stay out.
    pub waiting_at_turn_end: bool,
    /// The top-level tool row whose call this is: Iced's `items.get(tool_use_id)`,
    /// which shows the waiting badge.
    pub badge_row: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub id: u64,
    pub kind: RowKind,
    pub scope: String,
    pub title: String,
    pub body: String,
    pub images: Vec<gitterm_core::chat::types::ImageAttachment>,
    pub delivery: Option<gitterm_core::chat::types::DeliveryCard>,
    pub delegation: Option<crate::delegation::DelegationCard>,
    pub roster_id: Option<String>,
    pub tool: Option<Tool>,
    pub decision: Option<DecisionState>,
    pub request: Option<Request>,
    /// The prompt's message id, so its bubble can show the queued state (C04).
    pub prompt_id: Option<String>,
    /// Images a relayed prompt had on the device that sent it (Iced's `.user-note`).
    pub relayed_images: usize,
    /// Iced's `title` on a block: a reconnect notice's error.
    pub tooltip: Option<String>,
}
impl Row {
    /// Iced draws no transcript block for this row: a removed one, or a
    /// permission/question request, which only the footer panel shows.
    pub fn hidden(&self) -> bool {
        self.kind == RowKind::Withdrawn || self.request.is_some()
    }
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
        let Some(path) = input["file_path"].as_str().filter(|path| !path.is_empty()) else {
            return vec![];
        };
        let path = path.to_owned();
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
                .filter(|edits| !edits.is_empty())
                .and_then(|edits| edits.iter().map(edit).collect::<Option<Vec<_>>>())
                .unwrap_or_default(),
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
    pub fn find_body(&self) -> String {
        let edits = self.edits();
        if !edits.is_empty() {
            let mut text = edits
                .iter()
                .flat_map(|edit| {
                    crate::interactions::edit_lines(edit)
                        .rows
                        .into_iter()
                        .map(|line| line.text)
                })
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(output) = self.tool.as_ref().and_then(|tool| tool.output.as_ref()) {
                text.push('\n');
                text.push_str(output);
            }
            return text;
        }
        let mut text = plain_markdown(
            &if self.kind == RowKind::Assistant && self.scope.is_empty() {
                crate::markdown::assistant_source(&self.body)
            } else {
                self.body.clone()
            },
        );
        if let Some(findings) = self.delegation.as_ref().and_then(|card| card.findings()) {
            for finding in &findings.findings {
                text.push_str(&format!(
                    "\n{:?} {} {}\n{}\n{}",
                    finding.severity,
                    finding.id,
                    finding.title,
                    crate::delegation::finding_location(finding).unwrap_or_default(),
                    finding.body
                ));
            }
        }
        text
    }
    pub fn search_text(&self) -> String {
        format!("{}\n{}", self.title, plain_markdown(&self.body))
    }
}

#[derive(Debug)]
pub struct CompletedTurn {
    pub rows: Range<usize>,
    pub duration_ms: Option<u64>,
    /// Iced's fold segments: one per prompt in the turn, each with its own
    /// start and end (a mid-turn prompt's time ends one and starts the next).
    pub segments: Vec<(Option<u64>, Option<u64>)>,
}

#[derive(Default)]
pub struct ChatModel {
    pub rows: Vec<Row>,
    pub completed_turns: Vec<CompletedTurn>,
    turn_row_start: HashMap<String, usize>,
    streams: HashMap<String, (RowKind, usize)>,
    items: HashMap<(String, String), usize>,
    requests: HashMap<(Vec<String>, String), usize>,
    request_owner: Vec<String>,
    prompts: HashMap<String, usize>,
    /// Prompts a withdrawal can no longer remove: started, or dropped by an exit.
    settled_prompts: HashSet<String>,
    turn_start: HashMap<String, u64>,
    turn_end: HashMap<String, u64>,
    next_id: u64,
    recorded_time: Option<u64>,
    /// The harness, for Iced's `harnessName()` and `isAcp()` in notes.
    pub backend: Option<gitterm_core::chat::types::AgentBackend>,
    /// Ids of tool rows whose result arrived since the view last drained
    /// them: Iced closes a tool card when its result arrives.
    pub completions: Vec<u64>,
    /// Iced's `busy`: a turn is in flight, so an echoed prompt is queued.
    busy: bool,
    /// Iced's `queued`: prompts sent mid-turn and not yet picked up.
    queued: Vec<String>,
    /// Iced's `linkNote`: the reconnect notice while it shows.
    link_note: Option<usize>,
    /// The last `ready` lists, for Iced's mode and model labels.
    ready_models: Vec<Value>,
    ready_modes: Vec<Value>,
    /// Each thinking row's text as it streamed.
    thinking: HashMap<usize, String>,
    /// Iced's `subTools`: a tool a subagent called -> its root Agent call.
    sub_tools: HashMap<String, String>,
    /// Iced's `turn.nextPromptAtMs`: a split marker's time for the next prompt.
    next_split: Option<u64>,
    /// When each mid-turn prompt row arrived, for its turn's segments.
    prompt_at: HashMap<usize, u64>,
    /// Ids of rows whose folds open (Iced's `openFoldsAround`) since the view
    /// last drained them: a subagent resumed or asked for the human.
    pub reopened: Vec<u64>,
    /// An event was applied live since the model was built: Iced groups a
    /// running turn's tools only then (`refreshLiveRuns` skips a replay).
    pub live: bool,
}
impl ChatModel {
    pub fn with_backend(backend: Option<gitterm_core::chat::types::AgentBackend>) -> Self {
        Self {
            backend,
            ..Self::default()
        }
    }
    fn is_acp(&self) -> bool {
        matches!(
            self.backend,
            Some(gitterm_core::chat::types::AgentBackend::Acp(_))
        )
    }
    /// Iced's `harnessName()`.
    fn harness_name(&self) -> &'static str {
        match self.backend {
            Some(gitterm_core::chat::types::AgentBackend::Claude) => "Claude",
            Some(gitterm_core::chat::types::AgentBackend::Acp(agent)) => agent.label(),
            Some(gitterm_core::chat::types::AgentBackend::Pi) | None => "the agent",
        }
    }
    /// Iced's `modeLabel`.
    fn mode_label(&self, mode: &str) -> String {
        if self.is_acp() {
            return self
                .ready_modes
                .iter()
                .find(|info| info["value"] == mode)
                .and_then(|info| info["displayName"].as_str())
                .filter(|name| !name.is_empty())
                .unwrap_or(mode)
                .into();
        }
        match mode {
            "default" => "Default",
            "acceptEdits" => "Accept edits",
            "plan" => "Plan mode",
            "auto" => "Auto mode",
            "dontAsk" => "Don't ask",
            "bypassPermissions" => "Bypass permissions",
            other => other,
        }
        .into()
    }
    /// Iced's `modelLabel` once the change is confirmed (the value is the
    /// chip's current model).
    fn model_label(&self, value: &str) -> String {
        let find = |value: &str| self.ready_models.iter().find(|info| info["value"] == value);
        if self.is_acp() {
            if value == "default" {
                return format!("{} default", self.harness_name());
            }
            return find(value)
                .and_then(|info| info["displayName"].as_str())
                .filter(|name| !name.is_empty())
                .unwrap_or(value)
                .into();
        }
        let base = value.strip_suffix("[1m]").unwrap_or(value);
        if self.ready_models.is_empty() {
            return if base == "default" {
                "Claude settings".into()
            } else {
                base.into()
            };
        }
        // Not listed: Iced's modelList adds the current model as its own name.
        let info = find(base).or_else(|| find(value));
        if base == "default" {
            let description = info
                .and_then(|info| info["description"].as_str())
                .unwrap_or("");
            let end = [" · ", " — ", " - "]
                .iter()
                .filter_map(|separator| description.find(separator))
                .min()
                .unwrap_or(description.len());
            let label = description[..end].trim();
            return if label.is_empty() {
                "Claude settings".into()
            } else {
                label.into()
            };
        }
        match info {
            Some(info) => info["displayName"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap_or(base)
                .into(),
            None => value.into(),
        }
    }
    fn note(&mut self, kind: RowKind, scope: &str, text: String) -> usize {
        let body = if kind == RowKind::Error {
            preformatted(&text)
        } else {
            literal(&text)
        };
        self.push(kind, scope, String::new(), body)
    }
    /// A yellow note the host adds outside the event stream: Iced's
    /// `__reviewNote`, why a review, consult or delegation action was not done.
    pub fn push_warning(&mut self, text: &str) -> Vec<usize> {
        vec![self.note(RowKind::Warning, "", text.into())]
    }
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
        Self::from_records_for(records, None)
    }
    pub fn from_records_for(
        records: &[Record],
        backend: Option<gitterm_core::chat::types::AgentBackend>,
    ) -> Self {
        let mut model = Self::with_backend(backend);
        for record in records {
            model.apply(record);
        }
        // Built, not streamed: no card closes on a result it was built with.
        model.completions.clear();
        model.reopened.clear();
        model.live = false;
        model
    }
    /// Rows are a disposable projection of core State::conversation.
    pub fn from_conversation(events: &[gitterm_core::chat::state::AgentEvent]) -> Self {
        Self::from_conversation_for(events, None)
    }
    pub fn from_conversation_for(
        events: &[gitterm_core::chat::state::AgentEvent],
        backend: Option<gitterm_core::chat::types::AgentBackend>,
    ) -> Self {
        let mut model = Self::with_backend(backend);
        for event in events {
            model.apply_agent(event);
        }
        model.completions.clear();
        model.reopened.clear();
        model.live = false;
        model
    }
    pub fn apply_agent(&mut self, event: &gitterm_core::chat::state::AgentEvent) -> Vec<usize> {
        use gitterm_core::chat::state::AgentEvent;
        self.live = true;
        let entry = match event {
            AgentEvent::Harness(event) => Entry::Harness(event.clone()),
            AgentEvent::Other(value) if value["kind"] == "turn_timing" => Entry::TurnTiming {
                start_ms: value["start_ms"].as_u64(),
                end_ms: value["end_ms"].as_u64(),
                split_ms: value["split_ms"].as_u64(),
            },
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
                    image_count: value["image_count"].as_u64().unwrap_or(0) as usize,
                    delivery,
                })
            }
            AgentEvent::Other(value) if value["kind"] == "delegation" => {
                return match serde_json::from_value::<crate::delegation::DelegationCard>(
                    value.clone(),
                ) {
                    Ok(card) => self.upsert_delegation(card),
                    // An error box draws its text alone (Iced's `.error-block`).
                    Err(error) => vec![self.note(
                        RowKind::Error,
                        "",
                        format!("Invalid delegation card: {error}"),
                    )],
                };
            }
            AgentEvent::Other(value) if value["kind"] == "delegation_anchor" => {
                let Some(id) = value["delegation_id"].as_str() else {
                    return vec![];
                };
                let key = format!("dlg:{id}");
                if self
                    .rows
                    .iter()
                    .any(|row| row.roster_id.as_deref() == Some(&key))
                {
                    return vec![];
                }
                let index = self.push(
                    RowKind::Delegation,
                    "",
                    "Codex · requested".into(),
                    String::new(),
                );
                self.rows[index].roster_id = Some(key);
                return vec![index];
            }
            // Iced's `legacyEvent`: what no other entry point takes (pi).
            AgentEvent::Other(value) if value.get("kind").is_none() => {
                return self.legacy(value);
            }
            _ => return vec![],
        };
        let images = match event {
            AgentEvent::Other(value) if value["type"] == "user_prompt" => {
                let parsed = value
                    .get("images")
                    .map(|images| {
                        serde_json::from_value::<Vec<gitterm_core::chat::types::ImageAttachment>>(
                            images.clone(),
                        )
                        .map_err(|_| "Invalid sent image payload".to_string())
                        .and_then(|images| {
                            gitterm_core::chat::types::validate_images(&images)?;
                            Ok(images)
                        })
                    })
                    .transpose();
                match parsed {
                    Ok(images) => Some(images.unwrap_or_default()),
                    Err(error) => {
                        return self.apply(&Record {
                            at_ms: None,
                            entry: Entry::Harness(HarnessEvent::Error(error)),
                        });
                    }
                }
            }
            _ => None,
        };
        let at_ms = self.recorded_time.take();
        let changed = self.apply(&Record { at_ms, entry });
        if let Some(images) = images
            && let Some(&index) = changed.first()
        {
            self.rows[index].images = images;
        }
        changed
    }
    fn push(&mut self, kind: RowKind, scope: &str, title: String, body: String) -> usize {
        let index = self.rows.len();
        self.rows.push(Row {
            id: self.next_id,
            kind,
            scope: scope.into(),
            title,
            body,
            images: vec![],
            delivery: None,
            delegation: None,
            roster_id: None,
            tool: None,
            decision: None,
            request: None,
            prompt_id: None,
            relayed_images: 0,
            tooltip: None,
        });
        self.next_id += 1;
        index
    }
    pub fn upsert_delegation(&mut self, card: crate::delegation::DelegationCard) -> Vec<usize> {
        let key = format!("dlg:{}", card.delegation.delegation_id);
        let index = self
            .rows
            .iter()
            .position(|row| row.roster_id.as_deref() == Some(&key))
            .unwrap_or_else(|| self.push(RowKind::Delegation, "", String::new(), String::new()));
        let row = &mut self.rows[index];
        if row.delegation.as_ref() == Some(&card) {
            return vec![];
        }
        row.title = format!(
            "{} · {} · {}",
            card.header(),
            card.target(),
            card.state_text(crate::composer::now_ms())
        );
        row.body = card.body();
        row.roster_id = Some(key);
        row.delegation = Some(card);
        vec![index]
    }
    /// Iced's `turn` once live: the first row of the turn in flight.
    pub fn live_turn_start(&self) -> Option<usize> {
        self.turn_row_start
            .get("")
            .copied()
            .filter(|_| self.live && self.busy)
    }
    /// Returns changed row indexes so UI text state can update without scanning history.
    pub fn apply(&mut self, record: &Record) -> Vec<usize> {
        self.live = true;
        let mut changed = vec![];
        self.entry(&record.entry, record.at_ms, "", &mut changed);
        changed.sort_unstable();
        changed.dedup();
        changed
    }
    fn entry(&mut self, entry: &Entry, at: Option<u64>, scope: &str, changed: &mut Vec<usize>) {
        match entry {
            Entry::ImportedOffer(_) => {}
            Entry::TurnTiming {
                start_ms,
                end_ms,
                split_ms,
            } => {
                if scope.is_empty() && split_ms.is_some() {
                    self.next_split = *split_ms;
                }
                if let Some(start) = start_ms {
                    self.turn_start.insert(scope.into(), *start);
                }
                if let Some(end) = end_ms {
                    self.turn_end.insert(scope.into(), *end);
                }
            }
            Entry::UserPrompt(text) => {
                self.event(
                    &HarnessEvent::UserMessageEcho {
                        id: None,
                        text: text.clone(),
                        image_count: 0,
                        delivery: None,
                    },
                    at,
                    scope,
                    changed,
                );
            }
            Entry::SubmittedPrompt(prompt) => {
                self.recorded_time = at;
                changed.extend(self.apply_agent(
                    &gitterm_core::chat::state::AgentEvent::submitted_prompt(prompt, false),
                ));
            }
            Entry::Harness(event) => {
                let at = at.or(self.recorded_time.take());
                self.event(event, at, scope, changed);
            }
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
                    changed.push(self.note(
                        RowKind::Error,
                        scope,
                        format!("Unmatched display decision: {request_id}"),
                    ));
                }
            }
            Entry::DisplayDelegation {
                id,
                title,
                state,
                body,
            } => {
                let index = self.push(
                    RowKind::Delegation,
                    scope,
                    format!("Delegation {id} · {title} · {state}"),
                    literal(body),
                );
                self.rows[index].roster_id = Some(format!("dlg:{id}"));
                changed.push(index);
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
            self.end_requests(ends_turn(event), changed);
        }
        match event {
            HarnessEvent::TextDelta(text) | HarnessEvent::ThinkingDelta(text) => {
                if text.is_empty() {
                    return;
                }
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
                            ""
                        };
                        let index = self.push(kind, scope, title.into(), String::new());
                        self.streams.insert(scope.into(), (kind, index));
                        index
                    }
                };
                if kind == RowKind::Thinking {
                    // Iced's thinking body is `pre-wrap`: keep the raw text
                    // so a line's leading spaces survive a delta boundary.
                    let raw = self.thinking.entry(index).or_default();
                    raw.push_str(text);
                    self.rows[index].body = preformatted(raw);
                } else {
                    self.rows[index].body.push_str(text);
                }
                changed.push(index);
            }
            // A subagent's turn is in its Agent card (`subagent_event`).
            HarnessEvent::TurnStarted { .. } => {
                self.streams.remove(scope);
                if scope.is_empty() {
                    self.busy = true;
                }
                self.turn_row_start.insert(scope.into(), self.rows.len());
                if let Some(at) = at {
                    self.turn_start.insert(scope.into(), at);
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
                self.rows[index].roster_id = Some(format!("sub:{id}"));
                self.rows[index].tool = Some(Tool {
                    name: name.clone(),
                    input: input.clone(),
                    partial_input: String::new(),
                    output: None,
                    is_error: false,
                    badge: None,
                    sub: matches!(name.as_str(), "Agent" | "Task").then(Subagent::new),
                });
                self.items.insert((scope.into(), id.clone()), index);
                self.refresh_tool(index);
                changed.push(index);
            }
            HarnessEvent::ItemInputDelta { id, partial_json } => {
                if let Some(&index) = self.items.get(&(scope.into(), id.clone())) {
                    let tool = self.rows[index].tool.as_mut().expect("tool row");
                    tool.partial_input.push_str(partial_json);
                    // Iced shows the input once the accumulated JSON is a whole object.
                    if tool.partial_input.trim_end().ends_with('}')
                        && let Ok(input) = serde_json::from_str(&tool.partial_input)
                    {
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
                    tool.badge = None;
                    // A foreground subagent is done when its Agent call
                    // returns; a tracked (background) one only on its own
                    // completion.
                    if let Some(sub) = &mut tool.sub
                        && sub.state == SubState::Running
                        && !sub.tracked
                    {
                        sub.state = if *is_error {
                            SubState::Failed
                        } else {
                            SubState::Done
                        };
                    }
                    self.completions.push(self.rows[index].id);
                    self.refresh_tool(index);
                    changed.push(index);
                    // Iced's showHtmlRender appends the reply at completion.
                    if scope.is_empty()
                        && let Some(tool) = self.rows[index].tool.clone()
                        && visual_request(&tool).is_some()
                    {
                        self.streams.remove(scope);
                        let visual =
                            self.push(RowKind::Visual, scope, "Visual reply".into(), String::new());
                        self.rows[visual].tool = Some(tool);
                        changed.push(visual);
                    }
                } else {
                    self.orphan(scope, id, "result", changed);
                }
            }
            HarnessEvent::RuntimeRequest {
                request_id,
                tool_use_id,
                kind,
            } => {
                self.streams.remove(scope);
                let tool_row = match scope.split('/').next().filter(|root| !root.is_empty()) {
                    Some(root) => self.items.get(&(String::new(), root.to_string())),
                    None => self.items.get(&(String::new(), tool_use_id.clone())),
                }
                .copied();
                let badge_row = self
                    .items
                    .get(&(String::new(), tool_use_id.clone()))
                    .copied();
                // Iced's `subRoot.sub.waiting`: the block says it waits, and
                // the folds around its card open.
                if !scope.is_empty()
                    && let Some(root) = tool_row
                    && let Some(sub) = self.rows[root].tool.as_mut().and_then(|t| t.sub.as_mut())
                {
                    sub.waiting = true;
                    self.reopened.push(self.rows[root].id);
                    changed.push(root);
                }
                if let Some(row) = badge_row {
                    self.rows[row].tool.as_mut().expect("tool row").badge = Some(match kind {
                        RuntimeRequestKind::Question { .. } => "waiting for your answer",
                        RuntimeRequestKind::Permission { .. } => "waiting for approval",
                    });
                    changed.push(row);
                }
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
                    tool_row,
                    waiting_at_turn_end: false,
                    badge_row,
                });
                if let Some(old) = self
                    .requests
                    .insert((self.request_owner.clone(), request_id.clone()), index)
                {
                    if !self.rows[old].request.as_ref().unwrap().resolved {
                        self.rows[index].request.as_mut().unwrap().order =
                            self.rows[old].request.as_ref().unwrap().order;
                    }
                    self.rows[old].request.as_mut().unwrap().resolved = true;
                    changed.push(old);
                }
                changed.push(index);
            }
            HarnessEvent::RuntimeRequestResolved { request_id } => {
                if let Some(&index) = self
                    .requests
                    .get(&(self.request_owner.clone(), request_id.clone()))
                {
                    let row = &mut self.rows[index];
                    let badge = row.request.as_ref().and_then(|request| request.badge_row);
                    let root = row
                        .request
                        .as_ref()
                        .filter(|_| !row.scope.is_empty())
                        .and_then(|request| request.tool_row);
                    if let Some(root) = root
                        && let Some(sub) =
                            self.rows[root].tool.as_mut().and_then(|t| t.sub.as_mut())
                    {
                        sub.waiting = false;
                        changed.push(root);
                    }
                    let row = &mut self.rows[index];
                    if let Some(request) = &mut row.request {
                        request.resolved = true;
                    }
                    if let Some(tool) = badge.and_then(|badge| self.rows[badge].tool.as_mut()) {
                        tool.badge = None;
                        changed.push(badge.unwrap());
                    }
                    let row = &mut self.rows[index];
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
            HarnessEvent::TurnCompleted { .. } => {
                self.streams.remove(scope);
                let turn_start = self.turn_start.remove(scope);
                let end = self.turn_end.remove(scope).or(at);
                let start = self.turn_row_start.remove(scope).unwrap_or_else(|| {
                    self.rows
                        .iter()
                        .rposition(|row| row.scope == scope && row.kind == RowKind::User)
                        .map_or(0, |index| index + 1)
                });
                if scope.is_empty() {
                    self.complete_turn(start, turn_start, end, changed);
                    self.busy = false;
                }
                // Iced reports completion/usage in the activity strip; an
                // interruption is a yellow note, a failure its error box.
                match event {
                    HarnessEvent::TurnCompleted {
                        status: TurnStatus::Interrupted,
                        ..
                    } => changed.push(self.note(RowKind::Warning, scope, "Interrupted".into())),
                    HarnessEvent::TurnCompleted {
                        status: TurnStatus::Failed(reason),
                        ..
                    } => {
                        let text = if reason.is_empty() {
                            "Turn failed"
                        } else {
                            reason
                        };
                        changed.push(self.note(RowKind::Error, scope, text.into()))
                    }
                    _ => {}
                }
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
                match &**event {
                    // A subagent's request keeps its own (hidden) row: the
                    // footer panel answers it under the subagent's owner.
                    HarnessEvent::RuntimeRequest { .. }
                    | HarnessEvent::RuntimeRequestResolved { .. }
                    | HarnessEvent::SubagentEvent { .. } => {
                        self.event(event, at, &child_scope, changed)
                    }
                    inner => {
                        if matches!(
                            inner,
                            HarnessEvent::TurnCompleted { .. }
                                | HarnessEvent::ProcessExited { .. }
                                | HarnessEvent::ConversationReset { .. }
                        ) {
                            self.end_requests(ends_turn(inner), changed);
                        }
                        self.subagent_event(parent_tool_use_id, inner, changed);
                    }
                }
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
                let delivery = delivery
                    .clone()
                    .or_else(|| gitterm_core::chat::history::delivery_card(text));
                let kind = if delivery.is_some() {
                    RowKind::Delegation
                } else {
                    RowKind::User
                };
                let title = delivery
                    .as_ref()
                    .map_or_else(|| "You".into(), |card| card.header.clone());
                let body = literal(
                    delivery
                        .as_ref()
                        .map_or(text.as_str(), |card| card.body.as_str()),
                );
                let index = self.push(kind, scope, title, body);
                self.rows[index].relayed_images = *image_count;
                self.rows[index].delivery = delivery;
                // Iced's `queued: busy`: a prompt sent while a turn runs joins
                // the queue; any other starts the turn.
                if scope.is_empty() {
                    match id {
                        Some(id) if self.busy => self.queued.push(id.clone()),
                        _ => self.busy = true,
                    }
                }
                self.turn_row_start.entry(scope.into()).or_insert(index + 1);
                if let Some(at) = at {
                    self.turn_start.entry(scope.into()).or_insert(at);
                }
                // Iced's `append(msg, turn.nextPromptAtMs)`.
                if scope.is_empty()
                    && let Some(at) = self.next_split.take().or(at)
                {
                    self.prompt_at.insert(index, at);
                }
                if let Some(id) = id {
                    self.prompts.insert(id.clone(), index);
                    self.rows[index].prompt_id = Some(id.clone());
                }
                changed.push(index);
            }
            HarnessEvent::MessageLifecycle { id, state } => {
                if *state != MessageState::Queued {
                    self.queued.retain(|queued| queued != id);
                }
                if let Some(&index) = self.prompts.get(id) {
                    match state {
                        MessageState::Started | MessageState::Completed => {
                            self.settled_prompts.insert(id.clone());
                        }
                        // Iced's queuedLifecycle: a confirmed withdrawal removes the bubble.
                        MessageState::Cancelled if !self.settled_prompts.contains(id) => {
                            self.rows[index].kind = RowKind::Withdrawn;
                        }
                        _ => {}
                    }
                    if self.rows[index].delivery.is_none() {
                        self.rows[index].title = format!("You · {state:?}");
                    }
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
                    if !owner.starts_with(&self.request_owner) {
                        continue;
                    }
                    let row = &mut self.rows[index];
                    if let Some(request) = &mut row.request {
                        request.resolved = true;
                    }
                    if row.decision.is_none() {
                        row.title = row.title.replace("Pending", "Closed by reset");
                    }
                    changed.push(index);
                }
                self.requests
                    .retain(|(owner, _), _| !owner.starts_with(&self.request_owner));
                self.prompts.clear();
                self.settled_prompts.clear();
                self.sub_tools.clear();
                self.turn_start.clear();
                self.turn_row_start.clear();
                // `trigger` says why; Iced's note is the same for every reset.
                let _ = trigger;
                changed.push(self.note(
                    RowKind::Status,
                    scope,
                    "Conversation cleared · Claude starts fresh; the messages above stay in the previous session".into(),
                ));
            }
            HarnessEvent::Error(error) => {
                self.streams.remove(scope);
                changed.push(self.note(RowKind::Error, scope, error.clone()));
            }
            HarnessEvent::MessageWithdrawRefused { reason, .. } => {
                let text = if reason.starts_with("Not withdrawn") {
                    reason.clone()
                } else {
                    format!("Not withdrawn: {reason}")
                };
                changed.push(self.note(RowKind::Warning, scope, text));
            }
            HarnessEvent::ProcessExited { code: exit } => {
                // Core leaves an exit's queue unsent; a late cancel cannot withdraw it.
                self.settled_prompts.extend(self.prompts.keys().cloned());
                self.remove_link_note(changed);
                self.streams.remove(scope);
                // Iced's endTurn(true): the turn in flight folds.
                if scope.is_empty()
                    && let Some(start) = self.turn_row_start.remove(scope)
                {
                    let turn_start = self.turn_start.remove(scope);
                    let end = self.turn_end.remove(scope).or(at);
                    self.complete_turn(start, turn_start, end, changed);
                }
                let harness = self.harness_name();
                let code = exit.map_or_else(String::new, |code| format!(" (code {code})"));
                changed.push(self.note(
                    RowKind::Status,
                    scope,
                    format!(
                        "{harness} process exited{code}. The next message resumes the session."
                    ),
                ));
                // Iced's dropQueued: one warning for the queue the exit lost.
                let lost = std::mem::take(&mut self.queued).len();
                if lost > 0 {
                    let text = if lost == 1 {
                        format!(
                            "A queued message was not delivered because {harness} exited; send it again."
                        )
                    } else {
                        format!(
                            "{lost} queued messages were not delivered because {harness} exited; send them again."
                        )
                    };
                    changed.push(self.note(RowKind::Warning, scope, text));
                }
                if scope.is_empty() {
                    self.busy = false;
                }
            }
            HarnessEvent::RemoteDisconnected { machine, error } => {
                self.streams.remove(scope);
                let text = format!("Reconnecting to {machine}… the chat keeps running there.");
                // One notice, updated in place, until a reconnect removes it.
                let index = match self.link_note {
                    Some(index) => {
                        self.rows[index].body = literal(&text);
                        index
                    }
                    None => {
                        let index = self.note(RowKind::Warning, scope, text);
                        self.link_note = Some(index);
                        index
                    }
                };
                self.rows[index].tooltip = Some(error.clone()).filter(|error| !error.is_empty());
                changed.push(index);
            }
            HarnessEvent::RemoteReconnected {
                machine,
                history_gap,
            } => {
                self.remove_link_note(changed);
                changed.push(self.note(
                    RowKind::Status,
                    scope,
                    if *history_gap {
                        format!("Reconnected to {machine} · reloading the conversation")
                    } else {
                        format!("Reconnected to {machine}")
                    },
                ));
            }
            HarnessEvent::Ready {
                session_id,
                permission_mode,
                models,
                modes,
                ..
            } => {
                if !modes.is_empty() {
                    self.ready_modes = modes.clone();
                }
                if !models.is_empty() {
                    self.ready_models = models.clone();
                }
                let mode = permission_mode.as_deref().unwrap_or("?");
                let text = if self.is_acp() {
                    format!(
                        "{} session {}ready · mode {}",
                        self.harness_name(),
                        session_id.as_deref().map_or_else(String::new, |id| format!(
                            "{} ",
                            id.chars().take(12).collect::<String>()
                        )),
                        self.mode_label(mode)
                    )
                } else {
                    match session_id {
                        Some(id) => format!(
                            "Resumed Claude session {} · permission mode {mode}",
                            id.chars().take(8).collect::<String>()
                        ),
                        None => format!("Claude session ready · permission mode {mode}"),
                    }
                };
                changed.push(self.note(RowKind::Status, scope, text));
            }
            HarnessEvent::PermissionModeChanged(mode) => {
                let prefix = if self.is_acp() {
                    "Mode"
                } else {
                    "Permission mode"
                };
                let text = format!("{prefix} · {}", self.mode_label(mode));
                changed.push(self.note(RowKind::Status, scope, text));
            }
            HarnessEvent::ModelChanged(model) => {
                let text = format!("Model · {}", self.model_label(model));
                changed.push(self.note(RowKind::Status, scope, text));
            }
            HarnessEvent::EffortChanged { effort, applied } => {
                let text = format!(
                    "Effort · {}{}",
                    effort.as_deref().unwrap_or("auto"),
                    match (effort, applied) {
                        (None, Some(applied)) => format!(" ({applied})"),
                        _ => String::new(),
                    }
                );
                changed.push(self.note(RowKind::Status, scope, text));
            }
            // The unchanged Error that follows carries the visible text.
            HarnessEvent::SettingRejected { .. } => {}
        }
    }
    /// A turn ended, its process exited or the conversation reset: the
    /// requests of this owner (and below) can no longer be answered.
    fn end_requests(&mut self, turn_end: bool, changed: &mut Vec<usize>) {
        for ((owner, _), &index) in &self.requests {
            if owner.starts_with(&self.request_owner) {
                let waiting = self.rows[index].decision.is_none();
                let request = self.rows[index].request.as_mut().unwrap();
                if turn_end && !request.resolved && waiting {
                    request.waiting_at_turn_end = true;
                }
                request.resolved = true;
                changed.push(index);
            }
        }
        // Iced's clearPending; a reset leaves no request answerable either.
        self.clear_badges(changed);
    }
    /// Iced's `endTurn(true)`: the top-level turn from row `start` folds, one
    /// segment per prompt in it, and a subagent still running keeps its card
    /// out (`keepsOutOfFold` runs at that moment).
    fn complete_turn(
        &mut self,
        start: usize,
        turn_start: Option<u64>,
        end: Option<u64>,
        changed: &mut Vec<usize>,
    ) {
        let duration_ms = turn_start
            .zip(end)
            .map(|(start, end)| end.saturating_sub(start));
        let rows = start..self.rows.len();
        let mut segments = Vec::new();
        let mut segment_start = turn_start;
        for index in rows.clone() {
            if self.rows[index].kind == RowKind::User {
                let at = self.prompt_at.get(&index).copied();
                segments.push((segment_start, at));
                segment_start = at;
            }
            if let Some(sub) = self.rows[index].tool.as_mut().and_then(|t| t.sub.as_mut()) {
                sub.running_at_turn_end = sub.state == SubState::Running;
            }
        }
        segments.push((segment_start, end));
        self.completed_turns.push(CompletedTurn {
            rows: rows.clone(),
            duration_ms,
            segments,
        });
        changed.extend(rows);
    }
    /// Iced's `subagentEvent`: an event from inside a subagent updates the
    /// block in its root Agent card. Grandchildren roll up into the same
    /// block; an event for a call this chat never saw is dropped.
    fn subagent_event(&mut self, parent: &str, event: &HarnessEvent, changed: &mut Vec<usize>) {
        let root = if self
            .items
            .contains_key(&(String::new(), parent.to_string()))
        {
            Some(parent.to_string())
        } else {
            self.sub_tools.get(parent).cloned()
        };
        let Some((root, index)) = root.and_then(|root| {
            let index = *self.items.get(&(String::new(), root.clone()))?;
            Some((root, index))
        }) else {
            return;
        };
        let row_id = self.rows[index].id;
        let tool = self.rows[index].tool.as_mut().expect("tool row");
        let sub = tool.sub.get_or_insert_with(Subagent::new);
        match event {
            HarnessEvent::TurnStarted { model, .. } => {
                sub.tracked = true;
                sub.state = SubState::Running;
                if let Some(model) = model {
                    sub.model = Some(model.clone());
                }
                self.reopened.push(row_id);
            }
            HarnessEvent::TurnCompleted { status, .. } => {
                sub.state = match status {
                    TurnStatus::Completed => SubState::Done,
                    TurnStatus::Interrupted => SubState::Stopped,
                    TurnStatus::Failed(reason) => {
                        if !reason.is_empty() {
                            if !sub.text.is_empty() {
                                sub.text.push('\n');
                            }
                            sub.text.push_str(reason);
                        }
                        SubState::Failed
                    }
                };
            }
            HarnessEvent::TextDelta(text) => sub.text.push_str(text),
            HarnessEvent::ItemStarted {
                id,
                kind: ItemKind::ToolCall { name, input },
            } => {
                let tip = salient(name, input).unwrap_or(name).to_string();
                sub.tools.push(SubTool {
                    id: id.clone(),
                    name: name.clone(),
                    tip,
                    result: None,
                });
                // Separate the text before and after each tool call.
                if !sub.text.is_empty() && !sub.text.ends_with('\n') {
                    sub.text.push('\n');
                }
                self.sub_tools.insert(id.clone(), root);
            }
            HarnessEvent::ItemCompleted { id, is_error, .. } => {
                if let Some(tool) = sub.tools.iter_mut().find(|tool| &tool.id == id) {
                    tool.result = Some(*is_error);
                }
            }
            // Thinking only moves Iced's activity strip; the rest has no
            // place in the block.
            _ => {}
        }
        changed.push(index);
    }
    fn remove_link_note(&mut self, changed: &mut Vec<usize>) {
        if let Some(index) = self.link_note.take() {
            self.rows[index].kind = RowKind::Withdrawn;
            changed.push(index);
        }
    }
    /// Iced's `clearPending` for tool badges and waiting subagents.
    fn clear_badges(&mut self, changed: &mut Vec<usize>) {
        for (index, row) in self.rows.iter_mut().enumerate() {
            if let Some(tool) = &mut row.tool {
                let waited = tool
                    .sub
                    .as_mut()
                    .is_some_and(|sub| std::mem::take(&mut sub.waiting));
                if tool.badge.take().is_some() || waited {
                    changed.push(index);
                }
            }
        }
    }
    /// Iced's `legacyEvent` for pi's own events.
    fn legacy(&mut self, event: &Value) -> Vec<usize> {
        let mut changed = vec![];
        let pretty_or_text = |value: &Value| match value {
            Value::String(text) => text.clone(),
            value => pretty(value),
        };
        match event["type"].as_str() {
            Some("turn_end") => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("thinking") => {
                            if let Some(text) = block["thinking"].as_str().filter(|t| !t.is_empty())
                            {
                                changed.push(self.legacy_block(
                                    LegacyBlock::Thinking,
                                    "Thinking".into(),
                                    text,
                                ));
                            }
                        }
                        Some("text") => {
                            if let Some(text) = block["text"].as_str().filter(|t| !t.is_empty()) {
                                self.event(
                                    &HarnessEvent::TextDelta(text.into()),
                                    None,
                                    "",
                                    &mut changed,
                                );
                                self.streams.remove("");
                            }
                        }
                        Some("toolCall" | "tool_use") => {
                            let name = block["name"]
                                .as_str()
                                .filter(|n| !n.is_empty())
                                .unwrap_or("tool");
                            let arguments = match &block["arguments"] {
                                Value::Null | Value::Bool(false) => String::new(),
                                Value::String(text) if text.is_empty() => String::new(),
                                Value::Number(number) if number.as_f64() == Some(0.0) => {
                                    String::new()
                                }
                                value => pretty(value),
                            };
                            changed.push(self.legacy_block(
                                LegacyBlock::Tool,
                                format!("Tool · {name}"),
                                &arguments,
                            ));
                        }
                        _ => {}
                    }
                }
                for result in event["toolResults"].as_array().into_iter().flatten() {
                    changed.push(self.legacy_block(
                        LegacyBlock::ToolResult,
                        "Tool result".into(),
                        &pretty_or_text(result),
                    ));
                }
            }
            // Iced's endTurn(false): the turn ends without folding.
            Some("done" | "stopped") => self.end_unfolded(),
            Some("error") => {
                self.end_unfolded();
                let text = event["message"]
                    .as_str()
                    .filter(|message| !message.is_empty())
                    .map_or_else(|| event.to_string(), str::to_owned);
                changed.push(self.note(RowKind::Error, "", text));
            }
            _ => {}
        }
        changed
    }
    fn legacy_block(&mut self, block: LegacyBlock, label: String, body: &str) -> usize {
        self.streams.remove("");
        self.push(RowKind::Legacy(block), "", label, preformatted(body))
    }
    fn end_unfolded(&mut self) {
        self.streams.remove("");
        self.turn_row_start.remove("");
        self.turn_start.remove("");
        self.busy = false;
    }
    fn orphan(&mut self, scope: &str, id: &str, operation: &str, changed: &mut Vec<usize>) {
        changed.push(self.note(
            RowKind::Error,
            scope,
            format!("Incomplete fixture history: {operation} references unknown id {id}"),
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
        // Iced's tool body: its input JSON as last parsed, then the whole
        // result (the card shows `shown_output`; find searches all of it).
        let input = tool_input_text(tool);
        row.body = format!("**Input**\n\n{}", code(&input, "json"));
        if let Some(output) = &tool.output {
            let output = if output.is_empty() {
                "(no output)"
            } else {
                output
            };
            row.body.push_str(&format!(
                "\n\n**{}**\n\n{}",
                if tool.is_error { "Error" } else { "Result" },
                code(output, "text")
            ));
        }
    }
}

fn ends_turn(event: &HarnessEvent) -> bool {
    matches!(
        event,
        HarnessEvent::TurnCompleted { .. } | HarnessEvent::ProcessExited { .. }
    )
}

/// Iced's input `<pre>`: empty until a non-empty input object arrives.
pub fn tool_input_text(tool: &Tool) -> String {
    if tool.input.is_null() || tool.input.as_object().is_some_and(|input| input.is_empty()) {
        String::new()
    } else {
        pretty(&tool.input)
    }
}

/// Iced's `pre-wrap` text as Markdown: literal, with leading spaces kept.
pub fn preformatted(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let indent = line.len() - line.trim_start_matches(' ').len();
            format!("{}{}", "\u{a0}".repeat(indent), literal(&line[indent..]))
        })
        .collect::<Vec<_>>()
        .join("  \n")
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
/// Iced's find pattern: the literal query, `giu`: Unicode simple case
/// folding (as Rust's regex does), leftmost non-overlapping hits.
pub fn find_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return vec![];
    }
    // For an ASCII query, simple case folding is ASCII lowercasing, except
    // that KELVIN SIGN folds to `k` and LONG S to `s`; without those in the
    // text a byte search is exact (no other non-ASCII byte can match).
    if query.is_ascii() && !text.contains(['\u{212a}', '\u{17f}']) {
        let query = query.to_ascii_lowercase();
        return text
            .to_ascii_lowercase()
            .match_indices(&query)
            .map(|(start, hit)| start..start + hit.len())
            .collect();
    }
    let pattern = regex::RegexBuilder::new(&regex::escape(query))
        .case_insensitive(true)
        .build()
        .expect("an escaped literal is a valid pattern");
    pattern.find_iter(text).map(|hit| hit.range()).collect()
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
    fn sent_images_survive_projection_lifecycle_and_saved_record_replay() {
        use gitterm_core::chat::{
            state::AgentEvent,
            types::{ImageAttachment, UserPrompt},
        };
        let prompt = UserPrompt {
            text: "with image".into(),
            images: vec![ImageAttachment {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            }],
            id: Some("sent".into()),
        };
        let event = AgentEvent::submitted_prompt(&prompt, false);
        let mut model = ChatModel::from_conversation(std::slice::from_ref(&event));
        assert_eq!(model.rows[0].images, prompt.images);
        assert_eq!(model.rows[0].relayed_images, 0);
        model.apply_agent(&AgentEvent::Harness(HarnessEvent::MessageLifecycle {
            id: "sent".into(),
            state: MessageState::Started,
        }));
        assert_eq!(model.rows[0].images, prompt.images);
        model.apply_agent(&event);
        assert_eq!(model.rows.len(), 1, "duplicate echoes stay deduplicated");
        let saved = Record {
            at_ms: None,
            entry: Entry::SubmittedPrompt(prompt.clone()),
        };
        let json = serde_json::to_string(&saved).unwrap();
        let restored = ChatModel::from_records(&parse_fixture(&json).unwrap());
        assert_eq!(restored.rows[0].images, prompt.images);
        let remote = ChatModel::from_conversation(&[AgentEvent::Other(json!({
            "type": "user_prompt", "text": "remote", "image_count": 2
        }))]);
        assert!(remote.rows[0].images.is_empty());
        assert_eq!(remote.rows[0].relayed_images, 2);
    }

    #[test]
    fn invalid_sent_images_show_an_error_without_exposing_payload() {
        let model = ChatModel::from_conversation(&[gitterm_core::chat::state::AgentEvent::Other(
            json!({
                "type": "user_prompt", "text": "bad image", "images": [{ "media_type": "text/html", "data": "private-payload" }]
            }),
        )]);
        assert_eq!(model.rows[0].kind, RowKind::Error);
        assert!(!model.rows[0].body.contains("private-payload"));
        assert!(model.rows[0].images.is_empty());
    }

    #[test]
    fn fixtures_build_stable_rows_and_nest_subagent_text_in_its_agent_card() {
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
        // Iced nests a subagent's text in its Agent card, not in rows.
        assert!(a.rows.iter().any(|row| {
            row.tool
                .as_ref()
                .and_then(|tool| tool.sub.as_ref())
                .is_some_and(|sub| sub.text.contains("cap prevents"))
        }));
        assert!(!a.rows.iter().any(|row| row.body.contains("cap prevents")));
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
        assert!(!a.rows.iter().any(|row| row.title.contains("out tokens")));
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
        assert!(model.rows.is_empty());
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

#[cfg(test)]
mod transcript_parity_tests {
    use super::*;

    #[test]
    fn t06_results_show_iced_text_and_no_completion_message() {
        // Iced: `output || '(no output)'`, so only an empty string is replaced.
        for (output, shown) in [
            ("", "(no output)"),
            (" ", " "),
            ("null", "null"),
            ("\nnull\n", "\nnull\n"),
        ] {
            let mut model = ChatModel::default();
            for event in [
                HarnessEvent::ItemStarted {
                    id: "tool".into(),
                    kind: ItemKind::ToolCall {
                        name: "Bash".into(),
                        input: Value::Null,
                    },
                },
                HarnessEvent::ItemCompleted {
                    id: "tool".into(),
                    output: output.into(),
                    is_error: false,
                },
                HarnessEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                    usage: Value::Null,
                    cost_usd: None,
                    rate_limit: None,
                },
            ] {
                model.apply(&Record {
                    at_ms: None,
                    entry: Entry::Harness(event),
                });
            }
            assert_eq!(model.rows.len(), 1);
            assert_eq!(
                model.rows[0].body,
                format!("**Input**\n\n```json\n\n```\n\n**Result**\n\n```text\n{shown}\n```")
            );
            assert_eq!(shown_output(output), shown);
        }
    }

    #[test]
    fn assistant_has_no_heading_and_redacted_thinking_renders_nothing() {
        let mut model = ChatModel::default();
        for event in [
            HarnessEvent::ThinkingDelta(String::new()),
            HarnessEvent::TextDelta("Hello".into()),
        ] {
            model.apply(&Record {
                at_ms: None,
                entry: Entry::Harness(event),
            });
        }
        assert_eq!(model.rows.len(), 1);
        assert_eq!(model.rows[0].kind, RowKind::Assistant);
        assert_eq!(model.rows[0].title, "");
    }
}

#[cfg(test)]
mod delivery_and_timing_tests {
    use super::*;
    use gitterm_core::chat::{history::DELIVERY_SOURCE_LINE, state::AgentEvent};

    #[test]
    fn source_and_legacy_footer_deliveries_use_the_iced_card_fields() {
        for state in ["reports done", "is blocked"] {
            let header = format!("Worker worker-7 (Codex · model) {state}: TRU-274");
            let content = format!("{header}\nDetails\nFull record: delegation_get worker-7");
            for text in [
                content.clone(),
                format!("{DELIVERY_SOURCE_LINE}\n{content}"),
            ] {
                let records = [Record {
                    at_ms: None,
                    entry: Entry::UserPrompt(text.clone()),
                }];
                let model = ChatModel::from_records(&records);
                let row = &model.rows[0];
                assert_eq!(row.kind, RowKind::Delegation);
                assert!(row.collapsible());
                assert_eq!(row.title, header);
                assert_eq!(row.delivery.as_ref().unwrap().delegation_id, "worker-7");
                assert_eq!(
                    plain_markdown(&row.body).trim(),
                    "Details\nFull record: delegation_get worker-7"
                );
                let echoed = ChatModel::from_conversation(&[AgentEvent::Other(
                    serde_json::json!({"type":"user_prompt", "text":text}),
                )]);
                assert_eq!(echoed.rows[0].delivery, row.delivery);
            }
            let ordinary = ChatModel::from_records(&[Record {
                at_ms: None,
                entry: Entry::UserPrompt(header),
            }]);
            assert_eq!(
                ordinary.rows[0].kind,
                RowKind::User,
                "worker wording alone is not Iced's detection rule"
            );
        }
    }

    #[test]
    fn saved_end_marker_survives_intervening_events_and_turn_start_keeps_prompt_time() {
        let events = vec![
            AgentEvent::Other(serde_json::json!({"type":"user_prompt", "text":"check"})),
            AgentEvent::Other(serde_json::json!({"kind":"turn_timing", "start_ms":1000})),
            AgentEvent::Harness(HarnessEvent::TurnStarted {
                session_id: None,
                model: None,
            }),
            AgentEvent::Harness(HarnessEvent::ThinkingDelta("checking".into())),
            AgentEvent::Other(serde_json::json!({"kind":"turn_timing", "end_ms":146000})),
            AgentEvent::Harness(HarnessEvent::TextDelta("answer".into())),
            AgentEvent::Harness(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: Value::Null,
                cost_usd: None,
                rate_limit: None,
            }),
        ];
        let model = ChatModel::from_conversation(&events);
        assert_eq!(model.completed_turns[0].duration_ms, Some(145000));
    }
}

/// TRU-274 "rows" lane: the text and lifecycle Iced gives requests, notes,
/// errors, legacy blocks, tool results and prompts.
#[cfg(test)]
mod row_parity_tests {
    use super::*;
    use gitterm_core::chat::{
        state::AgentEvent,
        types::{AcpAgent, AgentBackend},
    };
    use serde_json::json;

    fn harness(event: HarnessEvent) -> Record {
        Record {
            at_ms: None,
            entry: Entry::Harness(event),
        }
    }
    fn texts(model: &ChatModel) -> Vec<(RowKind, String, String)> {
        model
            .rows
            .iter()
            .filter(|row| !row.hidden())
            .map(|row| {
                (
                    row.kind,
                    row.title.clone(),
                    plain_markdown(&row.body).trim().to_string(),
                )
            })
            .collect()
    }
    fn turn(status: TurnStatus) -> HarnessEvent {
        HarnessEvent::TurnCompleted {
            status,
            usage: Value::Null,
            cost_usd: None,
            rate_limit: None,
        }
    }
    fn note(kind: RowKind, text: &str) -> (RowKind, String, String) {
        (kind, String::new(), text.into())
    }

    /// N01, N02, N06: an error is its text alone; an interruption is a
    /// yellow note; a refused withdrawal says `Not withdrawn`.
    #[test]
    fn n01_n02_n06_errors_and_warnings_use_iced_text() {
        let model = ChatModel::from_records(&[
            harness(HarnessEvent::Error("boom\n  at line 2".into())),
            harness(turn(TurnStatus::Interrupted)),
            harness(turn(TurnStatus::Failed("quota".into()))),
            harness(turn(TurnStatus::Failed(String::new()))),
            harness(HarnessEvent::MessageWithdrawRefused {
                id: "m".into(),
                reason: "already read".into(),
            }),
            harness(HarnessEvent::MessageWithdrawRefused {
                id: "m".into(),
                reason: "Not withdrawn: busy".into(),
            }),
        ]);
        assert_eq!(
            texts(&model),
            [
                note(RowKind::Error, "boom\n\u{a0}\u{a0}at line 2"),
                note(RowKind::Warning, "Interrupted"),
                note(RowKind::Error, "quota"),
                note(RowKind::Error, "Turn failed"),
                note(RowKind::Warning, "Not withdrawn: already read"),
                note(RowKind::Warning, "Not withdrawn: busy"),
            ]
        );
    }

    /// N03, N04: session and setting notes, for Claude and an ACP agent.
    #[test]
    fn n03_n04_ready_and_setting_notes_name_the_session_and_value() {
        let ready = |session: Option<&str>, models: Vec<Value>, modes: Vec<Value>| {
            harness(HarnessEvent::Ready {
                session_id: session.map(str::to_owned),
                permission_mode: Some("plan".into()),
                models,
                modes,
                commands: vec![],
            })
        };
        let claude = ChatModel::from_records_for(
            &[
                ready(None, vec![], vec![]),
                harness(HarnessEvent::ModelChanged("default".into())),
                ready(
                    Some("0123456789abcdef"),
                    vec![
                        json!({"value": "default", "description": "Opus 5 · best"}),
                        json!({"value": "sonnet", "displayName": "Sonnet"}),
                    ],
                    vec![],
                ),
                harness(HarnessEvent::PermissionModeChanged("acceptEdits".into())),
                harness(HarnessEvent::ModelChanged("sonnet[1m]".into())),
                harness(HarnessEvent::ModelChanged("default".into())),
                harness(HarnessEvent::ModelChanged("custom".into())),
                harness(HarnessEvent::EffortChanged {
                    effort: None,
                    applied: Some("high".into()),
                }),
                harness(HarnessEvent::EffortChanged {
                    effort: Some("low".into()),
                    applied: Some("low".into()),
                }),
            ],
            Some(AgentBackend::Claude),
        );
        assert_eq!(
            texts(&claude),
            [
                note(
                    RowKind::Status,
                    "Claude session ready · permission mode plan"
                ),
                note(RowKind::Status, "Model · Claude settings"),
                note(
                    RowKind::Status,
                    "Resumed Claude session 01234567 · permission mode plan"
                ),
                note(RowKind::Status, "Permission mode · Accept edits"),
                note(RowKind::Status, "Model · Sonnet"),
                note(RowKind::Status, "Model · Opus 5"),
                note(RowKind::Status, "Model · custom"),
                note(RowKind::Status, "Effort · auto (high)"),
                note(RowKind::Status, "Effort · low"),
            ]
        );
        let acp = ChatModel::from_records_for(
            &[
                ready(
                    Some("0123456789abcdef"),
                    vec![json!({"value": "fast", "displayName": "Fast"})],
                    vec![json!({"value": "plan", "displayName": "Planning"})],
                ),
                harness(HarnessEvent::PermissionModeChanged("plan".into())),
                harness(HarnessEvent::ModelChanged("default".into())),
                harness(HarnessEvent::ModelChanged("fast".into())),
            ],
            Some(AgentBackend::Acp(AcpAgent::OpenCode)),
        );
        assert_eq!(
            texts(&acp),
            [
                note(
                    RowKind::Status,
                    "OpenCode session 0123456789ab ready · mode Planning"
                ),
                note(RowKind::Status, "Mode · Planning"),
                note(RowKind::Status, "Model · OpenCode default"),
                note(RowKind::Status, "Model · Fast"),
            ]
        );
    }

    /// N05: one reconnect notice, updated in place with its error as the
    /// tooltip; reconnecting removes it and adds a normal note.
    #[test]
    fn n05_reconnect_notice_is_one_warning_removed_on_reconnect() {
        let disconnect = |error: &str| {
            harness(HarnessEvent::RemoteDisconnected {
                machine: "mini".into(),
                error: error.into(),
            })
        };
        let mut model = ChatModel::from_records(&[disconnect("reset"), disconnect("timeout")]);
        assert_eq!(
            texts(&model),
            [note(
                RowKind::Warning,
                "Reconnecting to mini… the chat keeps running there."
            )]
        );
        assert_eq!(model.rows[0].tooltip.as_deref(), Some("timeout"));
        model.apply(&harness(HarnessEvent::RemoteReconnected {
            machine: "mini".into(),
            history_gap: true,
        }));
        assert_eq!(
            texts(&model),
            [note(
                RowKind::Status,
                "Reconnected to mini · reloading the conversation"
            )]
        );
    }

    /// N07, U10: the exit note names the harness and code, the turn in
    /// flight folds, and one warning counts the queue the exit lost.
    #[test]
    fn n07_u10_process_exit_folds_the_turn_and_warns_about_the_lost_queue() {
        let echo = |id: &str| {
            harness(HarnessEvent::UserMessageEcho {
                id: Some(id.into()),
                text: id.into(),
                image_count: 0,
                delivery: None,
            })
        };
        let records = [
            echo("first"),
            harness(HarnessEvent::ItemStarted {
                id: "t".into(),
                kind: ItemKind::ToolCall {
                    name: "Bash".into(),
                    input: json!({"command": "ls"}),
                },
            }),
            echo("queued-1"),
            echo("queued-2"),
            harness(HarnessEvent::MessageLifecycle {
                id: "queued-1".into(),
                state: MessageState::Started,
            }),
            echo("queued-3"),
            harness(HarnessEvent::ProcessExited { code: Some(1) }),
        ];
        let model = ChatModel::from_records_for(&records, Some(AgentBackend::Claude));
        assert_eq!(model.completed_turns.len(), 1);
        assert_eq!(model.completed_turns[0].rows, 1..5);
        let texts = texts(&model);
        assert_eq!(
            texts[texts.len() - 2..],
            [
                note(
                    RowKind::Status,
                    "Claude process exited (code 1). The next message resumes the session."
                ),
                note(
                    RowKind::Warning,
                    "2 queued messages were not delivered because Claude exited; send them again."
                ),
            ]
        );
        let idle = ChatModel::from_records(&[harness(HarnessEvent::ProcessExited { code: None })]);
        assert_eq!(
            super::super::model::ChatModel::from_records(&[]).rows.len(),
            0
        );
        assert_eq!(
            idle.rows
                .iter()
                .map(|row| plain_markdown(&row.body).trim().to_string())
                .collect::<Vec<_>>(),
            ["the agent process exited. The next message resumes the session."]
        );
        assert!(idle.completed_turns.is_empty());
    }

    /// N09: Iced's clear note, whatever triggered the reset.
    #[test]
    fn n09_conversation_clear_note_is_iced_text() {
        let model = ChatModel::from_records(&[harness(HarnessEvent::ConversationReset {
            trigger: Some("/clear".into()),
        })]);
        assert_eq!(
            texts(&model),
            [note(
                RowKind::Status,
                "Conversation cleared · Claude starts fresh; the messages above stay in the previous session"
            )]
        );
    }

    /// N10: pi's legacy events become Iced's labelled blocks and error box.
    #[test]
    fn n10_legacy_pi_events_draw_labelled_blocks() {
        let model = ChatModel::from_conversation(&[
            AgentEvent::Other(json!({"type": "turn_end", "message": {"content": [
                {"type": "thinking", "thinking": "consider"},
                {"type": "text", "text": "Answer"},
                {"type": "toolCall", "name": "read", "arguments": {"path": "a"}},
                {"type": "tool_use"},
            ]}, "toolResults": ["contents", {"ok": true}]})),
            AgentEvent::Other(json!({"type": "error", "message": "pi failed"})),
            AgentEvent::Other(json!({"type": "error"})),
            AgentEvent::Other(json!({"type": "agent_start"})),
        ]);
        assert_eq!(
            texts(&model),
            [
                (
                    RowKind::Legacy(LegacyBlock::Thinking),
                    "Thinking".into(),
                    "consider".into()
                ),
                (RowKind::Assistant, String::new(), "Answer".into()),
                (
                    RowKind::Legacy(LegacyBlock::Tool),
                    "Tool · read".into(),
                    "{\n\u{a0}\u{a0}\"path\": \"a\"\n}".into()
                ),
                (
                    RowKind::Legacy(LegacyBlock::Tool),
                    "Tool · tool".into(),
                    String::new()
                ),
                (
                    RowKind::Legacy(LegacyBlock::ToolResult),
                    "Tool result".into(),
                    "contents".into()
                ),
                (
                    RowKind::Legacy(LegacyBlock::ToolResult),
                    "Tool result".into(),
                    "{\n\u{a0}\u{a0}\"ok\": true\n}".into()
                ),
                note(RowKind::Error, "pi failed"),
                note(RowKind::Error, "{\"type\":\"error\"}"),
            ]
        );
    }

    /// T06, T07: `(no output)` for an empty result; past 20,000 JavaScript
    /// characters the rest is counted, not shown.
    #[test]
    fn t06_t07_shown_output_matches_iced() {
        assert_eq!(shown_output(""), "(no output)");
        assert_eq!(shown_output("ok"), "ok");
        let exact = "a".repeat(MAX_TOOL_OUTPUT);
        assert_eq!(shown_output(&exact), exact);
        // 🦀 is two UTF-16 units, as JavaScript counts it. JavaScript's
        // slice keeps half of the split crab; a Rust string cannot, so it goes.
        let long = format!("{}🦀🦀", "a".repeat(MAX_TOOL_OUTPUT - 1));
        assert_eq!(
            shown_output(&long),
            format!("{}\n… (3 more characters)", "a".repeat(MAX_TOOL_OUTPUT - 1))
        );
    }

    /// T08: Iced shows streamed input once the accumulated JSON is a whole
    /// object, never the partial text.
    #[test]
    fn t08_streaming_input_shows_only_whole_objects() {
        let mut model = ChatModel::from_records(&[harness(HarnessEvent::ItemStarted {
            id: "a".into(),
            kind: ItemKind::ToolCall {
                name: "Bash".into(),
                input: json!({}),
            },
        })]);
        model.apply(&harness(HarnessEvent::ItemInputDelta {
            id: "a".into(),
            partial_json: "{\"command\":\"car".into(),
        }));
        assert_eq!(tool_input_text(model.rows[0].tool.as_ref().unwrap()), "");
        assert!(!model.rows[0].body.contains("car"));
        assert!(!model.rows[0].body.contains("partial"));
        model.apply(&harness(HarnessEvent::ItemInputDelta {
            id: "a".into(),
            partial_json: "go\"}".into(),
        }));
        assert_eq!(
            model.rows[0].tool.as_ref().unwrap().input,
            json!({"command": "cargo"})
        );
    }

    /// T03: the badge follows the request: set for the call's own card,
    /// cleared on resolution, result or turn end.
    #[test]
    fn t03_waiting_badge_follows_the_request() {
        let request = |id: &str, tool: &str| {
            harness(HarnessEvent::RuntimeRequest {
                request_id: id.into(),
                tool_use_id: tool.into(),
                kind: RuntimeRequestKind::Permission {
                    tool_name: "Bash".into(),
                    input: json!({}),
                    suggestions: vec![],
                    decision_reason: None,
                },
            })
        };
        let mut model = ChatModel::from_records(&[
            harness(HarnessEvent::ItemStarted {
                id: "a".into(),
                kind: ItemKind::ToolCall {
                    name: "Bash".into(),
                    input: json!({"command": "ls"}),
                },
            }),
            request("p", "a"),
        ]);
        let badge = |model: &ChatModel| model.rows[0].tool.as_ref().unwrap().badge;
        assert_eq!(badge(&model), Some("waiting for approval"));
        model.apply(&harness(HarnessEvent::ItemCompleted {
            id: "a".into(),
            output: "ok".into(),
            is_error: false,
        }));
        assert_eq!(badge(&model), None);
        model.apply(&request("p2", "a"));
        assert_eq!(badge(&model), Some("waiting for approval"));
        model.apply(&harness(turn(TurnStatus::Completed)));
        assert_eq!(badge(&model), None);
        // A request for another call's id badges nothing.
        let other = ChatModel::from_records(&[request("p", "missing")]);
        assert!(other.rows.iter().all(|row| row.tool.is_none()));
    }

    /// T01: thinking keeps its lines and their leading spaces, across deltas.
    #[test]
    fn t01_thinking_body_is_pre_wrap_across_deltas() {
        let model = ChatModel::from_records(&[
            harness(HarnessEvent::ThinkingDelta("plan:\n ".into())),
            harness(HarnessEvent::ThinkingDelta(" - step *one*".into())),
        ]);
        assert_eq!(
            plain_markdown(&model.rows[0].body).trim(),
            "plan:\n\u{a0}\u{a0}- step *one*"
        );
    }

    /// U05: a relayed prompt's images are a count, not body text.
    #[test]
    fn u05_relayed_images_are_counted_outside_the_prompt_text() {
        let model = ChatModel::from_conversation(&[AgentEvent::Other(json!({
            "type": "user_prompt", "text": "look", "image_count": 1
        }))]);
        assert_eq!(model.rows[0].relayed_images, 1);
        assert_eq!(plain_markdown(&model.rows[0].body).trim(), "look");
    }
}

#[cfg(test)]
#[path = "subagent_tests.rs"]
pub(crate) mod subagent_tests;
