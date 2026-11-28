//! Standalone Iced Find sidebar and centre preview. No discovery or launch I/O.
//!
//! Host hookup: retain `Rc<RefCell<State>>`, with one selection owner (the tab's
//! stable session UID). Mount `FindSidebar` in the left content region and
//! `FindPreview` in the centre while that owner has a selection. Subscribe to
//! `FindOutcome` on both. Reduce `outcome.intent(state, owner)` and execute core
//! effects asynchronously, returning their exact index/preview stamps. Open
//! focuses an existing conversation. Process/workspace controls emit Unavailable
//! and the host shows its existing notice without performing the operation. On
//! workspace changes update Filter.machine/workspace_root; refresh both entities
//! after reductions or host-fact changes. Closing returns to the host's overview.

use std::{cell::RefCell, collections::HashSet, path::PathBuf, rc::Rc};

use gitterm_core::{
    chats::{self, ChatBackend, ChatIndexEntry, ChatRef, ChatScope},
    find::state::{Intent as CoreIntent, Machine},
};
use gpui::{Entity, EventEmitter, Subscription, Window, div, prelude::*, px, rgb};
use gpui_component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};

use crate::theme::{self, Palette};

#[cfg(test)]
#[path = "find_sidebar_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeMode {
    Session,
    Chat,
    RecreateDirectory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindOutcome {
    SetQuery(String),
    SetScope(ChatScope),
    SetBackend(Option<ChatBackend>),
    Select { id: ChatRef },
    ClosePreview,
    Open { id: ChatRef },
    Resume { id: ChatRef, mode: ResumeMode },
    Unavailable(String),
    OpenFile(PathBuf),
}

impl FindOutcome {
    /// Translate view input to the existing reducer. Open/Resume are host
    /// navigation/launch operations, never reducer or filesystem work here.
    pub fn intent(&self, state: &State, owner: &str) -> Option<Intent> {
        let mut filter = state.filter().clone();
        match self {
            Self::SetQuery(query) => filter.query = query.clone(),
            Self::SetScope(scope) => filter.scope = *scope,
            Self::SetBackend(backend) => filter.backend = *backend,
            Self::Select { id } => {
                let entry = entry(state, id)?;
                return Some(Intent::Select {
                    owner: owner.into(),
                    chat: id.clone(),
                    path: entry.path.clone(),
                    backend: entry.backend,
                });
            }
            Self::ClosePreview => return Some(Intent::ClosePreview(owner.into())),
            Self::Open { .. } | Self::Resume { .. } | Self::Unavailable(_) | Self::OpenFile(_) => {
                return None;
            }
        }
        Some(Intent::SetFilter(filter))
    }
}

/// Tail and summary share the reducer's source/selection generation guard.
pub type State = gitterm_core::find::state::State<PreviewData>;
type Intent = CoreIntent<PreviewData>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PreviewData {
    tail: chats::ChatPreview,
    summary: Summary,
}

impl std::ops::Deref for PreviewData {
    type Target = chats::ChatPreview;
    fn deref(&self) -> &Self::Target {
        &self.tail
    }
}

impl From<chats::ChatPreview> for PreviewData {
    fn from(tail: chats::ChatPreview) -> Self {
        let summary = Summary::from_messages(&tail.messages);
        Self { tail, summary }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Summary {
    ask: Option<String>,
    reply: Option<String>,
    files: Vec<String>,
}

impl Summary {
    fn from_messages(messages: &[chats::ChatPreviewMessage]) -> Self {
        Self {
            ask: messages
                .iter()
                .rev()
                .find(|m| m.is_user && human_request(&m.text))
                .map(|m| m.text.trim().to_string()),
            reply: messages
                .iter()
                .rev()
                .find(|m| !m.is_user)
                .map(|m| m.text.clone()),
            files: vec![],
        }
    }
}

fn human_request(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty()
        && !text.starts_with('<')
        && !text.starts_with("[Request interrupted")
        && !text.starts_with("This session is being continued from a previous conversation")
        && !gitterm_core::delegations::is_delivery_message(text)
}

// Same clipping limits as Iced's chat_summary::clip_lines.
fn clip_lines(text: &str, limit: usize) -> (String, bool) {
    let mut cut = false;
    let mut shown = vec![];
    for (i, line) in text.lines().enumerate() {
        if i == limit {
            cut = true;
            break;
        }
        if line.chars().count() > 200 {
            cut = true;
            shown.push(format!("{}…", line.chars().take(199).collect::<String>()));
        } else {
            shown.push(line.to_string());
        }
    }
    (shown.join("\n"), cut)
}

fn load_preview_data(path: &std::path::Path, backend: ChatBackend) -> Result<PreviewData, String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).map_err(|e| format!("Read {}: {e}", path.display()))?;
    let mut data = PreviewData::from(gitterm_core::find::load_preview(path, backend));
    if backend != ChatBackend::Claude {
        return Ok(data);
    }
    let lines = BufReader::new(file).lines();
    let mut calls = vec![];
    let mut failed = HashSet::new();
    for line in lines {
        let line = line.map_err(|e| format!("Read {}: {e}", path.display()))?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let content = value.pointer("/message/content");
        if value["isMeta"] != true && value["isSidechain"] != true {
            let text = content
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    content
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                });
            if !text.trim().is_empty() {
                if value["type"] == "user" && human_request(&text) {
                    data.summary.ask = Some(text.trim().to_string());
                }
                if value["type"] == "assistant" {
                    data.summary.reply = Some(text.trim().to_string());
                }
            }
        }
        if let Some(blocks) = content.and_then(|v| v.as_array()) {
            for block in blocks {
                if block["type"] == "tool_result"
                    && block["is_error"] == true
                    && let Some(id) = block["tool_use_id"].as_str()
                {
                    failed.insert(id.to_string());
                }
                if block["type"] == "tool_use"
                    && matches!(
                        block["name"].as_str(),
                        Some("Edit" | "MultiEdit" | "Write" | "NotebookEdit")
                    )
                    && let Some(path) = block
                        .pointer("/input/file_path")
                        .or_else(|| block.pointer("/input/notebook_path"))
                        .and_then(|v| v.as_str())
                {
                    calls.push((
                        block["id"].as_str().unwrap_or_default().to_string(),
                        path.to_string(),
                    ));
                }
            }
        }
    }
    let mut seen = HashSet::new();
    data.summary.files = calls
        .into_iter()
        .rev()
        .filter(|(id, path)| !failed.contains(id) && seen.insert(path.clone()))
        .map(|(_, path)| path)
        .collect();
    Ok(data)
}

/// In display order: local machine, then configured remotes (including offline
/// ones). The active machine is `State::filter().machine`, not a second filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineFacts {
    pub id: Machine,
    pub name: String,
    pub status: String,
    pub reachable: bool,
    pub claude_chat: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenSession {
    pub id: ChatRef,
    pub title: String,
    pub workspace: String,
    /// Session-strip marker and branch/about text, resolved by the host.
    pub marker: String,
    pub detail: String,
    pub selected: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindFacts {
    pub machines: Vec<MachineFacts>,
    /// Iced's open-tab order: needs-you, working, idle. Independent of filters.
    pub open_sessions: Vec<OpenSession>,
    /// Running remote native chats with no local tab. Resume joins the process.
    pub running_remote: Vec<ChatRef>,
    pub workspaces: Vec<WorkspaceFacts>,
    /// Iced only offers Relevant with a configured ranking client.
    pub relevance_available: bool,
    pub relevance_down: bool,
    pub local_files_openable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceFacts {
    pub name: String,
    pub chats: Vec<String>,
    pub open: bool,
}

impl FindFacts {
    fn machine(&self, id: &Machine) -> Option<&MachineFacts> {
        self.machines.iter().find(|machine| machine.id == *id)
    }

    fn machine_name(&self, id: &Machine) -> String {
        self.machine(id)
            .map(|machine| machine.name.clone())
            .unwrap_or_else(|| id.clone().unwrap_or_else(|| "This Mac".into()))
    }

    fn open(&self, id: &ChatRef) -> Option<&OpenSession> {
        self.open_sessions.iter().find(|session| session.id == *id)
    }
}

fn entry<'a>(state: &'a State, id: &ChatRef) -> Option<&'a ChatIndexEntry> {
    state
        .index(&id.remote_id)?
        .entries
        .iter()
        .find(|e| e.id == id.id)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Control {
    label: String,
    outcome: FindOutcome,
}

#[derive(Debug)]
struct ChatRow {
    id: ChatRef,
    title: String,
    age: String,
    branch: Option<String>,
    meta: Vec<String>,
    backend: ChatBackend,
    details: Vec<String>,
    selected: bool,
    child: bool,
    fold: Option<bool>,
    open: Option<Control>,
}

#[derive(Debug)]
enum Row {
    Machine {
        id: Machine,
        label: String,
        count: usize,
        expanded: bool,
    },
    Group {
        label: String,
        count: usize,
    },
    Chat(ChatRow),
    Note(String),
}

#[derive(Default)]
struct Disclosures {
    machines: HashSet<Machine>,
    roots: HashSet<ChatRef>,
    open_collapsed: bool,
}

fn chat_row(
    entry: &ChatIndexEntry,
    machine: &Machine,
    state: &State,
    owner: &str,
    facts: &FindFacts,
    child: bool,
    fold: Option<bool>,
) -> ChatRow {
    let id = ChatRef::new(machine.as_deref(), &entry.id);
    let open = facts.open(&id);
    let mut details = vec![];
    if let Some(latest) = &entry.latest {
        details.push(format!("↳ {latest}"));
    }
    if let Some(label) = (!state.filter().query.is_empty())
        .then(|| entry.query_match(&state.filter().query.to_lowercase()))
        .flatten()
        .and_then(|matched| matched.label())
    {
        details.push(label);
    }
    if let Some(facts) = &entry.facts {
        let capped = |items: Vec<String>| {
            let mut line = items.iter().take(3).cloned().collect::<Vec<_>>().join(" ");
            if items.len() > 3 {
                line.push_str(&format!(" +{}", items.len() - 3));
            }
            (!line.is_empty()).then_some(line)
        };
        let parts = [
            capped(facts.issue_keys.clone()),
            capped(facts.edited_repo_names()),
            facts.fan_out_line(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !parts.is_empty() {
            details.push(parts.join(" · "));
        }
    }
    let mut meta = vec![];
    if open.is_none() && entry.possibly_running() {
        meta.push("◐ possibly running".into());
    }
    if entry.is_worktree {
        meta.push("worktree".into());
    }
    if machine.is_some() {
        meta.push(format!("@ {}", facts.machine_name(machine)));
    }
    if entry.dead_cwd {
        meta.push("⚠ dir gone".into());
    }
    ChatRow {
        id: id.clone(),
        title: entry.title.clone(),
        age: chats::format_age(entry.mtime),
        branch: entry.branch.clone(),
        meta,
        backend: entry.backend,
        details,
        selected: state.selected(owner).is_some_and(|s| s.chat == id),
        child,
        fold,
        open: open.map(|session| Control {
            label: format!("● open in {}", session.workspace),
            outcome: FindOutcome::Open { id },
        }),
    }
}

#[cfg(test)]
fn rows(state: &State, owner: &str, facts: &FindFacts, folds: &Disclosures) -> Vec<Row> {
    rows_ordered(state, owner, facts, folds, None)
}

fn rows_ordered(
    state: &State,
    owner: &str,
    facts: &FindFacts,
    folds: &Disclosures,
    rank: Option<&[String]>,
) -> Vec<Row> {
    let filter = state.filter();
    let everywhere = filter.scope == ChatScope::Everywhere;
    let searching = !filter.query.is_empty();
    // Core owns filtering and index order. Partition its result by the host's
    // configured-machine order rather than the reducer's HashMap iteration.
    let visible = state.filtered_entries();
    let machines = if everywhere {
        facts.machines.iter().map(|m| m.id.clone()).collect()
    } else {
        vec![filter.machine.clone()]
    };
    let mut rows = vec![];
    for machine in machines {
        let mut entries: Vec<_> = visible
            .iter()
            .filter(|(m, _)| *m == machine)
            .map(|(_, e)| *e)
            .collect();
        if machine.is_none()
            && let Some(rank) = rank
        {
            entries.sort_by_key(|e| rank.iter().position(|id| id == &e.id).unwrap_or(usize::MAX));
        }
        let expanded = !everywhere || searching || folds.machines.contains(&machine);
        rows.push(Row::Machine {
            id: machine.clone(),
            label: facts.machine_name(&machine).to_uppercase(),
            count: entries.len(),
            expanded,
        });
        if !expanded {
            continue;
        }
        if let Some(index) = state.index(&machine)
            && let Some(error) = &index.error
        {
            rows.push(Row::Note(error.clone()));
        }
        if entries.is_empty() {
            let note = if state.loading_index(&machine) {
                "indexing conversations…"
            } else if everywhere && searching {
                "no matches"
            } else if everywhere {
                "no conversations indexed"
            } else if searching {
                "no matches in this scope"
            } else {
                "no conversations in this scope"
            };
            rows.push(Row::Note(note.into()));
        }
        let all = state
            .index(&machine)
            .map(|i| i.entries.as_slice())
            .unwrap_or_default();
        let folded = chats::fold_under_roots(&entries, all);
        let mut groups: Vec<(&std::path::Path, Vec<_>)> = vec![];
        for row in &folded {
            let root = row.root.group_root();
            let existing = if machine.is_none() && rank.is_some() {
                groups.last_mut().filter(|(key, _)| *key == root)
            } else {
                groups.iter_mut().find(|(key, _)| *key == root)
            };
            if let Some((_, group)) = existing {
                group.push(row);
            } else {
                groups.push((root, vec![row]));
            }
        }
        for (_, group) in groups {
            rows.push(Row::Group {
                label: group[0].root.group_name().to_uppercase(),
                count: group.len(),
            });
            for folded in group {
                let root = folded.root;
                let id = ChatRef::new(machine.as_deref(), &root.id);
                let has_children = !folded.children.is_empty()
                    || root.facts.as_ref().is_some_and(|f| !f.children.is_empty());
                let expanded = has_children
                    && (folds.roots.contains(&id)
                        || searching
                            && (!folded.children.is_empty()
                                || matches!(
                                    root.query_match(&filter.query.to_lowercase()),
                                    Some(chats::QueryMatch::Child(_))
                                )));
                rows.push(Row::Chat(chat_row(
                    root,
                    &machine,
                    state,
                    owner,
                    facts,
                    false,
                    has_children.then_some(expanded),
                )));
                if !expanded {
                    continue;
                }
                let children = if searching {
                    folded.children.clone()
                } else {
                    all.iter()
                        .filter(|e| e.root_id.as_deref() == Some(&root.id))
                        .collect()
                };
                for child in children {
                    rows.push(Row::Chat(chat_row(
                        child, &machine, state, owner, facts, true, None,
                    )));
                }
                if let Some(facts) = &root.facts {
                    let others: Vec<_> = facts
                        .children
                        .iter()
                        .filter(|child| {
                            !all.iter().any(|e| {
                                e.root_id.as_deref() == Some(&root.id) && e.id == child.session_id
                            })
                        })
                        .filter(|child| {
                            !searching
                                || child.title.as_ref().is_some_and(|t| {
                                    t.to_lowercase().contains(&filter.query.to_lowercase())
                                })
                        })
                        .collect();
                    for child in others.iter().rev().take(8) {
                        let kind = match child.kind {
                            gitterm_core::chat_facts::ChildKind::Subagent => "subagent",
                            gitterm_core::chat_facts::ChildKind::Worker => "worker",
                            gitterm_core::chat_facts::ChildKind::Review => "review",
                            gitterm_core::chat_facts::ChildKind::Consult => "consult",
                        };
                        rows.push(Row::Note(format!(
                            "{kind} · {}",
                            child.title.as_deref().unwrap_or("untitled")
                        )));
                    }
                    if others.len() > 8 {
                        rows.push(Row::Note(format!("… {} more", others.len() - 8)));
                    }
                }
            }
        }
    }
    rows
}

fn scopes(state: &State, facts: &FindFacts) -> Vec<Control> {
    [
        ("This workspace".into(), ChatScope::Workspace),
        (
            facts.machine_name(&state.filter().machine),
            ChatScope::Machine,
        ),
        ("Everywhere".into(), ChatScope::Everywhere),
    ]
    .into_iter()
    .map(|(label, scope)| Control {
        label,
        outcome: FindOutcome::SetScope(scope),
    })
    .collect()
}

fn widen_suggestions(state: &State, facts: &FindFacts) -> Vec<Control> {
    let filter = state.filter();
    if filter.query.is_empty() || filter.scope == ChatScope::Everywhere {
        return vec![];
    }
    let query = filter.query.to_lowercase();
    let count = |machine: &Machine, workspace: bool| {
        state.index(machine).map_or(0, |index| {
            gitterm_core::find::filter_chat_entries(
                &index.entries,
                if workspace {
                    filter.workspace_root.as_deref()
                } else {
                    None
                },
                filter.backend,
                &query,
            )
            .len()
        })
    };
    let mut controls = vec![];
    if filter.scope == ChatScope::Workspace {
        let n = count(&filter.machine, false).saturating_sub(count(&filter.machine, true));
        if n > 0 {
            controls.push(Control {
                label: format!(
                    "{n} match{} elsewhere on this machine",
                    if n == 1 { "" } else { "es" }
                ),
                outcome: FindOutcome::SetScope(ChatScope::Machine),
            });
        }
    }
    let n: usize = facts
        .machines
        .iter()
        .filter(|m| m.id != filter.machine)
        .map(|m| count(&m.id, false))
        .sum();
    if n > 0 {
        controls.push(Control {
            label: format!(
                "{n} match{} on other machines",
                if n == 1 { "" } else { "es" }
            ),
            outcome: FindOutcome::SetScope(ChatScope::Everywhere),
        });
    }
    controls
}

fn backends() -> Vec<Control> {
    std::iter::once(Control {
        label: "All".into(),
        outcome: FindOutcome::SetBackend(None),
    })
    .chain(ChatBackend::ALL.into_iter().map(|b| Control {
        label: b.label().into(),
        outcome: FindOutcome::SetBackend(Some(b)),
    }))
    .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RankKey {
    filter: gitterm_core::find::state::Filter,
    sources: Vec<(String, std::time::SystemTime, u64)>,
}

fn rank_key(state: &State) -> Option<RankKey> {
    let filter = state.filter();
    if filter.machine.is_some() && filter.scope != ChatScope::Everywhere {
        return None;
    }
    let mut sources = state
        .filtered_entries()
        .into_iter()
        .filter(|(m, _)| m.is_none())
        .map(|(_, e)| (e.id.clone(), e.mtime, e.size))
        .collect::<Vec<_>>();
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    Some(RankKey {
        filter: filter.clone(),
        sources,
    })
}

#[derive(Default)]
struct Relevance {
    in_flight: bool,
    result: Option<(RankKey, Vec<String>, bool)>,
    error: Option<String>,
    down: bool,
}

fn ranking_key() -> Option<String> {
    // Tests inject facts/results and never use the developer's credentials.
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        std::env::var("TYPESAFE_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
    }
}

fn ranked_probabilities(
    value: &serde_json::Value,
    count: usize,
) -> Result<(Vec<f64>, f64, bool), String> {
    let answer = value
        .pointer("/answers/relevant")
        .ok_or("TypeSafe: missing relevance answer")?;
    if answer["type"] != "choice" {
        return Err("TypeSafe: not a choice answer".into());
    }
    let offered = |id: &str| id == "none" || (0..count).any(|n| id == format!("c{n}"));
    let choice = answer["choice"]
        .as_str()
        .filter(|id| offered(id))
        .ok_or("TypeSafe: invalid choice")?;
    let confidence = answer["confidence"]
        .as_f64()
        .filter(|n| n.is_finite() && (0.0..=1.0).contains(n))
        .ok_or("TypeSafe: invalid confidence")?;
    let probabilities = answer["probabilities"]
        .as_object()
        .ok_or("TypeSafe: missing probabilities")?;
    if probabilities.iter().any(|(id, p)| {
        !offered(id)
            || p.as_f64()
                .is_none_or(|p| !p.is_finite() || !(0.0..=1.0).contains(&p))
    }) {
        return Err("TypeSafe: invalid probability".into());
    }
    Ok((
        (0..count)
            .map(|i| {
                probabilities
                    .get(&format!("c{i}"))
                    .and_then(|p| p.as_f64())
                    .unwrap_or(0.)
            })
            .collect(),
        confidence,
        choice == "none",
    ))
}

async fn rank_chats(
    client: std::sync::Arc<dyn gpui::http_client::HttpClient>,
    key: String,
    filter: gitterm_core::find::state::Filter,
    entries: Vec<ChatIndexEntry>,
) -> Result<(Vec<String>, bool), (String, bool)> {
    use gpui::http_client::{HttpRequestExt, Inner, Request, http::HeaderValue};
    let clip = |text: &str, limit: usize| {
        if text.chars().count() > limit {
            format!("{}…", text.chars().take(limit - 1).collect::<String>())
        } else {
            text.to_string()
        }
    };
    let one_line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut state = format!(
        "A developer in a terminal workspace wants to pick up an earlier AI coding conversation.\nWorkspace: {}",
        filter
            .workspace_root
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    if !filter.query.trim().is_empty() {
        state.push_str(&format!(
            "\nSearching the conversations for: {}",
            clip(&one_line(&filter.query), 600)
        ));
    }
    let mut scored = vec![];
    let mut batches = vec![];
    for (batch_index, batch) in entries.chunks(254).enumerate() {
        let mut criteria = serde_json::Map::new();
        for (i, entry) in batch.iter().enumerate() {
            let mut label = format!(
                "{} | {}",
                clip(&one_line(&entry.title), 90),
                entry.cwd.display()
            );
            if let Some(branch) = &entry.branch {
                label.push_str(&format!(" @ {branch}"));
            }
            let age = chats::format_age(entry.mtime);
            label.push_str(&format!(
                " | {}",
                if age == "now" {
                    "active now".into()
                } else {
                    format!("{age} ago")
                }
            ));
            if let Ok(preview) =
                gitterm_core::find::load_snippet_preview(&entry.path, entry.backend)
            {
                let mut snippet = vec![];
                if let Some(ask) = preview.messages.iter().rev().find(|m| m.is_user) {
                    snippet.push(format!("asked: {}", clip(&one_line(&ask.text), 90)));
                }
                if let Some(reply) = preview.messages.iter().rev().find(|m| !m.is_user) {
                    snippet.push(format!("reply: {}", clip(&one_line(&reply.text), 60)));
                }
                if !snippet.is_empty() {
                    label.push_str(&format!(" | {}", snippet.join(" / ")));
                }
            }
            criteria.insert(format!("c{i}"), clip(&label, 300).into());
        }
        criteria.insert(
            "none".into(),
            "None of these conversations is about the current work".into(),
        );
        let body = serde_json::json!({"state": state, "model": "jev-latest", "questions": {"relevant": {"type":"choice", "instructions":"Which earlier conversation is most relevant to what the developer is doing now (the workspace, branch, search text and current request in the state)? Prefer a conversation about the same task or topic; the same directory or branch alone is weaker evidence than the same subject. Choose \"none\" if no conversation is about this work.", "criteria": criteria}}});
        let mut authorization = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| ("TypeSafe: invalid key header".into(), true))?;
        authorization.set_sensitive(true);
        let request = Request::post("https://api.typesafe.ai/v1/systemone")
            .timeout(std::time::Duration::from_secs(15))
            .header("Content-Type", "application/json")
            .header("Authorization", authorization)
            .body(body.to_string().into())
            .map_err(|_| ("TypeSafe: invalid ranking request".into(), false))?;
        let response = client
            .send(request)
            .await
            .map_err(|_| ("TypeSafe is unreachable".into(), true))?;
        let status = response.status();
        if !status.is_success() {
            return Err((
                format!("TypeSafe: HTTP {}", status.as_u16()),
                matches!(status.as_u16(), 401 | 403),
            ));
        }
        let bytes = match response.into_body().0 {
            Inner::Empty => vec![],
            Inner::Bytes(bytes) => bytes.into_inner().to_vec(),
            Inner::AsyncReader(mut reader) => {
                let mut bytes = vec![];
                loop {
                    let mut chunk = [0; 8192];
                    let n = std::future::poll_fn(|cx| reader.as_mut().poll_read(cx, &mut chunk))
                        .await
                        .map_err(|_| ("TypeSafe: response read failed".into(), true))?;
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if bytes.len() > 1024 * 1024 {
                        return Err(("TypeSafe: response too large".into(), false));
                    }
                }
                bytes
            }
        };
        let value = serde_json::from_slice(&bytes)
            .map_err(|_| ("TypeSafe: invalid response JSON".into(), false))?;
        let (probabilities, confidence, none) =
            ranked_probabilities(&value, batch.len()).map_err(|e| (e, false))?;
        scored.extend(
            probabilities
                .into_iter()
                .enumerate()
                .map(|(i, p)| (batch_index * 254 + i, p)),
        );
        batches.push((confidence, none));
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let confident = scored
        .first()
        .and_then(|(i, _)| batches.get(i / 254))
        .is_some_and(|(confidence, none)| !none && *confidence >= 0.5);
    Ok((
        scored
            .into_iter()
            .map(|(i, _)| entries[i].id.clone())
            .collect(),
        confident,
    ))
}

pub struct FindSidebar {
    state: Rc<RefCell<State>>,
    owner: String,
    facts: FindFacts,
    folds: Disclosures,
    query: Entity<InputState>,
    _subscription: Subscription,
    relevant: bool,
    relevance: Relevance,
    active: bool,
}

impl EventEmitter<FindOutcome> for FindSidebar {}

impl FindSidebar {
    pub fn new(
        state: Rc<RefCell<State>>,
        owner: String,
        facts: FindFacts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("search conversations…");
            input.set_value(state.borrow().filter().query.clone(), window, cx);
            input
        });
        let subscription = cx.subscribe(&query, |this, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                let query = input.read(cx).value().to_string();
                if query != this.state.borrow().filter().query {
                    cx.emit(FindOutcome::SetQuery(query));
                }
            }
        });
        Self {
            state,
            owner,
            facts,
            folds: Disclosures::default(),
            relevant: false,
            relevance: Relevance::default(),
            active: true,
            query,
            _subscription: subscription,
        }
    }

    #[cfg(test)]
    pub(crate) fn open_machines(&self) -> &HashSet<Machine> {
        &self.folds.machines
    }

    /// Iced's "Show {machine} chats": only that machine's section open.
    pub fn show_only_machine(&mut self, machine: Machine, cx: &mut Context<Self>) {
        self.folds.machines.clear();
        self.folds.machines.insert(machine);
        cx.notify();
    }

    pub fn refresh(&mut self, facts: FindFacts, window: &mut Window, cx: &mut Context<Self>) {
        self.facts = facts;
        let query = self.state.borrow().filter().query.clone();
        if self.query.read(cx).value().as_str() != query {
            self.query
                .update(cx, |input, cx| input.set_value(query, window, cx));
        }
        self.request_rank(cx);
        cx.notify();
    }

    fn request_rank(&mut self, cx: &mut Context<Self>) {
        if !self.active || !self.relevant || self.relevance.down || self.relevance.in_flight {
            return;
        }
        let state = self.state.borrow();
        let Some(stamp) = rank_key(&state) else {
            return;
        };
        if stamp.sources.is_empty()
            || self
                .relevance
                .result
                .as_ref()
                .is_some_and(|(key, _, _)| *key == stamp)
        {
            return;
        }
        let Some(key) = ranking_key() else {
            return;
        };
        let entries = state
            .filtered_entries()
            .into_iter()
            .filter(|(m, _)| m.is_none())
            .map(|(_, e)| e.clone())
            .collect::<Vec<_>>();
        drop(state);
        self.relevance.in_flight = true;
        self.relevance.error = None;
        let client = cx.http_client();
        cx.spawn(async move |view, cx| {
            let filter = stamp.filter.clone();
            let result = cx
                .background_executor()
                .spawn(rank_chats(client, key, filter, entries))
                .await;
            _ = view.update(cx, |this, cx| {
                this.relevance.in_flight = false;
                match result {
                    Ok((order, confident)) => {
                        this.relevance.result = Some((stamp, order, confident))
                    }
                    Err((message, down)) => {
                        this.relevance.result = Some((stamp, vec![], false));
                        this.relevance.error = Some(message);
                        this.relevance.down = down;
                    }
                }
                this.request_rank(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn control(
        &self,
        id: String,
        control: Control,
        active: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let p = Palette::current(cx);
        let selector = id.clone();
        if id == "find-backfill" || id.starts_with("find-widen-") {
            return row_button(id, cx)
                .debug_selector({
                    let selector = selector.clone();
                    move || selector.clone()
                })
                .aria_label(control.label.clone())
                .px(px(8.))
                .py(px(3.))
                .text_size(px(theme::font_sizes(cx).ui - 2.))
                .child(
                    div()
                        .debug_selector({
                            let selector = selector.clone();
                            move || format!("{selector}-label")
                        })
                        .child(control.label),
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.emit(control.outcome.clone());
                }))
                .into_any_element();
        }
        let backend = matches!(control.outcome, FindOutcome::SetBackend(_));
        let mut content = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .text_size(px(theme::font_sizes(cx).ui - 2.));
        if let FindOutcome::SetBackend(Some(backend)) = &control.outcome {
            content = content.child(
                div()
                    .text_size(px(7.))
                    .text_color(backend_color(*backend, cx))
                    .child("●"),
            );
        }
        Button::new(id)
            .debug_selector(move || selector.clone())
            .ghost()
            .xsmall()
            .h_auto()
            .line_height(gpui::relative(theme::LABEL_LINE_HEIGHT))
            .px(px(if backend { 9. } else { 8. }))
            .py(px(if backend { 2. } else { 3. }))
            .when(backend, |b| {
                b.rounded_full().border_1().border_color(rgb(if active {
                    p.border
                } else {
                    p.surface
                }))
            })
            .accessibility_label(control.label.clone())
            .child(content.child(control.label))
            .when(active, |b| b.bg(rgb(p.surface)))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                cx.emit(control.outcome.clone());
            }))
            .into_any_element()
    }
}

// Iced's open-tab title/detail limits are character counts, not byte offsets.
fn open_lines(session: &OpenSession) -> (String, String) {
    let truncate = |text: &str, limit: usize| {
        if text.chars().count() > limit {
            format!("{}…", text.chars().take(limit - 1).collect::<String>())
        } else {
            text.to_string()
        }
    };
    let detail = if session.detail.is_empty() {
        session.workspace.clone()
    } else {
        format!("{} · {}", session.workspace, session.detail)
    };
    (truncate(&session.title, 48), truncate(&detail, 72))
}

// Plain clickable rows keep content at the Iced inset; Button centres its child.
fn row_button(id: impl Into<gpui::ElementId>, cx: &gpui::App) -> gpui::Stateful<gpui::Div> {
    let p = Palette::current(cx);
    div()
        .id(id)
        .role(gpui::Role::Button)
        .w_full()
        .flex_shrink_0()
        .flex()
        .items_start()
        .text_left()
        .line_height(gpui::relative(theme::LABEL_LINE_HEIGHT))
        .cursor_pointer()
        .hover(move |row| row.bg(rgb(p.overlay)))
}

fn header(id: String, parts: Vec<String>, cx: &gpui::App) -> gpui::Stateful<gpui::Div> {
    let p = Palette::current(cx);
    let selector = id.clone();
    row_button(id, cx)
        .aria_label(parts.join(" "))
        .px(px(6.))
        .py(px(6.))
        .items_center()
        .gap(px(8.))
        .text_size(px(10.))
        .text_color(rgb(p.subtext0))
        .children(parts.into_iter().enumerate().map(move |(i, part)| {
            let selector = selector.clone();
            div()
                .debug_selector(move || format!("{selector}-part-{i}"))
                .when(i == 2, |part| part.text_color(rgb(p.muted)))
                .child(part)
        }))
}

impl Render for FindSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let font_small = theme::font_sizes(cx).ui - 1.;
        let secondary = font_small - 1.;
        let state = self.state.borrow();
        let current_rank_key = rank_key(&state);
        let rank = self
            .relevance
            .result
            .as_ref()
            .filter(|(key, _, confident)| {
                self.relevant
                    && !self.relevance.down
                    && *confident
                    && Some(key) == current_rank_key.as_ref()
            })
            .map(|(_, order, _)| order.as_slice());
        let mut scope = div().flex().gap(px(2.));
        for (i, control) in scopes(&state, &self.facts).into_iter().enumerate() {
            let active = control.outcome == FindOutcome::SetScope(state.filter().scope);
            scope = scope.child(self.control(format!("find-scope-{i}"), control, active, cx));
        }
        let mut backend = div().flex().gap(px(6.));
        for (i, control) in backends().into_iter().enumerate() {
            let active = control.outcome == FindOutcome::SetBackend(state.filter().backend);
            backend = backend.child(self.control(format!("find-backend-{i}"), control, active, cx));
        }
        let mut list = div()
            .id("find-results")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(2.))
            .overflow_y_scroll()
            .px(px(6.))
            .py(px(4.));
        if !self.facts.open_sessions.is_empty() {
            list = list.child(
                header(
                    "find-open".into(),
                    vec![
                        if self.folds.open_collapsed {
                            "▸"
                        } else {
                            "▾"
                        }
                        .into(),
                        "OPEN".into(),
                        self.facts.open_sessions.len().to_string(),
                    ],
                    cx,
                )
                .debug_selector(|| "find-open".into())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.folds.open_collapsed = !this.folds.open_collapsed;
                    cx.notify();
                })),
            );
            if !self.folds.open_collapsed {
                for (i, session) in self.facts.open_sessions.iter().enumerate() {
                    let (title, detail) = open_lines(session);
                    let id = session.id.clone();
                    list = list.child(
                        row_button(format!("find-open-{i}"), cx)
                            .aria_label(format!("{title}, {detail}"))
                            .debug_selector(move || format!("find-open-{i}"))
                            .px(px(8.))
                            .py(px(4.))
                            .when(session.selected, |b| b.bg(rgb(p.surface)))
                            .child(
                                div()
                                    .w_full()
                                    .whitespace_normal()
                                    .text_left()
                                    .flex()
                                    .items_start()
                                    .gap(px(6.))
                                    .text_size(px(font_small))
                                    .child(
                                        div()
                                            .debug_selector(move || format!("find-open-icon-{i}"))
                                            .flex_shrink_0()
                                            .child(session.marker.trim_end().to_string()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap(px(1.))
                                            .child(
                                                div()
                                                    .debug_selector(move || {
                                                        format!("find-open-title-{i}")
                                                    })
                                                    .child(title),
                                            )
                                            .child(
                                                div()
                                                    .debug_selector(move || {
                                                        format!("find-open-detail-{i}")
                                                    })
                                                    .text_size(px(secondary))
                                                    .text_color(rgb(p.muted))
                                                    .child(detail),
                                            ),
                                    ),
                            )
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(FindOutcome::Open { id: id.clone() });
                            })),
                    );
                }
            }
        }
        for (i, row) in rows_ordered(&state, &self.owner, &self.facts, &self.folds, rank)
            .into_iter()
            .enumerate()
        {
            match row {
                Row::Machine {
                    id,
                    label,
                    count,
                    expanded,
                } => {
                    let status = self
                        .facts
                        .machine(&id)
                        .map(|m| m.status.clone())
                        .unwrap_or_default();
                    let mut parts = vec![label];
                    if state.filter().scope == ChatScope::Everywhere {
                        parts.insert(0, if expanded { "▾" } else { "▸" }.into());
                        parts.push(count.to_string());
                        if !status.is_empty() {
                            parts.push(status);
                        }
                        list = list.child(
                            header(format!("find-machine-{i}"), parts, cx)
                                .debug_selector(move || format!("find-machine-{i}"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.folds.machines.remove(&id) {
                                        this.folds.machines.insert(id.clone());
                                    }
                                    cx.notify();
                                })),
                        );
                    } else {
                        if !status.is_empty() {
                            parts.push(status);
                        }
                        list = list.child(
                            div()
                                .debug_selector(move || format!("find-machine-{i}"))
                                .flex_shrink_0()
                                .flex()
                                .gap(px(8.))
                                .px(px(2.))
                                .py(px(6.))
                                .text_size(px(10.))
                                .text_color(rgb(p.subtext0))
                                .children(parts.into_iter().map(|part| div().child(part))),
                        );
                    }
                }
                Row::Group { label, count } => {
                    list = list.child(
                        div()
                            .debug_selector(move || format!("find-group-{i}"))
                            .flex_shrink_0()
                            .flex()
                            .gap(px(6.))
                            .px(px(2.))
                            .py(px(6.))
                            .text_size(px(10.))
                            .text_color(rgb(p.muted))
                            .child(label)
                            .child(count.to_string()),
                    );
                }
                Row::Note(note) => {
                    list = list.child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(secondary))
                            .text_color(rgb(p.muted))
                            .child(note),
                    );
                }
                Row::Chat(row) => {
                    let id = row.id.clone();
                    let accessible_title = row.title.clone();
                    let mut meta = div()
                        .debug_selector(move || format!("find-meta-{i}"))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(secondary))
                        .text_color(rgb(p.muted))
                        .child(row.age);
                    if let Some(branch) = row.branch {
                        meta = meta.child(div().text_color(rgb(p.subtext0)).child(branch));
                    }
                    if let Some(control) = row.open {
                        meta = meta.child(
                            Button::new(format!("find-focus-{i}"))
                                .accessibility_label(control.label.clone())
                                .debug_selector(move || format!("find-focus-{i}"))
                                .ghost()
                                .h_auto()
                                .line_height(gpui::relative(theme::LABEL_LINE_HEIGHT))
                                .p_0()
                                .child(
                                    div()
                                        .text_size(px(secondary))
                                        .text_color(rgb(p.green))
                                        .child(control.label),
                                )
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.stop_propagation();
                                    cx.emit(control.outcome.clone());
                                })),
                        );
                    }
                    for text in row.meta {
                        meta = meta.child(div().child(text));
                    }
                    let mut lines = div()
                        .w_full()
                        .whitespace_normal()
                        .text_left()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .debug_selector(move || format!("find-title-{i}"))
                                .flex()
                                .gap(px(6.))
                                .text_size(px(font_small))
                                .child(
                                    div()
                                        .text_size(px(8.))
                                        .text_color(backend_color(row.backend, cx))
                                        .child("●"),
                                )
                                .child(div().flex_1().min_w_0().child(row.title)),
                        );
                    for (line, detail) in row.details.into_iter().enumerate() {
                        lines = lines.child(
                            div()
                                .debug_selector(move || format!("find-detail-{i}-{line}"))
                                .text_size(px(secondary))
                                .text_color(rgb(p.subtext0))
                                .child(detail),
                        );
                    }
                    let content = row_button(format!("find-chat-{i}"), cx)
                        .aria_label(accessible_title)
                        .debug_selector(move || format!("find-chat-{i}"))
                        .flex_1()
                        .min_w_0()
                        .px(px(8.))
                        .py(px(6.))
                        .when(row.child, |s| s.ml(px(18.)))
                        .when(row.selected, |s| {
                            s.bg(rgb(p.surface)).border_1().border_color(rgb(p.border))
                        })
                        .child(lines.child(meta))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(FindOutcome::Select { id: id.clone() })
                        }));
                    let mut line = div()
                        .w_full()
                        .flex_shrink_0()
                        .flex()
                        .items_start()
                        .child(content);
                    if let Some(expanded) = row.fold {
                        let id = row.id;
                        line = line.child(
                            Button::new(format!("find-fold-{i}"))
                                .label(if expanded { "▾" } else { "▸" })
                                .ghost()
                                .h_auto()
                                .px(px(8.))
                                .py(px(6.))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if !this.folds.roots.remove(&id) {
                                        this.folds.roots.insert(id.clone());
                                    }
                                    cx.notify();
                                })),
                        );
                    }
                    list = list.child(line);
                }
            }
        }
        if (state.filter().scope == ChatScope::Everywhere || state.filter().machine.is_none())
            && state
                .index(&None)
                .is_some_and(|index| !index.entries.is_empty())
        {
            list = list.child(self.control(
                "find-backfill".into(),
                Control {
                    label: "Index older chats".into(),
                    outcome: FindOutcome::Unavailable("Index older chats".into()),
                },
                false,
                cx,
            ));
        }
        for (i, control) in widen_suggestions(&state, &self.facts)
            .into_iter()
            .enumerate()
        {
            list = list.child(self.control(format!("find-widen-{i}"), control, false, cx));
        }
        let mut search = div()
            .flex()
            .gap_1()
            .child(Input::new(&self.query).small().text_size(px(font_small)));
        if self.facts.relevance_available {
            search = search.child(
                Button::new("find-relevant")
                    .debug_selector(|| "find-relevant".into())
                    .label("Relevant")
                    .outline()
                    .xsmall()
                    .disabled(self.facts.relevance_down || self.relevance.down)
                    .when(self.relevant && !self.facts.relevance_down, |b| {
                        b.bg(rgb(p.surface))
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.relevant = !this.relevant;
                        if !this.relevant {
                            this.relevance.error = None;
                        }
                        this.request_rank(cx);
                        cx.notify();
                    })),
            );
        }
        div()
            .id("find-sidebar")
            .debug_selector(|| "find-sidebar".into())
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .min_h_0()
            .bg(rgb(p.base))
            .text_color(rgb(p.text))
            .text_left()
            .font_family(theme::LABEL_FONT)
            .line_height(gpui::relative(theme::LABEL_LINE_HEIGHT))
            .text_size(px(font_small))
            .child(div().px_2().py(px(6.)).child(search))
            .child(div().px_2().child(scope))
            .child(div().px_2().py(px(2.)).child(backend))
            .when(self.relevant, |panel| {
                let note = if let Some(error) = &self.relevance.error {
                    Some(format!("{error} · showing recent"))
                } else if current_rank_key.is_none() {
                    Some("relevance order covers this Mac's chats only".into())
                } else if self.relevance.in_flight {
                    Some("ranking by relevance…".into())
                } else if self
                    .relevance
                    .result
                    .as_ref()
                    .is_some_and(|(key, _, confident)| {
                        Some(key) == current_rank_key.as_ref() && !confident
                    })
                {
                    Some("not confident, showing recent".into())
                } else {
                    None
                };
                panel.children(note.map(|note| div().px_2().child(note)))
            })
            .child(list)
    }
}

fn backend_color(backend: ChatBackend, cx: &gpui::App) -> gpui::Hsla {
    let dark = gpui_component::Theme::global(cx).mode.is_dark();
    rgb(match (backend, dark) {
        (ChatBackend::Claude, true) => 0xfab387,
        (ChatBackend::Claude, false) => 0xfe640b,
        (ChatBackend::Codex, true) => 0x89b4fa,
        (ChatBackend::Codex, false) => 0x1e66f5,
        (ChatBackend::Pi, true) => 0xcba6f7,
        (ChatBackend::Pi, false) => 0x8839ef,
    })
    .into()
}

fn preview_actions(
    entry: &ChatIndexEntry,
    id: &ChatRef,
    facts: &FindFacts,
) -> (Vec<Control>, String) {
    let resume = |label: &str, mode| Control {
        label: label.into(),
        outcome: FindOutcome::Resume {
            id: id.clone(),
            mode,
        },
    };
    if facts.open(id).is_some() {
        return (
            vec![Control {
                label: "Go to Session".into(),
                outcome: FindOutcome::Open { id: id.clone() },
            }],
            "already open in a session — one process per conversation".into(),
        );
    }
    let remote = id.remote_id.is_some();
    let machine = facts.machine(&id.remote_id);
    let name = facts.machine_name(&id.remote_id);
    if remote && machine.is_none_or(|m| !m.reachable) {
        return (
            vec![],
            format!(
                "{name} is unreachable — shown from the cached index; resume when it reconnects"
            ),
        );
    }
    if remote && facts.running_remote.contains(id) {
        return (
            vec![resume("Open Chat", ResumeMode::Chat)],
            format!("● running on {name}"),
        );
    }
    if entry.dead_cwd {
        return if remote {
            (
                vec![],
                "the recorded directory is gone on the remote machine".into(),
            )
        } else {
            (
                vec![resume(
                    "Recreate directory & resume",
                    ResumeMode::RecreateDirectory,
                )],
                "the recorded directory was deleted; resume only works from it".into(),
            )
        };
    }
    let mut controls = vec![resume("Resume as Session", ResumeMode::Session)];
    if entry.backend == ChatBackend::Claude && (!remote || machine.is_some_and(|m| m.claude_chat)) {
        controls.push(resume("Resume as Chat", ResumeMode::Chat));
    }
    let note = if entry.possibly_running() {
        "◐ transcript is still growing — may be running in a terminal GitTerm didn't start".into()
    } else if remote {
        format!("starts an agentd session on {name}")
    } else {
        "resumes in the recorded directory".into()
    };
    (controls, note)
}

/// Companion centre view; subscribe to the same outcomes as the sidebar.
pub struct FindPreview {
    state: Rc<RefCell<State>>,
    owner: String,
    facts: FindFacts,
    raw_expanded: bool,
    idle_expanded: bool,
    ask_expanded: bool,
    reply_expanded: bool,
    file_overlay: bool,
}

impl EventEmitter<FindOutcome> for FindPreview {}

impl FindPreview {
    pub fn new(state: Rc<RefCell<State>>, owner: String, facts: FindFacts) -> Self {
        Self {
            state,
            owner,
            facts,
            raw_expanded: false,
            idle_expanded: false,
            ask_expanded: false,
            reply_expanded: false,
            file_overlay: false,
        }
    }

    pub fn refresh(&mut self, facts: FindFacts, cx: &mut Context<Self>) {
        self.facts = facts;
        cx.notify();
    }
}

fn overview_place(
    facts: &FindFacts,
    session: &OpenSession,
    indexed: Option<&ChatIndexEntry>,
) -> String {
    let mut parts = vec![facts.machine_name(&session.id.remote_id)];
    if let Some(entry) = indexed {
        parts.push(entry.group_name());
        if let Some(branch) = &entry.branch {
            parts.push(branch.clone());
        }
        parts.push(entry.backend.label().into());
    } else {
        parts.extend([session.workspace.clone(), session.detail.clone()]);
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

impl FindPreview {
    fn unavailable_button(&self, id: String, label: String, cx: &Context<Self>) -> Button {
        let selector = id.clone();
        Button::new(id)
            .debug_selector(move || selector.clone())
            .label(label.clone())
            .outline()
            .xsmall()
            .on_click(
                cx.listener(move |_, _, _, cx| cx.emit(FindOutcome::Unavailable(label.clone()))),
            )
    }

    fn workspace_actions(&self, entry: &ChatIndexEntry, cx: &Context<Self>) -> gpui::Div {
        let root = entry.root_id.as_ref().unwrap_or(&entry.id);
        let owner = self
            .facts
            .workspaces
            .iter()
            .find(|w| w.chats.contains(root));
        let mut line = div().flex().flex_wrap().gap_2();
        if let Some(owner) = owner {
            let name = if owner.name.chars().count() > 30 {
                format!("{}…", owner.name.chars().take(29).collect::<String>())
            } else {
                owner.name.clone()
            };
            line =
                line.child(format!("in workspace {}", owner.name))
                    .child(self.unavailable_button(
                        "find-workspace-owner".into(),
                        format!("Go to {name}"),
                        cx,
                    ));
        } else if entry.root_id.is_none() {
            line = line.child(self.unavailable_button(
                "find-workspace-make".into(),
                "Make workspace".into(),
                cx,
            ));
        }
        if self
            .facts
            .workspaces
            .iter()
            .any(|w| w.open && !w.chats.contains(root))
        {
            line = line.child(
                self.unavailable_button(
                    "find-workspace-add".into(),
                    if entry.root_id.is_some() {
                        "Add its coordinator to workspace…"
                    } else {
                        "Add to workspace…"
                    }
                    .into(),
                    cx,
                ),
            );
        }
        line
    }

    fn overview(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let p = Palette::current(cx);
        let font = theme::font_sizes(cx).ui;
        let small = font - 1.;
        let class = |s: &&OpenSession| match s.marker.as_str() {
            "●" => 0,
            "▶" => 1,
            _ => 2,
        };
        let counts = [0, 1, 2].map(|n| {
            self.facts
                .open_sessions
                .iter()
                .filter(|s| class(s) == n)
                .count()
        });
        let mut summary = vec![];
        if counts[0] > 0 {
            summary.push(format!("{} need you", counts[0]));
        }
        if counts[1] > 0 {
            summary.push(format!("{} working", counts[1]));
        }
        summary.push(format!("{} open", self.facts.open_sessions.len()));
        let mut body = div()
            .id("find-overview")
            .debug_selector(|| "find-overview".into())
            .size_full()
            .overflow_y_scroll()
            .px(px(18.))
            .py(px(14.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(div().text_size(px(font + 2.)).child("Overview"))
            .child(div().text_color(rgb(p.subtext0)).child(summary.join(" · ")));
        for machine in &self.facts.machines {
            body = body.child(
                div()
                    .text_color(rgb(p.muted))
                    .child(format!("{} {}", machine.name, machine.status)),
            );
        }
        if self.facts.open_sessions.is_empty() && self.facts.running_remote.is_empty() {
            return body.child(div().p_4().child("Nothing open or running")).child("Chats, CLI sessions and Codex delegations show up here while they are open or running, on this Mac and connected machines. Pick a conversation on the left to preview it.");
        }
        for (group, label) in ["NEEDS YOU", "WORKING NOW", "IDLE BUT OPEN"]
            .into_iter()
            .enumerate()
        {
            if counts[group] == 0 {
                continue;
            }
            if group == 2 {
                body = body.child(
                    header(
                        "find-overview-idle".into(),
                        vec![
                            if self.idle_expanded { "▾" } else { "▸" }.into(),
                            label.into(),
                            counts[group].to_string(),
                        ],
                        cx,
                    )
                    .debug_selector(|| "find-overview-idle".into())
                    .aria_expanded(self.idle_expanded)
                    .pt(px(10.))
                    .pb(px(4.))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.idle_expanded = !this.idle_expanded;
                        cx.notify();
                    })),
                );
                if !self.idle_expanded {
                    continue;
                }
            } else {
                body = body.child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .gap(px(8.))
                        .pt(px(14.))
                        .pb(px(4.))
                        .px(px(6.))
                        .text_size(px(10.))
                        .text_color(rgb(p.subtext0))
                        .child(label)
                        .child(
                            div()
                                .text_color(rgb(p.muted))
                                .child(counts[group].to_string()),
                        ),
                );
            }
            for (i, session) in self
                .facts
                .open_sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| class(s) == group)
            {
                let id = session.id.clone();
                let indexed = entry(&self.state.borrow(), &id).cloned();
                let place = overview_place(&self.facts, session, indexed.as_ref());
                let color = match group {
                    0 => p.peach,
                    1 => p.green,
                    _ => p.muted,
                };
                // shortcut: OpenSession exposes no elapsed/cost or gist facts;
                // show indexed activity age until the host supplies those facts.
                let right = indexed
                    .as_ref()
                    .map(|entry| format!("{} ago", chats::format_age(entry.mtime)))
                    .unwrap_or_default();
                let detail = match group {
                    0 => Some("waiting for you".to_string()),
                    1 => Some("Working".to_string()),
                    _ => indexed.as_ref().and_then(|entry| entry.latest.clone()),
                };
                let mut lines = div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(
                        div()
                            .debug_selector(move || format!("find-overview-title-{i}"))
                            .text_size(px(small))
                            .child(session.title.clone()),
                    )
                    .child(
                        div()
                            .debug_selector(move || format!("find-overview-meta-{i}"))
                            .text_size(px(small - 1.))
                            .text_color(rgb(p.muted))
                            .child(place),
                    );
                if let Some(detail) = detail {
                    lines = lines.child(
                        div()
                            .debug_selector(move || format!("find-overview-detail-{i}"))
                            .text_size(px(small))
                            .text_color(rgb(match group {
                                0 => p.peach,
                                1 => p.subtext0,
                                _ => p.muted,
                            }))
                            .child(detail),
                    );
                }
                body = body.child(
                    row_button(format!("find-overview-row-{i}"), cx)
                        .debug_selector(move || format!("find-overview-row-{i}"))
                        .aria_label(session.title.clone())
                        .px(px(10.))
                        .py(px(5.))
                        .gap(px(8.))
                        .child(
                            div()
                                .debug_selector(move || format!("find-overview-icon-{i}"))
                                .flex_shrink_0()
                                .text_size(px(small))
                                .text_color(rgb(color))
                                .child(session.marker.clone()),
                        )
                        .child(lines)
                        .child(
                            div()
                                .debug_selector(move || format!("find-overview-right-{i}"))
                                .flex_shrink_0()
                                .text_size(px(small - 1.))
                                .text_color(rgb(p.subtext0))
                                .child(right),
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(FindOutcome::Open { id: id.clone() })
                        })),
                );
            }
        }
        for (i, id) in self.facts.running_remote.iter().enumerate() {
            let state = self.state.borrow();
            let indexed = entry(&state, id);
            let title = indexed
                .map(|e| e.title.clone())
                .unwrap_or_else(|| id.id.clone());
            let disabled = indexed.is_none();
            let id = id.clone();
            body = body.child(
                row_button(format!("find-overview-remote-{i}"), cx)
                    .debug_selector(move || format!("find-overview-remote-{i}"))
                    .aria_label(format!(
                        "▶ {} · {}",
                        title,
                        self.facts.machine_name(&id.remote_id)
                    ))
                    .px(px(10.))
                    .py(px(5.))
                    .gap(px(8.))
                    .when(disabled, |row| row.cursor_default().opacity(0.5))
                    .child(
                        div()
                            .debug_selector(move || format!("find-overview-remote-icon-{i}"))
                            .text_color(rgb(p.green))
                            .child("▶"),
                    )
                    .child(
                        div()
                            .debug_selector(move || format!("find-overview-remote-title-{i}"))
                            .child(format!(
                                "{} · {}",
                                title,
                                self.facts.machine_name(&id.remote_id)
                            )),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if !disabled {
                            cx.emit(FindOutcome::Select { id: id.clone() })
                        }
                    })),
            );
        }
        body
    }

    fn summary(
        &self,
        entry: &ChatIndexEntry,
        id: &ChatRef,
        preview: Option<&PreviewData>,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let mut out = div().flex().flex_col().gap_3();
        let Some(preview) = preview else {
            return out.child("reading the conversation…");
        };
        if let Some(session) = self.facts.open(id)
            && session.marker == "▶"
        {
            out = out.child("DOING NOW").child(session.detail.clone());
        }
        if let Some(full) = &preview.summary.ask {
            let (clipped, cut) = clip_lines(full, 4);
            out = out.child("ASKED FOR").child(
                div()
                    .debug_selector(|| "find-summary-ask-text".into())
                    .child(if self.ask_expanded {
                        full.clone()
                    } else {
                        clipped
                    }),
            );
            if cut {
                out = out.child(
                    row_button("find-summary-ask", cx)
                        .debug_selector(|| "find-summary-ask".into())
                        .aria_label(if self.ask_expanded { "less" } else { "more" })
                        .px(px(6.))
                        .py(px(2.))
                        .child(
                            div()
                                .debug_selector(|| "find-summary-ask-label".into())
                                .child(if self.ask_expanded { "less" } else { "more" }),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ask_expanded = !this.ask_expanded;
                            cx.notify();
                        })),
                );
            }
        }
        if !preview.summary.files.is_empty() {
            out = out.child(format!("FILES TOUCHED {}", preview.summary.files.len()));
            for (i, path) in preview.summary.files.iter().take(8).enumerate() {
                let full = entry.cwd.join(path);
                let shown = full
                    .strip_prefix(&entry.cwd)
                    .ok()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| path.clone());
                if id.remote_id.is_none() && self.facts.local_files_openable {
                    out = out.child(
                        row_button(format!("find-summary-file-{i}"), cx)
                            .debug_selector(move || format!("find-summary-file-{i}"))
                            .aria_label(shown.clone())
                            .px(px(6.))
                            .py(px(2.))
                            .child(
                                div()
                                    .debug_selector(move || format!("find-summary-file-label-{i}"))
                                    .child(shown),
                            )
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(FindOutcome::OpenFile(full.clone()))
                            })),
                    );
                } else {
                    out = out.child(shown);
                }
            }
            if preview.summary.files.len() > 8 {
                out = out.child(format!("+{} more", preview.summary.files.len() - 8));
            }
        }
        if self.facts.open(id).is_some_and(|s| s.marker == "●") {
            // Iced's ChatOverviewGo jumps to the waiting session's tab; this
            // button shows only while that session is open here.
            let target = id.clone();
            out = out.child("WAITING ON YOU").child("waiting for you").child(
                Button::new("find-summary-prompt")
                    .debug_selector(|| "find-summary-prompt".into())
                    .label("Go to prompt")
                    .outline()
                    .xsmall()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(FindOutcome::Open { id: target.clone() })
                    })),
            );
        }
        if let Some(full) = &preview.summary.reply {
            let (clipped, cut) = clip_lines(full, 16);
            out = out.child("LATEST REPLY").child(
                div()
                    .debug_selector(|| "find-summary-reply-text".into())
                    .child(if self.reply_expanded {
                        full.clone()
                    } else {
                        clipped
                    }),
            );
            if cut {
                out = out.child(
                    row_button("find-summary-reply", cx)
                        .debug_selector(|| "find-summary-reply".into())
                        .aria_label(if self.reply_expanded { "less" } else { "more" })
                        .px(px(6.))
                        .py(px(2.))
                        .child(
                            div()
                                .debug_selector(|| "find-summary-reply-label".into())
                                .child(if self.reply_expanded { "less" } else { "more" }),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.reply_expanded = !this.reply_expanded;
                            cx.notify();
                        })),
                );
            }
        }
        out
    }
}

impl Render for FindPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::current(cx);
        let state = self.state.borrow();
        let mut pane = div()
            .id("find-preview")
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(p.base))
            .text_color(rgb(p.text))
            .font_family(theme::LABEL_FONT)
            .line_height(gpui::relative(theme::LABEL_LINE_HEIGHT))
            .text_size(px(theme::font_sizes(cx).ui - 1.));
        let Some(selection) = state.selected(&self.owner) else {
            return pane.child(self.overview(cx));
        };
        let close = Button::new("find-close-preview")
            .debug_selector(|| "find-close-preview".into())
            .label("✕")
            .ghost()
            .small()
            .on_click(cx.listener(|_, _, _, cx| cx.emit(FindOutcome::ClosePreview)));
        let Some(entry) = entry(&state, &selection.chat) else {
            return pane
                .child(div().p_4().child("conversation no longer in the index"))
                .child(close);
        };
        let preview = state.preview(&self.owner);
        let (actions, note) = preview_actions(entry, &selection.chat, &self.facts);
        let mut controls = div().flex().flex_wrap().gap_2();
        for (i, control) in actions.into_iter().enumerate() {
            controls = controls.child(
                Button::new(format!("find-preview-action-{i}"))
                    .debug_selector(move || format!("find-preview-action-{i}"))
                    .label(control.label.clone())
                    .outline()
                    .small()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(if matches!(control.outcome, FindOutcome::Resume { .. }) {
                            FindOutcome::Unavailable(control.label.clone())
                        } else {
                            control.outcome.clone()
                        });
                    })),
            );
        }
        let mut place = vec![
            self.facts.machine_name(&selection.chat.remote_id),
            entry.group_name(),
        ];
        if let Some(branch) = &entry.branch {
            place.push(branch.clone());
        }
        let count = preview
            .and_then(|p| p.message_count)
            .map(|n| format!(" · {n} messages"))
            .unwrap_or_default();
        pane =
            pane.child(
                div()
                    .px_4()
                    .py_3()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_size(px(15.)).child(entry.title.clone()))
                            .child(div().text_color(backend_color(entry.backend, cx)).child(
                                format!("● {} · {}", entry.backend.label(), place.join(" · ")),
                            ))
                            .children(self.facts.open(&selection.chat).map(|session| {
                                div().child(match session.marker.as_str() {
                                    "●" => "waiting for you",
                                    "▶" => "working",
                                    _ => "idle",
                                })
                            }))
                            .child(div().text_color(rgb(p.muted)).child(format!(
                                "{} · {} ago{count}",
                                chats::format_size(entry.size),
                                chats::format_age(entry.mtime)
                            )))
                            .child(format!(
                                "{}{}",
                                entry.cwd.display(),
                                if entry.dead_cwd {
                                    " — directory no longer exists"
                                } else {
                                    ""
                                }
                            ))
                            .child(controls)
                            .child(div().text_color(rgb(p.muted)).child(note))
                            .when(selection.chat.remote_id.is_none(), |header| {
                                header.child(self.workspace_actions(entry, cx))
                            }),
                    )
                    .child(close),
            );
        let mut body = div()
            .id("find-preview-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_3();
        body = body.child(self.summary(entry, &selection.chat, preview, cx));
        body = body.child(
            header(
                "find-preview-conversation".into(),
                vec![
                    if self.raw_expanded { "▾" } else { "▸" }.into(),
                    "CONVERSATION".into(),
                    preview
                        .map(|p| format!("last {}", p.messages.len()))
                        .unwrap_or_default(),
                ],
                cx,
            )
            .debug_selector(|| "find-preview-conversation".into())
            .on_click(cx.listener(|this, _, _, cx| {
                this.raw_expanded = !this.raw_expanded;
                cx.notify();
            })),
        );
        if let Some(error) = state.preview_error(&self.owner) {
            body = body.child(div().text_color(rgb(p.red)).child(error.to_string()));
        }
        if self.raw_expanded {
            match preview {
                None if state.preview_error(&self.owner).is_none() => {
                    body = body.child("loading preview…")
                }
                None => {}
                Some(preview) => {
                    if let Some(count) = preview
                        .message_count
                        .filter(|n| *n > preview.messages.len() as u64)
                    {
                        body = body.child(format!(
                            "— {} earlier messages —",
                            count - preview.messages.len() as u64
                        ));
                    }
                    if preview.messages.is_empty() {
                        body = body.child("no renderable messages in the transcript tail");
                    }
                    for message in &preview.messages {
                        body = body.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(9.))
                                        .text_color(if message.is_user {
                                            rgb(p.green).into()
                                        } else {
                                            backend_color(entry.backend, cx)
                                        })
                                        .child(if message.is_user {
                                            "YOU".into()
                                        } else {
                                            entry.backend.label().to_uppercase()
                                        }),
                                )
                                .child(message.text.clone()),
                        );
                    }
                    body =
                        body.child(div().text_color(rgb(p.muted)).child(
                            "end of transcript · read-only preview (resume lands in slice 2)",
                        ));
                }
            }
        }
        pane.child(body)
    }
}

/// Retained per tab, including subscriptions while another tab is selected.
pub(crate) struct FindViews {
    pub(crate) sidebar: Entity<FindSidebar>,
    preview: Entity<FindPreview>,
    _subscriptions: [Subscription; 2],
}

impl crate::Shell {
    fn find_owner_for(
        &self,
        workspace: &crate::WorkspaceDock,
        session: Option<crate::data::SessionId>,
        cx: &gpui::App,
    ) -> String {
        if let Some(session) = session
            && let Some(pane) = workspace.panes.get(&crate::session_pane_id(session))
        {
            if let Some(target) = &pane.read(cx).session {
                return target.session_uid.clone();
            }
            if let Some(saved) = self.owner.read(cx).iced_workspace(workspace.workspace.id)
                && let Some(index) = self
                    .owner
                    .read(cx)
                    .iced_sessions(saved)
                    .iter()
                    .position(|s| s.id == session)
                && let Some(uid) = saved
                    .tabs
                    .get(index)
                    .and_then(|tab| tab.session_uid.as_ref())
            {
                return uid.clone();
            }
            // Legacy imported tabs have no UID or runtime target. Their retained
            // entity identity is stable for this tab's lifetime, never its index.
            return format!("gpui-find-tab-{}", pane.entity_id());
        }
        format!("gpui-find-workspace-{}", workspace.runtime_uid)
    }

    pub(crate) fn find_owner(&self, cx: &gpui::App) -> String {
        let workspace = self.active(cx);
        self.find_owner_for(workspace, workspace.active_session, cx)
    }

    fn find_conversation(
        &self,
        workspace: &crate::WorkspaceDock,
        session: &crate::data::Session,
        cx: &gpui::App,
    ) -> Option<ChatRef> {
        // Remote workspaces are intentionally absent from this local-only host.
        let cwd = self.data.terminal_cwd(workspace.workspace.id, cx)?;
        if self.live_chat_enabled(cx)
            && let Some(pane) = workspace.panes.get(&crate::session_pane_id(session.id))
            && let crate::pane::PaneContent::PreviewChat(view) = &pane.read(cx).content
        {
            return view
                .read(cx)
                .chat_session_id()
                .map(|id| ChatRef::local(&id));
        }
        if let Some(saved) = self.owner.read(cx).iced_workspace(workspace.workspace.id)
            && let Some(index) = self
                .owner
                .read(cx)
                .iced_sessions(saved)
                .iter()
                .position(|s| s.id == session.id)
            && let Some(id) = saved
                .tabs
                .get(index)
                .and_then(|t| t.chat_session_id.as_deref())
        {
            return Some(ChatRef::new(None, id));
        }
        let pane = workspace
            .panes
            .get(&crate::session_pane_id(session.id))?
            .read(cx);
        let crate::pane::PaneContent::PreviewChat(view) = &pane.content else {
            return None;
        };
        self.owner
            .read(cx)
            .chat_history(Some(&cwd))
            .iter()
            .find_map(|saved| {
                let id = saved.session_id.as_deref().unwrap_or(&saved.uid);
                view.read(cx)
                    .matches_chat(id)
                    .then(|| ChatRef::new(None, id))
            })
    }

    fn find_facts(&self, cx: &gpui::App) -> FindFacts {
        let mut sessions = vec![];
        for workspace in &self.workspaces {
            for session in &workspace.sessions {
                let Some(id) = self.find_conversation(workspace, session, cx) else {
                    continue;
                };
                let Some(pane) = workspace.panes.get(&crate::session_pane_id(session.id)) else {
                    continue;
                };
                let pane = pane.read(cx);
                let title = pane.tab_title();
                // shortcut: native request priority/wait facts are private to
                // ChatPane; use exposed title/streaming facts until shared
                // attention accessors are available to this host.
                let needs_you = title.starts_with('✳');
                let working = match &pane.content {
                    crate::pane::PaneContent::PreviewChat(view) => view.read(cx).is_working(),
                    _ => title.trim_start().chars().next().is_some_and(|ch| {
                        matches!(ch, '◐' | '◑' | '◒' | '◓' | '✢' | '✶' | '✻' | '✽' | '·')
                            || ('\u{2800}'..='\u{28ff}').contains(&ch)
                    }),
                };
                sessions.push((
                    if needs_you {
                        0
                    } else if working {
                        1
                    } else {
                        2
                    },
                    OpenSession {
                        id,
                        title,
                        workspace: workspace.workspace.name.clone(),
                        marker: if needs_you {
                            "●"
                        } else if working {
                            "▶"
                        } else {
                            "✦"
                        }
                        .into(),
                        detail: self.data.status(workspace.workspace.id, cx).branch,
                        selected: Some(workspace.workspace.id) == self.active_workspace(cx)
                            && workspace.active_session == Some(session.id),
                    },
                ));
            }
        }
        // Stable sort preserves workspace/tab order among equal attention classes.
        sessions.sort_by_key(|(class, _)| *class);
        FindFacts {
            machines: std::iter::once(MachineFacts {
                id: None,
                name: "This Mac".into(),
                status: String::new(),
                reachable: true,
                claude_chat: false,
            })
            .chain(self.configured_remotes().iter().map(|remote| {
                let id = Some(remote.id.clone());
                let state = self.find_state.borrow();
                let index = state.index(&id).filter(|i| i.loaded_at.is_some() || state.loading_index(&id));
                let status = gitterm_core::find::state::RemoteIndexStatus::of(index.map(|i| {
                    (state.loading_index(&id), i.error.is_some(), !i.entries.is_empty())
                }));
                MachineFacts {
                    reachable: status == gitterm_core::find::state::RemoteIndexStatus::Connected,
                    id,
                    name: remote.name.clone(),
                    status: status.label().into(),
                    claude_chat: false,
                }
            }))
            .collect(),
            open_sessions: sessions.into_iter().map(|(_, session)| session).collect(),
            running_remote: vec![],
            workspaces: self.owner.read(cx).iced_catalog().map(|catalog| catalog.workspaces.iter().filter(|w| w.location.as_ref().is_none_or(|l| matches!(l, gitterm_core::remote::config::WorkspaceLocationConfig::Local { .. }))).map(|w| WorkspaceFacts { name: w.name.clone(), chats: w.chats.clone(), open: w.active }).collect()).unwrap_or_else(|| self.workspaces.iter().map(|w| WorkspaceFacts { name: w.workspace.name.clone(), chats: vec![], open: true }).collect()),
            relevance_available: ranking_key().is_some(),
            relevance_down: false,
            local_files_openable: self.active_agent_tab(cx),
        }
    }

    pub(crate) fn ensure_find_views(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.active_workspace(cx).is_none() {
            return;
        }
        let mut alive = HashSet::new();
        for workspace in &self.workspaces {
            if workspace.sessions.is_empty() {
                alive.insert(self.find_owner_for(workspace, None, cx));
            }
            for session in &workspace.sessions {
                alive.insert(self.find_owner_for(workspace, Some(session.id), cx));
            }
        }
        self.find_views.retain(|owner, _| {
            if alive.contains(owner) {
                true
            } else {
                self.find_state
                    .borrow_mut()
                    .reduce(Intent::ClosePreview(owner.clone()));
                false
            }
        });
        let owner = self.find_owner(cx);
        let facts = self.find_facts(cx);
        if !self.find_views.contains_key(&owner) {
            let sidebar = cx.new(|cx| {
                FindSidebar::new(
                    self.find_state.clone(),
                    owner.clone(),
                    facts.clone(),
                    window,
                    cx,
                )
            });
            let preview =
                cx.new(|_| FindPreview::new(self.find_state.clone(), owner.clone(), facts.clone()));
            let sidebar_owner = owner.clone();
            let preview_owner = owner.clone();
            let subscriptions = [
                cx.subscribe_in(
                    &sidebar,
                    window,
                    move |shell, _, outcome: &FindOutcome, window, cx| {
                        shell.find_outcome(&sidebar_owner, outcome, window, cx)
                    },
                ),
                cx.subscribe_in(
                    &preview,
                    window,
                    move |shell, _, outcome: &FindOutcome, window, cx| {
                        shell.find_outcome(&preview_owner, outcome, window, cx)
                    },
                ),
            ];
            self.find_views.insert(
                owner.clone(),
                FindViews {
                    sidebar,
                    preview,
                    _subscriptions: subscriptions,
                },
            );
        }
        let mut filter = self.find_state.borrow().filter().clone();
        let root = self.data.terminal_cwd(self.active(cx).workspace.id, cx);
        if filter.machine.is_some() || filter.workspace_root != root {
            filter.machine = None;
            filter.workspace_root = root;
            self.reduce_find(Intent::SetFilter(filter), window, cx);
        }
        if self
            .find_views
            .values()
            .any(|views| views.sidebar.read(cx).facts != facts)
        {
            self.refresh_find(window, cx);
        }
        let regions = &self.active(cx).store.regions;
        let find_open =
            regions.sidebar_mode == crate::SidebarMode::Find && !regions.sidebar_collapsed;
        for (id, views) in &self.find_views {
            views.sidebar.update(cx, |sidebar, cx| {
                let active = find_open && *id == owner;
                if active && !sidebar.active {
                    sidebar.relevance.down = false;
                    if sidebar.relevance.error.take().is_some() {
                        sidebar.relevance.result = None;
                    }
                }
                sidebar.active = active;
                sidebar.request_rank(cx);
            });
        }
        if find_open {
            self.reduce_find(
                Intent::LoadIndex {
                    machine: None,
                    now: std::time::Instant::now(),
                    force: false,
                },
                window,
                cx,
            );
            if self.find_state.borrow().filter().scope == ChatScope::Everywhere {
                self.load_remote_indexes(window, cx);
            }
        }
    }

    fn refresh_find(&self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        // No workspace (the empty content's Open Folder) has no Find views.
        if self.active_workspace(cx).is_none() {
            return;
        }
        let facts = self.find_facts(cx);
        let owner = self.find_owner(cx);
        let regions = &self.active(cx).store.regions;
        let find_open =
            regions.sidebar_mode == crate::SidebarMode::Find && !regions.sidebar_collapsed;
        for (id, views) in &self.find_views {
            views.sidebar.update(cx, |sidebar, cx| {
                let active = find_open && *id == owner;
                if active && !sidebar.active {
                    sidebar.relevance.down = false;
                    if sidebar.relevance.error.take().is_some() {
                        sidebar.relevance.result = None;
                    }
                }
                sidebar.active = active;
                sidebar.refresh(facts.clone(), window, cx);
            });
            views
                .preview
                .update(cx, |preview, cx| preview.refresh(facts.clone(), cx));
        }
    }

    pub(crate) fn reduce_find(
        &mut self,
        intent: Intent,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        use gitterm_core::find::state::Effect;
        let loading_index = matches!(&intent, Intent::LoadIndex { .. });
        let effects = self.find_state.borrow_mut().reduce(intent);
        // A freshness no-op must not schedule another render/load loop.
        if effects.is_empty() && loading_index {
            return;
        }
        for effect in effects {
            match effect {
                Effect::LoadIndex(stamp) if stamp.machine.is_some() => {
                    self.load_remote_index(stamp, window, cx)
                }
                Effect::LoadIndex(stamp) => {
                    #[cfg(test)]
                    let home = self.find_home.clone();
                    #[cfg(not(test))]
                    let home = dirs::home_dir();
                    cx.spawn_in(window, async move |shell, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move {
                                home.map(|home| gitterm_core::find::build_local_index(&home))
                                    .ok_or_else(|| {
                                        "Local transcript source not available in preview".into()
                                    })
                            })
                            .await;
                        _ = shell.update_in(cx, |shell, window, cx| {
                            shell.reduce_find(
                                Intent::IndexFinished {
                                    stamp,
                                    result,
                                    now: std::time::Instant::now(),
                                },
                                window,
                                cx,
                            )
                        });
                    })
                    .detach();
                }
                Effect::LoadPreview(stamp) => {
                    let source = stamp.clone();
                    cx.spawn_in(window, async move |shell, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move {
                                if source.chat.remote_id.is_some() {
                                    return Err("Remote previews not available in preview".into());
                                }
                                load_preview_data(&source.path, source.backend)
                            })
                            .await;
                        _ = shell.update_in(cx, |shell, window, cx| {
                            shell.reduce_find(Intent::PreviewFinished { stamp, result }, window, cx)
                        });
                    })
                    .detach();
                }
                Effect::FilterChanged | Effect::IndexChanged(_) => {}
            }
        }
        self.refresh_find(window, cx);
        self.refresh_picker_recents(cx);
        cx.notify();
    }

    /// Sidebar mode Find, as the Chats overview opens it.
    pub(crate) fn open_find_sidebar(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.active_workspace(cx).is_none() {
            return;
        }
        if self.usage_open {
            self.toggle_usage(window, cx);
        }
        if self.plans_open {
            self.toggle_plans(window, cx);
        }
        self.update_regions(
            |r| {
                r.sidebar_mode = crate::SidebarMode::Find;
                r.sidebar_collapsed = false;
            },
            cx,
        );
    }

    pub(crate) fn find_outcome(
        &mut self,
        owner: &str,
        outcome: &FindOutcome,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if let FindOutcome::Unavailable(label) = outcome {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some(format!("{label} · not available in preview"));
                cx.notify();
            });
            return;
        }
        if let FindOutcome::OpenFile(path) = outcome {
            if let Some(views) = self.find_views.get(owner) {
                views
                    .preview
                    .update(cx, |preview, _| preview.file_overlay = true);
            }
            if let Some(workspace) = self.active_workspace(cx) {
                self.open_file_viewer(
                    &crate::FileOpen {
                        workspace,
                        path: path.clone(),
                        changed: None,
                        from_files: true,
                    },
                    window,
                    cx,
                );
            }
            return;
        }
        let intent = outcome.intent(&self.find_state.borrow(), owner);
        if let Some(intent) = intent {
            if matches!(
                outcome,
                FindOutcome::Select { .. } | FindOutcome::ClosePreview
            ) && let Some(views) = self.find_views.get(owner)
            {
                views
                    .preview
                    .update(cx, |preview, _| preview.file_overlay = false);
            }
            self.reduce_find(intent, window, cx);
            cx.notify();
            return;
        }
        let (FindOutcome::Open { id } | FindOutcome::Resume { id, .. }) = outcome else {
            return;
        };
        let target = self.workspaces.iter().find_map(|workspace| {
            workspace.sessions.iter().find_map(|session| {
                (self.find_conversation(workspace, session, cx).as_ref() == Some(id))
                    .then_some((workspace.workspace.id, session.id))
            })
        });
        if let Some((workspace, session)) = target {
            self.find_state
                .borrow_mut()
                .reduce(Intent::ClosePreview(owner.into()));
            self.reopen_workspace(workspace, window, cx);
            self.select_workspace_id(workspace, window, cx);
            self.close_file_viewer(workspace, window, cx);
            self.select_session_id(session, window, cx);
            let target_owner = self.find_owner(cx);
            self.find_state
                .borrow_mut()
                .reduce(Intent::ClosePreview(target_owner));
            self.refresh_find(window, cx);
            cx.notify();
        } else if self.live_chat_enabled(cx) && id.remote_id.is_none() {
            let indexed = entry(&self.find_state.borrow(), id).cloned();
            if let Some(workspace) = self.active_workspace(cx) {
                match self.open_saved_live_chat(workspace, &id.id, indexed, window, cx) {
                    Ok(()) => {
                        self.find_state
                            .borrow_mut()
                            .reduce(Intent::ClosePreview(owner.into()));
                    }
                    Err(error) => self.owner.update(cx, |owner, cx| {
                        owner.error = Some(format!("Resume chat: {error:#}"));
                        cx.notify();
                    }),
                }
            }
        } else {
            self.owner.update(cx, |owner, cx| {
                owner.error = Some("Conversation resume not available in preview".into());
                cx.notify();
            });
        }
    }

    pub(crate) fn find_preview_file_overlay(&self, cx: &gpui::App) -> bool {
        self.find_views
            .get(&self.find_owner(cx))
            .is_some_and(|views| views.preview.read(cx).file_overlay)
    }

    pub(crate) fn find_content(&self, cx: &gpui::Context<Self>) -> gpui::Stateful<gpui::Div> {
        let owner = self.find_owner(cx);
        let regions = &self.active(cx).store.regions;
        if regions.sidebar_mode == crate::SidebarMode::Find
            && !regions.sidebar_collapsed
            && self.content.read(cx).selected.is_none()
            && let Some(views) = self.find_views.get(&owner)
        {
            let selected = self.find_state.borrow().selected(&owner).is_some();
            return div()
                .id("find-centre")
                .debug_selector(move || {
                    if selected {
                        "find-centre".into()
                    } else {
                        "session-content".into()
                    }
                })
                .flex_1()
                .min_w_0()
                .h_full()
                .child(views.preview.clone());
        }
        self.file_viewer_content(cx)
    }
}
