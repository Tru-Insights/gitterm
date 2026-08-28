//! Per-repository `gh` identity resolution.
//!
//! One machine can hold several logged-in GitHub accounts (`gh auth login`
//! more than once), and the account `gh` uses by default is whichever one is
//! globally *active* — a piece of ambient state that is routinely wrong for
//! one of the orgs. Task delivery must not depend on it: this module works
//! out, per repository, which logged-in account can actually push there, and
//! hands back a token to pass per invocation via `GH_TOKEN`. The gh git
//! credential helper honors `GH_TOKEN` too, so HTTPS pushes follow the same
//! identity. Global gh state is never touched.
//!
//! Resolution order:
//! 1. An explicit account (workspace env `GITTERM_GH_ACCOUNT`) — must be
//!    logged in, and is used as-is.
//! 2. Otherwise every logged-in account for the remote's host is probed with
//!    `gh api repos/<owner>/<name>`; the first with push access wins, falling
//!    back to the first that can at least see the repository. The active
//!    account is tried first so the common single-org case costs one probe.
//!
//! Results are cached per (repository, explicit account) for the process
//! lifetime; callers drop the entry on a gh failure so a revoked token or a
//! changed login is re-resolved on the next attempt.

use crate::agentd::git::git_command;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};

/// Workspace environment key that pins the gh account for task delivery.
pub const GH_ACCOUNT_ENV_KEY: &str = "GITTERM_GH_ACCOUNT";

const GH_CANDIDATES: [&str; 3] = ["gh", "/opt/homebrew/bin/gh", "/usr/local/bin/gh"];

/// A resolved gh login plus the token that authenticates as it. The token is
/// only ever applied to a child process environment — never displayed.
#[derive(Clone, PartialEq, Eq)]
pub struct GhIdentity {
    account: String,
    token: String,
}

impl GhIdentity {
    pub fn account(&self) -> &str {
        &self.account
    }

    /// Makes `command` run as this identity (`GH_TOKEN`, which both `gh`
    /// itself and `gh auth git-credential` honor).
    pub fn apply(&self, command: &mut Command) -> &Self {
        command.env("GH_TOKEN", &self.token);
        self
    }
}

impl fmt::Debug for GhIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GhIdentity")
            .field("account", &self.account)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// `host/owner/name` parsed from a git remote URL.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepositorySlug {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepositorySlug {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

impl fmt::Display for RepositorySlug {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}/{}", self.host, self.owner, self.name)
    }
}

/// Parses the GitHub-style remote URL forms git produces:
/// `git@host:owner/name.git`, `ssh://git@host/owner/name`,
/// `https://host/owner/name.git`.
pub fn parse_repository_slug(remote_url: &str) -> Option<RepositorySlug> {
    let remote_url = remote_url.trim();
    let (host, path) = if let Some(rest) = remote_url.split_once("://").map(|(_, rest)| rest) {
        let rest = rest.rsplit_once('@').map(|(_, rest)| rest).unwrap_or(rest);
        rest.split_once('/')?
    } else {
        let (host, path) = remote_url.split_once(':')?;
        (
            host.rsplit_once('@').map(|(_, host)| host).unwrap_or(host),
            path,
        )
    };
    let host = host.split_once(':').map(|(host, _)| host).unwrap_or(host);
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    if host.is_empty() || owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(RepositorySlug {
        host: host.to_string(),
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// Logins reported by `gh auth status` for `host`, active account first.
pub fn parse_auth_status_accounts(status_text: &str, host: &str) -> Vec<String> {
    let mut accounts = Vec::new();
    let mut active = None;
    let mut current_host: Option<&str> = None;
    let mut pending: Option<String> = None;
    for line in status_text.lines() {
        let trimmed = line.trim();
        if !line.starts_with(' ') && !trimmed.is_empty() {
            current_host = Some(trimmed);
            continue;
        }
        if current_host != Some(host) {
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("✓ Logged in to ")
            .or_else(|| trimmed.strip_prefix("Logged in to "))
        {
            if let Some((_, account)) = rest.split_once(" account ") {
                let account = account
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                if !account.is_empty() {
                    pending = Some(account.clone());
                    accounts.push(account);
                }
            }
        } else if trimmed.starts_with("- Active account: true") {
            active = pending.take();
        }
    }
    if let Some(active) = active {
        accounts.retain(|account| account != &active);
        accounts.insert(0, active);
    }
    accounts
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhIdentityError {
    detail: String,
}

impl GhIdentityError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for GhIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for GhIdentityError {}

type CacheKey = (RepositorySlug, Option<String>);

fn cache() -> &'static Mutex<HashMap<CacheKey, GhIdentity>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, GhIdentity>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A `gh` command, tolerating the minimal PATH of a Finder-launched app by
/// falling back to the standard Homebrew/Intel install locations. stdin is
/// null so gh can never stall on interactive input.
pub fn gh_command() -> Result<Command, GhIdentityError> {
    static RESOLVED: OnceLock<Option<&'static str>> = OnceLock::new();
    let binary = RESOLVED.get_or_init(|| {
        GH_CANDIDATES.into_iter().find(|candidate| {
            !matches!(
                Command::new(candidate)
                    .arg("--version")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status(),
                Err(error) if error.kind() == io::ErrorKind::NotFound
            )
        })
    });
    let Some(binary) = binary else {
        return Err(GhIdentityError::new(
            "gh CLI not found on PATH, /opt/homebrew/bin, or /usr/local/bin",
        ));
    };
    let mut command = Command::new(binary);
    command.stdin(Stdio::null());
    Ok(command)
}

/// The origin remote of the repository containing `path`, as a slug.
pub fn repository_slug_for(path: &Path) -> Result<RepositorySlug, GhIdentityError> {
    let output = git_command()
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            GhIdentityError::new(format!(
                "could not read the origin URL under {}: {error}",
                path.display()
            ))
        })?;
    if !output.status.success() {
        return Err(GhIdentityError::new(format!(
            "{} has no origin remote to resolve a gh account for",
            path.display()
        )));
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_repository_slug(&url).ok_or_else(|| {
        GhIdentityError::new(format!(
            "origin URL {url} is not a host/owner/name repository URL"
        ))
    })
}

/// Resolves the gh identity to use for the repository containing `path`.
/// `explicit` pins a login (from the workspace's `GITTERM_GH_ACCOUNT`);
/// otherwise logged-in accounts are probed for access.
pub fn resolve(path: &Path, explicit: Option<&str>) -> Result<GhIdentity, GhIdentityError> {
    let slug = repository_slug_for(path)?;
    let explicit = explicit
        .map(str::trim)
        .filter(|account| !account.is_empty())
        .map(str::to_string);
    let key = (slug.clone(), explicit.clone());
    if let Some(identity) = cache().lock().unwrap().get(&key) {
        return Ok(identity.clone());
    }
    let identity = resolve_uncached(&slug, explicit.as_deref())?;
    cache().lock().unwrap().insert(key, identity.clone());
    Ok(identity)
}

/// Drops any cached identity for the repository containing `path`, so the
/// next `resolve` probes again (after a gh failure that may mean a revoked
/// token or a changed login).
pub fn forget(path: &Path) {
    if let Ok(slug) = repository_slug_for(path) {
        cache()
            .lock()
            .unwrap()
            .retain(|(cached, _), _| cached != &slug);
    }
}

fn resolve_uncached(
    slug: &RepositorySlug,
    explicit: Option<&str>,
) -> Result<GhIdentity, GhIdentityError> {
    let accounts = logged_in_accounts(&slug.host)?;
    if let Some(account) = explicit {
        if !accounts.iter().any(|candidate| candidate == account) {
            return Err(GhIdentityError::new(format!(
                "{GH_ACCOUNT_ENV_KEY}={account} names a gh account that is not logged in on \
                 {} (logged in: {}) — run `gh auth login -h {} -u {account}`",
                slug.host,
                join_or_none(&accounts),
                slug.host
            )));
        }
        let token = account_token(&slug.host, account)?;
        return Ok(GhIdentity {
            account: account.to_string(),
            token,
        });
    }
    if accounts.is_empty() {
        return Err(GhIdentityError::new(format!(
            "no gh account is logged in on {} — run `gh auth login -h {}`",
            slug.host, slug.host
        )));
    }
    let mut readable = None;
    for account in &accounts {
        let token = account_token(&slug.host, account)?;
        let identity = GhIdentity {
            account: account.clone(),
            token,
        };
        match probe_access(slug, &identity) {
            RepositoryAccess::Push => return Ok(identity),
            RepositoryAccess::Read => readable.get_or_insert(identity),
            RepositoryAccess::None => continue,
        };
    }
    readable.ok_or_else(|| {
        GhIdentityError::new(format!(
            "none of the logged-in gh accounts ({}) can see {} on {} — log in with an account \
             that has access, or set {GH_ACCOUNT_ENV_KEY} in the workspace settings",
            join_or_none(&accounts),
            slug.full_name(),
            slug.host
        ))
    })
}

fn join_or_none(accounts: &[String]) -> String {
    if accounts.is_empty() {
        "none".to_string()
    } else {
        accounts.join(", ")
    }
}

fn logged_in_accounts(host: &str) -> Result<Vec<String>, GhIdentityError> {
    let mut command = gh_command()?;
    // GH_TOKEN in the app's own environment would make gh report only that
    // token; strip it so the login list is what `gh auth login` recorded.
    command.env_remove("GH_TOKEN").env_remove("GITHUB_TOKEN");
    let output = command
        .args(["auth", "status", "--hostname", host])
        .output()
        .map_err(|error| GhIdentityError::new(format!("could not run gh auth status: {error}")))?;
    // gh exits nonzero when no account is logged in but still prints the
    // per-host report, so parse whatever came back on either stream.
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(parse_auth_status_accounts(&text, host))
}

fn account_token(host: &str, account: &str) -> Result<String, GhIdentityError> {
    let mut command = gh_command()?;
    command.env_remove("GH_TOKEN").env_remove("GITHUB_TOKEN");
    let output = command
        .args(["auth", "token", "--hostname", host, "--user", account])
        .output()
        .map_err(|error| GhIdentityError::new(format!("could not run gh auth token: {error}")))?;
    if !output.status.success() {
        return Err(GhIdentityError::new(format!(
            "gh has no token for account {account} on {host}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(GhIdentityError::new(format!(
            "gh returned an empty token for account {account} on {host}"
        )));
    }
    Ok(token)
}

enum RepositoryAccess {
    Push,
    Read,
    None,
}

fn probe_access(slug: &RepositorySlug, identity: &GhIdentity) -> RepositoryAccess {
    let Ok(mut command) = gh_command() else {
        return RepositoryAccess::None;
    };
    identity.apply(&mut command);
    let output: Option<Output> = command
        .args([
            "api",
            "--hostname",
            &slug.host,
            &format!("repos/{}", slug.full_name()),
            "--jq",
            ".permissions.push",
        ])
        .output()
        .ok();
    match output {
        Some(output) if output.status.success() => {
            if String::from_utf8_lossy(&output.stdout).trim() == "true" {
                RepositoryAccess::Push
            } else {
                RepositoryAccess::Read
            }
        }
        _ => RepositoryAccess::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_scp_https_and_ssh_url_remotes() {
        for (url, expected) in [
            (
                "git@github.com:Cree8-org/cree8-portal.git",
                ("github.com", "Cree8-org", "cree8-portal"),
            ),
            (
                "https://github.com/Tru-Insights/gitterm.git",
                ("github.com", "Tru-Insights", "gitterm"),
            ),
            (
                "https://github.com/Tru-Insights/gitterm",
                ("github.com", "Tru-Insights", "gitterm"),
            ),
            (
                "ssh://git@ghe.example.com:2222/team/repo.git\n",
                ("ghe.example.com", "team", "repo"),
            ),
            (
                "https://user@github.com/owner/name.git",
                ("github.com", "owner", "name"),
            ),
        ] {
            let slug = parse_repository_slug(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!(
                (slug.host.as_str(), slug.owner.as_str(), slug.name.as_str()),
                expected,
                "{url}"
            );
        }
        assert_eq!(parse_repository_slug("/local/path/repo"), None);
        assert_eq!(parse_repository_slug("git@github.com:no-owner"), None);
        assert_eq!(parse_repository_slug("https://github.com/only-owner"), None);
    }

    const STATUS: &str = "github.com\n  ✓ Logged in to github.com account traceyt-cree8 (keyring)\n  - Active account: false\n  - Git operations protocol: https\n\n  ✓ Logged in to github.com account traceyt (keyring)\n  - Active account: true\n  - Token: gho_****\n\nghe.example.com\n  ✓ Logged in to ghe.example.com account other (keyring)\n  - Active account: true\n";

    #[test]
    fn auth_status_accounts_list_active_first_per_host() {
        assert_eq!(
            parse_auth_status_accounts(STATUS, "github.com"),
            vec!["traceyt".to_string(), "traceyt-cree8".to_string()]
        );
        assert_eq!(
            parse_auth_status_accounts(STATUS, "ghe.example.com"),
            vec!["other".to_string()]
        );
        assert!(parse_auth_status_accounts(STATUS, "gitlab.com").is_empty());
        assert!(parse_auth_status_accounts(
            "You are not logged into any GitHub hosts.",
            "github.com"
        )
        .is_empty());
    }

    #[test]
    fn identity_debug_never_prints_the_token() {
        let identity = GhIdentity {
            account: "someone".to_string(),
            token: "gho_secret".to_string(),
        };
        let rendered = format!("{identity:?}");
        assert!(rendered.contains("someone"));
        assert!(!rendered.contains("gho_secret"));
        let mut command = Command::new("true");
        identity.apply(&mut command);
        assert!(command
            .get_envs()
            .any(|(key, value)| key == "GH_TOKEN" && value == Some("gho_secret".as_ref())));
    }
}
