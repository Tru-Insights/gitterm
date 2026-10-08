//! Relevance ordering for the Chats panel (TRU-141): the pure parts.
//!
//! The panel offers a "Relevant" toggle only when a Jev client exists
//! (`TYPESAFE_API_KEY` set). When it is on, the visible local entries
//! (newest first) become [`ChatCandidate`]s, one ranking call runs at a
//! time, and the result is cached under a [`RankKey`]. The panel shows
//! Jev's order only when the cached key matches what is on screen and the
//! ranking clears [`jev::DEFAULT_CONFIDENCE_GATE`]; otherwise it keeps
//! recency and says why in a one-line note.
//!
//! Every candidate carries a snippet of its conversation's tail, read
//! from at most [`chats::SNIPPET_TAIL_BYTES`] of the transcript inside the
//! ranking task ([`fill_snippets`]) and cached per (path, mtime, size) in
//! a [`SnippetCache`], so a re-rank or an index reload re-reads only the
//! transcripts that changed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use gitterm::chats::{self, ChatBackend, ChatIndexEntry, ChatScope};
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

/// Candidates in the entries' own (recency) order, snippets still empty:
/// [`fill_snippets`] reads them off the UI thread, from the
/// [`snippet_sources`] of the same entries.
pub(crate) fn candidates(entries: &[&ChatIndexEntry]) -> Vec<ChatCandidate> {
    entries
        .iter()
        .map(|e| ChatCandidate {
            id: e.id.clone(),
            title: e.title.clone(),
            cwd: e.cwd.clone(),
            branch: e.branch.clone(),
            age: chats::format_age(e.mtime),
            snippet: String::new(),
        })
        .collect()
}

/// A transcript as the index saw it: a snippet read for one key is
/// reused only while the file's mtime and size are unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SnippetKey {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
}

impl SnippetKey {
    pub(crate) fn of(entry: &ChatIndexEntry) -> Self {
        SnippetKey {
            path: entry.path.clone(),
            mtime: entry.mtime,
            size: entry.size,
        }
    }
}

/// Where each candidate's snippet comes from, aligned with
/// [`candidates`] of the same entries.
pub(crate) fn snippet_sources(entries: &[&ChatIndexEntry]) -> Vec<(SnippetKey, ChatBackend)> {
    entries
        .iter()
        .map(|e| (SnippetKey::of(e), e.backend))
        .collect()
}

/// Snippets by transcript. One slot per path: a file that changed
/// replaces its old snippet, so the cache never outgrows the index.
#[derive(Debug, Default)]
pub(crate) struct SnippetCache {
    by_path: HashMap<PathBuf, (SnippetKey, String)>,
}

impl SnippetCache {
    /// The snippet read for exactly this (path, mtime, size), if any.
    pub(crate) fn get(&self, key: &SnippetKey) -> Option<&str> {
        self.by_path
            .get(&key.path)
            .filter(|(cached, _)| cached == key)
            .map(|(_, snippet)| snippet.as_str())
    }

    pub(crate) fn insert(&mut self, key: SnippetKey, snippet: String) {
        self.by_path.insert(key.path.clone(), (key, snippet));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_path.len()
    }
}

/// One transcript's snippet: the last ask and reply in its bounded tail,
/// shaped by [`jev::snippet_from_preview`]. Empty when the tail holds no
/// conversation text. Blocking.
pub(crate) fn local_snippet(path: &Path, backend: ChatBackend) -> std::io::Result<String> {
    let preview = chats::load_snippet_preview(path, backend)?;
    Ok(jev::snippet_from_preview(&preview.messages))
}

/// What [`fill_snippets`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SnippetStats {
    /// Snippets taken from the cache.
    pub(crate) cached: usize,
    /// Transcripts read.
    pub(crate) read: usize,
    /// Transcripts that could not be read (logged, snippet left empty).
    pub(crate) failed: usize,
}

/// Fill each candidate's snippet from `cache`, reading the transcripts it
/// does not hold (each at most [`chats::SNIPPET_TAIL_BYTES`]) and caching
/// what was read. `sources` is aligned with `candidates`. Blocking: run it
/// on a blocking thread, never in `update()` or `view()`. The lock is not
/// held while files are read.
pub(crate) fn fill_snippets(
    cache: &Mutex<SnippetCache>,
    candidates: &mut [ChatCandidate],
    sources: &[(SnippetKey, ChatBackend)],
) -> SnippetStats {
    debug_assert_eq!(candidates.len(), sources.len());
    let mut stats = SnippetStats::default();
    let mut misses: Vec<usize> = Vec::new();
    {
        let cache = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (i, (key, _)) in sources.iter().enumerate() {
            match cache.get(key) {
                Some(snippet) => {
                    candidates[i].snippet = snippet.to_string();
                    stats.cached += 1;
                }
                None => misses.push(i),
            }
        }
    }
    if misses.is_empty() {
        return stats;
    }
    let read: Vec<(usize, String)> = misses
        .into_iter()
        .map(|i| {
            let (key, backend) = &sources[i];
            let snippet = match local_snippet(&key.path, *backend) {
                Ok(snippet) => {
                    stats.read += 1;
                    snippet
                }
                Err(e) => {
                    // Cached empty under this key, so it is reported once
                    // per change of the file, not on every re-rank.
                    eprintln!(
                        "[chats] snippet for {} ({}) unreadable: {e}",
                        key.path.display(),
                        backend.label()
                    );
                    stats.failed += 1;
                    String::new()
                }
            };
            (i, snippet)
        })
        .collect();
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for (i, snippet) in read {
        candidates[i].snippet = snippet.clone();
        cache.insert(sources[i].0.clone(), snippet);
    }
    stats
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
    fn candidates_keep_recency_with_snippets_left_to_fill() {
        let (a, b) = (entry("a", 5), entry("b", 120));
        let entries = vec![&a, &b];
        let c = candidates(&entries);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].id, "a");
        assert_eq!(c[0].title, "Title a");
        assert_eq!(c[0].cwd, PathBuf::from("/repo/app"));
        assert_eq!(c[0].branch.as_deref(), Some("branch-a"));
        assert_eq!(c[0].age, "5m");
        assert_eq!(c[1].age, "2h");
        assert!(c.iter().all(|c| c.snippet.is_empty()));
        let sources = snippet_sources(&entries);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[1].0, SnippetKey::of(&b));
        assert_eq!(sources[1].1, ChatBackend::Claude);
    }

    const CHAT_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/chats");

    #[test]
    fn local_snippet_takes_the_last_ask_and_reply_for_every_backend() {
        let cases = [
            (
                "claude.jsonl",
                ChatBackend::Claude,
                "asked: now add a test for the missing-file case / \
                 reply: Added missing_file_is_an_error; cargo test passes.",
            ),
            (
                "codex.jsonl",
                ChatBackend::Codex,
                "asked: add exponential backoff to the uploader / \
                 reply: Backoff now doubles from 500 ms, capped at 8 s.",
            ),
            (
                "pi.jsonl",
                ChatBackend::Pi,
                "asked: what about stale sockets? / \
                 reply: Unlink them when connect fails with ECONNREFUSED.",
            ),
        ];
        for (file, backend, want) in cases {
            let path = Path::new(CHAT_FIXTURES).join(file);
            let got = local_snippet(&path, backend).unwrap();
            assert_eq!(got, want, "{file}");
        }
    }

    #[test]
    fn local_snippet_reads_only_the_bounded_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.jsonl");
        let line = |kind: &str, text: &str| {
            serde_json::json!({"type": kind, "message": {"role": kind, "content": text}})
                .to_string()
        };
        // An ask far back, then more than the tail of filler replies that
        // the snippet must not reach past.
        let mut lines = vec![line("user", "the early ask")];
        let filler = line("assistant", &"x".repeat(1000));
        let n = (chats::SNIPPET_TAIL_BYTES as usize / filler.len()) + 2;
        lines.extend(vec![filler; n]);
        lines.push(line("assistant", "the last reply"));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let got = local_snippet(&path, ChatBackend::Claude).unwrap();
        assert_eq!(got, "reply: the last reply");

        // A tail that is one partial line holds no conversation.
        let huge = dir.path().join("huge.jsonl");
        let big = line("user", &"y".repeat(2 * chats::SNIPPET_TAIL_BYTES as usize));
        std::fs::write(&huge, big + "\n").unwrap();
        assert_eq!(local_snippet(&huge, ChatBackend::Claude).unwrap(), "");

        assert!(local_snippet(&dir.path().join("gone.jsonl"), ChatBackend::Pi).is_err());
    }

    #[test]
    fn snippet_cache_is_keyed_by_path_mtime_and_size() {
        let a = entry("a", 5);
        let key = SnippetKey::of(&a);
        let mut cache = SnippetCache::default();
        assert_eq!(cache.get(&key), None);
        cache.insert(key.clone(), "asked: x".into());
        assert_eq!(cache.get(&key), Some("asked: x"));

        let mut touched = a.clone();
        touched.mtime += Duration::from_secs(1);
        assert_eq!(cache.get(&SnippetKey::of(&touched)), None, "mtime");
        let mut grown = a.clone();
        grown.size += 1;
        assert_eq!(cache.get(&SnippetKey::of(&grown)), None, "size");
        let mut moved = a.clone();
        moved.path = PathBuf::from("/t/other.jsonl");
        assert_eq!(cache.get(&SnippetKey::of(&moved)), None, "path");

        // A changed file replaces its slot rather than adding one.
        cache.insert(SnippetKey::of(&grown), "asked: y".into());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&key), None);
        assert_eq!(cache.get(&SnippetKey::of(&grown)), Some("asked: y"));
    }

    #[test]
    fn fill_snippets_reads_each_unchanged_transcript_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = Vec::new();
        for (file, backend) in [
            ("claude.jsonl", ChatBackend::Claude),
            ("codex.jsonl", ChatBackend::Codex),
            ("pi.jsonl", ChatBackend::Pi),
        ] {
            let path = dir.path().join(file);
            std::fs::copy(Path::new(CHAT_FIXTURES).join(file), &path).unwrap();
            let mut e = entry(file, 1);
            e.backend = backend;
            e.size = std::fs::metadata(&path).unwrap().len();
            e.path = path;
            entries.push(e);
        }
        let mut gone = entry("gone", 1);
        gone.path = dir.path().join("gone.jsonl");
        entries.push(gone);
        let refs: Vec<&ChatIndexEntry> = entries.iter().collect();
        let cache = Mutex::new(SnippetCache::default());

        let mut first = candidates(&refs);
        let stats = fill_snippets(&cache, &mut first, &snippet_sources(&refs));
        assert_eq!(
            stats,
            SnippetStats {
                cached: 0,
                read: 3,
                failed: 1
            }
        );
        assert!(first[..3].iter().all(|c| c.snippet.starts_with("asked: ")));
        assert_eq!(first[3].snippet, "");

        // Same files: nothing is read again, even the unreadable one, and
        // the snippets are the same.
        for e in &entries[..3] {
            std::fs::write(&e.path, "").unwrap();
        }
        let mut again = candidates(&refs);
        let stats = fill_snippets(&cache, &mut again, &snippet_sources(&refs));
        assert_eq!(stats.cached, 4);
        assert_eq!(stats.read, 0);
        assert_eq!(again, first);

        // A changed file (new size) is read again.
        entries[0].size += 1;
        let refs: Vec<&ChatIndexEntry> = entries.iter().collect();
        let mut changed = candidates(&refs);
        let stats = fill_snippets(&cache, &mut changed, &snippet_sources(&refs));
        assert_eq!((stats.cached, stats.read), (3, 1));
        assert_eq!(changed[0].snippet, "", "the emptied file was re-read");
    }

    #[test]
    #[ignore = "reads every transcript tail on this machine; run manually with --ignored --nocapture"]
    fn real_index_cold_snippet_pass() {
        let started = std::time::Instant::now();
        let entries = chats::build_local_index();
        let indexed = started.elapsed();
        let refs: Vec<&ChatIndexEntry> = entries.iter().collect();
        let cache = Mutex::new(SnippetCache::default());
        let mut cands = candidates(&refs);
        let started = std::time::Instant::now();
        let stats = fill_snippets(&cache, &mut cands, &snippet_sources(&refs));
        let cold = started.elapsed();
        let mut warm_cands = candidates(&refs);
        let started = std::time::Instant::now();
        let warm = fill_snippets(&cache, &mut warm_cands, &snippet_sources(&refs));
        let warm_time = started.elapsed();
        let empty = cands.iter().filter(|c| c.snippet.is_empty()).count();
        let per_backend = |b: ChatBackend| entries.iter().filter(|e| e.backend == b).count();
        // Counts and timings only: never transcript contents.
        eprintln!(
            "index: {} chats (claude {}, codex {}, pi {}) in {indexed:?}; \
             cold snippets: {} read, {} unreadable, {empty} empty in {cold:?}; \
             warm: {} cached in {warm_time:?}",
            entries.len(),
            per_backend(ChatBackend::Claude),
            per_backend(ChatBackend::Codex),
            per_backend(ChatBackend::Pi),
            stats.read,
            stats.failed,
            warm.cached,
        );
        assert_eq!(warm.read, 0);
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
            .begin(key.clone(), candidates(&recency))
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
        assert!(s.begin(key.clone(), candidates(&recency)).is_none());
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
            s.begin(key.clone(), candidates(&recency)).unwrap();
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
        s.begin(key.clone(), candidates(&[&a, &b, &c])).unwrap();
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
        assert!(s.begin(k1.clone(), candidates(&[&a, &b])).is_some());
        assert!(s.begin(k1.clone(), candidates(&[&a, &b])).is_none());
        assert!(s.begin(k2.clone(), candidates(&[&a])).is_none());
        assert!(s.finish(Ok(ranking(&["b", "a"], 0.9, false))), "re-rank");
        assert!(s.begin(k2.clone(), candidates(&[&a])).is_some());
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
        s.begin(key.clone(), candidates(&[&a])).unwrap();
        assert!(!s.finish(Err(JevError::Http(500, "boom".into()))));
        assert!(s.active());
        assert!(s.has_error());
        let note = s.note(Some(&key)).unwrap();
        assert!(note.contains("TypeSafe HTTP 500: boom"), "{note}");
        assert!(!s.order(&key, vec![&a]).1);

        // TypeSafe down (a dummy key is refused): toggle disabled.
        s.begin(key.clone(), candidates(&[&a])).unwrap();
        s.finish(Err(JevError::KeyRefused(401, "bad key".into())));
        assert!(s.down());
        assert!(!s.active());
        assert!(!s.toggle(), "disabled while down");
        assert!(s.enabled());
        assert!(s.begin(key.clone(), candidates(&[&a])).is_none());
        assert!(s.note(Some(&key)).unwrap().contains("refused the key"));

        s.panel_opened();
        assert!(s.active());
        assert!(!s.has_error());
        assert!(s.begin(key.clone(), candidates(&[&a])).is_some());
    }

    #[test]
    fn remote_lists_get_the_local_only_note() {
        let mut s = ChatRankState::default();
        s.toggle();
        assert_eq!(s.note(None).as_deref(), Some(NOTE_LOCAL_ONLY));
    }
}
