//! The work-notes folder commits and pushes itself (TRU-201).
//!
//! Work workspaces live in `~/Work/<slug>`; when `~/Work` is a git repo of
//! its own with an `origin` remote, GitTerm commits what changed there
//! about [`QUIET_PERIOD`] after the last change (a `git status` poll every
//! [`POLL_INTERVAL`]), and at once when a work workspace is parked or
//! closed, then pushes. Files over [`MAX_FILE_BYTES`] are never committed.
//! A push that fails keeps the local commit and is retried with the next
//! commit or every [`PUSH_RETRY`]; it never force-pushes, rebases or
//! pulls. Git runs off the UI thread, one command at a time.
//!
//! [`WorkNotes`] is the pure schedule; [`run`] does one job's git work and
//! blocks.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

/// How often the folder's `git status` is read.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// A change is committed once nothing else changed for this long.
pub const QUIET_PERIOD: Duration = Duration::from_secs(10 * 60);
/// A failed push is tried again after this long (or with the next commit).
pub const PUSH_RETRY: Duration = Duration::from_secs(15 * 60);
/// Files larger than this are left unstaged.
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// A push still running after this long is stopped and counts as failed.
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(120);

/// What `git status` shows: the porcelain listing and the newest mtime of
/// the files in it, so further edits to an already changed file count as
/// a change too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub status: String,
    pub newest: Option<SystemTime>,
}

/// One piece of git work, run by [`run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Whether the root is a repo of its own with an `origin` remote.
    Probe,
    /// Read `git status`.
    Status,
    /// Stage, commit and push; push even with nothing to commit when an
    /// earlier push failed.
    Commit { push_pending: bool },
    /// Retry a failed push.
    Push,
}

/// What a commit job did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitReport {
    /// The commit message, when a commit was made.
    pub committed: Option<String>,
    /// Paths (relative to the root) left unstaged for their size.
    pub too_large: Vec<String>,
    /// The push, when one ran.
    pub push: Option<Result<(), String>>,
    /// The status after the commit: what is still uncommitted.
    pub after: Option<Fingerprint>,
}

/// A job's result, for [`WorkNotes::finish`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobResult {
    /// `Err` says why the root is not used.
    Probe(Result<(), String>),
    /// `None`: nothing uncommitted.
    Status(Result<Option<Fingerprint>, String>),
    Commit(Result<CommitReport, String>),
    Push(Result<(), String>),
    /// The job did not run to an end (the blocking task failed).
    Failed(String),
}

/// The schedule: which job to run next and what the Work page shows.
#[derive(Debug)]
pub struct WorkNotes {
    root: PathBuf,
    enabled: bool,
    /// `None` until probed; `Some(false)` when the root is not usable.
    usable: Option<bool>,
    running: bool,
    last_poll: Option<Instant>,
    /// The uncommitted state last seen; `None` when clean.
    seen: Option<Fingerprint>,
    /// When `seen` last changed to something uncommitted.
    changed_at: Option<Instant>,
    commit_requested: bool,
    /// A commit is not on `origin` yet.
    unpushed: bool,
    push_failed_at: Option<Instant>,
    note: Option<String>,
}

impl WorkNotes {
    /// `enabled` is the `work_notes.auto_commit` setting; off, no git runs.
    pub fn new(root: PathBuf, enabled: bool) -> Self {
        Self {
            root,
            enabled,
            usable: None,
            running: false,
            last_poll: None,
            seen: None,
            changed_at: None,
            commit_requested: false,
            unpushed: false,
            push_failed_at: None,
            note: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `dir` is a folder in the root (a work workspace's home).
    pub fn covers(&self, dir: &Path) -> bool {
        dir != self.root && dir.starts_with(&self.root)
    }

    /// A short muted line for the Work page, e.g. "notes not pushed: offline".
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Commit now (a work workspace was parked or closed): the next job,
    /// once any running one ends.
    pub fn request_commit(&mut self) {
        if self.enabled {
            self.commit_requested = true;
        }
    }

    /// The job to run now, if any; it is marked running until
    /// [`WorkNotes::finish`]. One at a time.
    pub fn next_job(&mut self, now: Instant) -> Option<Job> {
        if !self.enabled || self.running {
            return None;
        }
        let job = match self.usable {
            None => Job::Probe,
            Some(false) => return None,
            Some(true) if self.commit_requested || self.commit_due(now) => {
                self.commit_requested = false;
                Job::Commit {
                    push_pending: self.unpushed,
                }
            }
            Some(true) if self.push_retry_due(now) => Job::Push,
            Some(true)
                if self
                    .last_poll
                    .is_none_or(|at| now.saturating_duration_since(at) >= POLL_INTERVAL) =>
            {
                self.last_poll = Some(now);
                Job::Status
            }
            Some(true) => return None,
        };
        self.running = true;
        Some(job)
    }

    /// The debounce: something is uncommitted and nothing changed for
    /// [`QUIET_PERIOD`].
    pub fn commit_due(&self, now: Instant) -> bool {
        self.seen.is_some()
            && self
                .changed_at
                .is_some_and(|at| now.saturating_duration_since(at) >= QUIET_PERIOD)
    }

    fn push_retry_due(&self, now: Instant) -> bool {
        self.unpushed
            && self
                .push_failed_at
                .is_none_or(|at| now.saturating_duration_since(at) >= PUSH_RETRY)
    }

    /// Takes in a job's result.
    pub fn finish(&mut self, result: JobResult, now: Instant) {
        self.running = false;
        match result {
            JobResult::Probe(Ok(())) => {
                eprintln!(
                    "[work-notes] {} commits and pushes itself",
                    self.root.display()
                );
                self.usable = Some(true);
            }
            JobResult::Probe(Err(reason)) => {
                eprintln!(
                    "[work-notes] {}: {reason}; not committing there",
                    self.root.display()
                );
                self.usable = Some(false);
            }
            JobResult::Status(Ok(status)) => self.observe(status, now),
            JobResult::Status(Err(error)) => {
                eprintln!("[work-notes] status failed: {error}");
            }
            JobResult::Commit(Ok(report)) => {
                for path in &report.too_large {
                    eprintln!(
                        "[work-notes] left {} uncommitted: over {} MB",
                        self.root.join(path).display(),
                        MAX_FILE_BYTES / (1024 * 1024)
                    );
                }
                if let Some(message) = &report.committed {
                    eprintln!("[work-notes] committed \"{message}\"");
                    self.unpushed = true;
                    self.note = None;
                }
                // What is left (files too large) is the new baseline, not
                // a change to commit again.
                self.seen = report.after;
                self.changed_at = None;
                if let Some(push) = report.push {
                    self.pushed(push, now);
                }
            }
            JobResult::Commit(Err(error)) => {
                eprintln!("[work-notes] commit failed: {error}");
                // Tried again on the next change, or the next park.
                self.changed_at = None;
                self.note = Some(format!("notes not committed: {}", short(&error)));
            }
            JobResult::Push(push) => self.pushed(push, now),
            JobResult::Failed(error) => eprintln!("[work-notes] {error}"),
        }
    }

    fn observe(&mut self, status: Option<Fingerprint>, now: Instant) {
        if status != self.seen {
            self.changed_at = status.is_some().then_some(now);
            self.seen = status;
        }
    }

    fn pushed(&mut self, push: Result<(), String>, now: Instant) {
        match push {
            Ok(()) => {
                eprintln!("[work-notes] pushed");
                self.unpushed = false;
                self.push_failed_at = None;
                self.note = None;
            }
            Err(reason) => {
                eprintln!("[work-notes] push failed: {reason}");
                self.unpushed = true;
                self.push_failed_at = Some(now);
                self.note = Some(format!("notes not pushed: {reason}"));
            }
        }
    }
}

fn short(text: &str) -> String {
    const MAX: usize = 80;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() <= MAX {
        return line.to_string();
    }
    let mut cut: String = line.chars().take(MAX - 1).collect();
    cut.push('\u{2026}');
    cut
}

// ---------------------------------------------------------------------------
// Pure helpers.

/// The commit message for these changed paths (relative to the root):
/// `notes: <slugs>`, the workspace folders they are in, sorted, or
/// `notes: update` when none is in a workspace folder.
pub fn commit_message(paths: &[String]) -> String {
    let mut slugs: Vec<&str> = paths
        .iter()
        .filter_map(|path| {
            let (first, rest) = path.split_once('/')?;
            (!first.is_empty() && !first.starts_with('.') && !rest.is_empty()).then_some(first)
        })
        .collect();
    slugs.sort_unstable();
    slugs.dedup();
    if slugs.is_empty() {
        "notes: update".to_string()
    } else {
        format!("notes: {}", slugs.join(", "))
    }
}

/// Of `paths` (relative to `root`), the files larger than `max` bytes.
/// Missing paths (deletions) and directories are not.
pub fn too_large(root: &Path, paths: &[String], max: u64) -> Vec<String> {
    paths
        .iter()
        .filter(|path| {
            std::fs::symlink_metadata(root.join(path))
                .is_ok_and(|meta| meta.is_file() && meta.len() > max)
        })
        .cloned()
        .collect()
}

/// The paths in `git status --porcelain=v1 -z` output: each entry's path,
/// and a rename's or copy's original path too.
pub fn porcelain_paths(output: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut entries = output.split('\0').filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        let (Some(code), Some(path)) = (entry.get(..2), entry.get(3..)) else {
            continue;
        };
        paths.push(path.to_string());
        if code.contains('R') || code.contains('C') {
            if let Some(original) = entries.next() {
                paths.push(original.to_string());
            }
        }
    }
    paths
}

/// A short reason for a failed `git push`, from its stderr.
pub fn push_failure_reason(stderr: &str) -> String {
    let has = |needle: &str| stderr.contains(needle);
    if has("[rejected]") || has("non-fast-forward") || has("fetch first") {
        return "the remote has newer commits; pull them by hand".to_string();
    }
    let offline = [
        "Could not resolve host",
        "Could not resolve hostname",
        "Network is unreachable",
        "No route to host",
        "Connection timed out",
        "Operation timed out",
        "Connection refused",
    ];
    if offline.iter().any(|needle| has(needle)) {
        return "offline".to_string();
    }
    let line = stderr
        .lines()
        .map(str::trim)
        .rev()
        .find_map(|line| {
            ["fatal:", "error:", "ERROR:"]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
        })
        .or_else(|| stderr.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or("git push failed");
    short(line.trim())
}

// ---------------------------------------------------------------------------
// Git (blocking; never on the UI thread).

fn git(root: &Path) -> Command {
    let mut command = crate::agentd::git::git_command();
    command.current_dir(root).stdin(Stdio::null());
    command
}

/// Runs git in `root`; stdout on success, else the operation and stderr.
fn git_output(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = git(root)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run git {}: {error}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// `:(literal)<path>`: a path git matches as written.
fn literal(path: &str) -> String {
    format!(":(literal){path}")
}

/// Does `job` in `root`. Blocking.
pub fn run(root: &Path, job: Job) -> JobResult {
    match job {
        Job::Probe => JobResult::Probe(probe(root)),
        Job::Status => JobResult::Status(status(root)),
        Job::Commit { push_pending } => JobResult::Commit(commit(root, push_pending)),
        Job::Push => JobResult::Push(push(root)),
    }
}

/// `Ok` when `root` is a git repo's top level (not a folder inside a
/// bigger repo) with an `origin` remote.
pub fn probe(root: &Path) -> Result<(), String> {
    if !root.is_dir() {
        return Err("no such folder".to_string());
    }
    let top = git_output(root, &["rev-parse", "--show-toplevel"])
        .map_err(|_| "not a git repo".to_string())?;
    let top = PathBuf::from(top.trim());
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same(&top, root) {
        return Err(format!(
            "inside the git repo at {}, not a repo of its own",
            top.display()
        ));
    }
    git_output(root, &["remote", "get-url", "origin"])
        .map_err(|_| "the repo has no origin remote".to_string())?;
    Ok(())
}

fn porcelain(root: &Path) -> Result<String, String> {
    git_output(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
}

/// The uncommitted state, `None` when clean.
pub fn status(root: &Path) -> Result<Option<Fingerprint>, String> {
    let listing = porcelain(root)?;
    Ok(fingerprint(root, listing))
}

fn fingerprint(root: &Path, listing: String) -> Option<Fingerprint> {
    if listing.is_empty() {
        return None;
    }
    let newest = porcelain_paths(&listing)
        .iter()
        .filter_map(|path| std::fs::symlink_metadata(root.join(path)).ok())
        .filter_map(|meta| meta.modified().ok())
        .max();
    Some(Fingerprint {
        status: listing,
        newest,
    })
}

fn staged_paths(root: &Path) -> Result<Vec<String>, String> {
    let names = git_output(root, &["diff", "--cached", "--name-only", "-z"])?;
    Ok(names
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

/// Stages everything but files over [`MAX_FILE_BYTES`], commits it with
/// the configured identity, and pushes when it committed (or
/// `push_pending`).
pub fn commit(root: &Path, push_pending: bool) -> Result<CommitReport, String> {
    let listing = porcelain(root)?;
    let mut report = CommitReport::default();
    if !listing.is_empty() {
        // Staging everything mid-merge would commit conflict markers and
        // end the user's merge for them.
        if let Some(operation) = operation_in_progress(root)? {
            return Err(format!("a {operation} is in progress"));
        }
        let changed = porcelain_paths(&listing);
        report.too_large = too_large(root, &changed, MAX_FILE_BYTES);
        let excludes: Vec<String> = report
            .too_large
            .iter()
            .map(|path| format!(":(exclude,literal){path}"))
            .collect();
        let mut add = vec!["add", "-A", "--", "."];
        add.extend(excludes.iter().map(String::as_str));
        git_output(root, &add)?;
        // A large file staged by hand is unstaged too.
        let staged_large = too_large(root, &staged_paths(root)?, MAX_FILE_BYTES);
        if !staged_large.is_empty() {
            let has_head = git(root)
                .args(["rev-parse", "--verify", "-q", "HEAD"])
                .stdout(Stdio::null())
                .status()
                .map_err(|error| format!("cannot run git rev-parse: {error}"))?
                .success();
            for path in &staged_large {
                let spec = literal(path);
                let args: [&str; 4] = if has_head {
                    ["reset", "-q", "--", &spec]
                } else {
                    ["rm", "--cached", "-q", &spec]
                };
                git_output(root, &args)?;
                if !report.too_large.contains(path) {
                    report.too_large.push(path.clone());
                }
            }
        }
        report.too_large.sort();
        let staged = staged_paths(root)?;
        if !staged.is_empty() {
            let message = commit_message(&staged);
            git_output(root, &["commit", "-q", "-m", &message])?;
            report.committed = Some(message);
        }
    }
    if report.committed.is_some() || push_pending {
        report.push = Some(push(root));
    }
    report.after = status(root)?;
    Ok(report)
}

/// The merge, rebase, cherry-pick or revert the repo is in the middle of.
fn operation_in_progress(root: &Path) -> Result<Option<&'static str>, String> {
    let git_dir = PathBuf::from(git_output(root, &["rev-parse", "--absolute-git-dir"])?.trim());
    let markers = [
        ("MERGE_HEAD", "merge"),
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
    ];
    Ok(markers
        .iter()
        .find(|(marker, _)| git_dir.join(marker).exists())
        .map(|(_, operation)| *operation))
}

/// `git push origin HEAD`, stopped after [`PUSH_TIMEOUT`]. Uses the
/// remote's own credentials (the user's SSH key); never forces.
pub fn push(root: &Path) -> Result<(), String> {
    let mut child = git(root)
        .args(["push", "origin", "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run git push: {error}"))?;
    let mut stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.read_to_string(&mut text);
        }
        text
    });
    let deadline = Instant::now() + PUSH_TIMEOUT;
    let exit = loop {
        match child.try_wait() {
            Ok(Some(exit)) => break exit,
            Ok(None) if Instant::now() >= deadline => {
                if let Err(error) = child.kill() {
                    eprintln!("[work-notes] cannot stop a stuck git push: {error}");
                }
                let _ = child.wait();
                return Err("timed out".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(error) => return Err(format!("cannot wait for git push: {error}")),
        }
    };
    let stderr = reader.join().unwrap_or_default();
    if exit.success() {
        Ok(())
    } else {
        Err(push_failure_reason(&stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn messages_name_the_workspace_folders_that_changed() {
        assert_eq!(
            commit_message(&paths(&[
                "acme-launch/notes.md",
                "beta-site/plan/a.md",
                "acme-launch/b.md"
            ])),
            "notes: acme-launch, beta-site"
        );
        assert_eq!(
            commit_message(&paths(&[".gitignore", "README.md", ".github/x.yml"])),
            "notes: update"
        );
        assert_eq!(commit_message(&[]), "notes: update");
    }

    #[test]
    fn porcelain_lists_paths_and_rename_origins() {
        let out = " M acme-launch/a.md\0?? beta site/b.md\0R  new/c.md\0old/c.md\0";
        assert_eq!(
            porcelain_paths(out),
            paths(&["acme-launch/a.md", "beta site/b.md", "new/c.md", "old/c.md"])
        );
    }

    #[test]
    fn only_files_over_the_limit_are_too_large() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("acme-launch")).unwrap();
        std::fs::write(temp.path().join("acme-launch/big.bin"), vec![0u8; 11]).unwrap();
        std::fs::write(temp.path().join("acme-launch/ok.md"), vec![0u8; 10]).unwrap();
        let listed = paths(&[
            "acme-launch/big.bin",
            "acme-launch/ok.md",
            "gone.md",
            "acme-launch",
        ]);
        assert_eq!(
            too_large(temp.path(), &listed, 10),
            paths(&["acme-launch/big.bin"])
        );
    }

    #[test]
    fn push_failures_read_short() {
        let rejected = " ! [rejected]        HEAD -> main (fetch first)\nerror: failed to push some refs to 'origin'\n";
        assert_eq!(
            push_failure_reason(rejected),
            "the remote has newer commits; pull them by hand"
        );
        let offline = "ssh: Could not resolve hostname example.com: nodename nor servname provided\nfatal: Could not read from remote repository.\n";
        assert_eq!(push_failure_reason(offline), "offline");
        assert_eq!(
            push_failure_reason("fatal: 'origin' does not appear to be a git repository\n"),
            "'origin' does not appear to be a git repository"
        );
        assert_eq!(push_failure_reason(""), "git push failed");
    }

    fn fp(status: &str) -> Option<Fingerprint> {
        Some(Fingerprint {
            status: status.to_string(),
            newest: None,
        })
    }

    #[test]
    fn commits_wait_for_quiet_and_parks_commit_at_once() {
        let t0 = Instant::now();
        let mut notes = WorkNotes::new(PathBuf::from("/w"), true);
        assert_eq!(notes.next_job(t0), Some(Job::Probe));
        // One job at a time.
        assert_eq!(notes.next_job(t0), None);
        notes.finish(JobResult::Probe(Ok(())), t0);
        assert_eq!(notes.next_job(t0), Some(Job::Status));
        notes.finish(JobResult::Status(Ok(fp("a"))), t0);
        // Polled once a minute; not committed before ten quiet minutes.
        assert_eq!(notes.next_job(t0 + Duration::from_secs(30)), None);
        let t1 = t0 + POLL_INTERVAL;
        assert_eq!(notes.next_job(t1), Some(Job::Status));
        // It changed again: the quiet period restarts.
        notes.finish(JobResult::Status(Ok(fp("ab"))), t1);
        assert!(!notes.commit_due(t0 + QUIET_PERIOD));
        assert!(notes.commit_due(t1 + QUIET_PERIOD));
        let t2 = t1 + POLL_INTERVAL;
        assert_eq!(notes.next_job(t2), Some(Job::Status));
        notes.finish(JobResult::Status(Ok(fp("ab"))), t2);
        assert!(notes.commit_due(t1 + QUIET_PERIOD));
        assert_eq!(
            notes.next_job(t1 + QUIET_PERIOD),
            Some(Job::Commit {
                push_pending: false
            })
        );
        // A big file left behind is the new baseline, not a new change.
        let report = CommitReport {
            committed: Some("notes: acme-launch".into()),
            too_large: paths(&["acme-launch/big.bin"]),
            push: Some(Err("offline".into())),
            after: fp("big"),
        };
        let t3 = t1 + QUIET_PERIOD;
        notes.finish(JobResult::Commit(Ok(report)), t3);
        assert_eq!(notes.note(), Some("notes not pushed: offline"));
        assert!(!notes.commit_due(t3 + QUIET_PERIOD * 2));
        // The failed push is retried after 15 minutes, or with a commit.
        notes.request_commit();
        assert_eq!(notes.next_job(t3), Some(Job::Commit { push_pending: true }));
        notes.finish(JobResult::Commit(Ok(CommitReport::default())), t3);
        let t4 = t3 + PUSH_RETRY;
        assert_eq!(notes.next_job(t4), Some(Job::Push));
        notes.finish(JobResult::Push(Ok(())), t4);
        assert_eq!(notes.note(), None);
        assert_eq!(notes.next_job(t4), Some(Job::Status));
    }

    #[test]
    fn off_or_unusable_runs_no_git() {
        let t0 = Instant::now();
        let mut off = WorkNotes::new(PathBuf::from("/w"), false);
        off.request_commit();
        assert_eq!(off.next_job(t0), None);
        let mut plain = WorkNotes::new(PathBuf::from("/w"), true);
        assert_eq!(plain.next_job(t0), Some(Job::Probe));
        plain.finish(JobResult::Probe(Err("not a git repo".into())), t0);
        plain.request_commit();
        assert_eq!(plain.next_job(t0 + QUIET_PERIOD), None);
        assert!(plain.covers(Path::new("/w/acme-launch")));
        assert!(!plain.covers(Path::new("/w")));
        assert!(!plain.covers(Path::new("/elsewhere/acme-launch")));
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let status = crate::agentd::git::git_command()
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        git_output(dir, args).expect("git output")
    }

    #[test]
    fn commits_and_pushes_to_a_local_bare_remote() {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        let root = temp.path().join("Work");
        std::fs::create_dir(&root).unwrap();
        git_ok(
            temp.path(),
            &["init", "-q", "--bare", "-b", "main", "remote.git"],
        );
        git_ok(&root, &["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "t"),
            ("user.email", "t@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            git_ok(&root, &["config", key, value]);
        }
        // Not usable until it has an origin.
        assert_eq!(
            probe(&root),
            Err("the repo has no origin remote".to_string())
        );
        git_ok(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        assert_eq!(probe(&root), Ok(()));
        // A folder inside it is not a repo of its own.
        std::fs::create_dir(root.join("acme-launch")).unwrap();
        assert!(probe(&root.join("acme-launch")).is_err());

        std::fs::write(root.join("acme-launch/notes.md"), "hello").unwrap();
        std::fs::write(
            root.join("acme-launch/big file.bin"),
            vec![1u8; (MAX_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        // One staged by hand is unstaged too (before the first commit).
        std::fs::write(
            root.join("acme-launch/staged.bin"),
            vec![1u8; (MAX_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        git_ok(&root, &["add", "acme-launch/staged.bin"]);
        assert!(status(&root).unwrap().is_some());
        let report = commit(&root, false).unwrap();
        assert_eq!(report.committed.as_deref(), Some("notes: acme-launch"));
        assert_eq!(
            report.too_large,
            paths(&["acme-launch/big file.bin", "acme-launch/staged.bin"])
        );
        assert_eq!(report.push, Some(Ok(())));
        // The big file is still there, uncommitted.
        assert!(report.after.is_some());
        let tree = git_out(&root, &["ls-tree", "-r", "--name-only", "HEAD"]);
        assert_eq!(tree.trim(), "acme-launch/notes.md");
        let pushed = git_out(&remote, &["log", "-1", "--format=%s", "main"]);
        assert_eq!(pushed.trim(), "notes: acme-launch");
        // Nothing new: no commit and no push.
        let again = commit(&root, false).unwrap();
        assert_eq!((again.committed, again.push), (None, None));
        // Never in the middle of the user's merge.
        let merge_head = root.join(".git/MERGE_HEAD");
        std::fs::write(&merge_head, "0000000000000000000000000000000000000000\n").unwrap();
        std::fs::write(root.join("acme-launch/more.md"), "more").unwrap();
        assert_eq!(
            commit(&root, false),
            Err("a merge is in progress".to_string())
        );
        std::fs::remove_file(&merge_head).unwrap();
        assert_eq!(
            commit(&root, false).unwrap().committed.as_deref(),
            Some("notes: acme-launch")
        );

        // The remote moved on: the push fails, the commit stays.
        let other = temp.path().join("other");
        git_ok(
            temp.path(),
            &["clone", "-q", remote.to_str().unwrap(), "other"],
        );
        for (key, value) in [
            ("user.name", "t"),
            ("user.email", "t@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "/dev/null"),
        ] {
            git_ok(&other, &["config", key, value]);
        }
        std::fs::write(other.join("elsewhere.md"), "x").unwrap();
        git_ok(&other, &["add", "-A"]);
        git_ok(&other, &["commit", "-q", "-m", "elsewhere"]);
        git_ok(&other, &["push", "-q", "origin", "HEAD"]);
        std::fs::write(root.join("README.md"), "top").unwrap();
        let report = commit(&root, false).unwrap();
        assert_eq!(report.committed.as_deref(), Some("notes: update"));
        assert_eq!(
            report.push,
            Some(Err(
                "the remote has newer commits; pull them by hand".to_string()
            ))
        );
        let local = git_out(&root, &["log", "-1", "--format=%s"]);
        assert_eq!(local.trim(), "notes: update");
        let remote_head = git_out(&remote, &["log", "-1", "--format=%s", "main"]);
        assert_eq!(remote_head.trim(), "elsewhere");
    }
}
