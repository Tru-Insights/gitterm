//! Iced composition adapter. Core never discovers the production snapshot root.
use gitterm_core::turn_snapshots::{self as core, SnapshotRoots};
pub use gitterm_core::turn_snapshots::{
    Snapshot, Turn, MAX_BYTES, MAX_FILES, PREFIX, RETENTION_SECS,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[cfg(test)]
fn store_root() -> Result<PathBuf, String> {
    thread_local! {
        static ROOT: tempfile::TempDir = tempfile::tempdir().expect("private snapshot test root");
    }
    Ok(ROOT.with(|root| root.path().join("turn-snapshots")))
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

fn explicit_root() -> Result<SnapshotRoots, String> {
    SnapshotRoots::new(store_root()?, std::env::temp_dir())
}

pub async fn store_path(dir: &Path) -> Result<PathBuf, String> {
    core::store_path(&explicit_root()?, dir).await
}
pub async fn snapshot(dir: &Path) -> Result<Option<Snapshot>, String> {
    core::snapshot(&explicit_root()?, dir).await
}
pub async fn save(dir: &Path, chat: &str, turn: u64, phase: &str, oid: &str) -> Result<(), String> {
    core::save(&explicit_root()?, dir, chat, turn, phase, oid).await
}
pub async fn turns(dir: &Path, chat: &str) -> Result<Vec<Turn>, String> {
    core::turns(&explicit_root()?, dir, chat).await
}
pub async fn cleanup(dir: &Path, chat: Option<&str>, now: i64) -> Result<(), String> {
    core::cleanup(&explicit_root()?, dir, chat, now).await
}
pub async fn maintain(
    facts_dir: PathBuf,
    dirs: BTreeSet<PathBuf>,
    chats: Option<Vec<String>>,
) -> Result<(), String> {
    core::maintain(&explicit_root()?, facts_dir, dirs, chats).await
}

/// Invalid legacy config disables recording, as failed captures did before.
/// It must never manufacture an unrooted core recorder or choose a fallback.
#[derive(Default)]
pub struct Recorder {
    inner: Option<core::Recorder>,
    closed: tokio_util::sync::CancellationToken,
}
impl Recorder {
    pub fn new(cwd: PathBuf, chat: Option<String>, facts_dir: Option<PathBuf>) -> Self {
        Self::with_root(explicit_root(), cwd, chat, facts_dir)
    }
    fn with_root(
        root: Result<SnapshotRoots, String>,
        cwd: PathBuf,
        chat: Option<String>,
        facts_dir: Option<PathBuf>,
    ) -> Self {
        let mut recorder = Self::default();
        match root {
            Ok(root) => {
                recorder.inner = Some(
                    core::Recorder::new(root, cwd, chat, facts_dir, dirs::home_dir())
                        .with_close_token(recorder.closed.clone()),
                );
            }
            Err(error) => eprintln!("[turns] {error}; snapshot recording disabled"),
        }
        recorder
    }
    pub fn with_close_token(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.closed = token.clone();
        self.inner = self.inner.map(|recorder| recorder.with_close_token(token));
        self
    }
    pub fn observe(&mut self, event: &crate::harness::HarnessEvent) {
        if let Some(recorder) = &mut self.inner {
            recorder.observe(event);
        }
    }
    pub fn invalidate_end(&mut self) {
        if let Some(recorder) = &mut self.inner {
            recorder.invalidate_end();
        }
    }
    pub fn discard_turn(&mut self) {
        if let Some(recorder) = &mut self.inner {
            recorder.discard_turn();
        }
    }
    pub async fn bind(&mut self, chat: &str) {
        if let Some(recorder) = &mut self.inner {
            recorder.bind(chat).await;
        }
    }
    pub async fn begin(&mut self) {
        if let Some(recorder) = &mut self.inner {
            recorder.begin().await;
        }
    }
    pub async fn capture_end(&mut self) {
        if let Some(recorder) = &mut self.inner {
            recorder.capture_end().await;
        }
    }
    pub async fn finish(&mut self) {
        if let Some(recorder) = &mut self.inner {
            recorder.finish().await;
        }
    }
    pub async fn close(&mut self) {
        self.closed.cancel();
        if let Some(recorder) = &mut self.inner {
            recorder.close().await;
        }
    }
    pub async fn end(&mut self) {
        if let Some(recorder) = &mut self.inner {
            recorder.end().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_root_disables_the_infallible_iced_recorder() {
        let fixture = repo();
        let dir = fixture.path();
        let index = std::fs::read(dir.join(".git/index")).unwrap();
        let refs = g(dir, &["for-each-ref"]);
        let closed = tokio_util::sync::CancellationToken::new();
        let mut recorder = Recorder::with_root(
            SnapshotRoots::new(PathBuf::new(), dir.into()),
            dir.into(),
            Some("disabled".into()),
            Some(dir.join("facts")),
        );
        recorder = recorder.with_close_token(closed.clone());
        assert!(recorder.inner.is_none());
        recorder.begin().await;
        recorder.bind("disabled").await;
        recorder.capture_end().await;
        recorder.finish().await;
        recorder.end().await;
        recorder.close().await;
        assert!(closed.is_cancelled());
        assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), index);
        assert_eq!(g(dir, &["for-each-ref"]), refs);
        assert!(!dir.join("facts").exists());
    }
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
}
