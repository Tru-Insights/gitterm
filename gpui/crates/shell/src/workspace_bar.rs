//! Standalone Iced workspace bar. The caller projects its catalog and handles
//! outcomes; rendering never selects, persists, launches or closes anything.
//!
//! Hookup: replace the plain bottom bar with `render(&input, Colors::new(dark),
//! emit)`. Project the owner roster in catalog order, using
//! `work::catalog::is_work` for BarChip::work. Resolve outcome IDs against the
//! current owner; selection uses the existing selection reducer, NewWork opens
//! the named-work sheet, and OpenReposMenu toggles the host's Repos popup.
//! Native GPUI tooltips replace Iced's delayed overlay hints; their placement
//! and rasterization are framework-specific. No shell hookup is installed here.
use std::rc::Rc;

use gitterm_core::{park::WakeReason, settings::WorkspaceColor, workspace};
use gpui::{App, Div, SharedString, Stateful, Window, div, prelude::*, px, rgb};
use gpui_component::{
    button::{Button, ButtonVariants},
    popover::Popover,
    tooltip::Tooltip,
};

use crate::data::WorkspaceId;

/// A read-only projection of Iced's saved PR and parked facts. The catalog
/// retains only open workspaces, so closed entries come from its source.
#[derive(Clone, Default)]
pub(crate) struct SavedFacts {
    pub ready: usize,
    pub news_count: usize,
    pub parked: usize,
    pub attention_counts: std::collections::HashMap<WorkspaceId, usize>,
    pub news: std::collections::HashMap<WorkspaceId, News>,
    pub machines: Vec<News>,
    pub repo_machines: Vec<Machine>,
    pub remote_workspaces: Vec<String>,
    /// Iced's Ready list: each home with a next-to-merge PR or open news.
    pub ready_groups: Vec<crate::park_sheet::ReadyGroup>,
    /// Iced's Parked list entries read from `workspaces.json`, newest first.
    pub parked_rows: Vec<SavedParked>,
}
/// A closed, parked workspace of Iced's file, as the Parked list shows it.
#[derive(Clone, Debug)]
pub(crate) struct SavedParked {
    pub key: gitterm_core::park::Key,
    pub color: u32,
    pub chats: usize,
    pub info: Option<gitterm_core::park::Info>,
    pub news: Option<News>,
}
impl SavedFacts {
    pub(crate) fn read_from(
        source: &std::path::Path,
        workspaces: &[gitterm_core::workspace_config::WorkspaceConfig],
        roots: &std::collections::HashMap<WorkspaceId, Vec<String>>,
        colors: Colors,
        tasks: &crate::data::TaskSource,
    ) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        use gitterm_core::{chat_facts, chats::ChatBackend, merge_plan, prs};
        use std::collections::{HashMap, HashSet};
        let path = source.join("workspaces.json");
        let file: gitterm_core::workspace_config::WorkspacesFile = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("parse {}", path.display()))?;
        let watch_path = prs::WatchFile::path_in(source);
        let watch = prs::WatchFile::load(&watch_path);
        let plans_dir = merge_plan::plans_dir(source);
        let plans = merge_plan::load_plans(&plans_dir);
        let tasks = tasks.load().map_err(anyhow::Error::msg)?;
        let briefs: HashMap<_, _> = watch
            .prs
            .iter()
            .filter_map(|(id, pr)| Some((id.clone(), pr.brief()?)))
            .collect();
        let mut result = Self {
            parked: file.workspaces.iter().filter(|w| w.is_parked()).count(),
            ..Default::default()
        };
        let news_color = |kind: prs::NewsKind| match kind {
            prs::NewsKind::ChecksFailed | prs::NewsKind::Conflicting | prs::NewsKind::Closed => {
                colors.red
            }
            prs::NewsKind::RequiredChecksPassed | prs::NewsKind::Merged => colors.green,
            prs::NewsKind::NewActivity => colors.blue,
        };
        let mut parked_news: HashMap<(String, String), News> = HashMap::new();
        result.attention_counts = iced_tab_attention_counts(workspaces, &tasks);
        // Iced's machines: remote-agents.json, then workspaces.json's copies.
        result.repo_machines = crate::remote_client::read_remotes(source)?
            .iter()
            .map(|machine| Machine {
                key: format!("agent:{}", machine.id),
                name: machine.name.clone(),
                status: Some(News {
                    label: "not connected".into(),
                    color: colors.muted,
                }),
                connected: false,
                serves_phones: false,
            })
            .collect();
        result.machines = result
            .repo_machines
            .iter()
            .filter_map(|machine| {
                machine.status.as_ref().map(|status| News {
                    label: format!("{} {}", machine.name, status.label),
                    color: status.color,
                })
            })
            .collect();
        result.remote_workspaces = file
            .remote_sessions
            .iter()
            .filter(|session| {
                !file.workspaces.iter().any(|workspace| {
                    workspace
                        .remote_session
                        .as_ref()
                        .is_some_and(|open| open.session_name == session.session_name)
                })
            })
            .map(|session| format!("{}:{}", session.host_name, session.label))
            .collect();
        let homes = workspaces
            .iter()
            .enumerate()
            .map(|(i, w)| (Some(WorkspaceId(i as u64 + 1)), w))
            .chain(
                file.workspaces
                    .iter()
                    .filter(|w| w.is_parked())
                    .map(|w| (None, w)),
            );
        for (id, saved) in homes {
            if saved.is_legacy_remote()
                || matches!(
                    saved.location,
                    Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent { .. })
                )
            {
                continue;
            }
            let mut home_roots = saved.chats.clone();
            if let Some(extra) = id.and_then(|id| roots.get(&id)) {
                home_roots.extend(extra.iter().cloned());
            }
            home_roots.extend(saved.tabs.iter().filter_map(|t| t.chat_session_id.clone()));
            let mut seen_roots = HashSet::new();
            let mut seen_prs = HashSet::new();
            let mut newest = None;
            let mut next_rows = Vec::new();
            let mut home_news: Vec<(String, crate::park_sheet::ReadyNews)> = Vec::new();
            for root in home_roots
                .into_iter()
                .filter(|root| seen_roots.insert(root.clone()))
            {
                if let Some(plan) = plans.get(&root)
                    && let Some(next) =
                        merge_plan::next_to_merge(&merge_plan::order_plan(plan, &briefs))
                {
                    result.ready += 1;
                    let key = next.entry.key();
                    next_rows.push(crate::park_sheet::ReadyNext {
                        number: key.number,
                        repo: key.repo.clone(),
                        title: watch
                            .prs
                            .get(&key.id())
                            .and_then(|pr| pr.snapshot.core.as_ref())
                            .map(|core| core.title.clone())
                            .unwrap_or_default(),
                    });
                }
                let mut keys: Vec<_> = plans
                    .get(&root)
                    .into_iter()
                    .flat_map(|plan| plan.entries.iter().map(|entry| entry.key()))
                    .collect();
                let mut pending = vec![root];
                let mut seen_chats = HashSet::new();
                while let Some(chat) = pending.pop() {
                    if !seen_chats.insert(chat.clone()) {
                        continue;
                    }
                    for backend in ChatBackend::ALL {
                        let path =
                            chat_facts::facts_path(&chat_facts::facts_dir(source), backend, &chat)
                                .map_err(anyhow::Error::msg)?;
                        if let Some(facts) = chat_facts::load_facts(&path)
                            .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?
                        {
                            pending
                                .extend(facts.children.into_iter().map(|child| child.session_id));
                            keys.extend(facts.prs.iter().map(|pr| pr.key()));
                        }
                    }
                }
                for key in keys.into_iter().filter(|key| seen_prs.insert(key.id())) {
                    if let Some(pr) = watch.prs.get(&key.id()) {
                        for news in pr.open_news() {
                            result.news_count += 1;
                            home_news.push((
                                news.at.clone(),
                                crate::park_sheet::ReadyNews {
                                    number: pr.key.number,
                                    label: pr.key.label(),
                                    text: news.text.clone(),
                                    color: news_color(news.kind),
                                },
                            ));
                            if newest
                                .as_ref()
                                .is_none_or(|(_, old): &(_, &prs::NewsItem)| news.at > old.at)
                            {
                                newest = Some((pr.key.number, news));
                            }
                        }
                    }
                }
            }
            // Newest first, as Iced's `home_news`.
            home_news.sort_by(|a, b| b.0.cmp(&a.0));
            let home_news: Vec<_> = home_news.into_iter().map(|(_, news)| news).collect();
            if id.is_none()
                && let Some(first) = home_news.first()
            {
                let mut label = format!("{} {}", first.label, first.text);
                if home_news.len() > 1 {
                    label.push_str(&format!(" \u{00B7} +{} more", home_news.len() - 1));
                }
                parked_news.insert(
                    (saved.name.clone(), saved.dir.clone()),
                    News {
                        label,
                        color: first.color,
                    },
                );
            }
            if !next_rows.is_empty() || !home_news.is_empty() {
                result.ready_groups.push(crate::park_sheet::ReadyGroup {
                    name: saved.name.clone(),
                    home: match id {
                        Some(id) => crate::park_sheet::ReadyHome::Open(id),
                        None => crate::park_sheet::ReadyHome::Parked(gitterm_core::park::Key {
                            name: saved.name.clone(),
                            dir: saved.dir.clone(),
                        }),
                    },
                    next: next_rows,
                    news: home_news,
                });
            }
            if let (Some(id), Some((number, news))) = (id, newest) {
                result.news.insert(
                    id,
                    News {
                        label: format!("#{number} {}", news.text),
                        color: news_color(news.kind),
                    },
                );
            }
        }
        // Iced's `parked_order`: most recently parked first; stamp-less entries
        // keep file order after them.
        let mut parked: Vec<_> = file
            .workspaces
            .iter()
            .filter(|w| w.is_parked())
            .map(|w| {
                let at = w
                    .parked
                    .as_ref()
                    .and_then(|p| gitterm_core::work::summary::parse_stamp(&p.parked_at));
                (at, w)
            })
            .collect();
        parked.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        result.parked_rows = parked
            .into_iter()
            .map(|(_, w)| SavedParked {
                key: gitterm_core::park::Key {
                    name: w.name.clone(),
                    dir: w.dir.clone(),
                },
                color: colors.workspace(w.color),
                chats: w.chats.len(),
                info: w
                    .parked
                    .as_ref()
                    .map(|p| gitterm_core::park::Info::new(&p.note, p.parked_at.clone())),
                news: parked_news.remove(&(w.name.clone(), w.dir.clone())),
            })
            .collect();
        Ok(result)
    }
}

/// Iced's workspace chip counts tabs whose live `TabState.attention` is set.
/// GPUI projects the durable equivalent per configured tab: task attention and
/// completed/failed/blocked workers both belong to that tab, so they count once.
fn iced_tab_attention_counts(
    workspaces: &[gitterm_core::workspace_config::WorkspaceConfig],
    tasks: &gitterm_core::tasks::TaskStore,
) -> std::collections::HashMap<WorkspaceId, usize> {
    use gitterm_core::tasks::{DelegationStatus, TaskAttentionReason};
    let mut counts = std::collections::HashMap::new();
    for (index, workspace) in workspaces.iter().enumerate() {
        if !workspace.active {
            continue;
        }
        let count = workspace
            .tabs
            .iter()
            .filter(|tab| {
                let task_attention = tab
                    .task_id
                    .as_deref()
                    .and_then(|id| tasks.get(id))
                    .is_some_and(|task| {
                        task.attention.reason != Some(TaskAttentionReason::Interrupted)
                            && (task.attention.unread || task.attention.reason.is_some())
                    });
                let worker_attention = tab.session_uid.as_deref().is_some_and(|uid| {
                    tasks.delegations().iter().any(|worker| {
                        worker.dismissed_at.is_none()
                            && worker.parent.session_uid == uid
                            && matches!(
                                worker.status,
                                DelegationStatus::Completed
                                    | DelegationStatus::Failed { .. }
                                    | DelegationStatus::Blocked
                            )
                    })
                });
                task_attention || worker_attention
            })
            .count();
        if count > 0 {
            counts.insert(WorkspaceId(index as u64 + 1), count);
        }
    }
    counts
}

#[derive(Default)]
pub(crate) struct SavedFactsCache(SavedFacts);
impl SavedFactsCache {
    pub fn snapshot(&self) -> SavedFacts {
        self.0.clone()
    }

    pub fn replace(&mut self, facts: SavedFacts) {
        self.0 = facts;
    }
}

#[cfg(test)]
mod saved_facts_cache_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn render_snapshots_do_not_read_and_refresh_replaces_counts() {
        let reads = Cell::new(0);
        let mut cache = SavedFactsCache::default();
        let read = || {
            reads.set(reads.get() + 1);
            SavedFacts {
                ready: reads.get(),
                parked: reads.get() * 2,
                ..Default::default()
            }
        };

        cache.replace(read());
        let first_render = cache.snapshot();
        let second_render = cache.snapshot();
        assert_eq!(reads.get(), 1);
        assert_eq!(first_render.ready, 1);
        assert_eq!(second_render.parked, 2);

        cache.replace(read());
        let refreshed_render = cache.snapshot();
        assert_eq!(reads.get(), 2);
        assert_eq!(refreshed_render.ready, 2);
        assert_eq!(refreshed_render.parked, 4);
    }
}

#[cfg(test)]
mod attention_count_tests {
    use super::*;

    #[test]
    fn counts_attention_once_per_open_tab_and_excludes_seen_interrupted_and_dismissed() {
        use gitterm_core::tasks::{
            Delegation, DelegationChild, DelegationKind, DelegationParent, DelegationStatus,
            TaskStore, TaskStoreDocument,
        };
        let dir = tempfile::tempdir().unwrap();
        let task_path = dir.path().join("tasks.json");
        let task = |id: &str, reason: Option<&str>, unread: bool| {
            let fixture: serde_json::Value = serde_json::from_str(include_str!(
                "../../../../tests/fixtures/stage45/tasks/old.tasks.json"
            ))
            .unwrap();
            let mut record: gitterm_core::tasks::TaskRecord =
                serde_json::from_value(fixture["tasks"][0].clone()).unwrap();
            record.task_id = id.into();
            record.attention.reason = reason.map(|reason| match reason {
                "completed_unread" => gitterm_core::tasks::TaskAttentionReason::CompletedUnread,
                "interrupted" => gitterm_core::tasks::TaskAttentionReason::Interrupted,
                "requires_input" => gitterm_core::tasks::TaskAttentionReason::RequiresInput,
                "ready_for_review" => gitterm_core::tasks::TaskAttentionReason::ReadyForReview,
                "failed" => gitterm_core::tasks::TaskAttentionReason::ExecutionFailed,
                _ => unreachable!(),
            });
            record.attention.unread = unread;
            record
        };
        let worker = |id: &str, uid: &str, status: DelegationStatus, dismissed_at: Option<&str>| {
            Delegation {
                delegation_id: id.into(),
                kind: DelegationKind::Implement,
                parent: DelegationParent {
                    session_uid: uid.into(),
                    chat_session_id: None,
                    workspace: "fixture".into(),
                    cwd: "/fixture".into(),
                },
                child: DelegationChild::TaskSession {
                    task_id: "needs_input".into(),
                    task_session_id: None,
                    conversation: None,
                    preset_name: None,
                    model: None,
                    role: None,
                },
                brief: "fixture".into(),
                target: None,
                previous: None,
                superseded_by: None,
                result: matches!(&status, DelegationStatus::Completed).then(|| {
                    gitterm_core::tasks::DelegationResult {
                        handoff: Some(gitterm_core::tasks::TaskHandoff {
                            summary: "done".into(),
                            decisions: vec![],
                            next_steps: vec![],
                            blockers: vec![],
                            updated_by_session_id: None,
                            updated_at: "2026-01-01T00:00:00Z".into(),
                        }),
                        ..Default::default()
                    }
                }),
                status,
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                delivered_at: None,
                delivered: false,
                dismissed_at: dismissed_at.map(str::to_owned),
                options: Default::default(),
                time_limit_reached_at: None,
                time_limit_deadline_unix_ms: None,
            }
        };
        let document = TaskStoreDocument {
            tasks: vec![
                task("unread", Some("completed_unread"), true),
                task("seen", None, false),
                task("restart", Some("interrupted"), true),
                task("needs_input", Some("requires_input"), false),
                task("ready_for_review", Some("ready_for_review"), false),
                task("failed", Some("failed"), false),
            ],
            delegations: vec![
                worker("visible", "parent-a", DelegationStatus::Completed, None),
                worker(
                    "dismissed",
                    "parent-b",
                    DelegationStatus::Failed {
                        message: "failed".into(),
                    },
                    Some("now"),
                ),
                worker(
                    "interrupted",
                    "parent-c",
                    DelegationStatus::Interrupted,
                    None,
                ),
                worker("completed", "parent-d", DelegationStatus::Completed, None),
            ],
            ..Default::default()
        };
        std::fs::write(&task_path, serde_json::to_vec(&document).unwrap()).unwrap();
        let store = TaskStore::load(&task_path).unwrap();
        let workspace: gitterm_core::workspace_config::WorkspaceConfig =
            serde_json::from_value(serde_json::json!({
                "name":"fixture", "abbrev":"f", "dir":"/fixture", "color":"lavender", "active":true,
                "tabs":[
                    {"dir":"/fixture/a", "task_id":"unread"},
                    {"dir":"/fixture/b", "task_id":"unread"},
                    {"dir":"/fixture/c", "task_id":"seen"},
                    {"dir":"/fixture/d", "task_id":"restart"},
                    {"dir":"/fixture/e", "task_id":"needs_input", "session_uid":"parent-a"},
                    {"dir":"/fixture/f", "session_uid":"parent-b"},
                    {"dir":"/fixture/g", "task_id":"ready_for_review"},
                    {"dir":"/fixture/h", "task_id":"failed"},
                    {"dir":"/fixture/i", "session_uid":"parent-c"},
                    {"dir":"/fixture/j", "session_uid":"parent-d"}
                ]
            }))
            .unwrap();
        let counts = iced_tab_attention_counts(&[workspace], &store);
        assert_eq!(counts.get(&WorkspaceId(1)), Some(&6));
    }
}

pub type Handler<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;

/// Resolved Iced colours, also used by the adjacent strip and spine.
#[derive(Clone, Copy, Debug)]
pub struct Colors {
    pub dark: bool,
    pub base: u32,
    pub crust: u32,
    pub surface0: u32,
    pub surface2: u32,
    pub text: u32,
    pub subtext0: u32,
    pub muted: u32,
    pub overlay0: u32,
    pub peach: u32,
    pub yellow: u32,
    pub red: u32,
    pub green: u32,
    pub blue: u32,
}
impl Colors {
    pub fn new(dark: bool) -> Self {
        let p = crate::theme::Palette::for_mode(dark);
        Self {
            dark,
            base: p.base,
            crust: p.crust,
            surface0: p.surface,
            surface2: p.surface2,
            text: p.text,
            subtext0: p.subtext0,
            muted: p.muted,
            overlay0: p.overlay0,
            peach: p.peach,
            yellow: p.yellow,
            red: p.red,
            green: p.green,
            blue: p.blue,
        }
    }
    pub fn workspace(self, color: WorkspaceColor) -> u32 {
        let p = crate::theme::Palette::for_mode(self.dark);
        match color {
            WorkspaceColor::Lavender => p.lavender,
            WorkspaceColor::Blue => p.blue,
            WorkspaceColor::Green => p.green,
            WorkspaceColor::Peach => p.peach,
            WorkspaceColor::Pink => p.pink,
            WorkspaceColor::Yellow => p.yellow,
            WorkspaceColor::Red => p.red,
            WorkspaceColor::Teal => p.teal,
        }
    }
}

/// Presentation facts for attention still owned by the Iced/root host.
/// The reason is supplied verbatim (including review/consult/worker wording).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attention {
    pub count: usize,
    pub kind: AttentionKind,
    pub reason: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionKind {
    NeedsYou,
    Failed,
    Review,
}
impl AttentionKind {
    pub fn color(self, colors: Colors) -> u32 {
        match self {
            Self::NeedsYou => colors.peach,
            Self::Failed => colors.red,
            Self::Review => colors.green,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct News {
    pub label: String,
    pub color: u32,
}
#[derive(Clone, Debug)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub color: WorkspaceColor,
    /// Use core work::catalog::is_work for `work`, and the host's machine key.
    pub placement: workspace::BarChip,
    pub attention: Option<Attention>,
    pub woke: Option<WakeReason>,
    pub pr_news: Option<News>,
}
impl Workspace {
    pub fn label(&self, active: bool) -> String {
        if active {
            self.name.clone()
        } else {
            workspace::short_label(&self.name)
        }
    }
    pub fn attention(&self) -> Option<&Attention> {
        self.attention.as_ref().filter(|a| a.count > 0)
    }
    pub fn wake_label(&self, active: bool) -> Option<&'static str> {
        self.woke.filter(|_| !active).map(|reason| match reason {
            WakeReason::NeedsYou => "needs you",
            WakeReason::Finished => "finished",
            WakeReason::Failed => "failed",
        })
    }
    pub fn dot_color(&self, active: bool, pulse_bright: bool, c: Colors) -> u32 {
        match self.attention() {
            Some(a) if a.kind != AttentionKind::NeedsYou => a.kind.color(c),
            Some(_) if pulse_bright => c.peach,
            Some(_) => c.yellow,
            None if active => c.workspace(self.color),
            None => c.surface2,
        }
    }
    pub fn hint(&self) -> String {
        let mut parts = vec![self.name.clone()];
        if let Some(a) = self.attention() {
            parts.push(format!("{} · {}", a.count, a.reason));
        }
        if let Some(news) = &self.pr_news {
            parts.push(news.label.clone());
        }
        parts.join(" · ")
    }
}

#[derive(Clone, Debug)]
pub struct Input {
    /// Catalog order, never display-sorted by the host.
    pub workspaces: Vec<Workspace>,
    pub active: Option<WorkspaceId>,
    pub repos_on_bar: bool,
    pub repos_open: bool,
    pub machine_news: Vec<News>,
    pub repo_machines: Vec<Machine>,
    pub remote_workspaces: Vec<String>,
    pub pulse_bright: bool,
    pub ready_count: usize,
    pub ready_news_count: usize,
    pub parked_count: usize,
    pub terminal_expanded: bool,
    pub terminal_running: bool,
    pub browser: Browser,
    pub more_open: bool,
}
#[derive(Clone, Debug)]
pub struct Browser {
    pub color: u32,
    pub driving: bool,
    pub hint: String,
    /// None while busy or unavailable; otherwise Open or Focus, per host status.
    pub action: Option<BrowserAction>,
    pub has_evidence: bool,
}
#[derive(Clone, Debug)]
pub struct Machine {
    pub key: String,
    pub name: String,
    pub status: Option<News>,
    pub connected: bool,
    pub serves_phones: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserAction {
    Open,
    Focus,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Menu {
    Repos,
    More,
}
#[derive(Clone, Debug)]
pub struct MenuChanged {
    pub menu: Menu,
    pub open: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    SelectWorkspace(WorkspaceId),
    OpenSettings(WorkspaceId),
    OpenReposMenu,
    NewWork,
    OpenReady,
    OpenParked,
    ToggleTerminal,
    Browser(BrowserAction),
    OpenMoreMenu,
    /// A machine header's "+": the folder picker on that machine (key).
    AddWorkspace(String),
    /// "Open a repo…": the folder picker on the active machine.
    OpenFolder,
    /// "Show {machine} chats" (machine key): Find, that machine only.
    ShowMachineChats(String),
    /// "Pair a phone…" (machine key): the phone pairing panel.
    PairPhone(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Workspace(usize),
    Repos,
    NewWork,
}
impl Input {
    pub fn layout(&self) -> workspace::BarLayout {
        let chips: Vec<_> = self
            .workspaces
            .iter()
            .map(|w| w.placement.clone())
            .collect();
        let active = self
            .workspaces
            .iter()
            .position(|w| Some(w.id) == self.active)
            .unwrap_or(usize::MAX);
        workspace::bar_layout(&chips, active, self.repos_on_bar)
    }
    pub fn items(&self) -> Vec<Item> {
        let layout = self.layout();
        layout
            .work
            .into_iter()
            .map(Item::Workspace)
            .chain([Item::Repos])
            .chain(layout.after.into_iter().map(Item::Workspace))
            .chain([Item::NewWork])
            .collect()
    }
    pub fn hidden_news(&self) -> bool {
        self.layout().hidden.into_iter().any(|i| {
            let w = &self.workspaces[i];
            w.attention().is_some() || w.pr_news.is_some()
        })
    }
    pub fn repos_hint(&self) -> String {
        let mut parts = vec!["Repos and machines".to_string()];
        parts.extend(self.machines().iter().skip(1).map(|machine| {
            format!(
                "{} {}",
                machine.name,
                machine
                    .status
                    .as_ref()
                    .map_or("remote session", |status| status.label.as_str())
            )
        }));
        if self.hidden_news() {
            parts.push("news inside".into());
        }
        for news in &self.machine_news {
            if !parts.contains(&news.label) {
                parts.push(news.label.clone());
            }
        }
        parts.join(" · ")
    }
    pub fn repos_label(&self) -> &'static str {
        if self.repos_open {
            "Repos ▴"
        } else {
            "Repos ▾"
        }
    }
    pub fn more_entries(&self) -> Vec<&'static str> {
        let mut entries = vec!["Keyboard shortcuts"];
        match self.browser.action {
            Some(BrowserAction::Open) => entries.push("Open browser"),
            Some(BrowserAction::Focus) => entries.extend(["Focus browser", "Disconnect browser"]),
            None => {}
        }
        if self.browser.has_evidence {
            entries.push("Browser evidence");
        }
        entries
    }
    fn machines(&self) -> Vec<Machine> {
        let mut machines = vec![Machine {
            key: workspace::LOCAL_GROUP.into(),
            name: "this Mac".into(),
            status: None,
            connected: true,
            serves_phones: false,
        }];
        for key in self
            .layout()
            .sections
            .into_iter()
            .map(|s| s.key)
            .chain(self.repo_machines.iter().map(|m| m.key.clone()))
        {
            if machines.iter().any(|m| m.key == key) {
                continue;
            }
            machines.push(
                self.repo_machines
                    .iter()
                    .find(|m| m.key == key)
                    .cloned()
                    .unwrap_or_else(|| Machine {
                        name: key
                            .split_once(':')
                            .map_or(key.as_str(), |(_, name)| name)
                            .into(),
                        status: key.starts_with("agent:").then(|| News {
                            label: "not connected".into(),
                            color: self.browser.color,
                        }),
                        key,
                        connected: false,
                        serves_phones: false,
                    }),
            );
        }
        machines
    }
    /// The outcome a repo-menu action emits; None is a preview notice.
    fn repo_action_outcome(machine: &Machine, label: &str) -> Option<Outcome> {
        if label == "New workspace…" && machine.key == workspace::LOCAL_GROUP {
            // The bar `+` opens the same sheet.
            Some(Outcome::NewWork)
        } else if label == "Open a repo…" {
            Some(Outcome::OpenFolder)
        } else if label == format!("Show {} chats", machine.name) {
            Some(Outcome::ShowMachineChats(machine.key.clone()))
        } else if label == "Pair a phone…" {
            Some(Outcome::PairPhone(machine.key.clone()))
        } else {
            None
        }
    }
    fn repo_actions(&self, machine: &Machine, has_workspaces: bool) -> Vec<String> {
        let mut entries = vec![];
        if machine.key == workspace::LOCAL_GROUP {
            entries.push("New workspace…".into());
            if let Some(workspace) = self.workspaces.iter().find(|workspace| {
                Some(workspace.id) == self.active
                    && workspace.placement.group == workspace::LOCAL_GROUP
            }) {
                entries.push(format!("Add a repo to {}…", workspace.name));
            }
            entries.push("Open a repo…".into());
            if has_workspaces {
                entries.push(
                    if self.repos_on_bar {
                        "Keep repos in this menu only"
                    } else {
                        "Show repos on the bar too"
                    }
                    .into(),
                );
            }
        }
        entries.push(format!("Show {} chats", machine.name));
        if machine.key.starts_with("agent:") {
            if !machine.connected {
                entries.push("Reconnect…".into());
            }
            if machine.serves_phones {
                entries.push("Pair a phone…".into());
            }
        }
        entries
    }
    pub fn terminal_color(&self, c: Colors) -> u32 {
        if self.terminal_expanded {
            c.blue
        } else if self.terminal_running {
            c.green
        } else {
            c.muted
        }
    }
}

pub(crate) fn dot(color: u32, width: f32, height: f32) -> Div {
    div()
        .flex_none()
        .w(px(width))
        .h(px(height))
        .rounded(px(3.))
        .bg(rgb(color))
}
pub(crate) fn control<T: Clone + 'static>(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    color: u32,
    colors: Colors,
    outcome: T,
    emit: Handler<T>,
) -> Stateful<Div> {
    let hint = hint.into();
    div()
        .id(id.into())
        .role(gpui::Role::Button)
        .aria_label(hint.clone())
        .font_family(crate::theme::LABEL_FONT)
        .text_size(px(11.))
        .text_color(rgb(color))
        .px(px(8.))
        .py(px(4.))
        .rounded(px(4.))
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(colors.surface0)).text_color(rgb(colors.text)))
        .tooltip(move |window, cx| {
            Tooltip::new(hint.clone())
                .font_family(crate::theme::LABEL_FONT)
                .build(window, cx)
        })
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            emit(outcome.clone(), window, cx);
        })
        .child(label.into())
}

/// A menu entry that closes the popover, then emits `outcome`.
fn menu_control(
    id: String,
    label: String,
    outcome: Outcome,
    colors: Colors,
    emit: Handler<Outcome>,
    cx: &mut gpui::Context<gpui_component::popover::PopoverState>,
) -> Stateful<Div> {
    let weak = cx.entity().downgrade();
    let selector = id.clone();
    control(
        id,
        label.clone(),
        label,
        colors.muted,
        colors,
        outcome,
        Rc::new(move |outcome, window, cx| {
            let _ = weak.update(cx, |state, cx| state.dismiss(window, cx));
            emit(outcome, window, cx);
        }),
    )
    .debug_selector(move || selector.clone())
}

fn preview_control(
    id: String,
    label: String,
    colors: Colors,
    cx: &mut gpui::Context<gpui_component::popover::PopoverState>,
) -> Stateful<Div> {
    let weak = cx.entity().downgrade();
    let selector = id.clone();
    control(
        id,
        label.clone(),
        label.clone(),
        colors.muted,
        colors,
        (),
        Rc::new(move |_, window, cx| {
            let _ = weak.update(cx, |state, cx| state.dismiss(window, cx));
            window.dispatch_action(
                Box::new(crate::app_menus::PreviewUnavailable {
                    label: label.clone(),
                }),
                cx,
            );
        }),
    )
    .debug_selector(move || selector.clone())
}

/// 28px including its top separator; fixed controls stay outside the scroller.
pub fn render(
    input: &Input,
    colors: Colors,
    emit: Handler<Outcome>,
    menus: Handler<MenuChanged>,
) -> gpui::Stateful<gpui::Div> {
    let mut chips = div()
        .id("pstrip-workspace-scroll")
        .debug_selector(|| "pstrip-workspace-scroll".into())
        .flex()
        .items_center()
        .flex_1()
        .min_w_0()
        .overflow_x_scroll()
        .px(px(4.));
    for (position, item) in input.items().into_iter().enumerate() {
        if position > 0 && item != Item::NewWork {
            chips = chips.child(
                div()
                    .flex_none()
                    .w(px(1.))
                    .h(px(14.))
                    .bg(rgb(colors.surface0)),
            );
        }
        match item {
            Item::Workspace(index) => {
                let w = &input.workspaces[index];
                let active = input.active == Some(w.id);
                let id = w.id;
                let on_select = emit.clone();
                let hint = w.hint();
                let mut chip = div()
                    .id(SharedString::from(format!("pstrip-workspace-{}", id.0)))
                    .debug_selector(move || format!("pstrip-workspace-{}", id.0))
                    .role(gpui::Role::Tab)
                    .aria_label(w.name.clone())
                    .aria_selected(active)
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(6.))
                    .px(px(12.))
                    .h(px(27.))
                    .border_t_2()
                    .border_color(if active {
                        rgb(colors.workspace(w.color)).into()
                    } else {
                        gpui::transparent_black()
                    })
                    .when(active, |s| s.bg(rgb(colors.base)))
                    .when(!active, |s| s.hover(move |s| s.bg(rgb(colors.surface0))))
                    .cursor_pointer()
                    .tooltip(move |window, cx| {
                        Tooltip::new(hint.clone())
                            .font_family(crate::theme::LABEL_FONT)
                            .build(window, cx)
                    })
                    .on_click(move |_, window, cx| {
                        on_select(Outcome::SelectWorkspace(id), window, cx)
                    })
                    .child(dot(w.dot_color(active, input.pulse_bright, colors), 6., 6.))
                    .child(
                        div()
                            .text_color(rgb(if active {
                                colors.workspace(w.color)
                            } else {
                                colors.subtext0
                            }))
                            .child(w.label(active)),
                    );
                if active {
                    chip = chip.child(
                        control(
                            format!("pstrip-settings-{}", id.0),
                            "⚙",
                            "Workspace settings",
                            colors.muted,
                            colors,
                            Outcome::OpenSettings(id),
                            emit.clone(),
                        )
                        .text_size(px(10.))
                        .px(px(4.))
                        .py_0()
                        .debug_selector(|| "pstrip-settings".into()),
                    );
                }
                if let Some(a) = w.attention() {
                    chip = chip.child(
                        div()
                            .text_size(px(9.))
                            .text_color(rgb(colors.crust))
                            .bg(rgb(a.kind.color(colors)))
                            .rounded(px(6.))
                            .px(px(4.))
                            .child(a.count.to_string())
                            .debug_selector(move || format!("pstrip-attention-{}", id.0)),
                    );
                }
                if let Some(label) = w.wake_label(active) {
                    let color = match w.woke {
                        Some(WakeReason::Failed) => colors.red,
                        Some(WakeReason::Finished) => colors.green,
                        _ => colors.peach,
                    };
                    chip = chip.child(
                        div()
                            .text_size(px(9.))
                            .text_color(rgb(color))
                            .child(label)
                            .debug_selector(move || format!("pstrip-wake-{}", id.0)),
                    );
                }
                if let Some(news) = &w.pr_news {
                    chip = chip.child(
                        dot(news.color, 6., 6.)
                            .debug_selector(move || format!("pstrip-pr-news-{}", id.0)),
                    );
                }
                chips = chips.child(chip);
            }
            Item::Repos => {
                let mut repos = Button::new("pstrip-repos")
                    .ghost()
                    .label(input.repos_label())
                    .tooltip(input.repos_hint())
                    .font_family(crate::theme::LABEL_FONT)
                    .text_size(px(11.))
                    .text_color(rgb(if input.repos_open {
                        colors.text
                    } else {
                        colors.muted
                    }))
                    .when(input.repos_open, |s| s.bg(rgb(colors.surface0)))
                    .h(px(27.))
                    .px(px(12.))
                    .flex_none()
                    .debug_selector(|| "pstrip-repos".into());
                if input.hidden_news() {
                    repos = repos.child(
                        dot(colors.peach, 6., 6.).debug_selector(|| "pstrip-repos-news".into()),
                    );
                }
                for (index, machine) in input.machines().iter().skip(1).enumerate() {
                    let color = machine
                        .status
                        .as_ref()
                        .map_or(colors.overlay0, |status| status.color);
                    repos = repos.child(
                        dot(color, 5., 5.)
                            .debug_selector(move || format!("pstrip-machine-dot-{index}")),
                    );
                }
                for news in &input.machine_news {
                    if !input.machines().iter().any(|machine| {
                        machine.status.as_ref().is_some_and(|status| {
                            news.label == format!("{} {}", machine.name, status.label)
                        })
                    }) {
                        repos = repos.child(dot(news.color, 5., 5.));
                    }
                }
                let menu_input = input.clone();
                let menu_emit = emit.clone();
                let on_change = menus.clone();
                chips = chips.child(
                    Popover::new("pstrip-repos-menu")
                        .anchor(gpui::Anchor::BottomLeft)
                        .open(input.repos_open)
                        .on_open_change(move |open, window, cx| {
                            on_change(
                                MenuChanged {
                                    menu: Menu::Repos,
                                    open: *open,
                                },
                                window,
                                cx,
                            )
                        })
                        .trigger(repos)
                        .content(move |_, _, cx| {
                            let mut menu = div()
                                .id("pstrip-repos-list")
                                .debug_selector(|| "pstrip-repos-list".into())
                                .flex()
                                .flex_col()
                                .w(px(280.))
                                .max_h(px(420.))
                                .overflow_y_scroll()
                                .font_family(crate::theme::LABEL_FONT)
                                .text_size(px(11.));
                            for (position, machine) in menu_input.machines().iter().enumerate() {
                                let mut header = div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .px(px(8.))
                                    .py(px(4.))
                                    .child(machine.name.to_uppercase());
                                if let Some(status) = &machine.status {
                                    header = header.child(dot(status.color, 6., 6.)).child(
                                        div()
                                            .text_color(rgb(colors.muted))
                                            .child(status.label.clone()),
                                    );
                                }
                                header = header.child(div().flex_1());
                                if machine.key == workspace::LOCAL_GROUP
                                    || machine.key.starts_with("agent:")
                                {
                                    header = header.child(menu_control(
                                        format!("pstrip-machine-add-{position}"),
                                        "+".into(),
                                        Outcome::AddWorkspace(machine.key.clone()),
                                        colors,
                                        menu_emit.clone(),
                                        cx,
                                    ));
                                }
                                menu = menu.child(header);
                                let section = menu_input
                                    .layout()
                                    .sections
                                    .into_iter()
                                    .find(|s| s.key == machine.key);
                                let indices = section.map(|s| s.workspaces).unwrap_or_default();
                                for &index in &indices {
                                    let workspace = &menu_input.workspaces[index];
                                    let id = workspace.id;
                                    let weak = cx.entity().downgrade();
                                    let emit = menu_emit.clone();
                                    menu = menu.child(
                                        control(
                                            format!("pstrip-repo-{}", id.0),
                                            "",
                                            workspace.hint(),
                                            colors.text,
                                            colors,
                                            Outcome::SelectWorkspace(id),
                                            Rc::new(move |outcome, window, cx| {
                                                let _ = weak.update(cx, |state, cx| {
                                                    state.dismiss(window, cx)
                                                });
                                                emit(outcome, window, cx);
                                            }),
                                        )
                                        .w_full()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(8.))
                                                .child(dot(
                                                    if workspace.attention().is_some() {
                                                        colors.peach
                                                    } else if menu_input.active == Some(id) {
                                                        colors.workspace(workspace.color)
                                                    } else {
                                                        colors.surface2
                                                    },
                                                    6.,
                                                    6.,
                                                ))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .text_color(rgb(
                                                            if menu_input.active == Some(id) {
                                                                colors.workspace(workspace.color)
                                                            } else {
                                                                colors.text
                                                            },
                                                        ))
                                                        .child(workspace.name.clone()),
                                                )
                                                .children(workspace.attention().map(|attention| {
                                                    div().child(attention.count.to_string())
                                                })),
                                        )
                                        .debug_selector(move || format!("pstrip-repo-{}", id.0)),
                                    );
                                }
                                for (index, label) in menu_input
                                    .repo_actions(machine, !indices.is_empty())
                                    .into_iter()
                                    .enumerate()
                                {
                                    let id = format!("pstrip-repo-action-{position}-{index}");
                                    menu = menu.child(
                                        match Input::repo_action_outcome(machine, &label) {
                                            Some(outcome) => menu_control(
                                                id,
                                                label,
                                                outcome,
                                                colors,
                                                menu_emit.clone(),
                                                cx,
                                            ),
                                            None => preview_control(id, label, colors, cx),
                                        },
                                    );
                                }
                            }
                            if !menu_input.remote_workspaces.is_empty() {
                                menu = menu
                                    .child(div().px(px(8.)).py(px(4.)).child("REMOTE SESSIONS"));
                                for (index, name) in menu_input.remote_workspaces.iter().enumerate()
                                {
                                    menu = menu.child(preview_control(
                                        format!("pstrip-remote-workspace-{index}"),
                                        format!("+ {name}"),
                                        colors,
                                        cx,
                                    ));
                                }
                            }
                            menu
                        }),
                );
            }
            Item::NewWork => {
                chips = chips.child(
                    control(
                        "pstrip-new-work",
                        "+",
                        "New workspace",
                        colors.muted,
                        colors,
                        Outcome::NewWork,
                        emit.clone(),
                    )
                    .flex_none()
                    .debug_selector(|| "pstrip-new-work".into()),
                )
            }
        }
    }
    let mut right = div().flex().flex_none().items_center();
    if input.ready_count > 0 || input.ready_news_count > 0 {
        right = right.child(
            control(
                "pstrip-ready",
                format!("↑{}", input.ready_count),
                if input.ready_news_count > 0 {
                    format!(
                        "{} to merge · {} new",
                        input.ready_count, input.ready_news_count
                    )
                } else {
                    format!("{} to merge", input.ready_count)
                },
                if input.ready_news_count > 0 {
                    colors.blue
                } else {
                    colors.muted
                },
                colors,
                Outcome::OpenReady,
                emit.clone(),
            )
            .debug_selector(|| "pstrip-ready".into()),
        );
    }
    if input.parked_count > 0 {
        right = right.child(
            control(
                "pstrip-parked",
                format!("⏸{}", input.parked_count),
                format!("{} parked", input.parked_count),
                colors.muted,
                colors,
                Outcome::OpenParked,
                emit.clone(),
            )
            .debug_selector(|| "pstrip-parked".into()),
        );
    }
    right = right
        .child(div().w(px(1.)).h(px(16.)).bg(rgb(colors.surface0)))
        .child(
            control(
                "pstrip-terminal",
                ">_",
                match (input.terminal_expanded, input.terminal_running) {
                    (true, _) => "Fold the terminal panel (⌘J)",
                    (false, true) => "Terminal panel, a command is running (⌘J)",
                    (false, false) => "Terminal panel (⌘J)",
                },
                input.terminal_color(colors),
                colors,
                Outcome::ToggleTerminal,
                emit.clone(),
            )
            .debug_selector(|| "pstrip-terminal".into()),
        );
    let browser = div()
        .w(px(13.))
        .h(px(10.))
        .border_1()
        .border_color(rgb(input.browser.color))
        .rounded(px(2.))
        .when(input.browser.driving, |s| {
            s.bg(gpui::rgba((input.browser.color << 8) | 0x59))
        })
        .child(div().h(px(2.)).w_full().bg(rgb(input.browser.color)));
    let browser_hint = input.browser.hint.clone();
    let mut browser_button = div()
        .id("pstrip-browser")
        .role(gpui::Role::Button)
        .aria_label(browser_hint.clone())
        .debug_selector(|| "pstrip-browser".into())
        .px(px(7.))
        .py(px(7.))
        .rounded(px(4.))
        .hover(move |s| s.bg(rgb(colors.surface0)))
        .tooltip(move |window, cx| Tooltip::new(browser_hint.clone()).build(window, cx))
        .child(browser);
    if let Some(action) = input.browser.action {
        let emit = emit.clone();
        browser_button = browser_button
            .cursor_pointer()
            .on_click(move |_, window, cx| emit(Outcome::Browser(action), window, cx));
    }
    let more_input = input.clone();
    let more = Button::new("pstrip-more")
        .ghost()
        .label("⋯")
        .tooltip("Help and browser actions")
        .text_color(rgb(if input.more_open {
            colors.text
        } else {
            colors.muted
        }))
        .h(px(27.))
        .px(px(8.))
        .debug_selector(|| "pstrip-more".into());
    right = right
        .child(browser_button)
        .child(
            Popover::new("pstrip-more-menu")
                .anchor(gpui::Anchor::BottomRight)
                .open(input.more_open)
                .on_open_change(move |open, window, cx| {
                    menus(
                        MenuChanged {
                            menu: Menu::More,
                            open: *open,
                        },
                        window,
                        cx,
                    )
                })
                .trigger(more)
                .content(move |_, _, cx| {
                    let mut menu = div()
                        .id("pstrip-more-list")
                        .debug_selector(|| "pstrip-more-list".into())
                        .flex()
                        .flex_col()
                        .w(px(250.))
                        .font_family(crate::theme::LABEL_FONT)
                        .text_size(px(11.));
                    for (index, label) in more_input.more_entries().into_iter().enumerate() {
                        let weak = cx.entity().downgrade();
                        menu = menu.child(
                            control(
                                format!("pstrip-more-item-{index}"),
                                if index == 0 {
                                    "Keyboard shortcuts    ⌥/"
                                } else {
                                    label
                                },
                                label,
                                colors.text,
                                colors,
                                (),
                                Rc::new(move |_, window, cx| {
                                    let _ = weak.update(cx, |state, cx| state.dismiss(window, cx));
                                    if label == "Keyboard shortcuts" {
                                        window.dispatch_action(
                                            Box::new(crate::app_menus::ToggleHelp),
                                            cx,
                                        );
                                    } else {
                                        window.dispatch_action(
                                            Box::new(crate::app_menus::PreviewUnavailable {
                                                label: label.into(),
                                            }),
                                            cx,
                                        );
                                    }
                                }),
                            )
                            .debug_selector(move || format!("pstrip-more-item-{index}")),
                        );
                        if index == 0 {
                            menu = menu.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .px(px(8.))
                                    .py(px(6.))
                                    .child(dot(more_input.browser.color, 6., 6.))
                                    .child(more_input.browser.hint.clone()),
                            );
                        }
                    }
                    menu
                }),
        )
        .pr(px(4.));
    div()
        .id("pstrip-workspace-bar")
        .debug_selector(|| "pstrip-workspace-bar".into())
        .whitespace_nowrap()
        .line_height(gpui::relative(crate::theme::LABEL_LINE_HEIGHT))
        .flex()
        .items_center()
        .w_full()
        .h(px(28.))
        .flex_none()
        .font_family(crate::theme::LABEL_FONT)
        .text_size(px(11.))
        .bg(rgb(colors.crust))
        .border_t_1()
        .border_color(rgb(colors.surface0))
        .child(chips)
        .child(right)
}

#[cfg(test)]
fn right_side_labels(input: &Input) -> Vec<String> {
    let mut labels = Vec::new();
    if input.ready_count > 0 || input.ready_news_count > 0 {
        labels.push(format!("↑{}", input.ready_count));
    }
    if input.parked_count > 0 {
        labels.push(format!("⏸{}", input.parked_count));
    }
    labels.extend([">_".to_string(), "Browser".to_string(), "⋯".to_string()]);
    labels
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui::{Context, Render, TestAppContext, VisualTestContext};
    use std::cell::RefCell;

    pub(crate) fn workspace(
        id: u64,
        name: &str,
        group: &str,
        work: bool,
        parked: bool,
    ) -> Workspace {
        Workspace {
            id: WorkspaceId(id),
            name: name.into(),
            color: WorkspaceColor::Lavender,
            placement: workspace::BarChip {
                group: group.into(),
                work,
                parked,
            },
            attention: None,
            woke: None,
            pr_news: None,
        }
    }
    pub(crate) fn input() -> Input {
        Input {
            workspaces: vec![
                workspace(1, "Folder", "local", false, false),
                workspace(2, "Docs and the release", "local", true, false),
                workspace(3, "Remote", "agent:mac", true, false),
                workspace(4, "Parked", "local", true, true),
            ],
            active: Some(WorkspaceId(2)),
            repos_on_bar: false,
            repos_open: false,
            machine_news: vec![],
            repo_machines: vec![],
            remote_workspaces: vec![],
            pulse_bright: false,
            ready_count: 0,
            ready_news_count: 0,
            parked_count: 0,
            terminal_expanded: false,
            terminal_running: false,
            browser: Browser {
                color: 0x9399b2,
                driving: false,
                hint: "Open browser".into(),
                action: Some(BrowserAction::Open),
                has_evidence: false,
            },
            more_open: false,
        }
    }
    #[test]
    fn labels_and_badges_follow_iced_rules() {
        let mut w = workspace(1, "Docs and the release", "local", true, false);
        assert_eq!(w.label(true), "Docs and the release");
        assert_eq!(w.label(false), "Docs release");
        assert_eq!(
            workspace(1, "  api  ", "local", true, false).label(false),
            "api"
        );
        assert_eq!(
            workspace(1, "Internationalisation rollout", "local", true, false).label(false),
            "Internationalisat…"
        );
        w.attention = Some(Attention {
            count: 0,
            kind: AttentionKind::Failed,
            reason: "Agent failed".into(),
        });
        let c = Colors::new(true);
        assert!(w.attention().is_none());
        assert_eq!(w.dot_color(false, false, c), c.surface2);
        w.attention.as_mut().unwrap().count = 2;
        assert_eq!(w.dot_color(false, false, c), c.red);
        w.attention.as_mut().unwrap().kind = AttentionKind::Review;
        assert_eq!(w.dot_color(false, false, c), c.green);
        w.attention.as_mut().unwrap().kind = AttentionKind::NeedsYou;
        assert_eq!(w.dot_color(false, false, c), c.yellow);
        assert_eq!(w.dot_color(false, true, c), c.peach);
        for (reason, label) in [
            (WakeReason::NeedsYou, "needs you"),
            (WakeReason::Finished, "finished"),
            (WakeReason::Failed, "failed"),
        ] {
            w.woke = Some(reason);
            assert_eq!(w.wake_label(false), Some(label));
            assert_eq!(w.wake_label(true), None);
        }
        w.pr_news = Some(News {
            label: "#841 checks failed".into(),
            color: c.red,
        });
        assert!(w.hint().contains("#841 checks failed"));
        assert!(w.hint().contains("Agent failed"));
    }
    #[test]
    fn core_orders_work_repos_folders_and_pinned_remote() {
        let mut i = input();
        assert_eq!(
            i.items(),
            vec![Item::Workspace(1), Item::Repos, Item::NewWork]
        );
        i.repos_on_bar = true;
        i.active = Some(WorkspaceId(3));
        assert_eq!(
            i.items(),
            vec![
                Item::Workspace(1),
                Item::Repos,
                Item::Workspace(0),
                Item::Workspace(2),
                Item::NewWork
            ]
        );
        let indices: Vec<_> = i
            .items()
            .into_iter()
            .filter_map(|i| match i {
                Item::Workspace(index) => Some(index),
                _ => None,
            })
            .collect();
        assert_eq!(indices, workspace::visible_order(&i.layout()));
        i.workspaces[0].pr_news = Some(News {
            label: "PR merged".into(),
            color: 0,
        });
        assert!(!i.hidden_news(), "visible news stays on its chip");
        i.repos_on_bar = false;
        assert!(i.hidden_news());
        i.workspaces[0].pr_news = None;
        assert!(!i.hidden_news());
        i.workspaces[3].attention = Some(Attention {
            count: 1,
            kind: AttentionKind::NeedsYou,
            reason: "Input".into(),
        });
        assert!(!i.hidden_news(), "parked work is omitted entirely");
    }
    #[test]
    fn right_controls_match_iced_order_and_hide_zero_counts() {
        let mut i = input();
        i.ready_count = 2;
        i.parked_count = 16;
        assert_eq!(right_side_labels(&i), ["↑2", "⏸16", ">_", "Browser", "⋯"]);
        i.ready_count = 0;
        i.parked_count = 0;
        i.ready_news_count = 0;
        assert_eq!(right_side_labels(&i), [">_", "Browser", "⋯"]);
    }
    #[test]
    fn more_repos_and_terminal_states_match_iced() {
        let mut i = input();
        let c = Colors::new(true);
        assert_eq!(i.repos_label(), "Repos ▾");
        i.repos_open = true;
        assert_eq!(i.repos_label(), "Repos ▴");
        assert_eq!(i.more_entries(), ["Keyboard shortcuts", "Open browser"]);
        i.browser.action = Some(BrowserAction::Focus);
        i.browser.has_evidence = true;
        assert_eq!(
            i.more_entries(),
            [
                "Keyboard shortcuts",
                "Focus browser",
                "Disconnect browser",
                "Browser evidence"
            ]
        );
        i.browser.action = None;
        assert_eq!(i.more_entries(), ["Keyboard shortcuts", "Browser evidence"]);
        i.browser.has_evidence = false;
        assert_eq!(i.more_entries(), ["Keyboard shortcuts"]);
        assert_eq!(i.terminal_color(c), c.muted);
        i.terminal_running = true;
        assert_eq!(i.terminal_color(c), c.green);
        i.terminal_expanded = true;
        assert_eq!(i.terminal_color(c), c.blue);
        let machines = i.machines();
        assert_eq!(
            i.repo_actions(&machines[0], true),
            [
                "New workspace…",
                "Add a repo to Docs and the release…",
                "Open a repo…",
                "Show repos on the bar too",
                "Show this Mac chats"
            ]
        );
        assert!(matches!(
            Input::repo_action_outcome(&machines[0], "New workspace…"),
            Some(Outcome::NewWork)
        ));
        i.repos_on_bar = true;
        assert_eq!(
            i.repo_actions(&machines[0], true)[3],
            "Keep repos in this menu only"
        );
        let mut remote = machines[1].clone();
        remote.serves_phones = true;
        assert_eq!(
            i.repo_actions(&remote, false),
            ["Show mac chats", "Reconnect…", "Pair a phone…"]
        );
        remote.connected = true;
        assert_eq!(
            i.repo_actions(&remote, false),
            ["Show mac chats", "Pair a phone…"]
        );
    }

    pub(crate) fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
    }
    pub(crate) fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing {selector}"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        draw(cx);
    }
    struct View {
        input: Input,
        events: Rc<RefCell<Vec<Outcome>>>,
        notices: Rc<RefCell<Vec<String>>>,
        focus: gpui::FocusHandle,
    }
    impl Render for View {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let events = self.events.clone();
            let weak = cx.weak_entity();
            let notices = self.notices.clone();
            div()
                .track_focus(&self.focus)
                .on_action(move |action: &crate::app_menus::PreviewUnavailable, _, _| {
                    notices.borrow_mut().push(action.label.clone());
                })
                .child(render(
                    &self.input,
                    Colors::new(true),
                    Rc::new(move |outcome, _, _| events.borrow_mut().push(outcome)),
                    Rc::new(move |change, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            match change.menu {
                                Menu::Repos => {
                                    this.input.repos_open = change.open;
                                    if change.open {
                                        this.input.more_open = false;
                                    }
                                }
                                Menu::More => {
                                    this.input.more_open = change.open;
                                    if change.open {
                                        this.input.repos_open = false;
                                    }
                                }
                            }
                            cx.notify();
                        });
                    }),
                ))
        }
    }
    #[gpui::test]
    fn browser_more_entries_only_emit_preview_notices(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let mut input = input();
        input.browser.action = Some(BrowserAction::Focus);
        input.browser.has_evidence = true;
        let events = Rc::new(RefCell::new(vec![]));
        let notices = Rc::new(RefCell::new(vec![]));
        let (view, cx) = cx.add_window_view(|_, cx| View {
            input,
            events: events.clone(),
            notices: notices.clone(),
            focus: cx.focus_handle(),
        });
        cx.update(|window, cx| {
            let focus = view.read(cx).focus.clone();
            focus.focus(window, cx);
        });
        cx.simulate_resize(gpui::size(px(1400.), px(800.)));
        draw(cx);
        for selector in [
            "pstrip-more-item-1",
            "pstrip-more-item-2",
            "pstrip-more-item-3",
        ] {
            click(cx, "pstrip-more");
            click(cx, selector);
        }
        assert_eq!(
            *notices.borrow(),
            ["Focus browser", "Disconnect browser", "Browser evidence"]
        );
        assert!(
            events.borrow().is_empty(),
            "preview entries cannot emit an owner effect"
        );
    }

    #[gpui::test]
    fn overflowing_chips_scroll_without_shrinking_and_right_controls_stay_pinned(
        cx: &mut TestAppContext,
    ) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let mut input = input();
        input.workspaces = (1..=30)
            .map(|id| workspace(id, "A long workspace name", "local", true, false))
            .collect();
        input.ready_count = 2;
        input.parked_count = 16;
        let (_, cx) = cx.add_window_view(|_, cx| View {
            input,
            events: Rc::new(RefCell::new(vec![])),
            notices: Rc::new(RefCell::new(vec![])),
            focus: cx.focus_handle(),
        });
        cx.simulate_resize(gpui::size(px(1400.), px(800.)));
        draw(cx);
        let chip_width = cx.debug_bounds("pstrip-workspace-1").unwrap().size.width;
        let ready_width = cx.debug_bounds("pstrip-ready").unwrap().size.width;
        for width in [320., 640., 1400.] {
            cx.simulate_resize(gpui::size(px(width), px(800.)));
            draw(cx);
            let bar = cx.debug_bounds("pstrip-workspace-bar").unwrap();
            let scroll = cx.debug_bounds("pstrip-workspace-scroll").unwrap();
            let ready = cx.debug_bounds("pstrip-ready").unwrap();
            let terminal = cx.debug_bounds("pstrip-terminal").unwrap();
            let browser = cx.debug_bounds("pstrip-browser").unwrap();
            assert_eq!(
                cx.debug_bounds("pstrip-workspace-1").unwrap().size.width,
                chip_width
            );
            assert_eq!(ready.size.width, ready_width);
            assert!(scroll.right() <= ready.left());
            assert!(ready.left() >= bar.left() && browser.right() <= bar.right());
            let terminal_before = terminal;
            cx.simulate_event(gpui::ScrollEvent {
                position: scroll.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-10000.))),
                modifiers: Default::default(),
                touch_phase: gpui::TouchPhase::Moved,
                source: gpui::ScrollSource::Pointer,
            });
            draw(cx);
            let plus = cx.debug_bounds("pstrip-new-work").unwrap();
            assert!(plus.left() >= scroll.left() && plus.right() <= scroll.right());
            assert_eq!(cx.debug_bounds("pstrip-terminal").unwrap(), terminal_before);
            assert!(cx.debug_bounds("pstrip-more").is_some());
        }
    }
    #[gpui::test]
    fn every_bar_control_emits_typed_outcome(cx: &mut TestAppContext) {
        cx.background_executor.allow_parking();
        cx.update(gpui_component::init);
        let events = Rc::new(RefCell::new(vec![]));
        let mut input = input();
        input.ready_count = 3;
        input.parked_count = 2;
        input.repos_on_bar = true;
        input.workspaces[0].attention = Some(Attention {
            count: 2,
            kind: AttentionKind::NeedsYou,
            reason: "Input or approval needed".into(),
        });
        input.workspaces[0].woke = Some(WakeReason::Finished);
        input.workspaces[0].pr_news = Some(News {
            label: "#841 merged".into(),
            color: 0xa6e3a1,
        });
        input.workspaces[1].woke = Some(WakeReason::NeedsYou);
        input.workspaces[2].pr_news = Some(News {
            label: "#842 failed".into(),
            color: 0xf38ba8,
        });
        let (view, cx) = cx.add_window_view(|_, cx| View {
            input,
            events: events.clone(),
            notices: Rc::new(RefCell::new(vec![])),
            focus: cx.focus_handle(),
        });
        cx.simulate_resize(gpui::size(px(1400.), px(800.)));
        draw(cx);
        for selector in [
            "pstrip-attention-1",
            "pstrip-wake-1",
            "pstrip-pr-news-1",
            "pstrip-repos-news",
        ] {
            assert!(cx.debug_bounds(selector).is_some(), "missing {selector}");
        }
        assert!(
            cx.debug_bounds("pstrip-wake-2").is_none(),
            "active workspace hides wake reason"
        );
        for selector in [
            "pstrip-workspace-1",
            "pstrip-settings",
            "pstrip-new-work",
            "pstrip-ready",
            "pstrip-parked",
            "pstrip-terminal",
            "pstrip-browser",
        ] {
            click(cx, selector);
        }
        assert_eq!(
            *events.borrow(),
            vec![
                Outcome::SelectWorkspace(WorkspaceId(1)),
                Outcome::OpenSettings(WorkspaceId(2)),
                Outcome::NewWork,
                Outcome::OpenReady,
                Outcome::OpenParked,
                Outcome::ToggleTerminal,
                Outcome::Browser(BrowserAction::Open)
            ]
        );
        assert!(cx.debug_bounds("pstrip-more").is_some());
        click(cx, "pstrip-repos");
        assert!(cx.debug_bounds("pstrip-repos-list").is_some());
        click(cx, "pstrip-repo-1");
        assert_eq!(
            events.borrow().last(),
            Some(&Outcome::SelectWorkspace(WorkspaceId(1)))
        );
        events.borrow_mut().clear();
        view.update(cx, |v, cx| {
            v.input.browser.action = Some(BrowserAction::Focus);
            v.input.workspaces[0].attention.as_mut().unwrap().count = 0;
            v.input.workspaces[0].woke = None;
            v.input.workspaces[0].pr_news = None;
            v.input.workspaces[2].pr_news = None;
            v.input.ready_count = 0;
            v.input.parked_count = 0;
            cx.notify();
        });
        draw(cx);
        for selector in [
            "pstrip-attention-1",
            "pstrip-wake-1",
            "pstrip-pr-news-1",
            "pstrip-repos-news",
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "badge should hide: {selector}"
            );
        }
        assert!(cx.debug_bounds("pstrip-ready").is_none());
        assert!(cx.debug_bounds("pstrip-parked").is_none());
        view.update(cx, |v, cx| {
            v.input.ready_news_count = 1;
            cx.notify();
        });
        draw(cx);
        assert!(
            cx.debug_bounds("pstrip-ready").is_some(),
            "news alone shows ↑0"
        );
        click(cx, "pstrip-browser");
        assert_eq!(*events.borrow(), [Outcome::Browser(BrowserAction::Focus)]);
        assert!(cx.debug_bounds("pstrip-more").is_some());
        click(cx, "pstrip-repos");
        assert!(cx.debug_bounds("pstrip-repos-list").is_some());
        click(cx, "pstrip-repo-1");
        assert_eq!(
            events.borrow().last(),
            Some(&Outcome::SelectWorkspace(WorkspaceId(1)))
        );
        events.borrow_mut().clear();
        view.update(cx, |v, cx| {
            v.input.browser.action = None;
            cx.notify();
        });
        draw(cx);
        click(cx, "pstrip-browser");
        assert!(events.borrow().is_empty());
    }
}
