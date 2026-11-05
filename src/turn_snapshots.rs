//! Local turn history in a private repository outside source refs/objects; never checkout,
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
static MAINTENANCE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

async fn checkout_key(dir: &Path) -> Result<String, String> {
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
    Ok(key)
}
async fn checkout_prefix(dir: &Path, chat: &str) -> Result<String, String> {
    Ok(format!(
        "{}{}/",
        chat_prefix(chat)?,
        checkout_key(dir).await?
    ))
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
    git_run(dir, index, None, args, input).await
}
async fn git_run(
    dir: &Path,
    index: Option<&Path>,
    objects: Option<(&Path, &Path)>,
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
            "-c",
            "core.logAllRefUpdates=false",
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
    if let Some((private, source)) = objects {
        // Git accepts C-quoted alternate paths, including path separators.
        let mut alternate = String::from("\"");
        for byte in source.as_os_str().as_encoded_bytes() {
            match byte {
                b'"' => alternate.push_str("\\\""),
                b'\\' => alternate.push_str("\\\\"),
                32..=126 => alternate.push(*byte as char),
                _ => alternate.push_str(&format!("\\{byte:03o}")),
            }
        }
        alternate.push('"');
        command
            .env("GIT_OBJECT_DIRECTORY", private)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate);
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
#[cfg(test)]
fn store_root() -> Result<PathBuf, String> {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    Ok(ROOT
        .get_or_init(|| tempfile::tempdir().expect("private snapshot test root"))
        .path()
        .join("turn-snapshots"))
}
#[cfg(not(test))]
fn store_root() -> Result<PathBuf, String> {
    let config = match std::env::var_os("GITTERM_V6_CONFIG_DIR").filter(|s| !s.is_empty()) {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("snapshot config directory must be absolute".into());
            }
            path
        }
        None => dirs::home_dir()
            .ok_or("snapshot config directory: no home directory")?
            .join(".config/gitterm-v6"),
    };
    Ok(config.join("turn-snapshots"))
}
async fn common_dir(dir: &Path) -> Result<PathBuf, String> {
    let path = text(
        dir,
        None,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    tokio::fs::canonicalize(&path)
        .await
        .map_err(|e| format!("resolve snapshot repository {path}: {e}"))
}
/// Separate repository: the source has neither snapshot refs nor snapshot-only
/// objects, nor an alternate pointing back here. Remote push configuration can
/// only enumerate and read the source's own namespace and object database.
pub async fn store_path(dir: &Path) -> Result<PathBuf, String> {
    let common = common_dir(dir).await?;
    let digest = ring::digest::digest(&ring::digest::SHA256, common.as_os_str().as_encoded_bytes());
    let key: String = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let root = crate::worktree_follow::resolve_for_match(&store_root()?);
    let checkout = text(dir, None, &["rev-parse", "--show-toplevel"]).await?;
    let checkout = tokio::fs::canonicalize(&checkout)
        .await
        .map_err(|e| format!("resolve snapshot checkout {checkout}: {e}"))?;
    if root.starts_with(&checkout) {
        return Err(format!(
            "snapshot store {} must be outside checkout {}",
            root.display(),
            checkout.display()
        ));
    }
    Ok(root.join(key))
}
async fn ensure_store(dir: &Path) -> Result<PathBuf, String> {
    let store = store_path(dir).await?;
    if tokio::fs::try_exists(store.join("HEAD"))
        .await
        .map_err(|e| format!("stat snapshot store {}: {e}", store.display()))?
    {
        return Ok(store);
    }
    let root = store.parent().ok_or("snapshot store has no parent")?;
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(root)
        .await
        .map_err(|e| format!("create snapshot store directory {}: {e}", root.display()))?;
    let temp = TemporaryIndex(root.join(format!(".init-{}", uuid::Uuid::new_v4())));
    builder
        .create(&temp.0)
        .await
        .map_err(|e| format!("create snapshot store {}: {e}", temp.0.display()))?;
    let format = text(dir, None, &["rev-parse", "--show-object-format"]).await?;
    git(
        &temp.0,
        None,
        &[
            "init",
            "--bare",
            "--template=",
            &format!("--object-format={format}"),
        ],
    )
    .await?;
    match tokio::fs::rename(&temp.0, &store).await {
        Ok(()) => std::mem::forget(temp),
        Err(e) => {
            if !tokio::fs::try_exists(store.join("HEAD"))
                .await
                .map_err(|err| {
                    format!("stat concurrent snapshot store {}: {err}", store.display())
                })?
            {
                return Err(format!("publish snapshot store {}: {e}", store.display()));
            }
        }
    }
    Ok(store)
}
async fn capture_input(
    dir: &Path,
    index: Option<&Path>,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let store = ensure_store(dir).await?;
    let source = common_dir(dir).await?.join("objects");
    git_run(
        dir,
        index,
        Some((&store.join("objects"), &source)),
        args,
        input,
    )
    .await
}
async fn capture(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>, String> {
    capture_input(dir, index, args, None).await
}
async fn capture_text(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<String, String> {
    String::from_utf8(capture(dir, index, args).await?)
        .map(|s| s.trim().into())
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
    remove_legacy_refs(dir).await?;
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
            capture(dir, Some(&index), &["read-tree", "--empty"]).await?;
        }
        Err(e) => return Err(format!("copy snapshot index {real_index}: {e}")),
    }
    let tracked = capture(dir, Some(&index), &["ls-files", "--cached", "-z"]).await?;
    // Only the private copy loses these flags: they must not conceal edits.
    capture_input(
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
    capture(dir, Some(&index), &["add", "-A", "--", "."]).await?;
    let tree = capture_text(dir, Some(&index), &["write-tree"]).await?;
    // No parent links: expiration can release every snapshot independently.
    let oid = capture_text(
        dir,
        Some(&index),
        &["commit-tree", &tree, "-m", "GitTerm local turn snapshot"],
    )
    .await?;
    materialize(dir, &ensure_store(dir).await?, &oid).await?;
    Ok(Some(oid))
}

async fn materialize(dir: &Path, store: &Path, oid: &str) -> Result<(), String> {
    let source = common_dir(dir).await?.join("objects");
    git_run(
        store,
        None,
        Some((&store.join("objects"), &source)),
        &["pack-objects", "--revs", "objects/pack/pack"],
        Some(format!("{oid}\n").as_bytes()),
    )
    .await?;
    Ok(())
}

pub async fn save(dir: &Path, chat: &str, turn: u64, phase: &str, oid: &str) -> Result<(), String> {
    if !matches!(phase, "start" | "end") {
        return Err("invalid snapshot phase".into());
    }
    let reference = format!("{}{turn}/{phase}", checkout_prefix(dir, chat).await?);
    let oid = capture_text(
        dir,
        None,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{oid}^{{commit}}"),
        ],
    )
    .await?;
    let store = ensure_store(dir).await?;
    // The copied index can reuse source blobs. Materialize the full closure
    // before publishing, so reads never depend on source objects or source GC.
    materialize(dir, &store, &oid).await?;
    git(&store, None, &["update-ref", &reference, &oid]).await?;
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
    let store = store_path(dir).await?;
    if !tokio::fs::try_exists(&store)
        .await
        .map_err(|e| format!("stat snapshot store {}: {e}", store.display()))?
    {
        return Ok(Vec::new());
    }
    store_refs(&store, prefix).await
}
async fn store_refs(dir: &Path, prefix: &str) -> Result<Vec<(String, String, i64)>, String> {
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
/// Legacy refs are unsafe even before expiration. Never prune user objects.
async fn remove_legacy_refs(dir: &Path) -> Result<(), String> {
    for (reference, oid, _) in store_refs(dir, PREFIX).await? {
        git(dir, None, &["update-ref", "-d", &reference, &oid]).await?;
    }
    Ok(())
}
async fn cleanup_store(store: &Path, chat: Option<&str>, now: i64) -> Result<(), String> {
    let _guard = MAINTENANCE.lock().await;
    let prefix = match chat {
        Some(chat) => chat_prefix(chat)?,
        None => PREFIX.to_string(),
    };
    for (reference, oid, timestamp) in store_refs(store, &prefix).await? {
        if chat.is_some() || timestamp < now - RETENTION_SECS {
            git(store, None, &["update-ref", "-d", &reference, &oid]).await?;
        }
    }
    // Private GC, with Git's normal grace period protecting in-flight captures.
    git(store, None, &["gc", "--prune=2.weeks.ago"]).await?;
    Ok(())
}
pub async fn cleanup(dir: &Path, chat: Option<&str>, now: i64) -> Result<(), String> {
    remove_legacy_refs(dir).await?;
    let store = store_path(dir).await?;
    if tokio::fs::try_exists(&store)
        .await
        .map_err(|e| format!("stat snapshot store {}: {e}", store.display()))?
    {
        cleanup_store(&store, chat, now).await?;
    }
    Ok(())
}

struct CapturedSnapshot {
    oid: String,
    store: PathBuf,
    checkout_key: String,
}
impl CapturedSnapshot {
    async fn capture(dir: &Path) -> Result<Option<Self>, String> {
        let Some(oid) = snapshot(dir).await? else {
            return Ok(None);
        };
        Ok(Some(Self {
            oid,
            store: store_path(dir).await?,
            checkout_key: checkout_key(dir).await?,
        }))
    }
    fn reference(&self, chat: &str, number: u64, phase: &str) -> Result<String, String> {
        Ok(format!(
            "{}{}/{number}/{phase}",
            chat_prefix(chat)?,
            self.checkout_key
        ))
    }
}

/// Owned by the harness loop: capture while execution is paused by a callback
/// (Claude) or before dispatching the next prompt (ACP).
#[derive(Default)]
pub struct Recorder {
    cwd: PathBuf,
    facts_dir: Option<PathBuf>,
    chat: Option<String>,
    number: Option<u64>,
    next_number: u64,
    starts: BTreeMap<PathBuf, CapturedSnapshot>,
    ends: BTreeMap<PathBuf, CapturedSnapshot>,
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
        snapshot: &CapturedSnapshot,
        chat: &str,
        number: u64,
        phase: &str,
    ) -> Result<(), String> {
        // Serialize ref publication and deletion. A cancelled recorder must not
        // recreate refs after a cleanup that overtook an in-flight capture.
        let _guard = MAINTENANCE.lock().await;
        if self.closed.is_cancelled() {
            return Ok(());
        }
        let reference = snapshot.reference(chat, number, phase)?;
        git(
            &snapshot.store,
            None,
            &["update-ref", &reference, &snapshot.oid],
        )
        .await?;
        if self.closed.is_cancelled() {
            git(
                &snapshot.store,
                None,
                &["update-ref", "-d", &reference, &snapshot.oid],
            )
            .await?;
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
            if let Some(facts_dir) = &self.facts_dir {
                if let Err(e) = maintain(facts_dir.clone(), dirs, Some(vec![chat.clone()])).await {
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
            match CapturedSnapshot::capture(&dir).await {
                Ok(Some(snapshot)) => {
                    self.starts.insert(dir, snapshot);
                }
                Ok(None) => {}
                Err(e) => eprintln!("[turns] {e}"),
            }
        }
        self.publish_starts().await;
    }
    async fn publish_starts(&self) {
        if let (Some(chat), Some(number)) = (&self.chat, self.number) {
            for snapshot in self.starts.values() {
                if let Err(e) = self.publish(snapshot, chat, number, "start").await {
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
    pub fn invalidate_end(&mut self) {
        self.ends.clear();
    }
    /// Capture under a synchronous execution barrier. Stop hooks can continue;
    /// keep the turn open until its result finalizes the latest candidate.
    pub async fn capture_end(&mut self) {
        self.ends.clear();
        if self.closed.is_cancelled() {
            self.discard_turn();
            return;
        }
        let Some(_) = self.number else {
            return;
        };
        if self.chat.is_some() {
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
                match CapturedSnapshot::capture(&dir).await {
                    Ok(Some(snapshot)) => {
                        self.ends.insert(dir, snapshot);
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("[turns] {e}"),
                }
            }
        }
    }
    /// No filesystem capture here: the CLI may already be in its next turn.
    pub async fn finish(&mut self) {
        if !self.closed.is_cancelled() {
            if let (Some(chat), Some(number)) = (&self.chat, self.number) {
                for snapshot in self.ends.values() {
                    if let Err(e) = self.publish(snapshot, chat, number, "end").await {
                        eprintln!("[turns] {e}");
                    }
                }
                if self.ends.is_empty() {
                    eprintln!("[turns] turn {number} incomplete: no synchronous end snapshot");
                }
            }
        }
        self.discard_turn();
    }
    pub fn discard_turn(&mut self) {
        self.number = None;
        self.starts.clear();
        self.ends.clear();
    }
    pub async fn end(&mut self) {
        self.capture_end().await;
        self.finish().await;
        if self.closed.is_cancelled() {
            self.close().await;
        }
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
                            Ok(facts) => dirs
                                .extend(facts.checkouts.into_iter().flat_map(|c| [c.dir, c.repo])),
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
        if dir.join(".git").exists() {
            if let Err(e) = remove_legacy_refs(&dir).await {
                errors.push(e);
            }
        }
    }
    let root = store_root()?;
    match tokio::fs::read_dir(&root).await {
        Ok(mut entries) => {
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| format!("list private snapshot stores {}: {e}", root.display()))?
            {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                    continue;
                }
                if let Some(chats) = &chats {
                    for chat in chats {
                        if let Err(e) = cleanup_store(&path, Some(chat), now).await {
                            errors.push(e);
                        }
                    }
                } else if let Err(e) = cleanup_store(&path, None, now).await {
                    errors.push(e);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => errors.push(format!(
            "list private snapshot stores {}: {e}",
            root.display()
        )),
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
        assert_eq!(
            g(
                &store_path(d).await.unwrap(),
                &["show", &format!("{start}:new.txt")]
            ),
            "untracked"
        );
        assert!(!g(
            &store_path(d).await.unwrap(),
            &["ls-tree", "-r", "--name-only", &start]
        )
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
            Some(
                g(
                    &store_path(d).await.unwrap(),
                    &["rev-parse", &format!("{end}:file.txt")]
                )
                .as_str()
            )
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
    async fn configured_push_keeps_snapshots_local(config: &[&str]) {
        for configure_before in [true, false] {
            let repo = repo();
            let dir = repo.path();
            let remote = tempfile::tempdir().unwrap();
            g(remote.path(), &["init", "--bare", "-q"]);
            g(
                dir,
                &["remote", "add", "origin", remote.path().to_str().unwrap()],
            );
            g(dir, &["push", "-u", "origin", "main"]);
            if configure_before {
                g(dir, config);
            }
            let index = std::fs::read(dir.join(".git/index")).unwrap();
            let source_config = std::fs::read(dir.join(".git/config")).unwrap();
            let alternates = dir.join(".git/objects/info/alternates");
            assert!(!alternates.exists());
            std::fs::write(dir.join("private-untracked"), "snapshot-only-content").unwrap();
            let oid = snapshot(dir).await.unwrap().unwrap();
            save(dir, "private-chat", 1, "start", &oid).await.unwrap();
            // Configure after capture too: retained snapshots must stay private.
            assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), index);
            assert_eq!(
                std::fs::read(dir.join(".git/config")).unwrap(),
                source_config
            );
            assert!(!alternates.exists());
            assert!(g(dir, &["for-each-ref", PREFIX]).is_empty());
            if !configure_before {
                g(dir, config);
            }
            g(dir, &["push"]);
            assert!(g(remote.path(), &["for-each-ref", PREFIX]).is_empty());
            for object in [&oid, &g(dir, &["hash-object", "private-untracked"])] {
                for location in [dir, remote.path()] {
                    let out = crate::agentd::git::git_command()
                        .current_dir(location)
                        .args(["cat-file", "-e", object])
                        .output()
                        .unwrap();
                    assert!(
                        !out.status.success(),
                        "snapshot object {object} reached {}",
                        location.display()
                    );
                }
            }
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn alternate_paths_preserve_separators_and_quotes() {
        let repo = repo();
        let holder = tempfile::tempdir().unwrap();
        let dir = holder.path().join("repo:quote\"");
        std::fs::rename(repo.path(), &dir).unwrap();
        let oid = snapshot(&dir).await.unwrap().unwrap();
        save(&dir, "alternate-path-chat", 1, "start", &oid)
            .await
            .unwrap();
        assert_eq!(
            g(
                &store_path(&dir).await.unwrap(),
                &["show", &format!("{oid}:file.txt")]
            ),
            "base"
        );
    }
    #[tokio::test]
    async fn private_history_survives_source_object_removal_and_removes_recent_legacy_refs() {
        let repo = repo();
        let dir = repo.path();
        let legacy = format!("{PREFIX}legacy/1/start");
        g(dir, &["update-ref", &legacy, "HEAD"]);
        let oid = snapshot(dir).await.unwrap().unwrap();
        save(dir, "self-contained", 1, "start", &oid).await.unwrap();
        assert!(g(dir, &["for-each-ref", PREFIX]).is_empty());
        let store = store_path(dir).await.unwrap();
        assert!(!store.join("objects/info/alternates").exists());
        std::fs::rename(dir.join(".git/objects"), dir.join("source-objects-away")).unwrap();
        assert_eq!(g(&store, &["show", &format!("{oid}:file.txt")]), "base");
        g(&store, &["fsck", "--full"]);
    }
    #[tokio::test]
    async fn mirror_push_keeps_snapshot_objects_and_refs_local() {
        configured_push_keeps_snapshots_local(&["config", "remote.origin.mirror", "true"]).await;
    }
    #[tokio::test]
    async fn wildcard_push_keeps_snapshot_objects_and_refs_local() {
        configured_push_keeps_snapshots_local(&["config", "remote.origin.push", "refs/*:refs/*"])
            .await;
    }
    #[tokio::test]
    async fn maintenance_sweeps_removed_worktree_via_surviving_repo() {
        for (chat_scoped, remove_repo) in [(true, false), (false, false), (false, true)] {
            let repo = repo();
            let linked = tempfile::tempdir().unwrap();
            let dir = linked.path().join("linked");
            g(
                repo.path(),
                &["worktree", "add", "-qb", "feature", dir.to_str().unwrap()],
            );
            let tree = g(&dir, &["rev-parse", "HEAD^{tree}"]);
            let output = crate::agentd::git::git_command()
                .current_dir(&dir)
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit-tree",
                    &tree,
                    "-m",
                    "expired snapshot",
                ])
                .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
                .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
                .output()
                .unwrap();
            assert!(output.status.success());
            let oid = String::from_utf8(output.stdout).unwrap().trim().to_string();
            save(&dir, "removed-chat", 1, "start", &oid).await.unwrap();
            let legacy = format!(
                "{}1/start",
                checkout_prefix(&dir, "legacy-removed-chat").await.unwrap()
            );
            g(&dir, &["update-ref", &legacy, &oid]);
            let facts_dir = tempfile::tempdir().unwrap();
            let facts = crate::chat_facts::ChatFacts {
                checkouts: vec![crate::chat_facts::CheckoutTouch {
                    repo: repo.path().into(),
                    dir: dir.clone(),
                    branch: None,
                    edits: 1,
                    last: String::new(),
                }],
                ..Default::default()
            };
            std::fs::write(
                facts_dir.path().join("claude-removed-chat.json"),
                serde_json::to_vec(&facts).unwrap(),
            )
            .unwrap();
            let store = store_path(&dir).await.unwrap();
            g(repo.path(), &["worktree", "remove", dir.to_str().unwrap()]);
            if remove_repo {
                std::fs::remove_dir_all(repo.path()).unwrap();
            }
            maintain(
                facts_dir.path().into(),
                BTreeSet::new(),
                chat_scoped.then(|| vec!["removed-chat".into()]),
            )
            .await
            .unwrap();
            assert!(store_refs(&store, PREFIX).await.unwrap().is_empty());
            if !remove_repo {
                assert!(g(repo.path(), &["for-each-ref", PREFIX]).is_empty());
            }
        }
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
        recorder.bind("recorder-chat").await;
        std::fs::write(dir.path().join("new"), "turn one").unwrap();
        recorder.end().await;
        let mut resumed = Recorder::new(
            dir.path().into(),
            Some("recorder-chat".into()),
            Some(facts.path().into()),
        );
        resumed.begin().await;
        resumed.end().await;
        let turns = turns(dir.path(), "recorder-chat").await.unwrap();
        assert_eq!(
            turns.iter().map(|t| t.number).collect::<Vec<_>>(),
            vec![2, 1]
        );
        maintain(
            facts.path().into(),
            [dir.path().into()].into(),
            Some(vec!["recorder-chat".into()]),
        )
        .await
        .unwrap();
        assert!(super::turns(dir.path(), "recorder-chat")
            .await
            .unwrap()
            .is_empty());
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
    async fn stop_candidates_replace_continuations_and_missing_stop_stays_incomplete() {
        let dir = repo();
        let facts = tempfile::tempdir().unwrap();
        let mut recorder = Recorder::new(
            dir.path().into(),
            Some("candidate-chat".into()),
            Some(facts.path().into()),
        );
        recorder.begin().await;
        std::fs::write(dir.path().join("file.txt"), "first stop").unwrap();
        recorder.capture_end().await;
        recorder.invalidate_end();
        std::fs::write(dir.path().join("file.txt"), "continued stop").unwrap();
        recorder.capture_end().await;
        // The next CLI turn may already have written by the time result is read.
        std::fs::write(dir.path().join("file.txt"), "later unrelated work").unwrap();
        recorder.finish().await;
        let recorded = turns(dir.path(), "candidate-chat").await.unwrap();
        let store = store_path(dir.path()).await.unwrap();
        assert_eq!(
            g(
                &store,
                &[
                    "show",
                    &format!("{}:file.txt", recorded[0].end.as_deref().unwrap())
                ]
            ),
            "continued stop"
        );
        recorder.begin().await;
        std::fs::write(dir.path().join("file.txt"), "interrupted edit").unwrap();
        recorder.finish().await;
        let recorded = turns(dir.path(), "candidate-chat").await.unwrap();
        assert!(recorded[0].start.is_some());
        assert!(recorded[0].end.is_none());
    }

    #[tokio::test]
    async fn result_publication_uses_only_already_captured_private_objects() {
        let repo = repo();
        let facts = tempfile::tempdir().unwrap();
        let mut recorder = Recorder::new(
            repo.path().into(),
            Some("private-finish-chat".into()),
            Some(facts.path().into()),
        );
        recorder.begin().await;
        std::fs::write(repo.path().join("file.txt"), "captured end").unwrap();
        recorder.capture_end().await;
        let store = store_path(repo.path()).await.unwrap();
        let prefix = checkout_prefix(repo.path(), "private-finish-chat")
            .await
            .unwrap();
        std::fs::rename(
            repo.path().join(".git/objects"),
            repo.path().join("source-objects-away"),
        )
        .unwrap();
        recorder.finish().await;
        let recorded = store_refs(&store, &prefix).await.unwrap();
        let end = recorded
            .iter()
            .find(|(reference, _, _)| reference.ends_with("/end"));
        assert!(
            end.is_some(),
            "result must publish the materialized candidate without consulting source objects"
        );
        assert_eq!(
            g(&store, &["show", &format!("{}:file.txt", end.unwrap().1)]),
            "captured end"
        );
    }
    #[tokio::test]
    async fn cancelled_finish_does_not_wait_for_global_cleanup() {
        let repo = repo();
        let facts = tempfile::tempdir().unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        let mut recorder = Recorder::new(
            repo.path().into(),
            Some("cancel-deadline-chat".into()),
            Some(facts.path().into()),
        )
        .with_close_token(token.clone());
        recorder.begin().await;
        let _slow_cleanup = MAINTENANCE.lock().await;
        token.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), recorder.finish())
                .await
                .is_ok(),
            "cancelled result handling must not await GC"
        );
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
        save(&d, "shared-chat", 1, "start", &oid).await.unwrap();
        assert_eq!(
            g(
                &store_path(&d).await.unwrap(),
                &["show", &format!("{oid}:file.txt")]
            ),
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
