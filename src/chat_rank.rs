//! Relevance ordering for the Chats panel (TRU-141): the pure parts.
//!
//! The panel offers a "Relevant" toggle only when a Jev client exists
//! (`TYPESAFE_API_KEY` set). When it is on, the visible local entries
//! (newest first) become [`ChatCandidate`]s, one ranking call runs at a
//! time, and the result is cached under a [`RankKey`]. The panel shows
//! Jev's order only when the cached key matches what is on screen and the
//! ranking clears [`jev::DEFAULT_CONFIDENCE_GATE`]; otherwise it keeps
//! recency and says why in a one-line note.

use std::path::{Path, PathBuf};

use gitterm::chats::{self, ChatBackend, ChatIndexEntry, ChatPreview, ChatScope};
use gitterm::jev::{self, ChatCandidate, JevError, Ranking};

use crate::tab::AgentEvent;

pub(crate) const NOTE_NOT_CONFIDENT: &str = "not confident, showing recent";
pub(crate) const NOTE_RANKING: &str = "ranking by relevance…";
pub(crate) const NOTE_LOCAL_ONLY: &str = "relevance order covers this Mac's chats only";

/// What a ranking was computed for. Two states with the same key show
/// the same cached ranking, so toggling off and on does not call again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RankKey {
    query: String,
    scope: ChatScope,
    filter: Option<ChatBackend>,
    /// The context's workspace: the same chats rank differently elsewhere.
    workspace: PathBuf,
    /// Candidate ids, sorted: the set, not the order.
    ids: Vec<String>,
}

impl RankKey {
    pub(crate) fn new(
        query: &str,
        scope: ChatScope,
        filter: Option<ChatBackend>,
        workspace: &Path,
        entries: &[&ChatIndexEntry],
    ) -> Self {
        let mut ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
        ids.sort();
        RankKey {
            query: query.to_string(),
            scope,
            filter,
            workspace: workspace.to_path_buf(),
            ids,
        }
    }
}

/// Candidates in the entries' own (recency) order. The snippet comes from
/// the one preview already loaded, if it is this chat's; otherwise empty.
pub(crate) fn candidates(
    entries: &[&ChatIndexEntry],
    preview: Option<&(String, ChatPreview)>,
) -> Vec<ChatCandidate> {
    entries
        .iter()
        .map(|e| ChatCandidate {
            id: e.id.clone(),
            title: e.title.clone(),
            cwd: e.cwd.clone(),
            branch: e.branch.clone(),
            age: chats::format_age(e.mtime),
            snippet: preview
                .filter(|(id, _)| *id == e.id)
                .map(|(_, p)| jev::snippet_from_preview(&p.messages))
                .unwrap_or_default(),
        })
        .collect()
}

/// The text of the last prompt the human sent in an agent tab.
pub(crate) fn last_prompt(conversation: &[AgentEvent]) -> Option<&str> {
    conversation.iter().rev().find_map(|ev| match ev {
        AgentEvent::Other(v) if v.get("type").and_then(|t| t.as_str()) == Some("user_prompt") => {
            v.get("text").and_then(|t| t.as_str())
        }
        _ => None,
    })
}

#[derive(Debug, Clone)]
struct Cached {
    key: RankKey,
    confident: bool,
    /// `Ranking::ordered_ids` at the default gate.
    order: Vec<String>,
}

/// The toggle, the one call in flight, the last ranking and the last error.
#[derive(Debug, Default)]
pub(crate) struct ChatRankState {
    enabled: bool,
    /// TypeSafe is unusable (`JevError::is_down`): the toggle is disabled
    /// until the panel is opened again.
    down: bool,
    in_flight: Option<(RankKey, Vec<ChatCandidate>)>,
    /// The panel changed while a call was in flight: rank the latest state
    /// when it lands.
    stale: bool,
    cached: Option<Cached>,
    error: Option<String>,
}

impl ChatRankState {
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn down(&self) -> bool {
        self.down
    }

    /// Relevance order is wanted and TypeSafe is usable.
    pub(crate) fn active(&self) -> bool {
        self.enabled && !self.down
    }

    /// Flip the toggle; true when the caller should rank now.
    pub(crate) fn toggle(&mut self) -> bool {
        if self.down {
            return false;
        }
        self.enabled = !self.enabled;
        if !self.enabled {
            self.error = None;
        }
        self.enabled
    }

    /// The Chats panel was opened: a "down" verdict gets another chance.
    pub(crate) fn panel_opened(&mut self) {
        if self.down {
            self.down = false;
            self.error = None;
        }
    }

    /// Start a call for `key` unless there is nothing to do: the toggle is
    /// off, there are no candidates, the cache already holds `key`, or a
    /// call is in flight (then the latest state is ranked when it lands).
    /// Returns the candidates to send.
    pub(crate) fn begin(
        &mut self,
        key: RankKey,
        candidates: Vec<ChatCandidate>,
    ) -> Option<Vec<ChatCandidate>> {
        if !self.active() || candidates.is_empty() {
            return None;
        }
        if self.cached.as_ref().is_some_and(|c| c.key == key) {
            return None;
        }
        if let Some((in_flight, _)) = &self.in_flight {
            if *in_flight != key {
                self.stale = true;
            }
            return None;
        }
        self.error = None;
        self.in_flight = Some((key, candidates.clone()));
        Some(candidates)
    }

    /// A call landed. Returns true when the panel changed meanwhile and
    /// the caller should rank the latest state.
    pub(crate) fn finish(&mut self, result: Result<Ranking, JevError>) -> bool {
        let Some((key, candidates)) = self.in_flight.take() else {
            eprintln!("[chats] relevance ranking landed with no call in flight; ignored");
            return false;
        };
        match result {
            Ok(ranking) => {
                let gate = jev::DEFAULT_CONFIDENCE_GATE;
                self.cached = Some(Cached {
                    key,
                    confident: ranking.is_confident(gate),
                    order: ranking.ordered_ids(gate, &candidates),
                });
                self.error = None;
            }
            Err(e) => {
                eprintln!("[chats] relevance ranking failed: {e}");
                if e.is_down() {
                    self.down = true;
                }
                self.error = Some(e.to_string());
            }
        }
        std::mem::take(&mut self.stale) && self.active()
    }

    /// `entries` (newest first) in the order to show, and whether that is
    /// Jev's order. Recency unless the cached ranking is for `key` and
    /// confident; entries the ranking does not know keep recency, last.
    pub(crate) fn order<'a>(
        &self,
        key: &RankKey,
        entries: Vec<&'a ChatIndexEntry>,
    ) -> (Vec<&'a ChatIndexEntry>, bool) {
        let Some(cached) = self
            .cached
            .as_ref()
            .filter(|c| self.active() && c.confident && c.key == *key)
        else {
            return (entries, false);
        };
        let pos: std::collections::HashMap<&str, usize> = cached
            .order
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        let mut entries = entries;
        entries.sort_by_key(|e| pos.get(e.id.as_str()).copied().unwrap_or(usize::MAX));
        (entries, true)
    }

    /// The one-line note under the panel header, if any. `key` is None
    /// when no local chats are on screen (a remote machine's list).
    pub(crate) fn note(&self, key: Option<&RankKey>) -> Option<String> {
        if !self.enabled {
            return None;
        }
        if let Some(err) = &self.error {
            return Some(format!("{err} · showing recent"));
        }
        let Some(key) = key else {
            return Some(NOTE_LOCAL_ONLY.to_string());
        };
        if let Some(c) = self.cached.as_ref().filter(|c| c.key == *key) {
            return (!c.confident).then(|| NOTE_NOT_CONFIDENT.to_string());
        }
        self.in_flight.is_some().then(|| NOTE_RANKING.to_string())
    }

    /// The note is an error.
    pub(crate) fn has_error(&self) -> bool {
        self.enabled && self.error.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitterm::chats::ChatPreviewMessage;
    use gitterm::jev::{RankedChat, Usage};
    use std::time::{Duration, SystemTime};

    fn entry(id: &str, mins_ago: u64) -> ChatIndexEntry {
        ChatIndexEntry {
            id: id.to_string(),
            backend: ChatBackend::Claude,
            path: PathBuf::from(format!("/t/{id}.jsonl")),
            cwd: PathBuf::from("/repo/app"),
            repo_root: Some(PathBuf::from("/repo/app")),
            is_worktree: false,
            branch: Some(format!("branch-{id}")),
            title: format!("Title {id}"),
            mtime: SystemTime::now() - Duration::from_secs(mins_ago * 60 + 5),
            size: 10,
            dead_cwd: false,
        }
    }

    fn key_for(query: &str, entries: &[&ChatIndexEntry]) -> RankKey {
        RankKey::new(
            query,
            ChatScope::Machine,
            None,
            Path::new("/repo/app"),
            entries,
        )
    }

    fn ranking(ids: &[&str], confidence: f64, chose_none: bool) -> Ranking {
        Ranking {
            chats: ids
                .iter()
                .enumerate()
                .map(|(i, id)| RankedChat {
                    id: id.to_string(),
                    probability: 1.0 / (i as f64 + 2.0),
                    rank: i + 1,
                })
                .collect(),
            confidence,
            chose_none,
            calls: 1,
            usage: Usage::default(),
        }
    }

    fn ids(entries: &[&ChatIndexEntry]) -> Vec<String> {
        entries.iter().map(|e| e.id.clone()).collect()
    }

    #[test]
    fn candidates_keep_recency_and_take_only_a_loaded_preview() {
        let (a, b) = (entry("a", 5), entry("b", 120));
        let entries = vec![&a, &b];
        let preview = (
            "b".to_string(),
            ChatPreview {
                messages: vec![
                    ChatPreviewMessage {
                        is_user: true,
                        text: "fix the build".into(),
                    },
                    ChatPreviewMessage {
                        is_user: false,
                        text: "done".into(),
                    },
                ],
                message_count: None,
            },
        );
        let c = candidates(&entries, Some(&preview));
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].id, "a");
        assert_eq!(c[0].title, "Title a");
        assert_eq!(c[0].cwd, PathBuf::from("/repo/app"));
        assert_eq!(c[0].branch.as_deref(), Some("branch-a"));
        assert_eq!(c[0].age, "5m");
        assert_eq!(c[0].snippet, "", "no preview loaded for a");
        assert_eq!(c[1].age, "2h");
        assert_eq!(c[1].snippet, "asked: fix the build / reply: done");
        assert!(candidates(&entries, None)
            .iter()
            .all(|c| c.snippet.is_empty()));
    }

    #[test]
    fn rank_key_is_the_candidate_set_and_the_filters() {
        let (a, b) = (entry("a", 1), entry("b", 2));
        assert_eq!(key_for("x", &[&a, &b]), key_for("x", &[&b, &a]), "a set");
        assert_ne!(key_for("x", &[&a, &b]), key_for("x", &[&a]));
        assert_ne!(key_for("x", &[&a]), key_for("y", &[&a]));
        let scoped = RankKey::new(
            "x",
            ChatScope::Workspace,
            None,
            Path::new("/repo/app"),
            &[&a],
        );
        assert_ne!(scoped, key_for("x", &[&a]));
        let filtered = RankKey::new(
            "x",
            ChatScope::Machine,
            Some(ChatBackend::Codex),
            Path::new("/repo/app"),
            &[&a],
        );
        assert_ne!(filtered, key_for("x", &[&a]));
    }

    #[test]
    fn last_prompt_is_the_latest_user_prompt() {
        let conv = vec![
            AgentEvent::user_prompt("first"),
            AgentEvent::AssistantText("ok".into()),
            AgentEvent::user_prompt("second"),
            AgentEvent::Other(serde_json::json!({"type": "status"})),
        ];
        assert_eq!(last_prompt(&conv), Some("second"));
        assert_eq!(last_prompt(&[]), None);
    }

    #[test]
    fn confident_ranking_orders_and_otherwise_recency_stays() {
        let (a, b, c) = (entry("a", 1), entry("b", 2), entry("c", 3));
        let recency = vec![&a, &b, &c];
        let key = key_for("", &recency);
        let mut s = ChatRankState::default();
        assert!(s.toggle());

        let sent = s
            .begin(key.clone(), candidates(&recency, None))
            .expect("first call");
        assert_eq!(sent.len(), 3);
        assert_eq!(s.note(Some(&key)).as_deref(), Some(NOTE_RANKING));
        assert!(!s.finish(Ok(ranking(&["c", "a", "b"], 0.8, false))));
        let (shown, ranked) = s.order(&key, recency.clone());
        assert!(ranked);
        assert_eq!(ids(&shown), ["c", "a", "b"]);
        assert_eq!(s.note(Some(&key)), None);

        // Another state: recency until its own ranking lands.
        let other = key_for("q", &recency);
        let (shown, ranked) = s.order(&other, recency.clone());
        assert!(!ranked);
        assert_eq!(ids(&shown), ["a", "b", "c"]);

        // Toggling off shows recency; on again reuses the cache, no call.
        assert!(!s.toggle());
        assert_eq!(ids(&s.order(&key, recency.clone()).0), ["a", "b", "c"]);
        assert_eq!(s.note(Some(&key)), None);
        assert!(s.toggle());
        assert!(s.begin(key.clone(), candidates(&recency, None)).is_none());
        assert_eq!(ids(&s.order(&key, recency.clone()).0), ["c", "a", "b"]);
    }

    #[test]
    fn below_the_gate_or_none_keeps_recency_with_a_note() {
        let (a, b) = (entry("a", 1), entry("b", 2));
        let recency = vec![&a, &b];
        let key = key_for("", &recency);
        for r in [
            ranking(&["b", "a"], 0.3, false),
            ranking(&["b", "a"], 0.95, true),
        ] {
            let mut s = ChatRankState::default();
            s.toggle();
            s.begin(key.clone(), candidates(&recency, None)).unwrap();
            s.finish(Ok(r));
            let (shown, ranked) = s.order(&key, recency.clone());
            assert!(!ranked);
            assert_eq!(ids(&shown), ["a", "b"]);
            assert_eq!(s.note(Some(&key)).as_deref(), Some(NOTE_NOT_CONFIDENT));
        }
    }

    #[test]
    fn entries_the_ranking_does_not_know_keep_recency_last() {
        let (a, b, c) = (entry("a", 1), entry("b", 2), entry("c", 3));
        let key = key_for("", &[&a, &b, &c]);
        let mut s = ChatRankState::default();
        s.toggle();
        s.begin(key.clone(), candidates(&[&a, &b, &c], None))
            .unwrap();
        s.finish(Ok(ranking(&["c"], 0.9, false)));
        assert_eq!(ids(&s.order(&key, vec![&a, &b, &c]).0), ["c", "a", "b"]);
    }

    #[test]
    fn one_call_at_a_time_then_the_latest_state() {
        let (a, b) = (entry("a", 1), entry("b", 2));
        let k1 = key_for("f", &[&a, &b]);
        let k2 = key_for("fo", &[&a]);
        let mut s = ChatRankState::default();
        s.toggle();
        assert!(s.begin(k1.clone(), candidates(&[&a, &b], None)).is_some());
        assert!(s.begin(k1.clone(), candidates(&[&a, &b], None)).is_none());
        assert!(s.begin(k2.clone(), candidates(&[&a], None)).is_none());
        assert!(s.finish(Ok(ranking(&["b", "a"], 0.9, false))), "re-rank");
        assert!(s.begin(k2.clone(), candidates(&[&a], None)).is_some());
        assert!(!s.finish(Ok(ranking(&["a"], 0.9, false))));
        // Nothing to rank: no call.
        assert!(s.begin(key_for("zzz", &[]), Vec::new()).is_none());
    }

    #[test]
    fn errors_show_a_note_and_down_disables_the_toggle_until_reopen() {
        let a = entry("a", 1);
        let key = key_for("", &[&a]);
        let mut s = ChatRankState::default();
        assert_eq!(s.note(Some(&key)), None, "off: no note");
        s.toggle();

        // One call's problem: note, recency, toggle still usable.
        s.begin(key.clone(), candidates(&[&a], None)).unwrap();
        assert!(!s.finish(Err(JevError::Http(500, "boom".into()))));
        assert!(s.active());
        assert!(s.has_error());
        let note = s.note(Some(&key)).unwrap();
        assert!(note.contains("TypeSafe HTTP 500: boom"), "{note}");
        assert!(!s.order(&key, vec![&a]).1);

        // TypeSafe down (a dummy key is refused): toggle disabled.
        s.begin(key.clone(), candidates(&[&a], None)).unwrap();
        s.finish(Err(JevError::KeyRefused(401, "bad key".into())));
        assert!(s.down());
        assert!(!s.active());
        assert!(!s.toggle(), "disabled while down");
        assert!(s.enabled());
        assert!(s.begin(key.clone(), candidates(&[&a], None)).is_none());
        assert!(s.note(Some(&key)).unwrap().contains("refused the key"));

        s.panel_opened();
        assert!(s.active());
        assert!(!s.has_error());
        assert!(s.begin(key.clone(), candidates(&[&a], None)).is_some());
    }

    #[test]
    fn remote_lists_get_the_local_only_note() {
        let mut s = ChatRankState::default();
        s.toggle();
        assert_eq!(s.note(None).as_deref(), Some(NOTE_LOCAL_ONLY));
    }
}
