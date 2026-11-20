//! Bounded aggregate Git review collection, shared by frontend adapters.
//! Snapshot reads reuse C4; collection never captures or publishes snapshots.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use super::{budget::*, model::*, patch::*};
use crate::git_facts::{
    git_text, resolve_commit, run_git_records, run_git_records_bounded, BaseChoice, DiffSource,
    GIT_TIMEOUT,
};
use crate::turn_snapshots::SnapshotRoots;

/// Paths per `git hash-object` call.
const HASH_CHUNK: usize = 200;

/// A loaded review: the diff plus what viewed marks need.
#[derive(Debug, Clone)]
pub struct ReviewLoad {
    pub diff: ReviewDiff,
    pub commits: Vec<ReviewCommit>,
    pub turns: Vec<crate::turn_snapshots::Turn>,
    pub commits_truncated: bool,
    pub commits_error: Option<String>,
    /// The marks' key for this checkout.
    pub canonical_checkout: PathBuf,
    /// Every path in a review source of the checkout, or `None` when
    /// a listing was incomplete or failed: marks are then not pruned.
    pub live_paths: Option<BTreeSet<String>>,
}

// ---------------------------------------------------------------------------
// Untracked file reads.

fn read_untracked(path: &Path, budget: &mut Budget) -> Result<AddedLines, String> {
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path).map_err(|e| format!("cannot read: {e}"))?;
    file.take(FILE_BYTE_CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read: {e}"))?;
    let more = bytes.len() > FILE_BYTE_CAP;
    bytes.truncate(FILE_BYTE_CAP);
    Ok(untracked_hunks(&bytes, more, budget))
}

// ---------------------------------------------------------------------------
// Loading.

/// What the loader knows about one file before reading content.
#[derive(Debug, Clone)]
enum Pending {
    Tracked(SummaryEntry),
    Untracked { path: String, sniff: Option<Sniff> },
}

impl Pending {
    fn path(&self) -> &str {
        match self {
            Pending::Tracked(entry) => &entry.path,
            Pending::Untracked { path, .. } => path,
        }
    }
}

fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect()
}

async fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    run_git_records(dir, args, GIT_TIMEOUT, 0, |record| {
        out.extend_from_slice(record);
        out.push(0);
        Ok(true)
    })
    .await?;
    Ok(out)
}

/// Lists only commits reachable from HEAD and not from the selected base.
/// The extra row tells the caller whether the UI cap cut the list.
pub async fn branch_commits(dir: &Path, base: &str) -> Result<(Vec<ReviewCommit>, bool), String> {
    let base = resolve_commit(dir, base).await?;
    let range = format!("{base}..HEAD");
    let mut commits = Vec::new();
    run_git_records_bounded(
        dir,
        &[
            "--no-optional-locks",
            "log",
            "--topo-order",
            &format!("--max-count={}", COMMIT_LIST_CAP + 1),
            "--format=%H%x00%h%x00%s%x00%an%x00%at",
            &range,
            "--",
        ],
        GIT_TIMEOUT,
        b'\n',
        COMMIT_RECORD_BYTE_CAP,
        COMMIT_TOTAL_BYTE_CAP,
        |record| {
            let fields: Vec<&[u8]> = record.split(|byte| *byte == 0).collect();
            if fields.len() != 5 {
                return Err(format!(
                    "git log returned a malformed commit record in {}",
                    dir.display()
                ));
            }
            let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
            let sha = text(fields[0]);
            let short_sha = text(fields[1]);
            if sha.is_empty() || short_sha.is_empty() {
                return Err(format!(
                    "git log returned an empty commit id in {}",
                    dir.display()
                ));
            }
            let authored_at = text(fields[4]).parse::<i64>().map_err(|error| {
                format!(
                    "git log returned an invalid author timestamp in {}: {error}",
                    dir.display()
                )
            })?;
            commits.push(ReviewCommit {
                sha,
                short_sha,
                subject: commit_metadata(fields[2]),
                author: commit_metadata(fields[3]),
                authored_at,
            });
            Ok(true)
        },
    )
    .await?;
    let truncated = commits.len() > COMMIT_LIST_CAP;
    commits.truncate(COMMIT_LIST_CAP);
    Ok((commits, truncated))
}

fn commit_metadata(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if text.len() > COMMIT_TEXT_BYTE_CAP {
        let mut end = COMMIT_TEXT_BYTE_CAP - '…'.len_utf8();
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

/// The two endpoints used for a commit diff: parent of the oldest selected
/// commit (or the empty tree for a root commit), through the newest selected.
pub async fn commit_diff_endpoints(
    dir: &Path,
    range: &CommitRange,
) -> Result<(String, String), String> {
    let first = resolve_commit(dir, &range.first).await?;
    let newest = resolve_commit(dir, &range.last).await?;
    let ancestry = git_text(
        dir,
        &[
            "--no-optional-locks",
            "rev-list",
            "--parents",
            "-n",
            "1",
            &first,
        ],
        GIT_TIMEOUT,
    )
    .await
    .map_err(|error| format!("cannot read selected commit {}: {error}", range.first))?;
    let ancestry: Vec<&str> = ancestry.split_whitespace().collect();
    if ancestry.is_empty() {
        return Err(format!("git returned no ancestry for {}", range.first));
    }
    let from = match ancestry.get(1) {
        Some(parent) => (*parent).to_string(),
        None => empty_tree_id(dir).await?,
    };
    Ok((from, newest))
}

async fn empty_tree_id(dir: &Path) -> Result<String, String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    let mut child = Command::new("git")
        .args([
            "--no-optional-locks",
            "hash-object",
            "-t",
            "tree",
            "--stdin",
        ])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("cannot start git hash-object in {}: {error}", dir.display()))?;
    child
        .stdin
        .take()
        .ok_or_else(|| format!("git hash-object stdin unavailable in {}", dir.display()))?
        .write_all(b"")
        .await
        .map_err(|error| {
            format!(
                "cannot send empty tree to git in {}: {error}",
                dir.display()
            )
        })?;
    let output = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("git hash-object timed out in {}", dir.display()))?
        .map_err(|error| {
            format!(
                "cannot read git hash-object output in {}: {error}",
                dir.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "git hash-object failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Untracked names, at most `cap`; the flag says the list is complete.
async fn untracked_names(dir: &Path, cap: usize) -> Result<(Vec<String>, bool), String> {
    let mut names = Vec::new();
    let mut complete = true;
    run_git_records(
        dir,
        &[
            "--no-optional-locks",
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
        ],
        GIT_TIMEOUT,
        0,
        |record| {
            if names.len() >= cap {
                complete = false;
                return Ok(false);
            }
            names.push(String::from_utf8_lossy(record).into_owned());
            Ok(true)
        },
    )
    .await?;
    Ok((names, complete))
}

/// `git hash-object` of each path, in chunks. A chunk that fails (a file
/// vanished between calls) is retried path by path; a path that still fails
/// gets `None`, logged.
async fn hash_objects(dir: &Path, paths: &[String]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for chunk in paths.chunks(HASH_CHUNK) {
        let mut args = vec!["--no-optional-locks", "hash-object", "--"];
        args.extend(chunk.iter().map(String::as_str));
        match git_text(dir, &args, GIT_TIMEOUT).await {
            Ok(text) => {
                let ids: Vec<&str> = text.lines().collect();
                if ids.len() == chunk.len() {
                    for (path, id) in chunk.iter().zip(ids) {
                        out.insert(path.clone(), id.to_string());
                    }
                    continue;
                }
                eprintln!(
                    "[review] git hash-object gave {} ids for {} paths in {}; hashing one by one",
                    ids.len(),
                    chunk.len(),
                    dir.display()
                );
            }
            Err(error) => {
                eprintln!("[review] {error}; hashing one by one");
            }
        }
        for path in chunk {
            match git_text(
                dir,
                &["--no-optional-locks", "hash-object", "--", path.as_str()],
                GIT_TIMEOUT,
            )
            .await
            {
                Ok(text) => {
                    out.insert(path.clone(), text.trim().to_string());
                }
                Err(error) => eprintln!("[review] cannot hash {path}: {error}"),
            }
        }
    }
    out
}

/// Loads the review of `target` for `source`. Never call on the UI thread.
pub async fn load_review(
    target: ReviewTarget,
    source: DiffSource,
    roots: &SnapshotRoots,
) -> Result<ReviewLoad, String> {
    load_review_source(target, source.into(), roots).await
}

/// Loads a diff selection and the branch commit list. Never call on the UI thread.
pub async fn load_review_source(
    target: ReviewTarget,
    source: ReviewSource,
    roots: &SnapshotRoots,
) -> Result<ReviewLoad, String> {
    let checkout = target.checkout.clone();
    let snapshot_source = source.clone();
    load_review_source_with_snapshots(
        target,
        source,
        read_snapshots(roots, &checkout, &snapshot_source),
    )
    .await
}

/// Read-only turn input over C4's store. An absent store is never initialized.
/// Base and commit sources do not consult the snapshot roots.
pub async fn read_snapshots(
    roots: &SnapshotRoots,
    checkout: &Path,
    source: &ReviewSource,
) -> Result<(Vec<crate::turn_snapshots::Turn>, PathBuf), String> {
    let turns = match source {
        ReviewSource::Turns { chat, .. } => {
            crate::turn_snapshots::turns(roots, checkout, chat).await?
        }
        _ => Vec::new(),
    };
    let diff_checkout = if matches!(
        source,
        ReviewSource::Turns {
            selected: Some(_),
            ..
        }
    ) {
        crate::turn_snapshots::store_path(roots, checkout).await?
    } else {
        checkout.to_path_buf()
    };
    Ok((turns, diff_checkout))
}

/// Composition adapter for hosts that already own snapshot-root discovery.
/// The future must defer all snapshot IO/root discovery until polled here,
/// after checkout/base/branch validation, and return C4 turns plus diff checkout.
/// Only turn sources poll it; selected turns must resolve the store before
/// validating the selected pair, preserving the desktop's error ordering.
pub async fn load_review_source_with_snapshots(
    target: ReviewTarget,
    source: ReviewSource,
    snapshot_read: impl std::future::Future<
        Output = Result<(Vec<crate::turn_snapshots::Turn>, PathBuf), String>,
    >,
) -> Result<ReviewLoad, String> {
    let checkout = target.checkout.clone();
    let canonical_checkout = tokio::fs::canonicalize(&checkout)
        .await
        .map_err(|error| format!("cannot resolve {}: {error}", checkout.display()))?;
    let base = match &source {
        ReviewSource::Uncommitted | ReviewSource::Commits(_) | ReviewSource::Turns { .. } => None,
        ReviewSource::VsBase => Some(target.base.clone().ok_or_else(|| {
            format!(
                "no base branch found for {}: no task base, tracked branch, or develop/main/master/trunk",
                checkout.display()
            )
        })?),
    };
    let (commits, commits_truncated, commits_error) = match &target.base {
        Some(base) => match branch_commits(&checkout, &base.reference).await {
            Ok((commits, truncated)) => (commits, truncated, None),
            Err(error) if matches!(&source, ReviewSource::Commits(_)) => return Err(error),
            Err(error) => (Vec::new(), false, Some(error)),
        },
        None if matches!(&source, ReviewSource::Commits(_)) => {
            return Err(format!("no base branch found for {}", checkout.display()));
        }
        None => (Vec::new(), false, None),
    };
    let (turns, diff_checkout) = match &source {
        ReviewSource::Turns { .. } => snapshot_read.await?,
        _ => (Vec::new(), checkout.clone()),
    };
    let selection_range = match &source {
        ReviewSource::Commits(Some(range)) => {
            let first = commits.iter().position(|commit| commit.sha == range.first);
            let last = commits.iter().position(|commit| commit.sha == range.last);
            match (first, last) {
                (Some(first), Some(last)) if first >= last => Some(range.clone()),
                _ => {
                    return Err(
                        "selected commits are not an ordered range on this branch".to_string()
                    )
                }
            }
        }
        _ => None,
    };
    let diff_args = match (&source, selection_range.as_ref()) {
        (ReviewSource::Turns { selected, .. }, _) => match selected {
            Some(number) => {
                let turn = turns
                    .iter()
                    .find(|turn| turn.number == *number)
                    .ok_or("selected turn is no longer available")?;
                let start = turn
                    .start
                    .as_deref()
                    .ok_or("turn has no start snapshot (checkout discovered late or skipped)")?;
                let end = turn
                    .end
                    .as_deref()
                    .ok_or("turn has no end snapshot (running or incomplete)")?;
                vec![
                    resolve_commit(&diff_checkout, start).await?,
                    resolve_commit(&diff_checkout, end).await?,
                ]
            }
            None => {
                let empty = empty_tree_id(&checkout).await?;
                vec![empty.clone(), empty]
            }
        },
        (ReviewSource::Uncommitted, _) => vec!["HEAD".to_string()],
        (ReviewSource::VsBase, _) => vec![format!(
            "{}...HEAD",
            resolve_commit(&checkout, &base.as_ref().unwrap().reference).await?
        )],
        (ReviewSource::Commits(None), _) => vec!["HEAD".to_string(), "HEAD".to_string()],
        (ReviewSource::Commits(Some(_)), Some(range)) => {
            let (from, to) = commit_diff_endpoints(&checkout, range).await?;
            vec![from, to]
        }
        _ => unreachable!("commit selection was validated"),
    };
    let diff_source = match &source {
        ReviewSource::VsBase => DiffSource::VsBase,
        ReviewSource::Uncommitted | ReviewSource::Commits(_) | ReviewSource::Turns { .. } => {
            DiffSource::Uncommitted
        }
    };

    // The file list.
    let mut summary_args = vec![
        "--no-optional-locks",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "-M",
        "--raw",
        "--numstat",
        "--no-abbrev",
        "-z",
    ];
    summary_args.extend(diff_args.iter().map(String::as_str));
    summary_args.push("--");
    let summary = git_bytes(&diff_checkout, &summary_args).await?;
    let summary = parse_raw_numstat(&summary)?;
    let (untracked, untracked_complete) = match &source {
        ReviewSource::Uncommitted => untracked_names(&checkout, UNTRACKED_SCAN_CAP).await?,
        ReviewSource::VsBase | ReviewSource::Commits(_) | ReviewSource::Turns { .. } => {
            (Vec::new(), true)
        }
    };
    let mut all_paths: BTreeSet<String> = summary.iter().map(|e| e.path.clone()).collect();
    all_paths.extend(untracked.iter().cloned());
    let mut pending: Vec<Pending> = summary.into_iter().map(Pending::Tracked).collect();
    pending.extend(
        untracked
            .into_iter()
            .map(|path| Pending::Untracked { path, sniff: None }),
    );
    pending.sort_by(|a, b| tree_order(a.path(), b.path()));
    let unlisted = pending.len().saturating_sub(FILE_LIST_CAP);
    pending.truncate(FILE_LIST_CAP);

    // A first look at untracked files: binary, size.
    let sniff_dir = checkout.clone();
    let pending = tokio::task::spawn_blocking(move || {
        for item in &mut pending {
            if let Pending::Untracked { path, sniff } = item {
                *sniff = Some(sniff_untracked(&sniff_dir.join(&*path)));
            }
        }
        pending
    })
    .await
    .map_err(|error| format!("reading untracked files failed: {error}"))?;

    // What is shown.
    let costs: Vec<(FileKind, usize)> = pending
        .iter()
        .map(|item| {
            let kind = classify_path(item.path());
            let cost = match item {
                Pending::Tracked(entry) if entry.binary => 0,
                Pending::Tracked(entry) => entry.additions + entry.deletions,
                Pending::Untracked {
                    sniff: Some(Sniff::Text { size }),
                    ..
                } => (*size as usize / 32) + 1,
                Pending::Untracked { .. } => 0,
            };
            (kind, cost)
        })
        .collect();
    let admitted = admit(&costs, ADMIT_LINE_BUDGET);

    // The patch, for admitted tracked files only.
    let mut pathspecs: Vec<String> = Vec::new();
    let mut headers: HashMap<String, usize> = HashMap::new();
    for (index, item) in pending.iter().enumerate() {
        if let Pending::Tracked(entry) = item {
            if admitted.contains(&index) && !entry.binary && entry.additions + entry.deletions > 0 {
                headers.insert(expected_header(entry), index);
                if let Some(old) = &entry.old_path {
                    pathspecs.push(old.clone());
                }
                pathspecs.push(entry.path.clone());
            }
        }
    }
    let mut parser = PatchParser::default();
    if !pathspecs.is_empty() {
        let mut args = vec![
            "-c",
            "core.quotePath=false",
            "--no-optional-locks",
            "--literal-pathspecs",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-M",
        ];
        args.extend(diff_args.iter().map(String::as_str));
        args.push("--");
        args.extend(pathspecs.iter().map(String::as_str));
        run_git_records(&diff_checkout, &args, GIT_TIMEOUT, b'\n', |record| {
            Ok(parser.feed(&String::from_utf8_lossy(record)))
        })
        .await?;
    }
    let mut patches: HashMap<usize, PatchFile> = HashMap::new();
    for patch in std::mem::take(&mut parser.files) {
        match headers.get(&patch.header) {
            Some(&index) => {
                patches.insert(index, patch);
            }
            None => eprintln!(
                "[review] {}: patch {:?} matches no listed file",
                checkout.display(),
                patch.header
            ),
        }
    }

    // Untracked content, then the files.
    let read_dir = checkout.clone();
    let mut budget = parser.budget;
    let admitted_for_read = admitted.clone();
    let untracked_content = tokio::task::spawn_blocking(move || {
        let mut out: HashMap<usize, Result<AddedLines, String>> = HashMap::new();
        for (index, item) in pending.iter().enumerate() {
            if let Pending::Untracked {
                path,
                sniff: Some(Sniff::Text { .. }),
            } = item
            {
                if admitted_for_read.contains(&index) && !budget.exhausted() {
                    out.insert(index, read_untracked(&read_dir.join(path), &mut budget));
                }
            }
        }
        (pending, out)
    })
    .await
    .map_err(|error| format!("reading untracked files failed: {error}"))?;
    let (pending, mut untracked_content) = untracked_content;

    let mut files = Vec::with_capacity(pending.len());
    let mut to_hash: Vec<String> = Vec::new();
    for (index, item) in pending.into_iter().enumerate() {
        let kind = costs[index].0;
        let shown = admitted.contains(&index);
        match item {
            Pending::Tracked(entry) => {
                let status = tracked_status(&entry);
                let wants_lines = !entry.binary && entry.additions + entry.deletions > 0;
                let patch = patches.remove(&index);
                let omitted = wants_lines && (!shown || patch.is_none());
                let (hunks, truncated) = match patch {
                    Some(patch) => (patch.hunks, patch.truncated),
                    None => (Vec::new(), false),
                };
                let content_id = if entry.is_submodule() {
                    Some(format!("commit:{}", entry.new_blob))
                } else if status == FileStatus::Deleted {
                    Some(format!("deleted:{}", entry.old_blob))
                } else if diff_source == DiffSource::VsBase
                    || matches!(
                        &source,
                        ReviewSource::Commits(Some(_))
                            | ReviewSource::Turns {
                                selected: Some(_),
                                ..
                            }
                    )
                {
                    Some(entry.new_blob.clone())
                } else {
                    to_hash.push(entry.path.clone());
                    None
                };
                files.push(ReviewFile {
                    path: entry.path,
                    old_path: entry.old_path.filter(|_| entry.status == 'R'),
                    status,
                    kind,
                    additions: entry.additions,
                    deletions: entry.deletions,
                    truncated,
                    omitted,
                    note: None,
                    hunks,
                    content_id,
                });
            }
            Pending::Untracked { path, sniff } => {
                let mut file = ReviewFile {
                    path,
                    old_path: None,
                    status: FileStatus::Untracked,
                    kind,
                    additions: 0,
                    deletions: 0,
                    truncated: false,
                    omitted: false,
                    note: None,
                    hunks: Vec::new(),
                    content_id: None,
                };
                match sniff {
                    Some(Sniff::Binary) => {
                        file.status = FileStatus::Binary;
                        to_hash.push(file.path.clone());
                    }
                    Some(Sniff::Unreadable(why)) => file.note = Some(why),
                    Some(Sniff::Text { .. }) => {
                        to_hash.push(file.path.clone());
                        match untracked_content.remove(&index) {
                            Some(Ok((hunks, lines, truncated))) => {
                                file.hunks = hunks;
                                file.additions = lines;
                                file.truncated = truncated;
                            }
                            Some(Err(why)) => file.note = Some(why),
                            None => file.omitted = true,
                        }
                    }
                    None => file.note = Some("not read".to_string()),
                }
                files.push(file);
            }
        }
    }
    if !to_hash.is_empty() {
        let ids = hash_objects(&checkout, &to_hash).await;
        for file in &mut files {
            if file.content_id.is_none() {
                file.content_id = ids.get(&file.path).cloned();
            }
        }
    }

    let merge_base = match &base {
        Some(base) => {
            let text = git_text(
                &checkout,
                &[
                    "--no-optional-locks",
                    "merge-base",
                    &resolve_commit(&checkout, &base.reference).await?,
                    "HEAD",
                ],
                GIT_TIMEOUT,
            )
            .await?;
            text.trim().get(..7).map(str::to_string)
        }
        None => None,
    };

    // Every path in the other source, so marks for files still in either
    // diff survive pruning.
    let live_paths = if !untracked_complete || matches!(&source, ReviewSource::Turns { .. }) {
        None
    } else {
        let other = if matches!(&source, ReviewSource::Commits(_)) {
            let worktree =
                other_source_paths(&checkout, DiffSource::VsBase, target.base.as_ref()).await;
            let base =
                other_source_paths(&checkout, DiffSource::Uncommitted, target.base.as_ref()).await;
            worktree.and_then(|mut paths| {
                base.map(|more| {
                    paths.extend(more);
                    paths
                })
            })
        } else {
            other_source_paths(&checkout, diff_source, target.base.as_ref()).await
        };
        match other {
            Ok(other) => {
                all_paths.extend(other);
                Some(all_paths)
            }
            Err(error) => {
                eprintln!("[review] not pruning viewed marks: {error}");
                None
            }
        }
    };

    let totals = ReviewTotals::of(&files);
    Ok(ReviewLoad {
        diff: ReviewDiff {
            checkout,
            branch: target.branch,
            base: target.base,
            source: diff_source,
            merge_base,
            files,
            totals,
            unlisted,
        },
        commits,
        turns,
        commits_truncated,
        commits_error,
        canonical_checkout,
        live_paths,
    })
}

async fn other_source_paths(
    checkout: &Path,
    source: DiffSource,
    base: Option<&BaseChoice>,
) -> Result<Vec<String>, String> {
    match source {
        DiffSource::Uncommitted => {
            let Some(base) = base else {
                return Ok(Vec::new());
            };
            let range = format!(
                "{}...HEAD",
                resolve_commit(checkout, &base.reference).await?
            );
            let names = git_bytes(
                checkout,
                &[
                    "--no-optional-locks",
                    "diff",
                    "--no-ext-diff",
                    "-M",
                    "--name-only",
                    "-z",
                    &range,
                    "--",
                ],
            )
            .await?;
            Ok(split_nul(&names))
        }
        DiffSource::VsBase => {
            let names = git_bytes(
                checkout,
                &[
                    "--no-optional-locks",
                    "diff",
                    "--no-ext-diff",
                    "-M",
                    "--name-only",
                    "-z",
                    "HEAD",
                    "--",
                ],
            )
            .await?;
            let mut out = split_nul(&names);
            let (untracked, complete) = untracked_names(checkout, UNTRACKED_SCAN_CAP).await?;
            if !complete {
                return Err(format!(
                    "more than {UNTRACKED_SCAN_CAP} untracked files in {}",
                    checkout.display()
                ));
            }
            out.extend(untracked);
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests;
