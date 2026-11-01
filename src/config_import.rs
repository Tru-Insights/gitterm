//! First-launch import of GitTerm V5 state into the V6 config dir (TRU-168).
//!
//! V6 runs beside V5, so it has its own config dir. When that dir does not
//! exist yet, the parts of the V5 dir that V6 can use are copied in. The V5
//! dir is only ever read: nothing is moved, linked or written there.
//!
//! The import is staged in a sibling dir and renamed into place at the end,
//! so a crash mid-import leaves no V6 dir and the next launch retries, and
//! once the V6 dir exists the import never runs again. A failure on one item
//! is logged and the rest still import.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config;

const LOG_PREFIX: &str = "V5 config import";

/// What happens to one top-level entry of the V5 config dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Copied as is (dirs recursively), permissions preserved.
    Copy,
    /// `workspaces.json`: tabs bound to V5 tasks, worktrees or live
    /// conversations are dropped or unbound; token refs rewritten.
    Workspaces,
    /// `remote-agents.json`: `file:` token refs into the V5 dir are
    /// rewritten to the copied tokens in the V6 dir.
    RemoteAgents,
    /// Deliberately not copied, with the reason logged.
    Skip(&'static str),
}

/// Every entry V5 keeps in its config dir, and what V6 does with it.
/// Entries not listed here are skipped as unknown.
const ITEMS: &[(&str, Plan)] = &[
    ("workspaces.json", Plan::Workspaces),
    ("remotes.json", Plan::Copy),
    ("remote-agents.json", Plan::RemoteAgents),
    ("profiles.json", Plan::Copy),
    ("tokens", Plan::Copy),
    // Whisper models. `fs::copy` clones on APFS (copy-on-write: instant,
    // no extra space, separate inode); elsewhere it is a real copy. A
    // hardlink would share the inode, so re-downloading the model in place
    // in one version would rewrite the other's.
    ("models", Plan::Copy),
    ("captures", Plan::Copy),
    (
        "tasks.json",
        Plan::Skip("task records point at V5-owned worktrees and sessions"),
    ),
    (
        "worktrees",
        Plan::Skip("task worktrees are registered with git under the V5 path"),
    ),
    (
        "browser-profile",
        Plan::Skip("the managed Chrome profile holds browser secrets and may be locked by V5"),
    ),
    (
        "delegations",
        Plan::Skip("run logs of V5 delegations, tied to V5 tasks"),
    ),
    (
        "task-briefs",
        Plan::Skip("audit records of V5 task sessions"),
    ),
    ("resource.log", Plan::Skip("V5 run log")),
    ("zsh", Plan::Skip("regenerated on every terminal launch")),
    ("acp", Plan::Skip("regenerated when an ACP chat starts")),
];

#[derive(Debug, Default)]
pub struct ImportReport {
    /// One line per V5 entry, in order.
    pub lines: Vec<String>,
    /// Entries deliberately not copied, with the reason.
    pub not_copied: Vec<String>,
    pub failures: usize,
}

#[derive(Debug)]
pub enum ImportOutcome {
    /// The V6 config dir already exists; nothing was read.
    AlreadyPresent,
    /// Nothing to import from, with the reason.
    Skipped(String),
    Imported(ImportReport),
}

/// Run the import if this is V6's first launch. Called before anything
/// else touches the config dir. Never fails the launch: problems are logged.
pub fn import_v5_config_on_first_launch() {
    let dest = config::global_config_dir();
    if dest.exists() {
        return;
    }
    let source = match v5_config_dir() {
        Ok(source) => source,
        Err(reason) => {
            eprintln!("{LOG_PREFIX}: skipped ({reason})");
            return;
        }
    };
    match import_if_absent(&source, &dest) {
        Ok(ImportOutcome::AlreadyPresent) => {}
        Ok(ImportOutcome::Skipped(reason)) => eprintln!("{LOG_PREFIX}: skipped ({reason})"),
        Ok(ImportOutcome::Imported(report)) => {
            for line in &report.lines {
                eprintln!("{LOG_PREFIX}: {line}");
            }
            eprintln!(
                "{LOG_PREFIX}: {} -> {} done, {} failure(s); not copied: {}",
                source.display(),
                dest.display(),
                report.failures,
                if report.not_copied.is_empty() {
                    "nothing".to_string()
                } else {
                    report.not_copied.join("; ")
                }
            );
        }
        Err(error) => eprintln!(
            "{LOG_PREFIX}: {} -> {} failed: {error}; V6 starts with a fresh config",
            source.display(),
            dest.display()
        ),
    }
}

/// The V5 config dir this V6 instance may read: the import's source, also
/// read (never written) when an imported chat tab picks up its V5
/// conversation (TRU-183).
pub(crate) fn v5_config_dir() -> Result<PathBuf, String> {
    import_source(
        config::config_dir_override().is_some(),
        config::resolve_dir_override(
            config::V5_CONFIG_DIR_ENV,
            std::env::var_os(config::V5_CONFIG_DIR_ENV),
        ),
        dirs::home_dir(),
    )
}

/// Where to import from. An isolated V6 instance (its config dir
/// overridden) only imports when the V5 dir is overridden too, so a test
/// instance never reads the real V5 state.
fn import_source(
    v6_overridden: bool,
    v5_override: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(dir) = v5_override {
        return Ok(dir);
    }
    if v6_overridden {
        return Err(format!(
            "{} is set and {} is not; an isolated instance does not read the real V5 config",
            config::CONFIG_DIR_ENV,
            config::V5_CONFIG_DIR_ENV
        ));
    }
    let home = home.ok_or_else(|| "no home directory".to_string())?;
    Ok(home.join(".config").join(config::V5_CONFIG_DIR_NAME))
}

/// Import `source` into `dest` unless `dest` exists. `source` is only read.
pub fn import_if_absent(source: &Path, dest: &Path) -> io::Result<ImportOutcome> {
    if dest.exists() {
        return Ok(ImportOutcome::AlreadyPresent);
    }
    if !source.is_dir() {
        return Ok(ImportOutcome::Skipped(format!(
            "no V5 config dir at {}",
            source.display()
        )));
    }
    if dest.starts_with(source) || source.starts_with(dest) {
        return Ok(ImportOutcome::Skipped(format!(
            "V5 dir {} and V6 dir {} overlap",
            source.display(),
            dest.display()
        )));
    }
    let parent = dest.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("V6 config dir {} has no parent", dest.display()),
        )
    })?;
    fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "gitterm".to_string());
    let staging = parent.join(format!(".{name}.import-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir(&staging)?;
    copy_permissions(source, &staging)?;

    let report = match import_entries(source, &staging, dest) {
        Ok(report) => report,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if let Err(error) = fs::rename(&staging, dest) {
        let _ = fs::remove_dir_all(&staging);
        if dest.exists() {
            // Another V6 instance finished its import first.
            return Ok(ImportOutcome::AlreadyPresent);
        }
        return Err(io::Error::new(
            error.kind(),
            format!(
                "could not move the staged import {} to {}: {error}",
                staging.display(),
                dest.display()
            ),
        ));
    }
    Ok(ImportOutcome::Imported(report))
}

/// Import every top-level entry of `source` into `staging`. `final_dest` is
/// where `staging` will live, for rewriting paths. Only the listing of
/// `source` itself is fatal; per-entry failures are counted and logged.
fn import_entries(source: &Path, staging: &Path, final_dest: &Path) -> io::Result<ImportReport> {
    let mut names: Vec<String> = fs::read_dir(source)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    let mut report = ImportReport::default();
    for name in names {
        let plan = ITEMS
            .iter()
            .find(|(item, _)| *item == name)
            .map(|(_, plan)| *plan)
            .unwrap_or(if name.starts_with("instance-") {
                Plan::Skip("per-process instance state, removed when that V5 process exits")
            } else {
                Plan::Skip("not a known V5 config entry")
            });
        let from = source.join(&name);
        let to = staging.join(&name);
        let result = match plan {
            Plan::Skip(reason) => {
                report.lines.push(format!("{name}: skipped ({reason})"));
                report.not_copied.push(format!("{name} ({reason})"));
                continue;
            }
            Plan::Copy => copy_entry(&from, &to).map(|()| "copied".to_string()),
            Plan::Workspaces => transform_json(&from, &to, |value| {
                sanitize_workspaces(value, source, final_dest)
            }),
            Plan::RemoteAgents => transform_json(&from, &to, |value| {
                let rewritten = rewrite_token_refs(value, source, final_dest);
                format!("{rewritten} token ref(s) moved to the V6 tokens dir")
            }),
        };
        match result {
            Ok(outcome) => report.lines.push(format!("{name}: {outcome}")),
            Err(error) => {
                report.failures += 1;
                report.lines.push(format!("{name}: failed ({error})"));
                let _ = if to.is_dir() {
                    fs::remove_dir_all(&to)
                } else {
                    fs::remove_file(&to)
                };
            }
        }
    }
    Ok(report)
}

/// Copy a file or a dir tree, preserving permission bits. Symlinks are
/// followed (their target's content is copied).
fn copy_entry(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::metadata(from)?;
    if metadata.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_entry(&entry.path(), &to.join(entry.file_name()))?;
        }
        copy_permissions(from, to)
    } else {
        // Copies the permission bits too.
        fs::copy(from, to).map(|_| ())
    }
}

fn copy_permissions(from: &Path, to: &Path) -> io::Result<()> {
    fs::set_permissions(to, fs::metadata(from)?.permissions())
}

/// Read JSON from `from`, change it, write it to `to` with `from`'s
/// permissions. Returns the outcome line.
fn transform_json(
    from: &Path,
    to: &Path,
    change: impl FnOnce(&mut Value) -> String,
) -> io::Result<String> {
    let contents = fs::read_to_string(from)?;
    let mut value: Value = serde_json::from_str(&contents).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not valid JSON: {error}", from.display()),
        )
    })?;
    let summary = change(&mut value);
    let json = serde_json::to_string_pretty(&value).map_err(io::Error::other)?;
    fs::write(to, json)?;
    copy_permissions(from, to)?;
    Ok(format!("copied, {summary}"))
}

/// Rewrite `file:` token refs that point into `source` so they point at the
/// same file under `dest`. Walks `remote_agents[].auth.token_ref`, which is
/// where both `remote-agents.json` and the legacy embedded list keep them.
fn rewrite_token_refs(value: &mut Value, source: &Path, dest: &Path) -> usize {
    let Some(agents) = value.get_mut("remote_agents").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut rewritten = 0;
    for agent in agents {
        let Some(token_ref) = agent.pointer_mut("/auth/token_ref") else {
            continue;
        };
        let Some(path) = token_ref.as_str().and_then(|raw| raw.strip_prefix("file:")) else {
            continue;
        };
        let path = expand_home(path.trim());
        if let Ok(rest) = path.strip_prefix(source) {
            *token_ref = Value::String(format!("file:{}", dest.join(rest).display()));
            rewritten += 1;
        }
    }
    rewritten
}

pub(crate) fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Fields of a restored tab that would make V6 act on V5's live state:
/// relaunching a startup command (often `claude --resume <id>`, which would
/// put two processes on one conversation) or reclaiming V5's session ids.
const TAB_FIELDS_DROPPED: &[&str] = &[
    "startup_command",
    "chat_session_id",
    "task_session_id",
    "session_uid",
];

/// Whether the import keeps V5 tab `tab`: not owned by a V5 task
/// (`task_id`) and not rooted inside the V5 dir `source`.
pub(crate) fn import_keeps_tab(tab: &Value, source: &Path) -> bool {
    let inside_source = |dir: Option<&str>| {
        dir.map(|dir| expand_home(dir).starts_with(source))
            .unwrap_or(false)
    };
    let task_bound = tab.get("task_id").is_some_and(|id| !id.is_null());
    !task_bound
        && !inside_source(tab.get("dir").and_then(Value::as_str))
        && !inside_source(tab.get("repo_dir").and_then(Value::as_str))
}

/// Make imported workspaces safe to restore beside a running V5:
/// - drop workspaces rooted inside the V5 config dir (task worktrees);
/// - drop tabs owned by a V5 task (`task_id`) or rooted inside the V5 dir;
/// - unbind the remaining tabs from V5 commands and conversations, and pi
///   chat tabs from their session file;
/// - rewrite embedded remote-agent token refs.
fn sanitize_workspaces(value: &mut Value, source: &Path, dest: &Path) -> String {
    let inside_source = |dir: Option<&str>| {
        dir.map(|dir| expand_home(dir).starts_with(source))
            .unwrap_or(false)
    };
    let mut dropped_workspaces = 0;
    let mut dropped_tabs = 0;
    let mut unbound_tabs = 0;
    if let Some(workspaces) = value.get_mut("workspaces").and_then(Value::as_array_mut) {
        let before = workspaces.len();
        workspaces.retain(|workspace| !inside_source(workspace.get("dir").and_then(Value::as_str)));
        dropped_workspaces = before - workspaces.len();

        for workspace in workspaces.iter_mut() {
            let root = workspace.get("dir").cloned();
            let Some(tabs) = workspace.get_mut("tabs").and_then(Value::as_array_mut) else {
                continue;
            };
            let before = tabs.len();
            tabs.retain(|tab| import_keeps_tab(tab, source));
            let removed = before - tabs.len();
            dropped_tabs += removed;
            if tabs.is_empty() {
                if let Some(root) = root {
                    tabs.push(serde_json::json!({ "dir": root }));
                }
            }
            for tab in tabs.iter_mut() {
                let Some(fields) = tab.as_object_mut() else {
                    continue;
                };
                let mut unbound = false;
                for field in TAB_FIELDS_DROPPED {
                    unbound |= fields.remove(*field).is_some();
                }
                if let Some(agent) = fields
                    .get_mut("agent_config")
                    .and_then(Value::as_object_mut)
                {
                    unbound |= agent.remove("session_path").is_some();
                }
                if unbound {
                    unbound_tabs += 1;
                }
            }
            if removed > 0 {
                if let Some(fields) = workspace.as_object_mut() {
                    fields.remove("active_tab");
                }
            }
        }
        if dropped_workspaces > 0 {
            if let Some(active) = value.get_mut("active_workspace") {
                *active = Value::from(0);
            }
        }
    }
    let rewritten = rewrite_token_refs(value, source, dest);
    format!(
        "dropped {dropped_workspaces} workspace(s) and {dropped_tabs} tab(s) bound to V5 tasks or \
         worktrees, unbound {unbound_tabs} tab(s) from V5 commands and conversations, moved \
         {rewritten} token ref(s)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// Every file under `root` with its bytes and permission bits, for
    /// proving the source was not touched.
    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>, u32)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>, u32)>) {
            let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let path = entry.path();
                let metadata = fs::metadata(&path).unwrap();
                #[cfg(unix)]
                let mode = metadata.permissions().mode();
                #[cfg(not(unix))]
                let mode = 0;
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                if metadata.is_dir() {
                    out.push((rel, Vec::new(), mode));
                    walk(root, &path, out);
                } else {
                    out.push((rel, fs::read(&path).unwrap(), mode));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A V5 dir with one of each kind of entry.
    fn v5_fixture(source: &Path) {
        fs::create_dir_all(source).unwrap();
        let token = source.join("tokens").join("build-host.token");
        fs::create_dir_all(token.parent().unwrap()).unwrap();
        fs::write(&token, "not-a-real-token\n").unwrap();
        #[cfg(unix)]
        {
            fs::set_permissions(source.join("tokens"), fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let agent = json!({
            "id": "build-host",
            "name": "Build host",
            "endpoint": "http://build-host.example:8797",
            "auth": {"type": "token", "token_ref": format!("file:{}", token.display())}
        });
        let env_agent = json!({
            "id": "other",
            "name": "Other",
            "endpoint": "http://other.example:8797",
            "auth": {"type": "token", "token_ref": "env:OTHER_TOKEN"}
        });
        fs::write(
            source.join("remote-agents.json"),
            json!({"remote_agents": [agent.clone(), env_agent]}).to_string(),
        )
        .unwrap();
        let worktree = source.join("worktrees").join("task-1");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            source.join("workspaces.json"),
            json!({
                "active_workspace": 1,
                "remote_sessions": [],
                "remote_agents": [agent],
                "workspaces": [
                    {
                        "name": "Repo", "abbrev": "RE", "dir": "/work/repo", "color": "Blue",
                        "active_tab": 2,
                        "env": {"EXAMPLE": "1"},
                        "tabs": [
                            {"dir": "/work/repo", "startup_command": "claude --resume abc",
                             "chat_session_id": "abc", "session_uid": "uid-1"},
                            {"dir": "/work/repo", "task_id": "task-1", "task_session_id": "s1"},
                            {"dir": worktree.display().to_string()},
                            {"dir": "/work/repo", "tab_kind": "agent",
                             "agent_config": {"backend": "pi", "model": "m", "session_path": "/p.jsonl"}}
                        ]
                    },
                    {
                        "name": "Task", "abbrev": "TA", "dir": worktree.display().to_string(),
                        "color": "Green", "tabs": [{"dir": worktree.display().to_string()}]
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();
        fs::write(source.join("remotes.json"), r#"{"remote_sessions":[]}"#).unwrap();
        fs::write(source.join("profiles.json"), "[]").unwrap();
        fs::write(source.join("tasks.json"), "{}").unwrap();
        fs::create_dir_all(source.join("models")).unwrap();
        fs::write(
            source.join("models").join("ggml-base.en.bin"),
            b"model-bytes",
        )
        .unwrap();
        fs::create_dir_all(source.join("browser-profile").join("Default")).unwrap();
        fs::create_dir_all(source.join("instance-1234")).unwrap();
        fs::write(source.join("instance-1234").join("config.json"), "{}").unwrap();
        fs::write(source.join("resource.log"), "log\n").unwrap();
        fs::create_dir_all(source.join("delegations")).unwrap();
        fs::write(source.join("mystery.bin"), "?").unwrap();
    }

    fn imported(outcome: ImportOutcome) -> ImportReport {
        match outcome {
            ImportOutcome::Imported(report) => report,
            other => panic!("expected an import, got {other:?}"),
        }
    }

    #[test]
    fn imports_what_v6_needs_and_skips_v5_owned_state() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("gitterm-v5");
        let dest = temp.path().join("gitterm-v6");
        v5_fixture(&source);

        let report = imported(import_if_absent(&source, &dest).unwrap());
        assert_eq!(report.failures, 0, "{:?}", report.lines);

        for copied in [
            "workspaces.json",
            "remotes.json",
            "remote-agents.json",
            "profiles.json",
            "tokens/build-host.token",
            "models/ggml-base.en.bin",
        ] {
            assert!(dest.join(copied).exists(), "{copied} missing");
        }
        for skipped in [
            "tasks.json",
            "worktrees",
            "browser-profile",
            "instance-1234",
            "resource.log",
            "delegations",
            "mystery.bin",
        ] {
            assert!(!dest.join(skipped).exists(), "{skipped} was copied");
            assert!(
                report
                    .not_copied
                    .iter()
                    .any(|line| line.starts_with(skipped)),
                "{skipped} not reported: {:?}",
                report.not_copied
            );
        }
        // One log line per V5 entry.
        assert_eq!(report.lines.len(), fs::read_dir(&source).unwrap().count());
        assert_eq!(
            fs::read(dest.join("models/ggml-base.en.bin")).unwrap(),
            b"model-bytes"
        );
        // No staging dir is left beside the result.
        let leftovers: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".import-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn token_refs_follow_the_copied_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("gitterm-v5");
        let dest = temp.path().join("gitterm-v6");
        v5_fixture(&source);
        imported(import_if_absent(&source, &dest).unwrap());

        let expected = format!("file:{}", dest.join("tokens/build-host.token").display());
        let agents: Value =
            serde_json::from_str(&fs::read_to_string(dest.join("remote-agents.json")).unwrap())
                .unwrap();
        assert_eq!(agents["remote_agents"][0]["auth"]["token_ref"], expected);
        assert_eq!(
            agents["remote_agents"][1]["auth"]["token_ref"],
            "env:OTHER_TOKEN"
        );

        let workspaces: Value =
            serde_json::from_str(&fs::read_to_string(dest.join("workspaces.json")).unwrap())
                .unwrap();
        assert_eq!(
            workspaces["remote_agents"][0]["auth"]["token_ref"],
            expected
        );
    }

    #[test]
    fn workspaces_drop_v5_tasks_and_unbind_conversations() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("gitterm-v5");
        let dest = temp.path().join("gitterm-v6");
        v5_fixture(&source);
        imported(import_if_absent(&source, &dest).unwrap());

        let workspaces: Value =
            serde_json::from_str(&fs::read_to_string(dest.join("workspaces.json")).unwrap())
                .unwrap();
        let list = workspaces["workspaces"].as_array().unwrap();
        assert_eq!(list.len(), 1, "the worktree-rooted workspace is dropped");
        assert_eq!(workspaces["active_workspace"], 0);
        let repo = &list[0];
        assert_eq!(repo["env"]["EXAMPLE"], "1");
        assert!(repo.get("active_tab").is_none());
        let tabs = repo["tabs"].as_array().unwrap();
        assert_eq!(tabs.len(), 2, "task and worktree tabs dropped: {tabs:?}");
        for tab in tabs {
            for field in TAB_FIELDS_DROPPED {
                assert!(tab.get(*field).is_none(), "{field} kept in {tab}");
            }
        }
        assert_eq!(tabs[1]["agent_config"]["backend"], "pi");
        assert!(tabs[1]["agent_config"].get("session_path").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn permissions_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("gitterm-v5");
        let dest = temp.path().join("gitterm-v6");
        v5_fixture(&source);
        fs::set_permissions(
            source.join("remote-agents.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
        imported(import_if_absent(&source, &dest).unwrap());

        assert_eq!(mode(&dest), 0o700);
        assert_eq!(mode(&dest.join("tokens")), 0o700);
        assert_eq!(mode(&dest.join("tokens/build-host.token")), 0o600);
        assert_eq!(mode(&dest.join("remote-agents.json")), 0o600);
    }

    #[test]
    fn source_is_never_touched_and_a_second_run_is_a_no_op() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("gitterm-v5");
        let dest = temp.path().join("gitterm-v6");
        v5_fixture(&source);
        let before = snapshot(&source);

        imported(import_if_absent(&source, &dest).unwrap());
        assert_eq!(snapshot(&source), before);

        // V6 writes its own state; a second run must not overwrite it.
        fs::write(dest.join("profiles.json"), "[\"v6\"]").unwrap();
        let after_first = snapshot(&dest);
        assert!(matches!(
            import_if_absent(&source, &dest).unwrap(),
            ImportOutcome::AlreadyPresent
        ));
        assert_eq!(snapshot(&dest), after_first);
        assert_eq!(snapshot(&source), before);
    }

    #[test]
    fn a_failed_item_is_logged_and_the_rest_still_import() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("gitterm-v5");
        let dest = temp.path().join("gitterm-v6");
        v5_fixture(&source);
        fs::write(source.join("workspaces.json"), "{ not json").unwrap();

        let report = imported(import_if_absent(&source, &dest).unwrap());
        assert_eq!(report.failures, 1);
        assert!(
            report
                .lines
                .iter()
                .any(|line| line.starts_with("workspaces.json: failed (")),
            "{:?}",
            report.lines
        );
        assert!(!dest.join("workspaces.json").exists());
        assert!(dest.join("remote-agents.json").exists());
        assert!(dest.join("tokens/build-host.token").exists());
    }

    #[test]
    fn missing_or_overlapping_source_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("gitterm-v6");
        assert!(matches!(
            import_if_absent(&temp.path().join("absent"), &dest).unwrap(),
            ImportOutcome::Skipped(_)
        ));
        assert!(!dest.exists());

        let source = temp.path().join("gitterm-v5");
        v5_fixture(&source);
        assert!(matches!(
            import_if_absent(&source, &source.join("nested-v6")).unwrap(),
            ImportOutcome::Skipped(_)
        ));
        assert!(!source.join("nested-v6").exists());
    }

    #[test]
    fn import_source_respects_the_config_dir_overrides() {
        let home = Some(PathBuf::from("/test-home"));
        assert_eq!(
            import_source(false, None, home.clone()).unwrap(),
            PathBuf::from("/test-home/.config/gitterm-v5")
        );
        // An isolated V6 instance never falls back to the real V5 dir.
        assert!(import_source(true, None, home.clone()).is_err());
        assert_eq!(
            import_source(true, Some(PathBuf::from("/fixtures/v5")), home.clone()).unwrap(),
            PathBuf::from("/fixtures/v5")
        );
        assert_eq!(
            import_source(false, Some(PathBuf::from("/fixtures/v5")), home).unwrap(),
            PathBuf::from("/fixtures/v5")
        );
        assert!(import_source(false, None, None).is_err());
    }
}
