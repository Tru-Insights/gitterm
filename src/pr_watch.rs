//! The PR watch (TRU-173): live state of the PRs a piece of work produced,
//! and news about them, notify only.
//!
//! One background poller in the app (never in `update()`, applied by
//! event) asks [`poll`] for the PRs that are due: every PR in a plan or in
//! the facts of an open workspace every [`OPEN_INTERVAL`], of a parked one
//! every [`PARKED_INTERVAL`]. A merged or closed PR is not polled again.
//!
//! Every read is a conditional REST request through `gh api -i` with
//! `If-None-Match` and the ETag of the last answer, so an unchanged
//! resource costs no rate limit; a 304 keeps the part of the snapshot that
//! answer produced. Per PR: the pull itself; the head commit's check runs
//! and combined status; the base branch's required checks (branch
//! protection and rulesets); reviews, issue comments and line comments.
//!
//! News ([`NewsKind`]) is the difference between two snapshots, each item
//! keyed so it is reported once ([`WatchedPr::absorb`]); the first snapshot
//! of a PR is the baseline and reports nothing.
//!
//! Commands run as the GitHub account the config maps the repo owner to
//! (`github_accounts`), with `GH_TOKEN` from `gh auth token --user <user>`
//! set on that command only; without a mapping, gh's active account. Global
//! gh state is never touched. Approve and merge ([`run_action`]) run only
//! from the user's click in the confirmation that shows
//! [`action_command_line`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::merge_plan::{MergeMethod, PrBrief, PrKey, PrState};

/// Poll interval for PRs of open workspaces.
pub const OPEN_INTERVAL: Duration = Duration::from_secs(120);
/// Poll interval for PRs of parked workspaces.
pub const PARKED_INTERVAL: Duration = Duration::from_secs(600);
/// One `gh` call gives up after this long; the process is killed.
pub const GH_TIMEOUT: Duration = Duration::from_secs(20);
/// A whole poll stops starting new PRs after this long; the rest wait for
/// the next one.
pub const POLL_DEADLINE: Duration = Duration::from_secs(90);
/// The watch state file in the config dir.
pub const STATE_FILE_NAME: &str = "pr-watch.json";
/// News kept per PR, newest last.
const NEWS_KEPT: usize = 20;
/// Items read per list endpoint (one page).
const PAGE: &str = "per_page=100";

// ---------------------------------------------------------------------------
// Snapshot pieces, one per endpoint.

/// The pull request itself (`GET repos/{repo}/pulls/{n}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullCore {
    pub title: String,
    pub state: Option<PrState>,
    pub url: String,
    pub author: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    /// `mergeable`: `None` while GitHub computes it.
    pub mergeable: Option<bool>,
    /// `mergeable_state` ("clean", "dirty", "blocked", …).
    pub mergeable_state: String,
    pub requested_reviewers: u32,
}

/// One check run or commit status context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckItem {
    pub name: String,
    pub outcome: CheckOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Pass,
    Fail,
    Pending,
}

/// A review, issue comment or line comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    /// `review:<id>`, `comment:<id>` or `line:<id>`.
    pub id: String,
    pub author: String,
    /// A review's state (APPROVED, CHANGES_REQUESTED, COMMENTED,
    /// DISMISSED); empty for comments.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub review_state: String,
}

/// Everything the watch knows of one PR, assembled from its endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSnapshot {
    #[serde(default)]
    pub core: Option<PullCore>,
    /// The head commit the checks below belong to.
    #[serde(default)]
    pub checks_sha: String,
    #[serde(default)]
    pub check_runs: Vec<CheckItem>,
    #[serde(default)]
    pub statuses: Vec<CheckItem>,
    /// Required check names of the base branch's classic protection.
    #[serde(default)]
    pub required: Vec<String>,
    /// Required check names of the rulesets on the base branch.
    #[serde(default)]
    pub required_rules: Vec<String>,
    #[serde(default)]
    pub reviews: Vec<Activity>,
    #[serde(default)]
    pub comments: Vec<Activity>,
}

/// The combined checks of the head commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    Pass,
    Fail,
    Pending,
    /// No checks reported.
    None,
}

impl ChecksState {
    pub fn label(self) -> &'static str {
        match self {
            ChecksState::Pass => "checks pass",
            ChecksState::Fail => "checks fail",
            ChecksState::Pending => "checks pending",
            ChecksState::None => "no checks",
        }
    }
}

/// The review decision, from each reviewer's latest deciding review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequested,
    None,
}

impl ReviewDecision {
    pub fn label(self) -> &'static str {
        match self {
            ReviewDecision::Approved => "approved",
            ReviewDecision::ChangesRequested => "changes requested",
            ReviewDecision::ReviewRequested => "review requested",
            ReviewDecision::None => "no review",
        }
    }
}

impl PrSnapshot {
    pub fn state(&self) -> Option<PrState> {
        self.core.as_ref().and_then(|core| core.state)
    }

    fn all_checks(&self) -> impl Iterator<Item = &CheckItem> {
        self.check_runs.iter().chain(self.statuses.iter())
    }

    pub fn checks(&self) -> ChecksState {
        let mut any = false;
        let mut pending = false;
        for check in self.all_checks() {
            any = true;
            match check.outcome {
                CheckOutcome::Fail => return ChecksState::Fail,
                CheckOutcome::Pending => pending = true,
                CheckOutcome::Pass => {}
            }
        }
        match (any, pending) {
            (false, _) => ChecksState::None,
            (true, true) => ChecksState::Pending,
            (true, false) => ChecksState::Pass,
        }
    }

    /// Names of the failing checks.
    pub fn failing_checks(&self) -> Vec<String> {
        self.all_checks()
            .filter(|check| check.outcome == CheckOutcome::Fail)
            .map(|check| check.name.clone())
            .collect()
    }

    /// Required check names of the base branch, both sources.
    pub fn required_checks(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for name in self.required.iter().chain(self.required_rules.iter()) {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
        names
    }

    /// Every required check passed. Without required checks on the base
    /// branch, every reported check passed (and at least one exists).
    pub fn required_passed(&self) -> bool {
        let required = self.required_checks();
        if required.is_empty() {
            return self.checks() == ChecksState::Pass;
        }
        required.iter().all(|name| {
            self.all_checks()
                .any(|check| check.name == *name && check.outcome == CheckOutcome::Pass)
        })
    }

    pub fn review_decision(&self) -> ReviewDecision {
        let mut latest: BTreeMap<&str, &str> = BTreeMap::new();
        for review in &self.reviews {
            if matches!(
                review.review_state.as_str(),
                "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
            ) {
                latest.insert(review.author.as_str(), review.review_state.as_str());
            }
        }
        if latest.values().any(|state| *state == "CHANGES_REQUESTED") {
            ReviewDecision::ChangesRequested
        } else if latest.values().any(|state| *state == "APPROVED") {
            ReviewDecision::Approved
        } else if self
            .core
            .as_ref()
            .is_some_and(|core| core.requested_reviewers > 0)
        {
            ReviewDecision::ReviewRequested
        } else {
            ReviewDecision::None
        }
    }

    /// `Some(true)` when GitHub reports merge conflicts, `None` while it has
    /// not computed mergeability.
    pub fn conflicting(&self) -> Option<bool> {
        let core = self.core.as_ref()?;
        if core.mergeable_state == "dirty" {
            return Some(true);
        }
        core.mergeable.map(|mergeable| !mergeable)
    }

    fn activity(&self) -> impl Iterator<Item = &Activity> {
        self.reviews.iter().chain(self.comments.iter())
    }
}

// ---------------------------------------------------------------------------
// News.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewsKind {
    ChecksFailed,
    RequiredChecksPassed,
    /// A new review or comment from someone other than the user.
    NewActivity,
    Conflicting,
    Merged,
    Closed,
}

/// One piece of news about a PR, reported once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsItem {
    /// Unique per PR; what makes it reported once.
    pub key: String,
    pub kind: NewsKind,
    /// "checks failed: test, lint".
    pub text: String,
    /// RFC 3339 UTC.
    pub at: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dismissed: bool,
    /// Sent to the coordinator with "Tell coordinator".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub told: bool,
}

/// The news between `previous` and `next` as `(key, kind, text)`, given the
/// user's login `me`. Keys name what happened (the head commit for checks
/// and conflicts, the activity ids for comments) so a condition that comes
/// back is not reported twice.
pub fn news_between(
    previous: &PrSnapshot,
    next: &PrSnapshot,
    me: &str,
) -> Vec<(String, NewsKind, String)> {
    let mut out = Vec::new();
    let Some(core) = next.core.as_ref() else {
        return out;
    };
    let sha = core.head_sha.as_str();
    let was = previous.state();
    match next.state() {
        Some(PrState::Merged) if was != Some(PrState::Merged) => {
            out.push(("merged".to_string(), NewsKind::Merged, "merged".to_string()));
        }
        Some(PrState::Closed) if was != Some(PrState::Closed) => {
            out.push(("closed".to_string(), NewsKind::Closed, "closed".to_string()));
        }
        _ => {}
    }
    let still_open = matches!(next.state(), Some(PrState::Open | PrState::Draft));
    if still_open && next.checks_sha == sha {
        if next.checks() == ChecksState::Fail {
            let failing = next.failing_checks();
            out.push((
                format!("checks_failed:{sha}"),
                NewsKind::ChecksFailed,
                format!("checks failed: {}", shorten_list(&failing, 3)),
            ));
        } else if next.required_passed() {
            let text = if next.required_checks().is_empty() {
                "checks passed"
            } else {
                "required checks passed"
            };
            out.push((
                format!("required_passed:{sha}"),
                NewsKind::RequiredChecksPassed,
                text.to_string(),
            ));
        }
    }
    if still_open && next.conflicting() == Some(true) {
        out.push((
            format!("conflicting:{sha}"),
            NewsKind::Conflicting,
            "has merge conflicts".to_string(),
        ));
    }
    let known: BTreeSet<&str> = previous.activity().map(|a| a.id.as_str()).collect();
    let fresh: Vec<&Activity> = next
        .activity()
        .filter(|activity| !known.contains(activity.id.as_str()))
        .filter(|activity| !activity.author.eq_ignore_ascii_case(me))
        .collect();
    if let Some(first) = fresh.first() {
        let mut authors: Vec<&str> = Vec::new();
        for activity in &fresh {
            if !authors.contains(&activity.author.as_str()) {
                authors.push(activity.author.as_str());
            }
        }
        let reviews = fresh.iter().filter(|a| a.id.starts_with("review:")).count();
        let comments = fresh.len() - reviews;
        let mut parts = Vec::new();
        if reviews > 0 {
            parts.push(plural(reviews, "review"));
        }
        if comments > 0 {
            parts.push(plural(comments, "comment"));
        }
        out.push((
            format!("activity:{}", first.id),
            NewsKind::NewActivity,
            format!(
                "new {} from {}",
                parts.join(" and "),
                shorten_list(
                    &authors.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                    3
                )
            ),
        ));
    }
    out
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("1 {word}")
    } else {
        format!("{count} {word}s")
    }
}

fn shorten_list(items: &[String], max: usize) -> String {
    if items.is_empty() {
        return "unnamed".to_string();
    }
    let mut text = items
        .iter()
        .take(max)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > max {
        text.push_str(&format!(" +{}", items.len() - max));
    }
    text
}

// ---------------------------------------------------------------------------
// One watched PR.

/// What the watch keeps per PR (persisted in [`STATE_FILE_NAME`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedPr {
    pub key: PrKey,
    #[serde(default)]
    pub snapshot: PrSnapshot,
    /// Request path -> ETag of the answer `snapshot` holds.
    #[serde(default)]
    pub etags: BTreeMap<String, String>,
    /// Keys of news already reported.
    #[serde(default)]
    pub reported: BTreeSet<String>,
    #[serde(default)]
    pub news: Vec<NewsItem>,
    /// Reviews and comments from others since the user last pressed Review.
    #[serde(default)]
    pub unseen_comments: u32,
    /// RFC 3339 UTC of the last poll, answered or failed.
    #[serde(default)]
    pub checked_at: Option<String>,
    /// Why the last poll failed.
    #[serde(default)]
    pub error: Option<String>,
}

impl WatchedPr {
    pub fn new(key: PrKey) -> Self {
        Self {
            key,
            snapshot: PrSnapshot::default(),
            etags: BTreeMap::new(),
            reported: BTreeSet::new(),
            news: Vec::new(),
            unseen_comments: 0,
            checked_at: None,
            error: None,
        }
    }

    /// Merged or closed: no longer polled.
    pub fn done(&self) -> bool {
        self.snapshot.state().is_some_and(PrState::is_final)
    }

    /// Whether a poll is due after `interval` at `now`.
    pub fn due(&self, interval: Duration, now: chrono::DateTime<chrono::Utc>) -> bool {
        if self.done() {
            return false;
        }
        let Some(at) = self
            .checked_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        else {
            return true;
        };
        let elapsed = now.signed_duration_since(at.with_timezone(&chrono::Utc));
        elapsed.to_std().is_ok_and(|elapsed| elapsed >= interval)
    }

    pub fn brief(&self) -> Option<PrBrief> {
        let core = self.snapshot.core.as_ref()?;
        Some(PrBrief {
            state: core.state?,
            head_ref: core.head_ref.clone(),
            base_ref: core.base_ref.clone(),
        })
    }

    /// News not dismissed, newest first.
    pub fn open_news(&self) -> impl Iterator<Item = &NewsItem> {
        self.news.iter().rev().filter(|item| !item.dismissed)
    }

    /// Takes a fetched snapshot at `at` for user `me` and returns the news
    /// it brings. The first snapshot is the baseline: whatever it shows is
    /// marked reported, so only later changes are news.
    pub fn absorb(&mut self, fetched: Fetched, me: &str, at: &str) -> Vec<NewsItem> {
        let baseline = self.snapshot.core.is_none();
        let found = news_between(&self.snapshot, &fetched.snapshot, me);
        let fresh_activity = {
            let known: BTreeSet<&str> = self.snapshot.activity().map(|a| a.id.as_str()).collect();
            fetched
                .snapshot
                .activity()
                .filter(|a| !known.contains(a.id.as_str()) && !a.author.eq_ignore_ascii_case(me))
                .count() as u32
        };
        self.snapshot = fetched.snapshot;
        self.etags = fetched.etags;
        self.checked_at = Some(at.to_string());
        self.error = None;
        let mut added = Vec::new();
        for (key, kind, text) in found {
            if !self.reported.insert(key.clone()) || baseline {
                continue;
            }
            let item = NewsItem {
                key,
                kind,
                text,
                at: at.to_string(),
                dismissed: false,
                told: false,
            };
            self.news.push(item.clone());
            added.push(item);
        }
        if !baseline {
            self.unseen_comments = self.unseen_comments.saturating_add(fresh_activity);
        }
        let excess = self.news.len().saturating_sub(NEWS_KEPT);
        self.news.drain(..excess);
        added
    }

    /// A poll of this PR failed.
    pub fn failed(&mut self, error: String, at: &str) {
        self.error = Some(error);
        self.checked_at = Some(at.to_string());
    }
}

/// The watch state file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchFile {
    /// Keyed by [`PrKey::id`].
    #[serde(default)]
    pub prs: BTreeMap<String, WatchedPr>,
}

impl WatchFile {
    pub fn path_in(config_dir: &Path) -> PathBuf {
        config_dir.join(STATE_FILE_NAME)
    }

    /// The file at `path`; missing is empty, unreadable is logged and empty
    /// (the file is left alone until the next save replaces it).
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                eprintln!(
                    "[pr-watch] {} is not a watch file ({error}); starting empty",
                    path.display()
                );
                Self::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                eprintln!(
                    "[pr-watch] cannot read {}: {error}; starting empty",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Blocking: run it off the UI thread.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let encoded = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("cannot encode {}: {error}", path.display()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        crate::merge_plan::write_atomically(path, &encoded)
    }
}

// ---------------------------------------------------------------------------
// Accounts and commands.

/// The gh account `owner`'s repos run as: the `github_accounts` mapping
/// (owner matched case-insensitively), else `None` for gh's active account.
pub fn account_for(owner: &str, accounts: &BTreeMap<String, String>) -> Option<String> {
    accounts
        .iter()
        .find(|(mapped, _)| mapped.trim().eq_ignore_ascii_case(owner))
        .map(|(_, user)| user.trim().to_string())
        .filter(|user| !user.is_empty())
}

/// A GitHub login that is safe to put on a command line unquoted.
pub fn valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// What the user can do to a PR from its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrAction {
    Approve,
    Merge(MergeMethod),
}

impl PrAction {
    pub fn verb(self) -> &'static str {
        match self {
            PrAction::Approve => "Approve",
            PrAction::Merge(_) => "Merge",
        }
    }
}

/// The `gh` arguments of `action` on `key`.
pub fn action_args(action: PrAction, key: &PrKey) -> Vec<String> {
    let number = key.number.to_string();
    match action {
        PrAction::Approve => vec![
            "pr".into(),
            "review".into(),
            number,
            "--repo".into(),
            key.repo.clone(),
            "--approve".into(),
        ],
        PrAction::Merge(method) => vec![
            "pr".into(),
            "merge".into(),
            number,
            "--repo".into(),
            key.repo.clone(),
            method.flag().into(),
        ],
    }
}

/// The command exactly as it runs, for the confirmation:
/// `GH_TOKEN="$(gh auth token --user <user>)" gh pr merge 12 --repo acme/api --merge`,
/// without the token prefix for gh's active account.
pub fn action_command_line(action: PrAction, key: &PrKey, account: Option<&str>) -> String {
    let command = format!("gh {}", action_args(action, key).join(" "));
    match account {
        Some(user) => format!("GH_TOKEN=\"$(gh auth token --user {user})\" {command}"),
        None => command,
    }
}

/// A `gh` command with no token from GitTerm's own environment: `GH_TOKEN`
/// for `account` when given, else gh's active account.
fn gh_async(token: Option<&str>) -> Result<tokio::process::Command, String> {
    let std_command = crate::gh_identity::gh_command().map_err(|error| error.to_string())?;
    let mut command = tokio::process::Command::from(std_command);
    command
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(token) = token {
        command.env("GH_TOKEN", token);
    }
    Ok(command)
}

async fn output_within(
    mut command: tokio::process::Command,
    what: &str,
) -> Result<std::process::Output, String> {
    match tokio::time::timeout(GH_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(format!("{what}: {error}")),
        Err(_) => Err(format!("{what}: timed out after {}s", GH_TIMEOUT.as_secs())),
    }
}

/// `gh auth token --user <user>`: the token commands for `user` carry.
pub async fn account_token(user: &str) -> Result<String, String> {
    if !valid_login(user) {
        return Err(format!("{user:?} is not a GitHub login"));
    }
    let mut command = gh_async(None)?;
    command.args(["auth", "token", "--user", user]);
    let what = format!("gh auth token --user {user}");
    let output = output_within(command, &what).await?;
    if !output.status.success() {
        return Err(format!(
            "{what}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(format!("{what} printed no token"));
    }
    Ok(token)
}

/// Runs `action` on `key` as `account` (gh's active account when `None`)
/// and returns what gh printed. Only ever called from the user's click in
/// the confirmation.
pub async fn run_action(
    action: PrAction,
    key: PrKey,
    account: Option<String>,
) -> Result<String, String> {
    let token = match account.as_deref() {
        Some(user) => Some(account_token(user).await?),
        None => None,
    };
    let mut command = gh_async(token.as_deref())?;
    let args = action_args(action, &key);
    command.args(&args);
    let what = action_command_line(action, &key, account.as_deref());
    let output = output_within(command, &what).await?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        Err(format!(
            "{what} failed: {}",
            if stderr.is_empty() { stdout } else { stderr }
        ))
    }
}

// ---------------------------------------------------------------------------
// Conditional REST through `gh api -i`.

/// One answer of `gh api -i`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiResponse {
    pub status: u16,
    pub etag: Option<String>,
    pub body: String,
}

/// Parses `gh api -i` output: a status line (`HTTP/2.0 200 OK`), headers,
/// a blank line, the body. `None` when no status line leads it.
pub fn parse_api_response(output: &str) -> Option<ApiResponse> {
    let mut lines = output.split_inclusive('\n');
    let status_line = lines.next()?;
    let mut words = status_line.split_whitespace();
    if !words.next()?.starts_with("HTTP/") {
        return None;
    }
    let status: u16 = words.next()?.parse().ok()?;
    let mut etag = None;
    let mut offset = status_line.len();
    let mut body = String::new();
    for line in lines {
        offset += line.len();
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            body = output.get(offset..).unwrap_or_default().to_string();
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("etag") {
                etag = Some(value.trim().to_string());
            }
        }
    }
    Some(ApiResponse { status, etag, body })
}

/// GETs `path` with `If-None-Match: etag` when one is known.
async fn gh_api(
    path: &str,
    etag: Option<&str>,
    token: Option<&str>,
) -> Result<ApiResponse, String> {
    let mut command = gh_async(token)?;
    command.args(["api", "-i", "-H", "Accept: application/vnd.github+json"]);
    if let Some(etag) = etag {
        command.args(["-H", &format!("If-None-Match: {etag}")]);
    }
    command.arg(path);
    let what = format!("gh api {path}");
    let output = output_within(command, &what).await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    match parse_api_response(&stdout) {
        Some(response) => Ok(response),
        None => Err(format!(
            "{what}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// One conditional read in a poll: what came back, new or unchanged.
enum Read {
    Changed(Value),
    Unchanged,
}

struct Reader<'a> {
    token: Option<&'a str>,
    previous: &'a BTreeMap<String, String>,
    etags: BTreeMap<String, String>,
}

impl Reader<'_> {
    /// Reads `path`; a 304 is `Unchanged` (the previous ETag carries over),
    /// anything but 200 or 304 an error naming the status.
    async fn read(&mut self, path: &str) -> Result<Read, String> {
        let known = self.previous.get(path).map(String::as_str);
        let response = gh_api(path, known, self.token).await?;
        match response.status {
            304 => {
                if let Some(etag) = known {
                    self.etags.insert(path.to_string(), etag.to_string());
                }
                Ok(Read::Unchanged)
            }
            200 => {
                let value: Value = serde_json::from_str(&response.body)
                    .map_err(|error| format!("gh api {path}: answer is not JSON: {error}"))?;
                if let Some(etag) = response.etag {
                    self.etags.insert(path.to_string(), etag);
                }
                Ok(Read::Changed(value))
            }
            status => Err(format!(
                "gh api {path}: HTTP {status}: {}",
                api_message(&response.body)
            )),
        }
    }
}

fn api_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(200).collect())
}

fn str_at<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    let mut current = value;
    for key in path {
        match current.get(key) {
            Some(next) => current = next,
            None => return "",
        }
    }
    current.as_str().unwrap_or("")
}

/// The pull JSON as a [`PullCore`].
pub fn parse_pull(value: &Value) -> Result<PullCore, String> {
    let state = match (
        str_at(value, &["state"]),
        value
            .get("merged")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || value.get("merged_at").is_some_and(|at| !at.is_null()),
        value.get("draft").and_then(Value::as_bool).unwrap_or(false),
    ) {
        ("open", _, true) => PrState::Draft,
        ("open", _, false) => PrState::Open,
        ("closed", true, _) => PrState::Merged,
        ("closed", false, _) => PrState::Closed,
        (other, _, _) => return Err(format!("pull has unknown state {other:?}")),
    };
    Ok(PullCore {
        title: str_at(value, &["title"]).to_string(),
        state: Some(state),
        url: str_at(value, &["html_url"]).to_string(),
        author: str_at(value, &["user", "login"]).to_string(),
        head_ref: str_at(value, &["head", "ref"]).to_string(),
        head_sha: str_at(value, &["head", "sha"]).to_string(),
        base_ref: str_at(value, &["base", "ref"]).to_string(),
        mergeable: value.get("mergeable").and_then(Value::as_bool),
        mergeable_state: str_at(value, &["mergeable_state"]).to_string(),
        requested_reviewers: value
            .get("requested_reviewers")
            .and_then(Value::as_array)
            .map_or(0, |list| list.len() as u32)
            + value
                .get("requested_teams")
                .and_then(Value::as_array)
                .map_or(0, |list| list.len() as u32),
    })
}

/// `commits/{sha}/check-runs` as check items.
pub fn parse_check_runs(value: &Value) -> Vec<CheckItem> {
    value
        .get("check_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|run| {
            let outcome = if str_at(run, &["status"]) != "completed" {
                CheckOutcome::Pending
            } else {
                match str_at(run, &["conclusion"]) {
                    "success" | "neutral" | "skipped" => CheckOutcome::Pass,
                    _ => CheckOutcome::Fail,
                }
            };
            CheckItem {
                name: str_at(run, &["name"]).to_string(),
                outcome,
            }
        })
        .collect()
}

/// `commits/{sha}/status` (combined) as check items.
pub fn parse_statuses(value: &Value) -> Vec<CheckItem> {
    value
        .get("statuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|status| CheckItem {
            name: str_at(status, &["context"]).to_string(),
            outcome: match str_at(status, &["state"]) {
                "success" => CheckOutcome::Pass,
                "pending" => CheckOutcome::Pending,
                _ => CheckOutcome::Fail,
            },
        })
        .collect()
}

/// Required check names in `branches/{base}` (classic protection).
pub fn parse_branch_required(value: &Value) -> Vec<String> {
    let checks = value
        .get("protection")
        .and_then(|protection| protection.get("required_status_checks"));
    let mut names: Vec<String> = Vec::new();
    let contexts = checks
        .and_then(|checks| checks.get("contexts"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    let listed = checks
        .and_then(|checks| checks.get("checks"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|check| check.get("context").and_then(Value::as_str));
    for name in contexts.chain(listed) {
        if !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// Required check names in `rules/branches/{base}` (rulesets).
pub fn parse_rules_required(value: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for rule in value.as_array().into_iter().flatten() {
        if str_at(rule, &["type"]) != "required_status_checks" {
            continue;
        }
        let checks = rule
            .get("parameters")
            .and_then(|parameters| parameters.get("required_status_checks"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|check| check.get("context").and_then(Value::as_str));
        for name in checks {
            if !names.iter().any(|known| known == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// A list of reviews (`prefix` "review") or comments ("comment", "line").
pub fn parse_activity(value: &Value, prefix: &str) -> Vec<Activity> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_u64)?;
            let review_state = str_at(item, &["state"]).to_string();
            // A review still being written is not news yet.
            if prefix == "review" && review_state == "PENDING" {
                return None;
            }
            Some(Activity {
                id: format!("{prefix}:{id}"),
                author: str_at(item, &["user", "login"]).to_string(),
                review_state: if prefix == "review" {
                    review_state
                } else {
                    String::new()
                },
            })
        })
        .collect()
}

/// A branch name as one path segment's worth of URL (slashes kept, as the
/// branches endpoints take them).
fn branch_path(branch: &str) -> String {
    branch
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// A snapshot and the ETags of the answers it is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub snapshot: PrSnapshot,
    pub etags: BTreeMap<String, String>,
}

/// Fetches `key` given its previous snapshot and ETags. Merged or closed,
/// only the pull is read.
pub async fn fetch_pr(
    key: &PrKey,
    previous: &PrSnapshot,
    etags: &BTreeMap<String, String>,
    token: Option<&str>,
) -> Result<Fetched, String> {
    let repo = &key.repo;
    let mut reader = Reader {
        token,
        previous: etags,
        etags: BTreeMap::new(),
    };
    let mut snapshot = previous.clone();
    let pull_path = format!("repos/{repo}/pulls/{}", key.number);
    match reader.read(&pull_path).await? {
        Read::Changed(value) => snapshot.core = Some(parse_pull(&value)?),
        Read::Unchanged if previous.core.is_some() => {}
        Read::Unchanged => return Err(format!("gh api {pull_path}: 304 with nothing cached")),
    }
    let Some(core) = snapshot.core.clone() else {
        return Err(format!("gh api {pull_path}: no pull read"));
    };
    if core.state.is_some_and(PrState::is_final) {
        return Ok(Fetched {
            snapshot,
            etags: reader.etags,
        });
    }

    // Checks of the head commit; a new head starts them over.
    if snapshot.checks_sha != core.head_sha {
        snapshot.checks_sha = core.head_sha.clone();
        snapshot.check_runs.clear();
        snapshot.statuses.clear();
    }
    if !core.head_sha.is_empty() {
        let runs = format!("repos/{repo}/commits/{}/check-runs?{PAGE}", core.head_sha);
        if let Read::Changed(value) = reader.read(&runs).await? {
            snapshot.check_runs = parse_check_runs(&value);
        }
        let status = format!("repos/{repo}/commits/{}/status?{PAGE}", core.head_sha);
        if let Read::Changed(value) = reader.read(&status).await? {
            snapshot.statuses = parse_statuses(&value);
        }
    }

    // Required checks of the base branch. A source the token cannot read
    // (protection is hidden from some tokens) keeps what it had and is
    // logged.
    if !core.base_ref.is_empty() {
        let base = branch_path(&core.base_ref);
        let protection = format!("repos/{repo}/branches/{base}");
        match reader.read(&protection).await {
            Ok(Read::Changed(value)) => snapshot.required = parse_branch_required(&value),
            Ok(Read::Unchanged) => {}
            Err(error) => eprintln!("[pr-watch] {}: required checks: {error}", key.label()),
        }
        let rules = format!("repos/{repo}/rules/branches/{base}");
        match reader.read(&rules).await {
            Ok(Read::Changed(value)) => snapshot.required_rules = parse_rules_required(&value),
            Ok(Read::Unchanged) => {}
            Err(error) => eprintln!("[pr-watch] {}: ruleset checks: {error}", key.label()),
        }
    }

    // Reviews and comments.
    let reviews = format!("repos/{repo}/pulls/{}/reviews?{PAGE}", key.number);
    if let Read::Changed(value) = reader.read(&reviews).await? {
        snapshot.reviews = parse_activity(&value, "review");
    }
    let mut comments: Vec<Activity> = previous.comments.clone();
    let mut comments_changed = false;
    let mut by_prefix: HashMap<&str, Vec<Activity>> = HashMap::new();
    for (path, prefix) in [
        (
            format!("repos/{repo}/issues/{}/comments?{PAGE}", key.number),
            "comment",
        ),
        (
            format!("repos/{repo}/pulls/{}/comments?{PAGE}", key.number),
            "line",
        ),
    ] {
        if let Read::Changed(value) = reader.read(&path).await? {
            comments_changed = true;
            by_prefix.insert(prefix, parse_activity(&value, prefix));
        }
    }
    if comments_changed {
        for (prefix, fresh) in by_prefix {
            let tag = format!("{prefix}:");
            comments.retain(|activity| !activity.id.starts_with(&tag));
            comments.extend(fresh);
        }
        snapshot.comments = comments;
    }
    Ok(Fetched {
        snapshot,
        etags: reader.etags,
    })
}

/// The login a token (or gh's active account) authenticates as.
pub async fn login_of(token: Option<&str>) -> Result<String, String> {
    let response = gh_api("user", None, token).await?;
    if response.status != 200 {
        return Err(format!(
            "gh api user: HTTP {}: {}",
            response.status,
            api_message(&response.body)
        ));
    }
    let value: Value = serde_json::from_str(&response.body)
        .map_err(|error| format!("gh api user: answer is not JSON: {error}"))?;
    let login = str_at(&value, &["login"]);
    if login.is_empty() {
        return Err("gh api user: no login in the answer".to_string());
    }
    Ok(login.to_string())
}

// ---------------------------------------------------------------------------
// The poll.

/// One PR a poll reads, with what the watch already has of it.
#[derive(Debug, Clone)]
pub struct PollItem {
    pub key: PrKey,
    /// The mapped account for the repo owner; `None` for gh's active one.
    pub account: Option<String>,
    pub snapshot: PrSnapshot,
    pub etags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct PollRequest {
    pub items: Vec<PollItem>,
}

/// What one PR's poll produced.
#[derive(Debug, Clone)]
pub struct PollResult {
    pub key: PrKey,
    /// The fetched snapshot and the login of the account that read it.
    pub fetched: Result<(Fetched, String), String>,
}

#[derive(Debug, Clone, Default)]
pub struct PollOutcome {
    pub results: Vec<PollResult>,
    /// Logins per account (`None` = gh's active account), for the
    /// confirmation's "runs as".
    pub logins: Vec<(Option<String>, String)>,
    /// PRs left for the next poll because the deadline passed.
    pub skipped: usize,
}

/// Reads every item, account by account, sequentially; never writes. An
/// account whose token or login cannot be read fails its items.
pub async fn poll(request: PollRequest) -> PollOutcome {
    let started = Instant::now();
    let mut outcome = PollOutcome::default();
    let mut by_account: BTreeMap<Option<String>, Vec<PollItem>> = BTreeMap::new();
    for item in request.items {
        by_account
            .entry(item.account.clone())
            .or_default()
            .push(item);
    }
    for (account, items) in by_account {
        let identity = async {
            let token = match account.as_deref() {
                Some(user) => Some(account_token(user).await?),
                None => None,
            };
            let login = match account.as_deref() {
                Some(user) => user.to_string(),
                None => login_of(None).await?,
            };
            Ok::<_, String>((token, login))
        }
        .await;
        let (token, login) = match identity {
            Ok(identity) => identity,
            Err(error) => {
                let who = account.as_deref().unwrap_or("gh's active account");
                for item in items {
                    outcome.results.push(PollResult {
                        key: item.key,
                        fetched: Err(format!("{who}: {error}")),
                    });
                }
                continue;
            }
        };
        outcome.logins.push((account.clone(), login.clone()));
        for item in items {
            if started.elapsed() >= POLL_DEADLINE {
                outcome.skipped += 1;
                continue;
            }
            let fetched = fetch_pr(&item.key, &item.snapshot, &item.etags, token.as_deref())
                .await
                .map(|fetched| (fetched, login.clone()));
            outcome.results.push(PollResult {
                key: item.key,
                fetched,
            });
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(state: PrState, sha: &str) -> PullCore {
        PullCore {
            title: "PRJ-101: add the export".to_string(),
            state: Some(state),
            url: "https://github.com/acme/api/pull/12".to_string(),
            author: "dev".to_string(),
            head_ref: "prj-101".to_string(),
            head_sha: sha.to_string(),
            base_ref: "main".to_string(),
            mergeable: Some(true),
            mergeable_state: "clean".to_string(),
            requested_reviewers: 0,
        }
    }

    fn check(name: &str, outcome: CheckOutcome) -> CheckItem {
        CheckItem {
            name: name.to_string(),
            outcome,
        }
    }

    fn snapshot(state: PrState, sha: &str, checks: Vec<CheckItem>) -> PrSnapshot {
        PrSnapshot {
            core: Some(core(state, sha)),
            checks_sha: sha.to_string(),
            check_runs: checks,
            ..PrSnapshot::default()
        }
    }

    fn fetched(snapshot: PrSnapshot) -> Fetched {
        Fetched {
            snapshot,
            etags: BTreeMap::new(),
        }
    }

    fn comment(id: &str, author: &str) -> Activity {
        Activity {
            id: id.to_string(),
            author: author.to_string(),
            review_state: String::new(),
        }
    }

    fn kinds(items: &[NewsItem]) -> Vec<NewsKind> {
        items.iter().map(|item| item.kind).collect()
    }

    #[test]
    fn the_first_snapshot_is_a_baseline_and_reports_nothing() {
        let mut pr = WatchedPr::new(PrKey::new("acme/api", 12));
        let failing = snapshot(PrState::Open, "a1", vec![check("test", CheckOutcome::Fail)]);
        let mut with_comment = failing.clone();
        with_comment.comments = vec![comment("comment:1", "reviewer")];
        assert!(pr
            .absorb(fetched(with_comment.clone()), "dev", "t1")
            .is_empty());
        assert_eq!(pr.unseen_comments, 0);
        // The same failure again is not news either: it was the baseline.
        assert!(pr.absorb(fetched(with_comment), "dev", "t2").is_empty());
    }

    #[test]
    fn news_is_reported_once_each() {
        let mut pr = WatchedPr::new(PrKey::new("acme/api", 12));
        let pending = snapshot(
            PrState::Open,
            "a1",
            vec![check("test", CheckOutcome::Pending)],
        );
        assert!(pr.absorb(fetched(pending.clone()), "dev", "t1").is_empty());

        let failing = snapshot(PrState::Open, "a1", vec![check("test", CheckOutcome::Fail)]);
        let news = pr.absorb(fetched(failing.clone()), "dev", "t2");
        assert_eq!(kinds(&news), vec![NewsKind::ChecksFailed]);
        assert_eq!(news[0].text, "checks failed: test");
        // Unchanged, and flapping back through pending on the same commit:
        // nothing new.
        assert!(pr.absorb(fetched(failing.clone()), "dev", "t3").is_empty());
        assert!(pr.absorb(fetched(pending), "dev", "t4").is_empty());
        assert!(pr.absorb(fetched(failing), "dev", "t5").is_empty());

        // A new head commit fails again: new news.
        let refailed = snapshot(PrState::Open, "b2", vec![check("test", CheckOutcome::Fail)]);
        assert_eq!(
            kinds(&pr.absorb(fetched(refailed), "dev", "t6")),
            vec![NewsKind::ChecksFailed]
        );
        let mut passing = snapshot(PrState::Open, "b3", vec![check("test", CheckOutcome::Pass)]);
        passing.required = vec!["test".to_string()];
        let news = pr.absorb(fetched(passing.clone()), "dev", "t7");
        assert_eq!(kinds(&news), vec![NewsKind::RequiredChecksPassed]);
        assert_eq!(news[0].text, "required checks passed");
        assert!(pr.absorb(fetched(passing.clone()), "dev", "t8").is_empty());

        // Comments: the user's own are not news; two from others are one
        // item, counted as unseen.
        let mut commented = passing.clone();
        commented.comments = vec![comment("comment:5", "dev")];
        assert!(pr
            .absorb(fetched(commented.clone()), "Dev", "t9")
            .is_empty());
        commented.comments.push(comment("line:6", "reviewer"));
        commented.reviews.push(Activity {
            id: "review:7".to_string(),
            author: "reviewer".to_string(),
            review_state: "CHANGES_REQUESTED".to_string(),
        });
        let news = pr.absorb(fetched(commented.clone()), "dev", "t10");
        assert_eq!(kinds(&news), vec![NewsKind::NewActivity]);
        assert_eq!(news[0].text, "new 1 review and 1 comment from reviewer");
        assert_eq!(pr.unseen_comments, 2);
        assert!(pr
            .absorb(fetched(commented.clone()), "dev", "t11")
            .is_empty());

        // Conflicts, then merged; merged stops the watch.
        let mut conflicting = commented.clone();
        if let Some(core) = conflicting.core.as_mut() {
            core.mergeable = Some(false);
            core.mergeable_state = "dirty".to_string();
        }
        assert_eq!(
            kinds(&pr.absorb(fetched(conflicting.clone()), "dev", "t12")),
            vec![NewsKind::Conflicting]
        );
        assert!(pr
            .absorb(fetched(conflicting.clone()), "dev", "t13")
            .is_empty());
        let mut merged = conflicting;
        if let Some(core) = merged.core.as_mut() {
            core.state = Some(PrState::Merged);
        }
        assert_eq!(
            kinds(&pr.absorb(fetched(merged.clone()), "dev", "t14")),
            vec![NewsKind::Merged]
        );
        assert!(pr.done());
        assert!(!pr.due(Duration::ZERO, chrono::Utc::now()));
        assert!(pr.absorb(fetched(merged), "dev", "t15").is_empty());
        assert_eq!(pr.news.len(), 6);
    }

    #[test]
    fn closed_is_news_and_drafts_still_report_checks() {
        let mut pr = WatchedPr::new(PrKey::new("acme/web", 3));
        pr.absorb(
            fetched(snapshot(
                PrState::Draft,
                "c1",
                vec![check("ci", CheckOutcome::Pending)],
            )),
            "dev",
            "t1",
        );
        assert_eq!(
            kinds(&pr.absorb(
                fetched(snapshot(
                    PrState::Draft,
                    "c1",
                    vec![check("ci", CheckOutcome::Pass)]
                )),
                "dev",
                "t2"
            )),
            vec![NewsKind::RequiredChecksPassed]
        );
        assert_eq!(pr.news[0].text, "checks passed");
        assert_eq!(
            kinds(&pr.absorb(
                fetched(snapshot(
                    PrState::Closed,
                    "c1",
                    vec![check("ci", CheckOutcome::Pass)]
                )),
                "dev",
                "t3"
            )),
            vec![NewsKind::Closed]
        );
    }

    #[test]
    fn required_checks_and_review_decision_derive_from_the_snapshot() {
        let mut s = snapshot(
            PrState::Open,
            "a1",
            vec![
                check("build", CheckOutcome::Pass),
                check("e2e", CheckOutcome::Pending),
            ],
        );
        assert_eq!(s.checks(), ChecksState::Pending);
        assert!(!s.required_passed());
        s.required = vec!["build".to_string()];
        assert!(s.required_passed());
        s.statuses = vec![check("deploy/preview", CheckOutcome::Fail)];
        assert_eq!(s.checks(), ChecksState::Fail);
        assert_eq!(s.failing_checks(), vec!["deploy/preview".to_string()]);

        let review = |author: &str, state: &str, id: u32| Activity {
            id: format!("review:{id}"),
            author: author.to_string(),
            review_state: state.to_string(),
        };
        assert_eq!(s.review_decision(), ReviewDecision::None);
        s.reviews = vec![
            review("a", "CHANGES_REQUESTED", 1),
            review("b", "COMMENTED", 2),
        ];
        assert_eq!(s.review_decision(), ReviewDecision::ChangesRequested);
        s.reviews.push(review("a", "APPROVED", 3));
        assert_eq!(s.review_decision(), ReviewDecision::Approved);
        s.reviews = Vec::new();
        if let Some(core) = s.core.as_mut() {
            core.requested_reviewers = 1;
            core.mergeable = None;
            core.mergeable_state = "unknown".to_string();
        }
        assert_eq!(s.review_decision(), ReviewDecision::ReviewRequested);
        assert_eq!(s.conflicting(), None);
    }

    #[test]
    fn due_follows_the_interval_and_never_for_finished_prs() {
        let mut pr = WatchedPr::new(PrKey::new("acme/api", 12));
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-31T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(pr.due(OPEN_INTERVAL, now));
        pr.checked_at = Some("2026-10-31T09:59:00Z".to_string());
        assert!(!pr.due(OPEN_INTERVAL, now));
        pr.checked_at = Some("2026-10-31T09:57:00Z".to_string());
        assert!(pr.due(OPEN_INTERVAL, now));
        assert!(!pr.due(PARKED_INTERVAL, now));
        pr.failed("gh api: HTTP 502".to_string(), "2026-10-31T10:00:00Z");
        assert!(!pr.due(OPEN_INTERVAL, now));
        assert_eq!(pr.error.as_deref(), Some("gh api: HTTP 502"));
    }

    #[test]
    fn accounts_map_by_owner_and_commands_show_exactly_what_runs() {
        let mut accounts = BTreeMap::new();
        accounts.insert("Acme".to_string(), " acme-dev ".to_string());
        accounts.insert("blank".to_string(), "  ".to_string());
        assert_eq!(account_for("acme", &accounts).as_deref(), Some("acme-dev"));
        assert_eq!(account_for("blank", &accounts), None);
        assert_eq!(account_for("other", &accounts), None);

        let key = PrKey::new("acme/api", 12);
        assert_eq!(
            action_command_line(PrAction::Merge(MergeMethod::Merge), &key, Some("acme-dev")),
            "GH_TOKEN=\"$(gh auth token --user acme-dev)\" gh pr merge 12 --repo acme/api --merge"
        );
        assert_eq!(
            action_command_line(PrAction::Merge(MergeMethod::Squash), &key, None),
            "gh pr merge 12 --repo acme/api --squash"
        );
        assert_eq!(
            action_command_line(PrAction::Approve, &key, None),
            "gh pr review 12 --repo acme/api --approve"
        );
        assert!(valid_login("acme-dev"));
        assert!(!valid_login("acme dev"));
        assert!(!valid_login("$(x)"));
    }

    #[test]
    fn parses_gh_api_include_output() {
        let ok = "HTTP/2.0 200 OK\r\nContent-Type: application/json\r\nEtag: W/\"abc\"\r\n\r\n{\"id\":1}";
        assert_eq!(
            parse_api_response(ok),
            Some(ApiResponse {
                status: 200,
                etag: Some("W/\"abc\"".to_string()),
                body: "{\"id\":1}".to_string(),
            })
        );
        let not_modified = "HTTP/2.0 304 Not Modified\nEtag: \"abc\"\n\n";
        let parsed = parse_api_response(not_modified).unwrap();
        assert_eq!(parsed.status, 304);
        assert_eq!(parsed.body, "");
        assert_eq!(parse_api_response("gh: Not Found"), None);
        assert_eq!(parse_api_response(""), None);
    }

    #[test]
    fn parses_rest_payloads() {
        let pull: Value = serde_json::json!({
            "title": "PRJ-101: export", "state": "closed", "merged": true, "draft": false,
            "html_url": "https://github.com/acme/api/pull/12", "user": {"login": "dev"},
            "head": {"ref": "prj-101", "sha": "a1"}, "base": {"ref": "main"},
            "mergeable": null, "mergeable_state": "unknown",
            "requested_reviewers": [{"login": "r"}], "requested_teams": []
        });
        let core = parse_pull(&pull).unwrap();
        assert_eq!(core.state, Some(PrState::Merged));
        assert_eq!(core.mergeable, None);
        assert_eq!(core.requested_reviewers, 1);
        let draft = serde_json::json!({"state": "open", "draft": true});
        assert_eq!(parse_pull(&draft).unwrap().state, Some(PrState::Draft));
        assert!(parse_pull(&serde_json::json!({"state": "odd"})).is_err());

        let runs = serde_json::json!({"check_runs": [
            {"name": "build", "status": "completed", "conclusion": "success"},
            {"name": "lint", "status": "completed", "conclusion": "skipped"},
            {"name": "test", "status": "completed", "conclusion": "failure"},
            {"name": "e2e", "status": "in_progress", "conclusion": null}
        ]});
        assert_eq!(
            parse_check_runs(&runs)
                .iter()
                .map(|c| c.outcome)
                .collect::<Vec<_>>(),
            vec![
                CheckOutcome::Pass,
                CheckOutcome::Pass,
                CheckOutcome::Fail,
                CheckOutcome::Pending
            ]
        );
        let statuses = serde_json::json!({"state": "pending", "statuses": [
            {"context": "deploy", "state": "error"}, {"context": "preview", "state": "pending"}
        ]});
        assert_eq!(
            parse_statuses(&statuses),
            vec![
                check("deploy", CheckOutcome::Fail),
                check("preview", CheckOutcome::Pending)
            ]
        );
        let branch = serde_json::json!({"protection": {"required_status_checks": {
            "contexts": ["build"], "checks": [{"context": "build"}, {"context": "test"}]
        }}});
        assert_eq!(parse_branch_required(&branch), vec!["build", "test"]);
        let rules = serde_json::json!([
            {"type": "pull_request"},
            {"type": "required_status_checks", "parameters": {"required_status_checks": [{"context": "e2e"}]}}
        ]);
        assert_eq!(parse_rules_required(&rules), vec!["e2e"]);
        let reviews = serde_json::json!([
            {"id": 7, "user": {"login": "r"}, "state": "APPROVED"},
            {"id": 8, "user": {"login": "r"}, "state": "PENDING"}
        ]);
        assert_eq!(
            parse_activity(&reviews, "review"),
            vec![Activity {
                id: "review:7".to_string(),
                author: "r".to_string(),
                review_state: "APPROVED".to_string(),
            }]
        );
        assert_eq!(branch_path("release/2026 q4"), "release/2026%20q4");
    }

    #[test]
    fn watch_files_round_trip_and_old_files_load() {
        let mut file = WatchFile::default();
        let mut pr = WatchedPr::new(PrKey::new("acme/api", 12));
        pr.absorb(
            fetched(snapshot(PrState::Open, "a1", Vec::new())),
            "dev",
            "2026-10-31T10:00:00Z",
        );
        file.prs.insert(pr.key.id(), pr);
        let path = std::env::temp_dir()
            .join(format!("gitterm-pr-watch-{}", uuid::Uuid::new_v4()))
            .join(STATE_FILE_NAME);
        file.save(&path).unwrap();
        assert_eq!(WatchFile::load(&path), file);
        // A minimal entry loads with every other field defaulted.
        std::fs::write(
            &path,
            r#"{"prs":{"acme/api#3":{"key":{"repo":"acme/api","number":3}}}}"#,
        )
        .unwrap();
        let loaded = WatchFile::load(&path);
        assert_eq!(
            loaded.prs["acme/api#3"],
            WatchedPr::new(PrKey::new("acme/api", 3))
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// Read-only, against GitHub: `GITTERM_REAL_PR=owner/repo#n cargo test
    /// --lib pr_watch::tests::real_pr -- --ignored --nocapture`. Optional
    /// `GITTERM_REAL_PR_ACCOUNT=<gh user>` reads as that account. Polls the
    /// PR twice through [`poll`], the second time with the first answers'
    /// ETags, and prints what it found.
    #[tokio::test]
    #[ignore = "reads a real PR from GitHub; set GITTERM_REAL_PR=owner/repo#n"]
    async fn real_pr() {
        let Ok(spec) = std::env::var("GITTERM_REAL_PR") else {
            eprintln!("GITTERM_REAL_PR is not set; nothing to read");
            return;
        };
        let key = PrKey::parse(&spec).expect("GITTERM_REAL_PR is owner/repo#n");
        let account = std::env::var("GITTERM_REAL_PR_ACCOUNT").ok();
        let mut watched = WatchedPr::new(key.clone());
        for round in 1..=2 {
            let started = Instant::now();
            let outcome = poll(PollRequest {
                items: vec![PollItem {
                    key: key.clone(),
                    account: account.clone(),
                    snapshot: watched.snapshot.clone(),
                    etags: watched.etags.clone(),
                }],
            })
            .await;
            let result = outcome.results.into_iter().next().expect("one result");
            let (fetched, login) = result.fetched.expect("the PR reads");
            let unchanged = fetched
                .etags
                .iter()
                .filter(|(path, etag)| watched.etags.get(*path) == Some(*etag))
                .count();
            let news = watched.absorb(fetched, &login, &chrono::Utc::now().to_rfc3339());
            let s = &watched.snapshot;
            let core = s.core.as_ref().expect("a pull");
            println!(
                "round {round} as {login} in {:?}: {} {:?} \"{}\" head {} -> base {}\n  {} ({} runs, {} statuses, required {:?})\n  {} · conflicting {:?} · {} reviews · {} comments\n  {} answers, {unchanged} with an unchanged ETag; news {:?}",
                started.elapsed(),
                key.label(),
                core.state,
                core.title,
                core.head_ref,
                core.base_ref,
                s.checks().label(),
                s.check_runs.len(),
                s.statuses.len(),
                s.required_checks(),
                s.review_decision().label(),
                s.conflicting(),
                s.reviews.len(),
                s.comments.len(),
                watched.etags.len(),
                news,
            );
        }
    }
}
