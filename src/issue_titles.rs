//! Issue titles for the Work page's Issues section (TRU-203), read from
//! the user's own `linear` CLI, read-only.
//!
//! Which workspaces: `linear auth list` (its WORKSPACE column). Per key:
//! `linear issue view <KEY> --json --no-comments --no-download
//! --workspace <slug>`, trying first the workspace that last resolved the
//! key's prefix ([`try_order`]). One CLI call at a time, each within
//! [`CALL_TIMEOUT`]; a round reads at most [`ROUND_MAX_KEYS`] keys.
//!
//! What was read is cached in `issue-titles.json` in the config dir
//! ([`IssueTitlesFile`]): per key its title, URL, state, assignee and when
//! it was read, plus the prefix -> workspace map. A found entry is read
//! again after [`FOUND_FRESH`] while its section shows; a key no
//! workspace knows is remembered as not found and tried again after
//! [`NOT_FOUND_RETRY`] ([`needs_fetch`]). Nothing here reads or prints a
//! token; `linear auth token` is never called.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// The cache file in the config dir.
pub const CACHE_FILE_NAME: &str = "issue-titles.json";
/// One `linear` call at most this long.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Keys one round reads at most; the rest wait for the next round.
pub const ROUND_MAX_KEYS: usize = 20;
/// A found entry is read again after this long.
pub const FOUND_FRESH: Duration = Duration::from_secs(3600);
/// A key no workspace knows is tried again after this long.
pub const NOT_FOUND_RETRY: Duration = Duration::from_secs(24 * 3600);
/// Issues the Work page lists before "n more".
pub const ISSUES_SHOWN: usize = 5;

/// Where `linear` is looked for after `PATH` (an app started from the
/// Finder gets a short PATH), as for `gh`.
const EXTRA_BIN_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

// ---------------------------------------------------------------------------
// Keys and workspaces.

/// A key's prefix (`ABC` of `ABC-12`), when `key` is a whole issue key:
/// `[A-Z][A-Z0-9]{1,9}-[0-9]{1,7}`. Also what makes a key safe to pass to
/// the CLI as an argument.
pub fn key_prefix(key: &str) -> Option<&str> {
    let (prefix, number) = key.split_once('-')?;
    let mut chars = prefix.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_uppercase());
    let rest_ok = chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    let prefix_ok = first_ok && rest_ok && (2..=10).contains(&prefix.len());
    let number_ok =
        (1..=7).contains(&number.len()) && number.bytes().all(|byte| byte.is_ascii_digit());
    (prefix_ok && number_ok).then_some(prefix)
}

/// A workspace slug safe to pass to the CLI.
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && !slug.starts_with('-')
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Removes terminal colour codes (`ESC [ ... letter`).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The workspace slugs of `linear auth list`, in its order: the first
/// column under the `WORKSPACE` header, the `*` default marker dropped.
/// An output without that header is an error.
pub fn parse_auth_list(output: &str) -> Result<Vec<String>, String> {
    let clean = strip_ansi(output);
    let mut lines = clean.lines();
    let header = lines
        .by_ref()
        .find(|line| line.split_whitespace().next() == Some("WORKSPACE"));
    if header.is_none() {
        return Err("`linear auth list` printed no WORKSPACE header".to_string());
    }
    let mut slugs: Vec<String> = Vec::new();
    for line in lines {
        let line = line.trim_start();
        let line = line.strip_prefix('*').unwrap_or(line);
        let Some(slug) = line.split_whitespace().next() else {
            continue;
        };
        if !valid_slug(slug) {
            return Err(format!(
                "`linear auth list` row does not start with a workspace slug: {:?}",
                shorten(line, 80)
            ));
        }
        if !slugs.iter().any(|known| known == slug) {
            slugs.push(slug.to_string());
        }
    }
    Ok(slugs)
}

/// The order to try `workspaces` in for `key`: the one that last resolved
/// its prefix first (when still listed), then the rest in listed order.
pub fn try_order(
    key: &str,
    prefixes: &BTreeMap<String, String>,
    workspaces: &[String],
) -> Vec<String> {
    let remembered = key_prefix(key)
        .and_then(|prefix| prefixes.get(prefix))
        .filter(|slug| workspaces.contains(slug));
    let mut order: Vec<String> = remembered.into_iter().cloned().collect();
    order.extend(
        workspaces
            .iter()
            .filter(|slug| Some(*slug) != remembered)
            .cloned(),
    );
    order
}

// ---------------------------------------------------------------------------
// Issues.

/// What `linear issue view --json` says about an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueInfo {
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub state: String,
    pub assignee: Option<String>,
    /// The start of the description as plain text ([`description_excerpt`]).
    pub description: Option<String>,
}

/// Characters of the description kept for the hover.
pub const DESCRIPTION_CHARS: usize = 400;

/// The first [`DESCRIPTION_CHARS`] of a Markdown description as one plain
/// paragraph: heading, list, quote and emphasis markers dropped, links
/// reduced to their text, whitespace collapsed. `None` when empty.
pub fn description_excerpt(markdown: &str) -> Option<String> {
    let mut words: Vec<String> = Vec::new();
    for line in markdown.lines() {
        let line = line
            .trim()
            .trim_start_matches(['#', '>', '-', '*', '+'])
            .trim();
        if line.starts_with("```") || line.starts_with("![") {
            continue;
        }
        for word in line.split_whitespace() {
            words.push(word.to_string());
        }
    }
    let joined = words.join(" ");
    let mut plain = String::with_capacity(joined.len());
    let mut chars = joined.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' | '_' | '`' => {}
            // [text](url) keeps the text.
            ']' if chars.peek() == Some(&'(') => {
                for c in chars.by_ref() {
                    if c == ')' {
                        break;
                    }
                }
            }
            '[' => {}
            c => plain.push(c),
        }
    }
    let plain = plain.trim();
    (!plain.is_empty()).then(|| shorten(plain, DESCRIPTION_CHARS))
}

/// Reads `identifier`, `title`, `url` and `state.name` (all required),
/// `assignee` (`displayName`, else `name`; null when unassigned) and the
/// optional `description`.
pub fn parse_issue(json: &str) -> Result<IssueInfo, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|error| format!("issue JSON: {error}"))?;
    let required = |field: &str, found: Option<&str>| -> Result<String, String> {
        found
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("issue JSON has no {field}"))
    };
    let identifier = required(
        "identifier",
        value.get("identifier").and_then(Value::as_str),
    )?;
    let title = required("title", value.get("title").and_then(Value::as_str))?;
    let url = required("url", value.get("url").and_then(Value::as_str))?;
    if !url.starts_with("https://") {
        return Err(format!(
            "issue JSON url is not https: {}",
            shorten(&url, 80)
        ));
    }
    let state = required(
        "state.name",
        value
            .get("state")
            .and_then(|state| state.get("name"))
            .and_then(Value::as_str),
    )?;
    let assignee = match value.get("assignee") {
        None | Some(Value::Null) => None,
        Some(assignee) => Some(
            ["displayName", "name"]
                .iter()
                .find_map(|field| {
                    assignee
                        .get(field)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                })
                .map(str::to_string)
                .ok_or("issue JSON assignee has no displayName or name")?,
        ),
    };
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .and_then(description_excerpt);
    Ok(IssueInfo {
        identifier,
        title,
        url,
        state,
        assignee,
        description,
    })
}

/// A failed `issue view` that means "this workspace has no such issue":
/// the CLI prints `... not found ...` (its NotFoundError and the API's
/// "Entity not found").
pub fn is_not_found(stderr: &str) -> bool {
    strip_ansi(stderr)
        .to_ascii_lowercase()
        .contains("not found")
}

// ---------------------------------------------------------------------------
// The cache.

/// One key's cached read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedIssue {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// The description's start, for the hover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// RFC 3339.
    pub fetched_at: String,
    /// No workspace knew the key at `fetched_at`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_found: bool,
}

impl CachedIssue {
    pub fn found(info: IssueInfo, at: &str) -> Self {
        Self {
            title: Some(info.title),
            url: Some(info.url),
            state: Some(info.state),
            assignee: info.assignee,
            description: info.description,
            fetched_at: at.to_string(),
            not_found: false,
        }
    }

    pub fn not_found(at: &str) -> Self {
        Self {
            fetched_at: at.to_string(),
            not_found: true,
            ..Self::default()
        }
    }
}

/// Whether `key` is read again at `now`: never read, a found entry older
/// than [`FOUND_FRESH`], a not-found one older than [`NOT_FOUND_RETRY`],
/// or an entry whose `fetched_at` does not parse.
pub fn needs_fetch(entry: Option<&CachedIssue>, now: chrono::DateTime<chrono::Utc>) -> bool {
    let Some(entry) = entry else {
        return true;
    };
    let Some(age) = chrono::DateTime::parse_from_rfc3339(&entry.fetched_at)
        .ok()
        .map(|at| now.signed_duration_since(at))
    else {
        return true;
    };
    let limit = if entry.not_found {
        NOT_FOUND_RETRY
    } else {
        FOUND_FRESH
    };
    // A stamp in the future (clock change) is stale too.
    age.to_std().map_or(true, |age| age >= limit)
}

/// `issue-titles.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueTitlesFile {
    #[serde(default)]
    pub issues: BTreeMap<String, CachedIssue>,
    /// Key prefix -> the workspace slug that last resolved it.
    #[serde(default)]
    pub prefixes: BTreeMap<String, String>,
}

impl IssueTitlesFile {
    pub fn path_in(config_dir: &Path) -> PathBuf {
        config_dir.join(CACHE_FILE_NAME)
    }

    /// The file at `path`; missing is empty, unreadable is logged and empty
    /// (left alone until the next save replaces it).
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                eprintln!(
                    "[issue-titles] {} is not an issue-titles file ({error}); starting empty",
                    path.display()
                );
                Self::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                eprintln!(
                    "[issue-titles] cannot read {}: {error}; starting empty",
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

    /// Applies one round's results stamped `at`; a failed key is left as
    /// it was.
    pub fn absorb(&mut self, outcome: &RoundOutcome, at: &str) {
        for (key, result) in &outcome.results {
            match result {
                KeyResult::Found { info, workspace } => {
                    if let Some(prefix) = key_prefix(key) {
                        self.prefixes.insert(prefix.to_string(), workspace.clone());
                    }
                    self.issues
                        .insert(key.clone(), CachedIssue::found(info.clone(), at));
                }
                KeyResult::NotFound => {
                    self.issues.insert(key.clone(), CachedIssue::not_found(at));
                }
                KeyResult::Failed(_) => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The fold.

/// How many of `total` issues show, and the fold button's label when some
/// are hidden: "▸ n more" folded, "▾ show fewer" expanded.
pub fn fold(total: usize, all: bool) -> (usize, Option<String>) {
    let hidden = total.saturating_sub(ISSUES_SHOWN);
    if hidden == 0 {
        return (total, None);
    }
    if all {
        (total, Some("\u{25BE} show fewer".to_string()))
    } else {
        (ISSUES_SHOWN, Some(format!("\u{25B8} {hidden} more")))
    }
}

/// `text` cut to `max` characters with an ellipsis.
pub fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}\u{2026}", cut.trim_end())
}

// ---------------------------------------------------------------------------
// The CLI.

/// `linear` on `path_var` (a `PATH` value), else in the usual Homebrew
/// directories.
pub fn find_linear(path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = path_var
        .map(|paths| std::env::split_paths(paths).collect())
        .unwrap_or_default();
    dirs.into_iter()
        .chain(EXTRA_BIN_DIRS.iter().map(PathBuf::from))
        .map(|dir| dir.join("linear"))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// One round: the keys to read and what is known of their prefixes.
#[derive(Debug, Clone)]
pub struct RoundRequest {
    pub binary: PathBuf,
    pub keys: Vec<String>,
    pub prefixes: BTreeMap<String, String>,
}

/// What reading one key gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyResult {
    Found {
        info: IssueInfo,
        workspace: String,
    },
    /// Every listed workspace said it has no such issue.
    NotFound,
    /// Not read (an error, a timeout, or no workspaces); tried again later.
    Failed(String),
}

#[derive(Debug, Clone, Default)]
pub struct RoundOutcome {
    pub results: Vec<(String, KeyResult)>,
}

enum CallError {
    NotFound,
    Failed(String),
    TimedOut(String),
}

async fn run_linear(binary: &Path, args: &[&str]) -> Result<String, CallError> {
    let what = format!("linear {}", args.join(" "));
    let mut command = tokio::process::Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(CALL_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(CallError::Failed(format!("{what}: {error}"))),
        Err(_) => {
            return Err(CallError::TimedOut(format!(
                "{what}: timed out after {}s",
                CALL_TIMEOUT.as_secs()
            )))
        }
    };
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_not_found(&stderr) {
        return Err(CallError::NotFound);
    }
    Err(CallError::Failed(format!(
        "{what}: exited {}: {}",
        output.status,
        shorten(strip_ansi(&stderr).trim(), 200)
    )))
}

/// Reads `request.keys` one CLI call at a time. Each key tries the
/// workspaces in [`try_order`]; a found key stops there. A timeout ends
/// the round (the rest are `Failed` and wait for the next one).
pub async fn fetch_round(request: RoundRequest) -> RoundOutcome {
    let mut outcome = RoundOutcome::default();
    let fail_all = |keys: &[String], why: &str| -> Vec<(String, KeyResult)> {
        keys.iter()
            .map(|key| (key.clone(), KeyResult::Failed(why.to_string())))
            .collect()
    };
    let workspaces = match run_linear(&request.binary, &["auth", "list"]).await {
        Ok(stdout) => parse_auth_list(&stdout),
        Err(CallError::NotFound) => Err("linear auth list: not found".to_string()),
        Err(CallError::Failed(error) | CallError::TimedOut(error)) => Err(error),
    };
    let workspaces = match workspaces {
        Ok(slugs) if slugs.is_empty() => {
            outcome.results = fail_all(&request.keys, "linear has no workspaces logged in");
            return outcome;
        }
        Ok(slugs) => slugs,
        Err(error) => {
            outcome.results = fail_all(&request.keys, &error);
            return outcome;
        }
    };
    let mut prefixes = request.prefixes.clone();
    for (index, key) in request.keys.iter().enumerate() {
        if key_prefix(key).is_none() {
            outcome.results.push((
                key.clone(),
                KeyResult::Failed("not an issue key".to_string()),
            ));
            continue;
        }
        let mut failure: Option<String> = None;
        let mut result: Option<KeyResult> = None;
        for workspace in try_order(key, &prefixes, &workspaces) {
            let args = [
                "issue",
                "view",
                key.as_str(),
                "--json",
                "--no-comments",
                "--no-download",
                "--workspace",
                workspace.as_str(),
            ];
            match run_linear(&request.binary, &args).await {
                Ok(stdout) => match parse_issue(&stdout) {
                    Ok(info) => {
                        if let Some(prefix) = key_prefix(key) {
                            prefixes.insert(prefix.to_string(), workspace.clone());
                        }
                        result = Some(KeyResult::Found { info, workspace });
                        break;
                    }
                    Err(error) => {
                        failure = Some(format!("{key} in {workspace}: {error}"));
                    }
                },
                Err(CallError::NotFound) => {}
                Err(CallError::Failed(error)) => failure = Some(error),
                Err(CallError::TimedOut(error)) => {
                    outcome
                        .results
                        .push((key.clone(), KeyResult::Failed(error.clone())));
                    outcome.results.extend(fail_all(
                        &request.keys[index + 1..],
                        &format!("skipped after a timeout ({error})"),
                    ));
                    return outcome;
                }
            }
        }
        // Not found only when every workspace said so: one that failed
        // might know it.
        let result = match (result, failure) {
            (Some(found), _) => found,
            (None, Some(error)) => KeyResult::Failed(error),
            (None, None) => KeyResult::NotFound,
        };
        outcome.results.push((key.clone(), result));
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTH_LIST: &str = "  WORKSPACE   ORG NAME    USER\n\
* acme        Acme        Pat Example <pat@example.com>\n  \
globex      Globex Corp Pat Example <pat@example.com>\n";

    #[test]
    fn auth_list_reads_the_workspace_column_without_the_default_marker() {
        assert_eq!(parse_auth_list(AUTH_LIST).unwrap(), vec!["acme", "globex"]);
        let coloured = "\u{1b}[1m  WORKSPACE  USER\u{1b}[22m\n\u{1b}[32m* acme\u{1b}[39m Pat\n";
        assert_eq!(parse_auth_list(coloured).unwrap(), vec!["acme"]);
        assert_eq!(
            parse_auth_list("  WORKSPACE  USER\n\n").unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn auth_list_without_its_header_or_with_odd_rows_is_an_error() {
        assert!(parse_auth_list("No workspaces configured\n").is_err());
        assert!(parse_auth_list("  WORKSPACE  USER\n* --flag x\n").is_err());
        assert!(parse_auth_list("  WORKSPACE  USER\n  a;b x\n").is_err());
    }

    const ISSUE: &str = r#"{
        "identifier": "ABC-12",
        "title": "Make the widget faster",
        "description": "long text",
        "url": "https://linear.app/acme/issue/ABC-12/make-the-widget-faster",
        "branchName": "pat/abc-12-make-the-widget-faster",
        "state": {"name": "In Progress", "color": "yellow"},
        "assignee": {"name": "pat", "displayName": "Pat Example"},
        "priority": 2
    }"#;

    #[test]
    fn descriptions_become_a_short_plain_paragraph() {
        assert_eq!(
            description_excerpt("## Goal\n\n- Make **the widget** [faster](https://x/y)\n```\ncode\n```\n> quoted `bit`").as_deref(),
            Some("Goal Make the widget faster code quoted bit")
        );
        assert_eq!(description_excerpt("  \n\n"), None);
        let long = "word ".repeat(200);
        let excerpt = description_excerpt(&long).unwrap();
        assert!(excerpt.chars().count() <= DESCRIPTION_CHARS + 1);
        assert_eq!(
            parse_issue(ISSUE).unwrap().description.as_deref(),
            Some("long text")
        );
    }

    #[test]
    fn issue_json_gives_title_url_state_and_assignee() {
        let info = parse_issue(ISSUE).unwrap();
        assert_eq!(info.identifier, "ABC-12");
        assert_eq!(info.title, "Make the widget faster");
        assert_eq!(
            info.url,
            "https://linear.app/acme/issue/ABC-12/make-the-widget-faster"
        );
        assert_eq!(info.state, "In Progress");
        assert_eq!(info.assignee.as_deref(), Some("Pat Example"));
    }

    #[test]
    fn issue_json_unassigned_and_name_only_assignees() {
        let unassigned = ISSUE.replace(r#"{"name": "pat", "displayName": "Pat Example"}"#, "null");
        assert_eq!(parse_issue(&unassigned).unwrap().assignee, None);
        let name_only = ISSUE.replace(r#", "displayName": "Pat Example""#, "");
        assert_eq!(
            parse_issue(&name_only).unwrap().assignee.as_deref(),
            Some("pat")
        );
    }

    #[test]
    fn issue_json_missing_required_fields_is_an_error() {
        assert!(parse_issue("not json").is_err());
        assert!(
            parse_issue(r#"{"identifier":"ABC-1","url":"https://x","state":{"name":"Todo"}}"#)
                .is_err()
        );
        assert!(
            parse_issue(r#"{"identifier":"ABC-1","title":"t","state":{"name":"Todo"}}"#).is_err()
        );
        assert!(parse_issue(r#"{"identifier":"ABC-1","title":"t","url":"https://x"}"#).is_err());
        assert!(parse_issue(
            r#"{"identifier":"ABC-1","title":"t","url":"file:///etc/passwd","state":{"name":"Todo"}}"#
        )
        .is_err());
        assert!(parse_issue(
            r#"{"identifier":"ABC-1","title":"t","url":"https://x","state":{"name":"Todo"},"assignee":{}}"#
        )
        .is_err());
    }

    #[test]
    fn not_found_is_read_from_stderr() {
        assert!(is_not_found("\n Issue not found: ABC-12\n"));
        assert!(is_not_found("\u{1b}[31mEntity not found\u{1b}[39m"));
        assert!(!is_not_found("Authentication failed"));
    }

    #[test]
    fn prefix_extraction_follows_the_key_pattern() {
        assert_eq!(key_prefix("ABC-12"), Some("ABC"));
        assert_eq!(key_prefix("A1-2"), Some("A1"));
        assert_eq!(key_prefix("ABCDEFGHIJ-1234567"), Some("ABCDEFGHIJ"));
        for bad in [
            "A-1",
            "ABCDEFGHIJK-1",
            "abc-1",
            "1BC-1",
            "ABC-",
            "ABC-12345678",
            "ABC-1x",
            "ABC",
            "-ABC-1",
            "ABC-1-2",
        ] {
            assert_eq!(key_prefix(bad), None, "{bad}");
        }
    }

    fn slugs(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn try_order_puts_the_remembered_workspace_first() {
        let workspaces = slugs(&["acme", "globex", "initech"]);
        let mut prefixes = BTreeMap::new();
        assert_eq!(try_order("ABC-1", &prefixes, &workspaces), workspaces);
        prefixes.insert("ABC".to_string(), "initech".to_string());
        assert_eq!(
            try_order("ABC-1", &prefixes, &workspaces),
            slugs(&["initech", "acme", "globex"])
        );
        // Another prefix keeps the listed order.
        assert_eq!(try_order("XYZ-1", &prefixes, &workspaces), workspaces);
        // A remembered workspace no longer listed is not tried.
        prefixes.insert("ABC".to_string(), "gone".to_string());
        assert_eq!(try_order("ABC-1", &prefixes, &workspaces), workspaces);
    }

    fn at(stamp: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(stamp)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn info() -> IssueInfo {
        parse_issue(ISSUE).unwrap()
    }

    #[test]
    fn found_entries_refresh_after_an_hour() {
        let entry = CachedIssue::found(info(), "2026-01-01T10:00:00Z");
        assert!(!needs_fetch(Some(&entry), at("2026-01-01T10:59:59Z")));
        assert!(needs_fetch(Some(&entry), at("2026-01-01T11:00:00Z")));
        assert!(needs_fetch(None, at("2026-01-01T10:00:00Z")));
    }

    #[test]
    fn not_found_entries_retry_after_a_day() {
        let entry = CachedIssue::not_found("2026-01-01T10:00:00Z");
        assert!(!needs_fetch(Some(&entry), at("2026-01-01T12:00:00Z")));
        assert!(!needs_fetch(Some(&entry), at("2026-01-02T09:59:59Z")));
        assert!(needs_fetch(Some(&entry), at("2026-01-02T10:00:00Z")));
    }

    #[test]
    fn bad_or_future_stamps_are_stale() {
        let mut entry = CachedIssue::found(info(), "yesterday");
        assert!(needs_fetch(Some(&entry), at("2026-01-01T10:00:00Z")));
        entry.fetched_at = "2026-01-01T12:00:00Z".to_string();
        assert!(needs_fetch(Some(&entry), at("2026-01-01T10:00:00Z")));
    }

    #[test]
    fn absorb_records_found_and_not_found_and_skips_failures() {
        let mut file = IssueTitlesFile::default();
        file.issues.insert(
            "XYZ-3".to_string(),
            CachedIssue::found(info(), "2026-01-01T09:00:00Z"),
        );
        let outcome = RoundOutcome {
            results: vec![
                (
                    "ABC-12".to_string(),
                    KeyResult::Found {
                        info: info(),
                        workspace: "globex".to_string(),
                    },
                ),
                ("ABC-99".to_string(), KeyResult::NotFound),
                (
                    "XYZ-3".to_string(),
                    KeyResult::Failed("offline".to_string()),
                ),
            ],
        };
        file.absorb(&outcome, "2026-01-01T10:00:00Z");
        assert_eq!(file.prefixes.get("ABC").map(String::as_str), Some("globex"));
        let found = &file.issues["ABC-12"];
        assert_eq!(found.title.as_deref(), Some("Make the widget faster"));
        assert!(!found.not_found);
        assert!(file.issues["ABC-99"].not_found);
        assert_eq!(file.issues["ABC-99"].url, None);
        assert_eq!(file.issues["XYZ-3"].fetched_at, "2026-01-01T09:00:00Z");
    }

    #[test]
    fn cache_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = IssueTitlesFile::path_in(dir.path());
        assert_eq!(IssueTitlesFile::load(&path), IssueTitlesFile::default());
        let mut file = IssueTitlesFile::default();
        file.issues.insert(
            "ABC-12".to_string(),
            CachedIssue::found(info(), "2026-01-01T10:00:00Z"),
        );
        file.issues.insert(
            "ABC-99".to_string(),
            CachedIssue::not_found("2026-01-01T10:00:00Z"),
        );
        file.prefixes.insert("ABC".to_string(), "acme".to_string());
        file.save(&path).unwrap();
        assert_eq!(IssueTitlesFile::load(&path), file);
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(IssueTitlesFile::load(&path), IssueTitlesFile::default());
    }

    #[test]
    fn fold_shows_five_then_more_or_fewer() {
        assert_eq!(fold(0, false), (0, None));
        assert_eq!(fold(5, false), (5, None));
        assert_eq!(fold(5, true), (5, None));
        assert_eq!(fold(8, false), (5, Some("\u{25B8} 3 more".to_string())));
        assert_eq!(fold(8, true), (8, Some("\u{25BE} show fewer".to_string())));
    }

    #[test]
    fn shorten_cuts_long_titles_with_an_ellipsis() {
        assert_eq!(shorten("short", 60), "short");
        let long = "x".repeat(80);
        let cut = shorten(&long, 60);
        assert_eq!(cut.chars().count(), 60);
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn find_linear_looks_on_the_given_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = std::ffi::OsString::from(dir.path());
        let found_elsewhere = find_linear(Some(&missing));
        assert!(found_elsewhere.is_none_or(|path| !path.starts_with(dir.path())));
        let binary = dir.path().join("linear");
        std::fs::write(&binary, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(find_linear(Some(&missing)), Some(binary));
    }

    /// A stand-in `linear`: two workspaces; `ABC-1` lives in `globex`
    /// only, `ABC-2` nowhere. Each `issue view` call is logged.
    #[cfg(unix)]
    fn fake_linear(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let binary = dir.join("linear");
        let script = r#"#!/bin/sh
here=$(dirname "$0")
if [ "$1" = auth ] && [ "$2" = list ]; then
  printf '  WORKSPACE  ORG  USER\n* acme  Acme  Pat\n  globex  Globex  Pat\n'
  exit 0
fi
echo "$*" >> "$here/calls.log"
if [ "$3" = ABC-1 ] && [ "$8" = globex ]; then
  printf '{"identifier":"ABC-1","title":"Fix it","url":"https://linear.app/globex/issue/ABC-1/fix-it","state":{"name":"Todo","color":"grey"},"assignee":null}'
  exit 0
fi
printf '\n Issue not found: %s\n' "$3" >&2
exit 1
"#;
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        binary
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_round_tries_workspaces_in_order_and_learns_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let binary = fake_linear(dir.path());
        let outcome = fetch_round(RoundRequest {
            binary,
            keys: slugs(&["ABC-1", "ABC-2"]),
            prefixes: BTreeMap::new(),
        })
        .await;
        assert_eq!(outcome.results.len(), 2);
        match &outcome.results[0] {
            (key, KeyResult::Found { info, workspace }) => {
                assert_eq!(key, "ABC-1");
                assert_eq!(workspace, "globex");
                assert_eq!(info.title, "Fix it");
                assert_eq!(info.assignee, None);
            }
            other => panic!("ABC-1: {other:?}"),
        }
        assert_eq!(
            outcome.results[1],
            ("ABC-2".to_string(), KeyResult::NotFound)
        );
        let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        let tried: Vec<&str> = calls
            .lines()
            .filter_map(|line| line.rsplit(' ').next())
            .collect();
        // ABC-1: acme then globex; ABC-2: globex (learned) first, then acme.
        assert_eq!(tried, ["acme", "globex", "globex", "acme"]);
        assert!(calls
            .lines()
            .all(|line| line.contains("--json --no-comments --no-download --workspace")));
    }
}
