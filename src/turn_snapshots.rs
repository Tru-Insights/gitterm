//! Local turn history. Only object writes and GitTerm-owned refs; never checkout,
//! reset, stash, or the real index. Call these async functions off the UI thread.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub const PREFIX: &str = "refs/gitterm/turns/";
pub const MAX_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_FILES: usize = 10_000;
pub const RETENTION_SECS: i64 = 14 * 24 * 60 * 60;
const TIMEOUT: Duration = Duration::from_secs(30);

fn chat_prefix(chat: &str) -> Result<String, String> {
    if chat.is_empty()
        || !chat
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("invalid snapshot chat id".into());
    }
    Ok(format!("{PREFIX}{chat}/"))
}

async fn checkout_prefix(dir: &Path, chat: &str) -> Result<String, String> {
    let path = tokio::fs::canonicalize(dir)
        .await
        .map_err(|e| format!("resolve snapshot checkout {}: {e}", dir.display()))?;
    // Worktrees share refs. Key the canonical checkout, without publishing its path.
    let digest = ring::digest::digest(&ring::digest::SHA256, path.as_os_str().as_encoded_bytes());
    let key: String = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(format!("{}{key}/", chat_prefix(chat)?))
}

async fn git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>, String> {
    git_input(dir, index, args, None).await
}
async fn git_input(
    dir: &Path,
    index: Option<&Path>,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncWriteExt;
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.splitIndex=false",
        ])
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Inherited repository overrides must not redirect writes into another repo.
    for key in [
        "GIT_NAMESPACE",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_DATE",
    ] {
        command.env_remove(key);
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command
        .env("GIT_AUTHOR_NAME", "GitTerm")
        .env("GIT_AUTHOR_EMAIL", "snapshots@gitterm.invalid")
        .env("GIT_COMMITTER_NAME", "GitTerm")
        .env("GIT_COMMITTER_EMAIL", "snapshots@gitterm.invalid");
    let run = async {
        let mut child = command.spawn()?;
        if let Some(input) = input {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| std::io::Error::other("snapshot git stdin unavailable"))?;
            stdin.write_all(input).await?;
        }
        child.wait_with_output().await
    };
    let out = tokio::time::timeout(TIMEOUT, run)
        .await
        .map_err(|_| format!("snapshot git {args:?} timed out in {}", dir.display()))?
        .map_err(|e| format!("snapshot git {args:?} in {}: {e}", dir.display()))?;
    if !out.status.success() {
        return Err(format!(
            "snapshot git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}
async fn text(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<String, String> {
    String::from_utf8(git(dir, index, args).await?)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("snapshot git output: {e}"))
}
struct TemporaryIndex(PathBuf);
impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            eprintln!("[turns] remove temporary index {}: {e}", self.0.display());
        }
    }
}

/// Returns None, with a log, for unsafe or oversized checkouts. A copied index
/// retains tracked ignored files and staged additions. Its flags are cleared
/// so assume-unchanged / skip-worktree cannot hide working-tree edits.
pub async fn snapshot(dir: &Path) -> Result<Option<String>, String> {
    let git_dir = text(dir, None, &["rev-parse", "--absolute-git-dir"]).await?;
    for state in ["MERGE_HEAD", "rebase-merge", "rebase-apply"] {
        if tokio::fs::try_exists(Path::new(&git_dir).join(state))
            .await
            .map_err(|e| format!("check {state}: {e}"))?
        {
            eprintln!(
                "[turns] skip {}: merge/rebase in progress ({state})",
                dir.display()
            );
            return Ok(None);
        }
    }
    let names = git(
        dir,
        None,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )
    .await?;
    let names: BTreeSet<&[u8]> = names.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
    if names.len() > MAX_FILES {
        eprintln!(
            "[turns] skip {}: more than {MAX_FILES} paths",
            dir.display()
        );
        return Ok(None);
    }
    let mut bytes = 0u64;
    for name in names {
        #[cfg(unix)]
        let name = {
            use std::os::unix::ffi::OsStrExt;
            std::ffi::OsStr::from_bytes(name)
        };
        #[cfg(not(unix))]
        let name = std::str::from_utf8(name).map_err(|e| format!("snapshot filename: {e}"))?;
        let path = dir.join(name);
        match tokio::fs::symlink_metadata(&path).await {
            Ok(meta) if meta.is_file() || meta.file_type().is_symlink() => {
                bytes = bytes.saturating_add(meta.len())
            }
            Ok(meta) if meta.is_dir() => {} // gitlinks are stored as commit ids, not traversed.
            Ok(_) => return Err(format!("snapshot refuses special file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("snapshot stat {}: {e}", path.display())),
        }
        if bytes > MAX_BYTES {
            eprintln!("[turns] skip {}: over {MAX_BYTES} bytes", dir.display());
            return Ok(None);
        }
    }
    let temp = TemporaryIndex(
        std::env::temp_dir().join(format!("gitterm-v6-turn-{}", uuid::Uuid::new_v4())),
    );
    tokio::fs::create_dir(&temp.0)
        .await
        .map_err(|e| format!("snapshot temporary directory: {e}"))?;
    let index = temp.0.join("index");
    // Resolve the real index through git, including linked worktrees.
    let real_index = text(
        dir,
        None,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )
    .await?;
    match tokio::fs::copy(&real_index, &index).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            git(dir, Some(&index), &["read-tree", "--empty"]).await?;
        }
        Err(e) => return Err(format!("copy snapshot index {real_index}: {e}")),
    }
    let tracked = git(dir, Some(&index), &["ls-files", "--cached", "-z"]).await?;
    // Only the private copy loses these flags: they must not conceal edits.
    git_input(
        dir,
        Some(&index),
        &[
            "update-index",
            "--no-assume-unchanged",
            "--no-skip-worktree",
            "-z",
            "--stdin",
        ],
        Some(&tracked),
    )
    .await?;
    git(dir, Some(&index), &["add", "-A", "--", "."]).await?;
    let tree = text(dir, Some(&index), &["write-tree"]).await?;
    // No parent links: expiration can release every snapshot independently.
    text(
        dir,
        Some(&index),
        &["commit-tree", &tree, "-m", "GitTerm local turn snapshot"],
    )
    .await
    .map(Some)
}

pub async fn save(dir: &Path, chat: &str, turn: u64, phase: &str, oid: &str) -> Result<(), String> {
    if !matches!(phase, "start" | "end") {
        return Err("invalid snapshot phase".into());
    }
    let reference = format!("{}{turn}/{phase}", checkout_prefix(dir, chat).await?);
    let oid = crate::workspace_overview::resolve_commit(dir, oid).await?;
    git(dir, None, &["update-ref", &reference, &oid]).await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub number: u64,
    pub start: Option<String>,
    pub end: Option<String>,
    pub timestamp: i64,
}
async fn refs(dir: &Path, prefix: &str) -> Result<Vec<(String, String, i64)>, String> {
    let out = text(
        dir,
        None,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(committerdate:unix)",
            prefix,
        ],
    )
    .await?;
    out.lines()
        .map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() != 3 {
                return Err(format!("invalid snapshot ref record {line:?}"));
            }
            Ok((
                fields[0].into(),
                fields[1].into(),
                fields[2]
                    .parse()
                    .map_err(|e| format!("snapshot date: {e}"))?,
            ))
        })
        .collect()
}
pub async fn turns(dir: &Path, chat: &str) -> Result<Vec<Turn>, String> {
    let prefix = checkout_prefix(dir, chat).await?;
    let mut turns = BTreeMap::new();
    for (reference, oid, timestamp) in refs(dir, &prefix).await? {
        let Some((number, phase)) = reference
            .strip_prefix(&prefix)
            .and_then(|s| s.split_once('/'))
        else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        if !matches!(phase, "start" | "end") {
            continue;
        }
        let turn = turns.entry(number).or_insert(Turn {
            number,
            start: None,
            end: None,
            timestamp,
        });
        if phase == "start" {
            turn.start = Some(oid);
        } else {
            turn.end = Some(oid);
        }
    }
    Ok(turns.into_values().rev().collect())
}
/// Deletes refs only. Object reclamation belongs to normal git gc.
pub async fn cleanup(dir: &Path, chat: Option<&str>, now: i64) -> Result<(), String> {
    let prefix = match chat {
        Some(chat) => chat_prefix(chat)?,
        None => PREFIX.to_string(),
    };
    for (reference, oid, timestamp) in refs(dir, &prefix).await? {
        if chat.is_some() || timestamp < now - RETENTION_SECS {
            git(dir, None, &["update-ref", "-d", &reference, &oid]).await?;
        }
    }
    Ok(())
}

/// Owned by the harness loop: await begin before dispatching a prompt and end
/// before dispatching the next one. Unknown first-session ids bind on init.
#[derive(Default)]
pub struct Recorder {
    cwd: PathBuf,
    facts_dir: Option<PathBuf>,
    chat: Option<String>,
    number: Option<u64>,
    next_number: u64,
    starts: BTreeMap<PathBuf, String>,
    known: BTreeSet<PathBuf>,
    tracker: crate::worktree_follow::ToolInputTracker,
    closed: tokio_util::sync::CancellationToken,
}
impl Recorder {
    pub fn new(cwd: PathBuf, chat: Option<String>, facts_dir: Option<PathBuf>) -> Self {
        Self {
            cwd,
            chat,
            facts_dir,
            ..Self::default()
        }
    }
    pub fn with_close_token(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.closed = token;
        self
    }
    pub fn observe(&mut self, event: &crate::harness::HarnessEvent) {
        if self.facts_dir.is_none() {
            return;
        }
        for path in self.tracker.observe(event, dirs::home_dir().as_deref()) {
            let path = crate::worktree_follow::resolve_for_match(&path);
            if let Some(root) = path.ancestors().find(|dir| dir.join(".git").exists()) {
                self.known.insert(root.to_path_buf());
            }
        }
    }
    async fn publish(
        &self,
        dir: &Path,
        chat: &str,
        number: u64,
        phase: &str,
        oid: &str,
    ) -> Result<(), String> {
        if self.closed.is_cancelled() {
            return Ok(());
        }
        save(dir, chat, number, phase, oid).await?;
        // A tab can close while update-ref is in flight. Remove only our write.
        if self.closed.is_cancelled() {
            let reference = format!("{}{number}/{phase}", checkout_prefix(dir, chat).await?);
            git(dir, None, &["update-ref", "-d", &reference, oid]).await?;
        }
        Ok(())
    }
    pub async fn close(&mut self) {
        self.closed.cancel();
        if self.facts_dir.is_none() {
            return;
        }
        if let Some(chat) = &self.chat {
            let mut dirs = self.known.clone();
            dirs.extend(self.starts.keys().cloned());
            match self.checkouts().await {
                Ok(more) => dirs.extend(more),
                Err(e) => eprintln!("[turns] {e}"),
            }
            for dir in dirs {
                if let Err(e) = cleanup(&dir, Some(chat), chrono::Utc::now().timestamp()).await {
                    eprintln!("[turns] {e}");
                }
            }
        }
    }
    async fn checkouts(&self) -> Result<BTreeSet<PathBuf>, String> {
        let mut dirs = self.known.clone();
        if let Some(root) = self.cwd.ancestors().find(|dir| dir.join(".git").exists()) {
            dirs.insert(root.to_path_buf());
        }
        if let (Some(facts_dir), Some(chat)) = (&self.facts_dir, &self.chat) {
            let path =
                crate::chat_facts::facts_path(facts_dir, crate::chats::ChatBackend::Claude, chat)?;
            match tokio::fs::read(&path).await {
                Ok(bytes) => {
                    let facts: crate::chat_facts::ChatFacts = serde_json::from_slice(&bytes)
                        .map_err(|e| format!("read snapshot checkouts {}: {e}", path.display()))?;
                    dirs.extend(facts.checkouts.into_iter().map(|c| c.dir));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("read snapshot checkouts {}: {e}", path.display())),
            }
        }
        Ok(dirs)
    }
    pub async fn begin(&mut self) {
        if self.facts_dir.is_none() || self.number.is_some() || self.closed.is_cancelled() {
            return;
        }
        let dirs = match self.checkouts().await {
            Ok(dirs) => dirs,
            Err(e) => {
                eprintln!("[turns] {e}");
                return;
            }
        };
        let mut number = self.next_number.max(1);
        if let Some(chat) = &self.chat {
            for dir in &dirs {
                match turns(dir, chat).await {
                    Ok(turns) => {
                        if let Some(turn) = turns.first() {
                            number = number.max(turn.number.saturating_add(1));
                        }
                    }
                    Err(e) => eprintln!("[turns] {e}"),
                }
            }
        }
        self.number = Some(number);
        self.next_number = number.saturating_add(1);
        self.known.extend(dirs.iter().cloned());
        for dir in dirs {
            match snapshot(&dir).await {
                Ok(Some(oid)) => {
                    self.starts.insert(dir, oid);
                }
                Ok(None) => {}
                Err(e) => eprintln!("[turns] {e}"),
            }
        }
        self.publish_starts().await;
    }
    async fn publish_starts(&self) {
        if let (Some(chat), Some(number)) = (&self.chat, self.number) {
            for (dir, oid) in &self.starts {
                if let Err(e) = self.publish(dir, chat, number, "start", oid).await {
                    eprintln!("[turns] {e}");
                }
            }
        }
    }
    pub async fn bind(&mut self, chat: &str) {
        if self.chat.as_deref() != Some(chat) {
            self.chat = Some(chat.into());
            self.publish_starts().await;
        }
    }
    pub async fn end(&mut self) {
        if self.closed.is_cancelled() {
            self.close().await;
            self.number = None;
            self.starts.clear();
            return;
        }
        let Some(number) = self.number.take() else {
            return;
        };
        if let Some(chat) = &self.chat {
            let dirs = match self.checkouts().await {
                Ok(dirs) => dirs,
                Err(e) => {
                    eprintln!("[turns] {e}");
                    self.starts.keys().cloned().collect()
                }
            };
            for dir in dirs {
                if !self.starts.contains_key(&dir) {
                    eprintln!(
                        "[turns] {}: checkout discovered after turn start; no baseline",
                        dir.display()
                    );
                }
                match snapshot(&dir).await {
                    Ok(Some(oid)) => {
                        if let Err(e) = self.publish(&dir, chat, number, "end", &oid).await {
                            eprintln!("[turns] {e}");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("[turns] {e}"),
                }
            }
        }
        self.starts.clear();
    }
}

/// Sweep all persisted checkout facts too: closed tabs may no longer be loaded.
pub async fn maintain(
    facts_dir: PathBuf,
    mut dirs: BTreeSet<PathBuf>,
    chats: Option<Vec<String>>,
) -> Result<(), String> {
    let mut errors = Vec::new();
    match tokio::fs::read_dir(&facts_dir).await {
        Ok(mut entries) => {
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| format!("list snapshot checkouts: {e}"))?
            {
                if entry.path().extension().is_none_or(|ext| ext != "json") {
                    continue;
                }
                match tokio::fs::read(entry.path()).await {
                    Ok(bytes) => {
                        match serde_json::from_slice::<crate::chat_facts::ChatFacts>(&bytes) {
                            Ok(facts) => dirs.extend(facts.checkouts.into_iter().map(|c| c.dir)),
                            Err(e) => errors.push(format!(
                                "parse snapshot facts {}: {e}",
                                entry.path().display()
                            )),
                        }
                    }
                    Err(e) => errors.push(format!(
                        "read snapshot facts {}: {e}",
                        entry.path().display()
                    )),
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("list snapshot facts {}: {e}", facts_dir.display())),
    }
    let now = chrono::Utc::now().timestamp();
    for dir in dirs {
        if !dir.join(".git").exists() {
            continue;
        }
        if let Some(chats) = &chats {
            for chat in chats {
                if let Err(e) = cleanup(&dir, Some(chat), now).await {
                    errors.push(e);
                }
            }
        } else if let Err(e) = cleanup(&dir, None, now).await {
            errors.push(e);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn g(dir: &Path, args: &[&str]) -> String {
        let out = crate::agentd::git::git_command()
            .current_dir(dir)
            .args([
                "--no-optional-locks",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        g(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("file.txt"), "base\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored\n").unwrap();
        g(dir.path(), &["add", "."]);
        g(dir.path(), &["commit", "-qm", "fixture"]);
        dir
    }
    #[tokio::test]
    async fn snapshot_publication_resolves_option_looking_revisions_as_commits() {
        let temp = repo();
        let dir = temp.path();
        g(dir, &["update-ref", "refs/heads/--stdin", "HEAD"]);
        save(dir, "option-test", 1, "start", "--stdin")
            .await
            .unwrap();
        let listed = turns(dir, "option-test").await.unwrap();
        assert_eq!(listed[0].start, Some(g(dir, &["rev-parse", "HEAD"])));
        assert!(save(dir, "option-test", 2, "start", "--not-a-revision")
            .await
            .is_err());
        let tree = g(dir, &["rev-parse", "HEAD^{tree}"]);
        assert!(save(dir, "option-test", 2, "start", &tree).await.is_err());
        assert_eq!(turns(dir, "option-test").await.unwrap().len(), 1);
    }

    #[test]
    fn fixture_commands_ignore_inherited_git_repository() {
        let sentinel = repo();
        let d = sentinel.path();
        let index = std::fs::read(d.join(".git/index")).unwrap();
        let config = std::fs::read(d.join(".git/config")).unwrap();
        let head = g(d, &["rev-parse", "HEAD"]);
        let branches = g(d, &["branch", "--list"]);
        // Git exports these to commit hooks. Reproduce in a subprocess so
        // parallel tests never share a modified process environment.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "turn_snapshots::tests::snapshots_preserve_user_state_and_diff_untracked_files",
                "--nocapture",
            ])
            .env("GIT_DIR", d.join(".git"))
            .env("GIT_COMMON_DIR", d.join(".git"))
            .env("GIT_WORK_TREE", d)
            .env("GIT_INDEX_FILE", d.join(".git/index"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(d.join(".git/index")).unwrap(), index);
        assert_eq!(std::fs::read(d.join(".git/config")).unwrap(), config);
        assert_eq!(g(d, &["rev-parse", "HEAD"]), head);
        assert_eq!(g(d, &["branch", "--list"]), branches);
    }
    #[tokio::test]
    async fn snapshots_preserve_user_state_and_diff_untracked_files() {
        let dir = repo();
        let d = dir.path();
        std::fs::write(d.join("file.txt"), "staged\n").unwrap();
        g(d, &["add", "file.txt"]);
        std::fs::write(d.join("file.txt"), "before\n").unwrap();
        std::fs::write(d.join("new.txt"), "untracked\n").unwrap();
        std::fs::write(d.join("ignored"), "private\n").unwrap();
        let index = std::fs::read(d.join(".git/index")).unwrap();
        let status = g(d, &["status", "--porcelain=v1", "--untracked-files=all"]);
        let head = g(d, &["rev-parse", "HEAD"]);
        let branches = g(d, &["branch", "--list"]);
        let start = snapshot(d).await.unwrap().unwrap();
        save(d, "chat", 1, "start", &start).await.unwrap();
        assert_eq!(std::fs::read(d.join(".git/index")).unwrap(), index);
        assert_eq!(
            g(d, &["status", "--porcelain=v1", "--untracked-files=all"]),
            status
        );
        assert_eq!(g(d, &["rev-parse", "HEAD"]), head);
        assert_eq!(g(d, &["branch", "--list"]), branches);
        assert_eq!(
            std::fs::read_to_string(d.join("file.txt")).unwrap(),
            "before\n"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("ignored")).unwrap(),
            "private\n"
        );
        assert_eq!(g(d, &["show", &format!("{start}:new.txt")]), "untracked");
        assert!(!g(d, &["ls-tree", "-r", "--name-only", &start])
            .lines()
            .any(|p| p == "ignored"));
        std::fs::write(d.join("file.txt"), "after\nextra\n").unwrap();
        let end = snapshot(d).await.unwrap().unwrap();
        save(d, "chat", 1, "end", &end).await.unwrap();
        let turns = turns(d, "chat").await.unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].end.as_deref(), Some(end.as_str()));
        std::fs::write(d.join("file.txt"), "later unrelated edit\n").unwrap();
        let target = crate::review_pane::ReviewTarget {
            checkout: d.into(),
            repo: "fixture".into(),
            branch: "main".into(),
            owner: "chat".into(),
            base: None,
        };
        let load = crate::review_pane::load_review_source(
            target,
            crate::review_pane::ReviewSource::Turns {
                chat: "chat".into(),
                selected: Some(1),
            },
        )
        .await
        .unwrap();
        let file = load.diff.file("file.txt").unwrap();
        assert_eq!(
            file.content_id.as_deref(),
            Some(g(d, &["rev-parse", &format!("{end}:file.txt")]).as_str())
        );
        assert_eq!((file.additions, file.deletions), (2, 1));
        let lines: Vec<_> = file
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(lines, vec!["before", "after", "extra"]);
        assert!(load.diff.file("new.txt").is_none());
        assert_eq!(std::fs::read(d.join(".git/index")).unwrap(), index);
    }
    #[tokio::test]
    async fn ordinary_and_all_pushes_do_not_send_snapshot_refs() {
        let dir = repo();
        let remote = tempfile::tempdir().unwrap();
        g(remote.path(), &["init", "--bare", "-q"]);
        g(
            dir.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        g(dir.path(), &["push", "-u", "origin", "main"]);
        let oid = snapshot(dir.path()).await.unwrap().unwrap();
        save(dir.path(), "chat", 1, "start", &oid).await.unwrap();
        g(dir.path(), &["push"]);
        g(dir.path(), &["push", "--all"]);
        assert!(g(remote.path(), &["for-each-ref", PREFIX]).is_empty());
        assert!(!g(remote.path(), &["rev-list", "--all"])
            .lines()
            .any(|line| line == oid));
    }
    #[tokio::test]
    async fn cleanup_is_chat_scoped_and_expires_after_fourteen_days() {
        let dir = repo();
        let d = dir.path();
        let oid = snapshot(d).await.unwrap().unwrap();
        for chat in ["a", "ab", "keep"] {
            save(d, chat, 1, "start", &oid).await.unwrap();
            save(d, chat, 1, "end", &oid).await.unwrap();
        }
        cleanup(d, Some("a"), chrono::Utc::now().timestamp())
            .await
            .unwrap();
        assert!(turns(d, "a").await.unwrap().is_empty());
        assert_eq!(turns(d, "ab").await.unwrap().len(), 1);
        let now = chrono::Utc::now().timestamp();
        cleanup(d, None, now + RETENTION_SECS - 10).await.unwrap();
        assert_eq!(turns(d, "keep").await.unwrap().len(), 1);
        cleanup(d, None, now + RETENTION_SECS + 10).await.unwrap();
        assert!(refs(d, PREFIX).await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn merge_rebase_and_oversized_repositories_are_skipped() {
        let dir = repo();
        let d = dir.path();
        for state in ["rebase-merge", "rebase-apply"] {
            std::fs::create_dir(d.join(".git").join(state)).unwrap();
            assert!(snapshot(d).await.unwrap().is_none());
            std::fs::remove_dir(d.join(".git").join(state)).unwrap();
        }
        std::fs::write(d.join(".git/MERGE_HEAD"), "fixture").unwrap();
        assert!(snapshot(d).await.unwrap().is_none());
        std::fs::remove_file(d.join(".git/MERGE_HEAD")).unwrap();
        let big = std::fs::File::create(d.join("big.bin")).unwrap();
        big.set_len(MAX_BYTES + 1).unwrap();
        assert!(snapshot(d).await.unwrap().is_none());
        assert!(refs(d, PREFIX).await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn recorder_brackets_turns_and_resumes_numbering() {
        let dir = repo();
        let facts = tempfile::tempdir().unwrap();
        let mut recorder = Recorder::new(dir.path().into(), None, Some(facts.path().into()));
        recorder.begin().await;
        recorder.bind("chat").await;
        std::fs::write(dir.path().join("new"), "turn one").unwrap();
        recorder.end().await;
        let mut resumed = Recorder::new(
            dir.path().into(),
            Some("chat".into()),
            Some(facts.path().into()),
        );
        resumed.begin().await;
        resumed.end().await;
        let turns = turns(dir.path(), "chat").await.unwrap();
        assert_eq!(
            turns.iter().map(|t| t.number).collect::<Vec<_>>(),
            vec![2, 1]
        );
        maintain(
            facts.path().into(),
            [dir.path().into()].into(),
            Some(vec!["chat".into()]),
        )
        .await
        .unwrap();
        assert!(super::turns(dir.path(), "chat").await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn closing_a_running_recorder_does_not_recreate_refs() {
        let dir = repo();
        let facts = tempfile::tempdir().unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        let mut recorder = Recorder::new(
            dir.path().into(),
            Some("closing".into()),
            Some(facts.path().into()),
        )
        .with_close_token(token.clone());
        recorder.begin().await;
        assert_eq!(turns(dir.path(), "closing").await.unwrap().len(), 1);
        token.cancel();
        cleanup(dir.path(), Some("closing"), chrono::Utc::now().timestamp())
            .await
            .unwrap();
        recorder.end().await;
        recorder.begin().await;
        assert!(turns(dir.path(), "closing").await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn linked_worktree_and_index_flags_preserve_the_real_index() {
        let dir = repo();
        let linked = tempfile::tempdir().unwrap();
        let d = linked.path().join("linked");
        g(
            dir.path(),
            &["worktree", "add", "-qb", "feature", d.to_str().unwrap()],
        );
        g(&d, &["update-index", "--split-index"]);
        g(&d, &["update-index", "--assume-unchanged", "file.txt"]);
        std::fs::write(d.join("file.txt"), "changed despite flag\n").unwrap();
        let index_path = g(
            &d,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        );
        let index = std::fs::read(&index_path).unwrap();
        let oid = snapshot(&d).await.unwrap().unwrap();
        assert_eq!(
            g(&d, &["show", &format!("{oid}:file.txt")]),
            "changed despite flag"
        );
        assert_eq!(std::fs::read(index_path).unwrap(), index);
        assert_eq!(g(&d, &["branch", "--show-current"]), "feature");
        save(&d, "shared-chat", 1, "start", &oid).await.unwrap();
        let main = snapshot(dir.path()).await.unwrap().unwrap();
        save(dir.path(), "shared-chat", 1, "start", &main)
            .await
            .unwrap();
        assert_eq!(
            turns(&d, "shared-chat").await.unwrap()[0].start.as_deref(),
            Some(oid.as_str())
        );
        assert_eq!(
            turns(dir.path(), "shared-chat").await.unwrap()[0]
                .start
                .as_deref(),
            Some(main.as_str())
        );
        assert_ne!(oid, main);
    }
}
