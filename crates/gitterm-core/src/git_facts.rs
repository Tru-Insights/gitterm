//! Git facts and base selection shared by review and the Work adapter.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// One git call gives up after this long; the process is killed.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Integration branch names tried as a base, remote then local, in this
/// order on a tie.
const INTEGRATION_BRANCHES: [&str; 4] = ["develop", "main", "master", "trunk"];

// ---------------------------------------------------------------------------
// Base selection.
//
// A repo's integration branch is not always `origin/HEAD` (one works off
// `develop` while `main` sits far behind), so every plausible base is
// measured and the one with the nearest merge-base wins: the fewest
// commits on HEAD that it lacks. The row names the winner and where it
// came from, so the comparison is never a guess.

/// Where a row's base came from; shown on the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSource {
    /// The task's recorded base branch.
    Task,
    /// The branch's tracking ref, when it is another branch.
    Tracking,
    /// `origin/develop`, `origin/main`, `origin/master`, `origin/trunk`.
    Remote,
    /// The local forms of those.
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseChoice {
    /// What git is given: `origin/develop`, `develop`, a task's base.
    pub reference: String,
    pub source: BaseSource,
    /// Candidates that exist here; the base is the nearest of them.
    pub considered: usize,
}

impl BaseChoice {
    pub fn source_label(&self) -> &'static str {
        match self.source {
            BaseSource::Task => "task base",
            BaseSource::Tracking => "tracked branch",
            BaseSource::Remote => "remote",
            BaseSource::Local => "local",
        }
    }

    /// "task base, nearest of 4".
    pub fn describe(&self) -> String {
        if self.considered > 1 {
            format!("{}, nearest of {}", self.source_label(), self.considered)
        } else {
            self.source_label().to_string()
        }
    }
}

/// A ref that may be a row's base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseCandidate {
    /// Full ref name measured (`refs/remotes/origin/develop`), or a raw
    /// revision for a task base that is not a ref.
    pub refname: String,
    pub reference: String,
    pub source: BaseSource,
}

/// The candidates, in tie-break order: the task base, the tracking ref when
/// it names another branch than `branch`, then `origin/<integration>` and
/// the local `<integration>` branches. The checked-out branch itself is
/// never its own base.
pub fn base_candidates(
    task_base: Option<&str>,
    branch: Option<&str>,
    upstream: Option<&str>,
) -> Vec<BaseCandidate> {
    fn push(out: &mut Vec<BaseCandidate>, refname: String, reference: &str, source: BaseSource) {
        if !out.iter().any(|known| known.refname == refname) {
            out.push(BaseCandidate {
                refname,
                reference: reference.to_string(),
                source,
            });
        }
    }
    let mut out = Vec::new();
    if let Some(base) = task_base.map(str::trim).filter(|base| !base.is_empty()) {
        if base.starts_with("refs/") {
            push(&mut out, base.to_string(), base, BaseSource::Task);
        } else {
            push(
                &mut out,
                format!("refs/heads/{base}"),
                base,
                BaseSource::Task,
            );
            push(
                &mut out,
                format!("refs/remotes/{base}"),
                base,
                BaseSource::Task,
            );
        }
    }
    if let Some(upstream) = upstream.map(str::trim).filter(|u| !u.is_empty()) {
        let tail = upstream.split_once('/').map(|(_, rest)| rest);
        let same = Some(upstream) == branch || (tail.is_some() && tail == branch);
        if !same {
            push(
                &mut out,
                format!("refs/remotes/{upstream}"),
                upstream,
                BaseSource::Tracking,
            );
            push(
                &mut out,
                format!("refs/heads/{upstream}"),
                upstream,
                BaseSource::Tracking,
            );
        }
    }
    for name in INTEGRATION_BRANCHES {
        push(
            &mut out,
            format!("refs/remotes/origin/{name}"),
            &format!("origin/{name}"),
            BaseSource::Remote,
        );
    }
    for name in INTEGRATION_BRANCHES {
        push(
            &mut out,
            format!("refs/heads/{name}"),
            name,
            BaseSource::Local,
        );
    }
    if let Some(branch) = branch {
        let own = format!("refs/heads/{branch}");
        out.retain(|candidate| candidate.refname != own);
    }
    out
}

/// One `for-each-ref` measuring every candidate ref against HEAD.
pub fn ahead_behind_args(candidates: &[BaseCandidate]) -> Vec<String> {
    let mut args: Vec<String> = [
        "--no-optional-locks",
        "for-each-ref",
        "--format=%(refname)%09%(ahead-behind:HEAD)",
    ]
    .iter()
    .map(|arg| arg.to_string())
    .collect();
    args.extend(
        candidates
            .iter()
            .filter(|candidate| candidate.refname.starts_with("refs/"))
            .map(|candidate| candidate.refname.clone()),
    );
    args
}

/// Parses `%(refname)%09%(ahead-behind:HEAD)` lines into refname ->
/// `(behind, ahead)` of HEAD against that ref (the atom prints commits only
/// in the ref, then commits only in HEAD).
pub fn parse_ahead_behind(output: &str) -> HashMap<String, (u32, u32)> {
    output
        .lines()
        .filter_map(|line| {
            let (name, counts) = line.split_once('\t')?;
            Some((name.to_string(), parse_left_right(counts)?))
        })
        .collect()
}

/// The measured candidate with the nearest merge-base (fewest commits
/// ahead of it); ties go to candidate order. With its `(behind, ahead)`.
pub fn choose_nearest(
    candidates: &[BaseCandidate],
    measured: &HashMap<String, (u32, u32)>,
) -> Option<(BaseChoice, (u32, u32))> {
    let present: Vec<(&BaseCandidate, (u32, u32))> = candidates
        .iter()
        .filter_map(|candidate| measured.get(&candidate.refname).map(|c| (candidate, *c)))
        .collect();
    let considered = present
        .iter()
        .map(|(candidate, _)| candidate.reference.as_str())
        .collect::<HashSet<_>>()
        .len();
    let mut best: Option<(&BaseCandidate, (u32, u32))> = None;
    for (candidate, counts) in present {
        if best.is_none_or(|(_, (_, ahead))| counts.1 < ahead) {
            best = Some((candidate, counts));
        }
    }
    best.map(|(candidate, counts)| {
        (
            BaseChoice {
                reference: candidate.reference.clone(),
                source: candidate.source,
                considered,
            },
            counts,
        )
    })
}

/// Parses `git rev-list --left-right --count base...HEAD`: `(behind,
/// ahead)`, the commits only on the base and only on HEAD.
pub fn parse_left_right(output: &str) -> Option<(u32, u32)> {
    let mut parts = output.split_whitespace();
    let behind = parts.next()?.parse().ok()?;
    let ahead = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((behind, ahead))
}

/// Which changes a review of a checkout shows (`crate::review_pane`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSource {
    /// Working tree and index against HEAD, plus untracked files.
    Uncommitted,
    /// HEAD against its merge-base with the base.
    VsBase,
}

#[cfg(feature = "runtime")]
pub use runtime::*;

#[cfg(feature = "runtime")]
mod runtime {
    use super::*;
    use std::path::Path;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    /// `git` (tokio) with repo-discovery variables scrubbed, as
    /// `agentd::git::git_command` does, killed when dropped (a timeout).
    fn git_async(dir: &Path) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("git");
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_PREFIX",
        ] {
            cmd.env_remove(var);
        }
        cmd.current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// Runs git in `dir`, feeding stdout lines to `on_line` until it returns
    /// false (the process is then killed). Errors name the command and dir.
    /// Output that is not UTF-8 is an error; [`run_git_records`] reads bytes.
    pub async fn run_git_lines(
        dir: &Path,
        args: &[&str],
        timeout: Duration,
        mut on_line: impl FnMut(&str) -> bool,
    ) -> Result<(), String> {
        let what = || format!("git {} in {}", args.join(" "), dir.display());
        run_git_records(dir, args, timeout, b'\n', |record| {
            let record = record.strip_suffix(b"\r").unwrap_or(record);
            match std::str::from_utf8(record) {
                Ok(line) => Ok(on_line(line)),
                Err(_) => Err(format!(
                    "reading {}: stream did not contain valid UTF-8",
                    what()
                )),
            }
        })
        .await
    }

    /// Runs git in `dir`, feeding each `separator`-terminated stdout record
    /// (without its separator) to `on_record` until it returns `Ok(false)` (the
    /// process is then killed) or an error, which is returned. Errors name the
    /// command and dir.
    pub async fn run_git_records(
        dir: &Path,
        args: &[&str],
        timeout: Duration,
        separator: u8,
        on_record: impl FnMut(&[u8]) -> Result<bool, String>,
    ) -> Result<(), String> {
        run_git_records_bounded(
            dir,
            args,
            timeout,
            separator,
            usize::MAX,
            usize::MAX,
            on_record,
        )
        .await
    }

    /// Like `run_git_records`, but limits each record and the aggregate stdout bytes
    /// (including separators) before buffering them. Exceeding either cap is an error.
    pub async fn run_git_records_bounded(
        dir: &Path,
        args: &[&str],
        timeout: Duration,
        separator: u8,
        record_byte_cap: usize,
        total_byte_cap: usize,
        mut on_record: impl FnMut(&[u8]) -> Result<bool, String>,
    ) -> Result<(), String> {
        let what = || format!("git {} in {}", args.join(" "), dir.display());
        let run = async {
            let mut child = git_async(dir)
                .args(args)
                .spawn()
                .map_err(|error| format!("cannot run {}: {error}", what()))?;
            let stdout = child.stdout.take().expect("stdout is piped");
            let stderr = child.stderr.take().expect("stderr is piped");
            // Stdout first: these commands write little to stderr, so its pipe
            // never fills while stdout is read. Stopping early must kill git
            // before anything waits on it, or git blocks on a full stdout pipe.
            let mut reader = BufReader::new(stdout);
            let mut record = Vec::new();
            let mut total = 0usize;
            loop {
                record.clear();
                let limit = record_byte_cap
                    .min(total_byte_cap.saturating_sub(total))
                    .saturating_add(1);
                match (&mut reader)
                    .take(limit as u64)
                    .read_until(separator, &mut record)
                    .await
                {
                    Ok(0) => break,
                    Ok(count) => {
                        if count > record_byte_cap || count > total_byte_cap.saturating_sub(total) {
                            return Err(format!("reading {}: output exceeds record ({record_byte_cap}) or total ({total_byte_cap}) byte limit", what()));
                        }
                        total += count;
                        let body = record.strip_suffix(&[separator]).unwrap_or(&record);
                        let keep_going = on_record(body);
                        if !matches!(keep_going, Ok(true)) {
                            // A cap or a bad record: the exit status says nothing.
                            let _ = child.start_kill();
                            return keep_going.map(|_| ());
                        }
                    }
                    Err(error) => return Err(format!("reading {}: {error}", what())),
                }
            }
            let mut err = Vec::new();
            let _ = stderr.take(4096).read_to_end(&mut err).await;
            let status = child
                .wait()
                .await
                .map_err(|error| format!("waiting for {}: {error}", what()))?;
            if status.success() {
                Ok(())
            } else {
                Err(format!(
                    "{} failed: {}",
                    what(),
                    String::from_utf8_lossy(&err).trim()
                ))
            }
        };
        tokio::time::timeout(timeout, run)
            .await
            .map_err(|_| format!("{} timed out after {}s", what(), timeout.as_secs()))?
    }

    /// Resolve an untrusted revision as a commit before using it in other git calls.
    pub async fn resolve_commit(dir: &Path, rev: &str) -> Result<String, String> {
        let oid = git_text(
            dir,
            &[
                "--no-optional-locks",
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{rev}^{{commit}}"),
            ],
            GIT_TIMEOUT,
        )
        .await?;
        let oid = oid.trim();
        if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "resolving commit {rev:?} in {}: git returned an invalid object id",
                dir.display()
            ));
        }
        Ok(oid.to_string())
    }

    pub async fn git_text(dir: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
        let mut out = String::new();
        run_git_lines(dir, args, timeout, |line| {
            out.push_str(line);
            out.push('\n');
            true
        })
        .await?;
        Ok(out)
    }

    /// `(behind, ahead)` per candidate ref that exists. One `for-each-ref`;
    /// on a git without `%(ahead-behind)`, one listing plus a `rev-list` per
    /// existing candidate. A task base that is no ref (a commit) is measured
    /// as a revision and added to `candidates`.
    pub async fn measure_bases(
        checkout: &Path,
        candidates: &mut Vec<BaseCandidate>,
        git_runs: &mut usize,
    ) -> Result<HashMap<String, (u32, u32)>, String> {
        let args = ahead_behind_args(candidates);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        *git_runs += 1;
        let mut measured = match git_text(checkout, &args, GIT_TIMEOUT).await {
            Ok(text) => parse_ahead_behind(&text),
            Err(_) => {
                // Older git: find which candidates exist, measure each.
                let mut list_args: Vec<&str> =
                    vec!["--no-optional-locks", "for-each-ref", "--format=%(refname)"];
                list_args.extend(args[3..].iter().copied());
                *git_runs += 1;
                let existing = git_text(checkout, &list_args, GIT_TIMEOUT).await?;
                let mut measured = HashMap::new();
                for refname in existing.lines().filter(|name| args[3..].contains(name)) {
                    *git_runs += 1;
                    if let Some(counts) = rev_list_counts(checkout, refname).await? {
                        measured.insert(refname.to_string(), counts);
                    }
                }
                measured
            }
        };
        let task_measured = candidates
            .iter()
            .any(|c| c.source == BaseSource::Task && measured.contains_key(&c.refname));
        let raw_task = candidates
            .iter()
            .find(|c| c.source == BaseSource::Task)
            .map(|c| c.reference.clone());
        if let (false, Some(base)) = (task_measured, raw_task) {
            *git_runs += 1;
            if let Ok(Some(counts)) = rev_list_counts(checkout, &base).await {
                candidates.insert(
                    0,
                    BaseCandidate {
                        refname: base.clone(),
                        reference: base.clone(),
                        source: BaseSource::Task,
                    },
                );
                measured.insert(base, counts);
            }
        }
        Ok(measured)
    }

    /// `git rev-list --left-right --count <rev>...HEAD`; `Ok(None)` when the
    /// output is not two counts.
    async fn rev_list_counts(checkout: &Path, rev: &str) -> Result<Option<(u32, u32)>, String> {
        let range = format!("{rev}...HEAD");
        let text = git_text(
            checkout,
            &[
                "--no-optional-locks",
                "rev-list",
                "--left-right",
                "--count",
                &range,
                "--",
            ],
            GIT_TIMEOUT,
        )
        .await?;
        Ok(parse_left_right(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_is_the_nearest_candidate() {
        let refnames = |list: &[BaseCandidate]| -> Vec<String> {
            list.iter().map(|c| c.refname.clone()).collect()
        };
        // A feature branch pushed to its own name: no tracking candidate.
        let feature = base_candidates(Some("release"), Some("feature-x"), Some("origin/feature-x"));
        assert_eq!(
            refnames(&feature),
            [
                "refs/heads/release",
                "refs/remotes/release",
                "refs/remotes/origin/develop",
                "refs/remotes/origin/main",
                "refs/remotes/origin/master",
                "refs/remotes/origin/trunk",
                "refs/heads/develop",
                "refs/heads/main",
                "refs/heads/master",
                "refs/heads/trunk",
            ]
        );
        // The branch is never its own base; its remote form still counts.
        let develop = base_candidates(None, Some("develop"), Some("origin/develop"));
        assert!(!refnames(&develop).contains(&"refs/heads/develop".to_string()));
        assert!(refnames(&develop).contains(&"refs/remotes/origin/develop".to_string()));
        // Tracking another branch puts it first.
        let tracking = base_candidates(None, Some("feature"), Some("origin/develop"));
        assert_eq!(tracking[0].source, BaseSource::Tracking);
        assert_eq!(tracking[0].reference, "origin/develop");

        let measured = parse_ahead_behind(
            "refs/remotes/origin/main\t0 762\n\
             refs/heads/develop\t3 2\n\
             refs/heads/main\t0 762\n\
             junk line\n",
        );
        let (base, counts) = choose_nearest(&feature, &measured).unwrap();
        assert_eq!(
            (
                base.reference.as_str(),
                base.source,
                base.considered,
                counts
            ),
            ("develop", BaseSource::Local, 3, (3, 2))
        );
        assert_eq!(base.describe(), "local, nearest of 3");
        // A tie goes to candidate order (the remote form first).
        let tie = HashMap::from([
            ("refs/heads/develop".to_string(), (0, 2)),
            ("refs/remotes/origin/develop".to_string(), (0, 2)),
        ]);
        let (base, _) = choose_nearest(&feature, &tie).unwrap();
        assert_eq!(base.reference, "origin/develop");
        assert_eq!(choose_nearest(&feature, &HashMap::new()), None);
    }

    #[test]
    fn left_right_counts_parse() {
        assert_eq!(parse_left_right("3\t5\n"), Some((3, 5)));
        assert_eq!(parse_left_right("0 0"), Some((0, 0)));
        assert_eq!(parse_left_right("x\t1"), None);
        assert_eq!(parse_left_right("1"), None);
        assert_eq!(parse_left_right("1 2 3"), None);
    }
}
