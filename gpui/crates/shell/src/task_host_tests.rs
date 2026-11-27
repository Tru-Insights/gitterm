//! H5b B4: the GPUI task host on temp preview roots, with a sentinel Iced
//! root that is protected, read-only and snapshotted.
use crate::task_host::{HostConfig, HostLock, Ingress, Reseed, TaskHostOwner, owns_in_flight_work};
use gitterm_core::{
    preview::{Preview, ProtectedRoots},
    task_commands::{TaskLaunchOrigin, runtime::command_channel},
    task_host::QueuedTaskLaunch,
    task_mcp::{
        LaunchTaskSessionRequest, ListTasksRequest, TaskControlOperation, TaskControlSender,
    },
    tasks::{Delegation, TaskLifecycle, TaskRecord, TaskStoreDocument},
};
use gpui::{AppContext as _, Entity, TestAppContext};
use std::{
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

const LOCK_CHILD_ENV: &str = "GITTERM_B4_LOCK_CHILD_ROOT";

struct Fixture {
    _dir: tempfile::TempDir,
    roots: Arc<Preview>,
    /// Stands in for Iced's config dir; protected from the preview.
    iced: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let iced = base.join("iced");
        std::fs::create_dir(&iced).unwrap();
        let protected = ProtectedRoots::new(&base.join("fixture-home"), [iced.clone()]).unwrap();
        let roots = Arc::new(Preview::validate(&base.join("preview"), protected).unwrap());
        Self {
            _dir: dir,
            roots,
            iced,
        }
    }
    fn source(&self) -> PathBuf {
        self.iced.join("tasks.json")
    }
    fn copy(&self) -> PathBuf {
        self.roots.root().join("tasks.json")
    }
    fn write_iced(&self, tasks: Vec<TaskRecord>, delegations: Vec<Delegation>) {
        let document = TaskStoreDocument {
            tasks,
            delegations,
            ..Default::default()
        };
        let path = self.source();
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        bump_mtime(&path);
    }
    fn config(&self) -> (HostConfig, TaskControlSender) {
        let (sender, receiver) = command_channel();
        (
            HostConfig {
                roots: self.roots.clone(),
                iced_config: self.iced.clone(),
                ingress: Ingress::Channel(sender.clone(), receiver),
            },
            sender,
        )
    }
    fn start(&self, cx: &mut TestAppContext) -> (Entity<TaskHostOwner>, TaskControlSender) {
        let (config, sender) = self.config();
        let owner = cx.update(|cx| TaskHostOwner::start(config, cx)).unwrap();
        (owner, sender)
    }
}

/// Moves the mtime well past the last one so a re-seed always sees it.
fn bump_mtime(path: &Path) {
    let next = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::now())
        + Duration::from_secs(5);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(next)
        .unwrap();
}

fn task(id: &str, lifecycle: TaskLifecycle) -> TaskRecord {
    use gitterm_core::tasks::{AttemptState, ExecutorTarget, TaskExecutionAttempt};
    let mut task = crate::tasks::fixture_task(id, &format!("Task {id}"), &format!("task/{id}"));
    task.lifecycle = lifecycle;
    // The store requires an active attempt for live lifecycles.
    let state = match lifecycle {
        TaskLifecycle::Running => Some(AttemptState::Running),
        TaskLifecycle::Queued => Some(AttemptState::Queued),
        _ => None,
    };
    if let Some(state) = state {
        task.attempts.push(TaskExecutionAttempt {
            attempt_id: format!("attempt-{id}"),
            executor: ExecutorTarget::Local,
            harness: None,
            state,
            started_at: task.created_at.clone(),
            ended_at: None,
            session_ref: None,
            failure: None,
        });
        task.active_attempt_id = Some(format!("attempt-{id}"));
    }
    task
}

fn delegation(id: &str, status: &str) -> Delegation {
    serde_json::from_value(serde_json::json!({
        "delegation_id": id, "kind": "implement",
        "parent": {"session_uid": "parent", "chat_session_id": "parent-history", "workspace": "A", "cwd": "/fixture/work"},
        "child": {"runner": "task_session", "task_id": "running", "task_session_id": format!("child-{id}"), "preset_name": "Codex", "model": "fixture-model"},
        "brief": "Fixture", "status": {"state": status},
        "created_at": "2026-11-20T12:00:00Z", "updated_at": "2026-11-20T12:01:00Z"
    }))
    .unwrap()
}

fn task_ids(owner: &Entity<TaskHostOwner>, cx: &mut TestAppContext) -> Vec<String> {
    owner.read_with(cx, |owner, _| {
        owner
            .host()
            .store()
            .as_ref()
            .unwrap()
            .tasks()
            .iter()
            .map(|task| task.task_id.clone())
            .collect()
    })
}

/// Runs the GPUI executor until `receiver` yields, so the owner's pump can
/// answer work that another thread is waiting on.
fn pump_until<T>(cx: &mut TestAppContext, receiver: &std::sync::mpsc::Receiver<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if let Ok(value) = receiver.try_recv() {
            return value;
        }
        assert!(Instant::now() < deadline, "host did not answer");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Sends one command through the host's ingress and returns its reply.
fn dispatch(
    cx: &mut TestAppContext,
    sender: &TaskControlSender,
    operation: TaskControlOperation,
) -> Result<serde_json::Value, String> {
    dispatch_as(cx, sender, operation, Some("caller"))
}

/// [`dispatch`] from the tab `caller`.
fn dispatch_as(
    cx: &mut TestAppContext,
    sender: &TaskControlSender,
    operation: TaskControlOperation,
    caller: Option<&str>,
) -> Result<serde_json::Value, String> {
    let pending = sender
        .enqueue(operation, caller.map(str::to_string))
        .unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        send.send(runtime.block_on(pending)).unwrap();
    });
    pump_until(cx, &receive)
}

/// A minimal streamable-HTTP MCP client over a plain socket.
struct McpClient {
    address: String,
    /// `/mcp`, with the caller query when the endpoint carries one.
    path: String,
    token: String,
    session: Option<String>,
}
impl McpClient {
    fn connect(endpoint: &str, token: &str) -> Self {
        let (address, path) = endpoint
            .strip_prefix("http://")
            .and_then(|rest| rest.split_once('/'))
            .unwrap();
        assert!(path == "mcp" || path.starts_with("mcp?"), "{endpoint}");
        let mut client = Self {
            address: address.into(),
            path: format!("/{path}"),
            token: token.into(),
            session: None,
        };
        let (headers, _) = client.post(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "b4-test", "version": "0"}}
        }));
        client.session = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("mcp-session-id")
                .then(|| value.trim().to_string())
        });
        assert!(client.session.is_some(), "no MCP session: {headers}");
        client.post(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        client
    }
    fn call(&self, tool: &str, arguments: serde_json::Value) -> serde_json::Value {
        let (_, body) = self.post(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}
        }));
        // The reply is either plain JSON or one SSE event carrying it.
        let json = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .rfind(|data| !data.is_empty())
            .unwrap_or(&body);
        let response: serde_json::Value =
            serde_json::from_str(json).unwrap_or_else(|error| panic!("{error}: {body:?}"));
        response["result"].clone()
    }
    fn post(&self, body: serde_json::Value) -> (String, String) {
        let body = body.to_string();
        let mut stream = std::net::TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let session = self
            .session
            .as_ref()
            .map(|id| format!("Mcp-Session-Id: {id}\r\nMcp-Protocol-Version: 2025-03-26\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "POST {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.path,
            self.address,
            self.token,
            body.len()
        )
        .unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let raw = String::from_utf8(raw).unwrap();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap();
        assert!(
            headers.starts_with("HTTP/1.1 2"),
            "MCP request failed: {headers}\n{body}"
        );
        let chunked = headers
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked");
        let body = if chunked { dechunk(body) } else { body.into() };
        (headers.into(), body)
    }
}
fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap();
        if size == 0 {
            break;
        }
        out.push_str(&rest[..size]);
        body = &rest[size + 2..];
    }
    out
}

/// H5b B7: the host's browser server, reached as a chat reaches it (its
/// descriptor's caller URL and token), serves reads and refuses launches in
/// its dispatch; its profile is the preview root's.
#[gpui::test]
fn host_browser_serves_reads_and_refuses_launch(cx: &mut TestAppContext) {
    use gitterm_core::{
        preview::service::preview_notice,
        task_commands::mcp_protocol::{BROWSER_MCP_TOKEN_ENV, BROWSER_MCP_URL_ENV},
    };
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![], vec![]);
    let (owner, _) = fixture.start(cx);
    let servers = owner.read_with(cx, |owner, _| owner.claude_mcp_servers("chat-b7"));
    assert_eq!(servers.len(), 2);
    let env = |key: &str| {
        servers[1]
            .env
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .unwrap()
    };
    let client = McpClient::connect(&env(BROWSER_MCP_URL_ENV), &env(BROWSER_MCP_TOKEN_ENV));
    let status = client.call("browser_status", serde_json::json!({}));
    assert_ne!(status["isError"], true, "{status}");
    assert_eq!(status["structuredContent"]["state"], "stopped", "{status}");
    let profile = PathBuf::from(
        status["structuredContent"]["profile_dir"]
            .as_str()
            .unwrap_or_else(|| panic!("no profile_dir: {status}")),
    );
    assert_eq!(profile, fixture.roots.root().join("browser-profile"));
    for tool in [
        "browser_open",
        "browser_navigate",
        "browser_network_capture",
    ] {
        let result = client.call(tool, serde_json::json!({}));
        assert_eq!(result["isError"], true, "{tool}: {result}");
        assert_eq!(
            result["content"][0]["text"],
            preview_notice(tool).as_str(),
            "{tool}"
        );
    }
    assert!(!profile.exists(), "nothing created the profile");
    owner.update(cx, |owner, _| owner.shutdown());
}

#[gpui::test]
fn gpui_host_lists_seeded_tasks(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(
        vec![
            task("first", TaskLifecycle::Ready),
            task("second", TaskLifecycle::Draft),
        ],
        vec![],
    );
    let (owner, _) = fixture.start(cx);
    let (endpoint, token, browser, store_path, state_root) = owner.read_with(cx, |owner, _| {
        let host = owner.host();
        (
            owner.task_endpoint().unwrap(),
            host.services().task_mcp().token().to_string(),
            owner.browser_endpoint().unwrap(),
            host.store().as_ref().unwrap().path().to_path_buf(),
            host.services().state_root().to_path_buf(),
        )
    });
    // The host's store and state root are the preview tree.
    assert_eq!(store_path, fixture.copy());
    assert_eq!(state_root, fixture.roots.root());
    for endpoint in [&endpoint, &browser] {
        assert!(endpoint.starts_with("http://127.0.0.1:"), "{endpoint}");
    }
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let client = McpClient::connect(&endpoint, &token);
        send.send(client.call("task_list", serde_json::json!({})))
            .unwrap();
    });
    let result = pump_until(cx, &receive);
    assert_ne!(result["isError"], true, "{result}");
    let ids: Vec<_> = result["structuredContent"]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["task_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, ["first", "second"]);
    // The log server is up on its own OS-assigned port.
    let deadline = Instant::now() + Duration::from_secs(5);
    while owner.read_with(cx, |owner, _| owner.log_port()).is_none() {
        assert!(Instant::now() < deadline, "log server did not bind");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[gpui::test]
fn gpui_host_shutdown_resolves_pending_replies(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let app = cx.new_app();
    let (config, sender) = fixture.config();
    let owner = app.update(|cx| TaskHostOwner::start(config, cx)).unwrap();
    let servers = owner.read_with(&app, |owner, _| owner.server_tasks());
    // Enqueued, never pumped: the reply is still pending at quit.
    let pending = sender
        .enqueue(
            TaskControlOperation::List(ListTasksRequest {
                include_archived: false,
            }),
            None,
        )
        .unwrap();
    app.quit();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let result = runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(5), pending).await })
        .expect("pending reply was not resolved by shutdown");
    assert!(result.is_err(), "{result:?}");
    assert!(
        sender
            .enqueue(TaskControlOperation::PlanLimits, None)
            .is_err()
    );
    // Servers are gone and the lifetime lock is free for the next host.
    // A released ephemeral port can already belong to another parallel test,
    // and a concurrent fork can retain a listener fd until exec. Check the
    // actual server tasks, rather than whether that address accepts TCP.
    assert!(servers.iter().all(tokio::task::AbortHandle::is_finished));
    HostLock::acquire(fixture.roots.root()).unwrap();
}

#[gpui::test]
fn shell_lifetime_holds_host_lock_beyond_quit_and_reaper(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let lifetime = crate::lifetime::Lifetime::acquire(&fixture.roots).unwrap();
    let app = cx.new_app();
    app.update(|cx| cx.set_global(lifetime.barrier.clone()));
    let (config, _) = fixture.config();
    let owner = app
        .update(|cx| TaskHostOwner::start_locked(config, lifetime.lock.clone(), cx))
        .unwrap();
    let servers = owner.read_with(&app, |owner, _| owner.server_tasks());
    app.quit();
    assert!(HostLock::acquire(fixture.roots.root()).is_err());
    lifetime.finish().unwrap();
    assert!(servers.iter().all(tokio::task::AbortHandle::is_finished));
    HostLock::acquire(fixture.roots.root()).unwrap();
}

#[gpui::test]
fn preview_runs_beside_desktop_lifetime_lock(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![task("desktop", TaskLifecycle::Ready)], vec![]);
    let desktop = HostLock::acquire(&fixture.iced).unwrap();
    let before = std::fs::read(fixture.source()).unwrap();
    let (owner, _) = fixture.start(cx);
    assert_eq!(task_ids(&owner, cx), ["desktop"]);
    assert_eq!(std::fs::read(fixture.source()).unwrap(), before);
    assert!(HostLock::acquire(desktop.root()).is_err());
}

#[gpui::test]
fn second_process_host_on_same_root_refused(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![task("first", TaskLifecycle::Ready)], vec![]);
    let (_owner, _) = fixture.start(cx);
    let seeded = std::fs::read(fixture.copy()).unwrap();
    // A second lock handle is a separate open file: flock refuses it, as it
    // would another process.
    let error = HostLock::acquire(fixture.roots.root())
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("already in use"), "{error}");
    // A second host fails before seeding or binding anything.
    fixture.write_iced(vec![task("changed", TaskLifecycle::Ready)], vec![]);
    let (config, _) = fixture.config();
    let error = cx
        .update(|cx| TaskHostOwner::start(config, cx))
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("already in use"), "{error}");
    assert_eq!(std::fs::read(fixture.copy()).unwrap(), seeded);
}

/// The child half of `host_lock_released_after_crash`; a no-op otherwise.
#[test]
fn host_lock_child_holds_until_killed() {
    let Some(root) = std::env::var_os(LOCK_CHILD_ENV) else {
        return;
    };
    let root = PathBuf::from(root);
    let protected = ProtectedRoots::new(&root.parent().unwrap().join("fixture-home"), []).unwrap();
    let roots = Preview::validate(&root, protected).unwrap();
    let _lock = HostLock::acquire(roots.root()).unwrap();
    println!("B4-HOST-LOCK-HELD");
    std::io::stdout().flush().unwrap();
    std::thread::sleep(Duration::from_secs(60));
    panic!("the parent test should have killed this child");
}

#[test]
fn host_lock_released_after_crash() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.roots.root()).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "task_host_tests::host_lock_child_holds_until_killed",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(LOCK_CHILD_ENV, fixture.roots.root())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(stdout.read_line(&mut line).unwrap(), 0, "child exited");
        if line.contains("B4-HOST-LOCK-HELD") {
            break;
        }
    }
    assert!(HostLock::acquire(fixture.roots.root()).is_err());
    // SIGKILL: no destructor runs, only the kernel releases the lock.
    child.kill().unwrap();
    child.wait().unwrap();
    HostLock::acquire(fixture.roots.root()).unwrap();
}

#[cfg(unix)]
fn recorded(dir: &Path) -> Vec<(PathBuf, Vec<u8>, SystemTime, u32)> {
    use std::os::unix::fs::PermissionsExt;
    let mut entries = vec![];
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        entries.push((
            path.clone(),
            std::fs::read(&path).unwrap(),
            meta.modified().unwrap(),
            meta.permissions().mode(),
        ));
    }
    entries.sort();
    entries
}

#[cfg(unix)]
#[gpui::test]
fn gpui_host_never_opens_v6_root(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![task("first", TaskLifecycle::Ready)], vec![]);
    std::fs::write(fixture.iced.join("workspaces.json"), b"{}").unwrap();
    // A write recorder: any write attempt fails, and every byte, mtime and
    // mode is compared afterwards.
    for name in ["tasks.json", "workspaces.json"] {
        std::fs::set_permissions(
            fixture.iced.join(name),
            std::fs::Permissions::from_mode(0o444),
        )
        .unwrap();
    }
    std::fs::set_permissions(&fixture.iced, std::fs::Permissions::from_mode(0o555)).unwrap();
    let dir_before = std::fs::metadata(&fixture.iced)
        .unwrap()
        .modified()
        .unwrap();
    let before = recorded(&fixture.iced);
    let app = cx.new_app();
    let (config, sender) = fixture.config();
    let owner = app.update(|cx| TaskHostOwner::start(config, cx)).unwrap();
    assert!(fixture.roots.scoped(&fixture.source()).is_err());
    let listed = dispatch(
        cx,
        &sender,
        TaskControlOperation::List(ListTasksRequest {
            include_archived: false,
        }),
    );
    assert_eq!(listed.unwrap()["tasks"][0]["task_id"], "first");
    assert_eq!(
        app.update(|cx| owner.update(cx, |owner, cx| owner.refresh_seed(cx))),
        Reseed::Unchanged
    );
    app.quit();
    assert_eq!(recorded(&fixture.iced), before);
    assert_eq!(
        std::fs::metadata(&fixture.iced)
            .unwrap()
            .modified()
            .unwrap(),
        dir_before
    );
    std::fs::set_permissions(&fixture.iced, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn assert_inert(owner: &Entity<TaskHostOwner>, cx: &mut TestAppContext, ids: &[&str]) {
    owner.read_with(cx, |owner, _| {
        let host = owner.host();
        assert!(host.launch_queue().is_empty());
        assert!(!owns_in_flight_work(host));
        for id in ids {
            assert!(host.delegations().pending_start(id).is_none());
            assert!(!host.delegations().admitted(id));
            assert!(host.delegations().activity(id).is_none());
        }
        let store = host.store().as_ref().unwrap();
        assert_eq!(
            store.get("running").unwrap().lifecycle,
            TaskLifecycle::Running
        );
        assert_eq!(
            store.get("queued").unwrap().lifecycle,
            TaskLifecycle::Queued
        );
    });
}

#[gpui::test]
fn imported_running_and_queued_records_inert_after_seed_and_reseed(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let tasks = || {
        vec![
            task("running", TaskLifecycle::Running),
            task("queued", TaskLifecycle::Queued),
        ]
    };
    let delegations = || {
        vec![
            delegation("worker-running", "running"),
            delegation("worker-requested", "requested"),
        ]
    };
    fixture.write_iced(tasks(), delegations());
    let (owner, sender) = fixture.start(cx);
    cx.run_until_parked();
    let ids = ["worker-running", "worker-requested"];
    assert_inert(&owner, cx, &ids);
    // The copy is exactly what Iced wrote: nothing recovered or rewritten.
    assert_eq!(
        std::fs::read(fixture.copy()).unwrap(),
        std::fs::read(fixture.source()).unwrap()
    );
    // A launch request for the queued task is refused and queues nothing.
    let launched = dispatch(
        cx,
        &sender,
        TaskControlOperation::LaunchSession(LaunchTaskSessionRequest {
            task_id: "queued".into(),
            preset_name: None,
            user_model_choice: false,
        }),
    );
    assert!(launched.unwrap_err().contains("not available in preview"));
    assert_inert(&owner, cx, &ids);

    let mut next = tasks();
    next.push(task("added", TaskLifecycle::Ready));
    fixture.write_iced(next, delegations());
    assert_eq!(
        owner.update(cx, |owner, cx| owner.refresh_seed(cx)),
        Reseed::Reseeded
    );
    cx.run_until_parked();
    assert_eq!(task_ids(&owner, cx), ["running", "queued", "added"]);
    assert_inert(&owner, cx, &ids);
    assert_eq!(
        std::fs::read(fixture.copy()).unwrap(),
        std::fs::read(fixture.source()).unwrap()
    );
}

#[gpui::test]
fn reseed_refused_while_work_in_flight(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![task("first", TaskLifecycle::Ready)], vec![]);
    let (owner, _) = fixture.start(cx);
    let seeded = std::fs::read(fixture.copy()).unwrap();
    owner.update(cx, |owner, _| {
        owner.host_mut().launch_queue_mut().push(QueuedTaskLaunch {
            task_id: "first".into(),
            preset_idx: None,
            origin: TaskLaunchOrigin::Agent,
            user_model_choice: false,
        });
    });
    fixture.write_iced(vec![task("second", TaskLifecycle::Ready)], vec![]);
    for _ in 0..2 {
        assert_eq!(
            owner.update(cx, |owner, cx| owner.refresh_seed(cx)),
            Reseed::Deferred
        );
    }
    assert_eq!(task_ids(&owner, cx), ["first"]);
    assert_eq!(std::fs::read(fixture.copy()).unwrap(), seeded);
    // Once the work is gone, the deferred change is picked up.
    owner.update(cx, |owner, _| owner.host_mut().launch_queue_mut().clear());
    assert_eq!(
        owner.update(cx, |owner, cx| owner.refresh_seed(cx)),
        Reseed::Reseeded
    );
    assert_eq!(task_ids(&owner, cx), ["second"]);
}

#[gpui::test]
fn failed_reseed_keeps_last_snapshot_and_raises_a_notice(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![task("first", TaskLifecycle::Ready)], vec![]);
    let (owner, _) = fixture.start(cx);
    let seeded = std::fs::read(fixture.copy()).unwrap();
    std::fs::write(fixture.source(), b"{not json").unwrap();
    bump_mtime(&fixture.source());
    assert_eq!(
        owner.update(cx, |owner, cx| owner.refresh_seed(cx)),
        Reseed::Failed
    );
    assert_eq!(task_ids(&owner, cx), ["first"]);
    assert_eq!(std::fs::read(fixture.copy()).unwrap(), seeded);
    assert!(!fixture.roots.root().join(".tasks.seed.json").exists());
    let notice = owner
        .read_with(cx, |owner, _| owner.notice.clone())
        .unwrap();
    assert!(notice.contains("keeping the last snapshot"), "{notice}");
    // The same bad file is not retried every poll; a fix clears the notice.
    assert_eq!(
        owner.update(cx, |owner, cx| owner.refresh_seed(cx)),
        Reseed::Unchanged
    );
    fixture.write_iced(vec![task("fixed", TaskLifecycle::Ready)], vec![]);
    assert_eq!(
        owner.update(cx, |owner, cx| owner.refresh_seed(cx)),
        Reseed::Reseeded
    );
    assert!(owner.read_with(cx, |owner, _| owner.notice.is_none()));
}

// ---------------------------------------------------------------------------
// H5b B5: the executor's read arms and notice arms.

/// A workspace owner with GPUI chat panes, published the way the shell
/// publishes it, so the host's arms find the chats.
struct Chats {
    owner: Entity<crate::WorkspaceOwner>,
}
impl Chats {
    fn new(fixture: &Fixture, cx: &mut TestAppContext) -> Self {
        cx.update(gpui_component::init);
        let owner = cx.new(|cx| {
            crate::WorkspaceOwner::new(
                crate::workspace_status::WorkspaceStatus::default(),
                None,
                cx,
            )
        });
        cx.update(|cx| {
            cx.set_global(crate::work_sidebar::IcedWorkSource {
                config: fixture.iced.clone(),
                workspaces: vec![],
                repos: vec![],
                owner: owner.downgrade(),
            })
        });
        Self { owner }
    }
    /// Opens a chat pane whose provider conversation is `conversation`;
    /// returns its task MCP caller id.
    fn open(
        &self,
        fixture: &Fixture,
        conversation: Option<&str>,
        cx: &mut TestAppContext,
    ) -> String {
        let cwd = fixture.iced.parent().unwrap().join("work");
        self.open_in(fixture, conversation, &cwd, cx)
    }
    /// [`Self::open`] with the pane working in `cwd`.
    fn open_in(
        &self,
        fixture: &Fixture,
        conversation: Option<&str>,
        cwd: &Path,
        cx: &mut TestAppContext,
    ) -> String {
        let saved = crate::chat::SavedChat {
            uid: "saved".into(),
            cwd: cwd.to_path_buf(),
            provider: fixture.roots.provider(),
            session_id: conversation.map(str::to_string),
            records: vec![],
        };
        let (pane, cx) = cx.add_window_view(|window, cx| {
            crate::chat::ChatPane::history(crate::chat::RuntimeShutdown::new(), saved, window, cx)
        });
        let uid = pane.read_with(cx, |pane, _| pane.target().session_uid);
        self.owner.update(cx, |owner, _| {
            owner.chats.insert(uid.clone(), pane.clone());
        });
        uid
    }
}

fn req<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).unwrap()
}

/// Every write, launch and run: through the gate, and straight into its
/// executor arm. Each answers the notice, and nothing is written or started.
#[cfg(unix)]
#[gpui::test]
fn every_write_tool_answers_preview_notice(cx: &mut TestAppContext) {
    use gitterm_core::{
        preview::service::preview_notice,
        task_commands::SessionLaunch,
        task_host::HostEffect,
        task_mcp::{TaskControlOperation as Op, TaskSessionEventRequest},
    };
    use serde_json::json;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(
        vec![
            task("running", TaskLifecycle::Running),
            task("queued", TaskLifecycle::Queued),
        ],
        vec![delegation("d1", "running")],
    );
    let (owner, sender) = fixture.start(cx);
    let iced_before = recorded(&fixture.iced);
    let preview_before = recorded(fixture.roots.root());
    let operations = || {
        vec![
            (
                Op::Create(req(
                    json!({"title": "t", "objective": "o", "repository_path": "/fixture/repo"}),
                )),
                "task_create",
            ),
            (
                Op::UpdateHandoff(req(json!({"task_id": "running", "summary": "s"}))),
                "task_update_handoff",
            ),
            (
                Op::SessionEvent(TaskSessionEventRequest {
                    task_id: "running".into(),
                    session_id: "child-d1".into(),
                    event_type: "agent-turn-complete".into(),
                }),
                "notify",
            ),
            (
                Op::MergePlanSet(req(json!({"entries": []}))),
                "merge_plan_set",
            ),
            (
                Op::LaunchSession(req(json!({"task_id": "queued"}))),
                "task_launch_session",
            ),
            (
                Op::RequestReview(req(json!({"target": "uncommitted"}))),
                "review_request",
            ),
            (
                Op::RequestConsult(req(json!({"brief": "b"}))),
                "consult_request",
            ),
            (
                Op::DelegateTask(req(json!({"title": "t", "objective": "o"}))),
                "delegate_task",
            ),
        ]
    };
    for (operation, label) in operations() {
        assert_eq!(
            dispatch(cx, &sender, operation),
            Err(preview_notice(label)),
            "{label}"
        );
    }
    // The arms themselves, for the effects the gate never lets through.
    for (operation, label) in operations() {
        let (commands, mut receiver) = command_channel();
        let pending = commands.enqueue(operation, Some("caller".into())).unwrap();
        let envelope = receiver.try_recv().unwrap();
        let reply = envelope.reply;
        let caller = envelope.caller;
        let effect = match envelope.operation {
            Op::Create(request) => HostEffect::Create {
                reply,
                caller,
                request,
            },
            Op::UpdateHandoff(request) => HostEffect::UpdateHandoff {
                reply,
                caller,
                request,
            },
            Op::SessionEvent(request) => HostEffect::TurnEnded {
                reply,
                task_id: request.task_id,
                session_id: request.session_id,
            },
            Op::MergePlanSet(request) => HostEffect::MergePlanSet {
                reply,
                caller,
                request,
            },
            Op::LaunchSession(request) => HostEffect::LaunchSession {
                reply,
                launch: SessionLaunch {
                    task_id: request.task_id,
                    preset_name: request.preset_name,
                    origin: TaskLaunchOrigin::Agent,
                    user_model_choice: request.user_model_choice,
                },
            },
            Op::RequestReview(request) => HostEffect::RequestReview {
                reply,
                caller,
                request,
            },
            Op::RequestConsult(request) => HostEffect::RequestConsult {
                reply,
                caller,
                request,
            },
            Op::DelegateTask(request) => HostEffect::DelegateTask {
                reply,
                caller,
                request,
            },
            other => panic!("not a write: {other:?}"),
        };
        owner.update(cx, |owner, cx| owner.execute_effect(effect, label, cx));
        let answer = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(pending);
        assert_eq!(answer, Err(preview_notice(label)), "{label}");
    }
    cx.run_until_parked();
    // Nothing written in either tree, nothing queued, admitted or started.
    assert_eq!(recorded(&fixture.iced), iced_before);
    assert_eq!(recorded(fixture.roots.root()), preview_before);
    assert_inert(&owner, cx, &["d1"]);
}

#[gpui::test]
fn reads_work_in_preview(cx: &mut TestAppContext) {
    use gitterm_core::task_mcp::TaskControlOperation as Op;
    use serde_json::json;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(
        vec![
            task("running", TaskLifecycle::Running),
            task("ready", TaskLifecycle::Ready),
        ],
        vec![delegation("d1", "running")],
    );
    std::fs::write(
        fixture.iced.join("settings.json"),
        serde_json::to_vec(&json!({
            "agent_presets": [{"name": "Fixture Agent", "command": "fixture-agent"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let chats = Chats::new(&fixture, cx);
    let caller = chats.open(&fixture, Some("root-chat"), cx);
    let (owner, sender) = fixture.start(cx);
    let stored = |id: &str, cx: &mut TestAppContext| {
        owner.read_with(cx, |owner, _| {
            owner
                .host()
                .store()
                .as_ref()
                .unwrap()
                .get(id)
                .unwrap()
                .clone()
        })
    };

    // task_get: the record with Iced's open_sessions (no GPUI pane is a
    // task session).
    let got = dispatch(cx, &sender, Op::Get(req(json!({"task_id": "ready"})))).unwrap();
    let expected = gitterm_core::task_host::task_control_value(&stored("ready", cx), &[]).unwrap();
    assert_eq!(got, expected);
    assert_eq!(got["open_sessions"], json!([]));

    // task_list: every task in that shape, and Iced's preset names.
    let listed = dispatch(cx, &sender, Op::List(req(json!({})))).unwrap();
    assert_eq!(
        listed,
        json!({
            "tasks": [
                gitterm_core::task_host::task_control_value(&stored("running", cx), &[]).unwrap(),
                gitterm_core::task_host::task_control_value(&stored("ready", cx), &[]).unwrap(),
            ],
            "available_presets": ["Fixture Agent"],
        })
    );

    // plan_limits: Iced's policy report on Iced's (here empty) file.
    let limits = dispatch(cx, &sender, Op::PlanLimits).unwrap();
    let file = gitterm_core::plan_limits::PlanLimitsFile::load(
        &gitterm_core::plan_limits::PlanLimitsFile::path_in(&fixture.iced),
    );
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert_eq!(
        limits,
        serde_json::to_value(gitterm_core::plan_limits::evaluate_policy(&file, now).report)
            .unwrap()
    );

    // delegation_get: the stored record, with no live worker card.
    let record = dispatch(
        cx,
        &sender,
        Op::GetDelegation(req(json!({"delegation_id": "d1"}))),
    )
    .unwrap();
    let expected = owner.read_with(cx, |owner, _| {
        let host = owner.host();
        let delegation = host.store().as_ref().unwrap().delegation("d1").unwrap();
        gitterm_core::delegations::delegation_record(delegation, host.services().state_root())
            .unwrap()
    });
    assert_eq!(record, expected);
    assert!(record.get("worker").is_none(), "{record}");
    // delegation_list is answered by the host itself.
    let listed = dispatch(cx, &sender, Op::ListDelegations(req(json!({})))).unwrap();
    assert!(listed.is_object(), "{listed}");

    // merge_plan_get: Iced's shape for a root with no plan or PRs.
    assert_eq!(
        dispatch_as(cx, &sender, Op::MergePlanGet, Some(&caller)).unwrap(),
        json!({
            "root_chat_id": "root-chat",
            "updated_at": null,
            "source": null,
            "next": null,
            "entries": [],
            "unplanned_prs": [],
        })
    );
    // Iced's errors: no caller, a caller that is no open chat, and a chat
    // with no conversation yet.
    assert_eq!(
        dispatch_as(cx, &sender, Op::MergePlanGet, None),
        Err(gitterm_core::task_mcp::missing_plan_caller_error(
            "merge_plan_get"
        ))
    );
    assert_eq!(
        dispatch_as(cx, &sender, Op::MergePlanGet, Some("closed")),
        Err("merge_plan_get: no GitTerm tab on this Mac has session closed; merge plans are for this Mac's chats".into())
    );
    let fresh = chats.open(&fixture, None, cx);
    assert_eq!(
        dispatch_as(cx, &sender, Op::MergePlanGet, Some(&fresh)),
        Err("merge_plan_get: this tab has no conversation id yet; call it again after your first reply".into())
    );
}

/// `merge_plan_get` resolves its caller on the UI thread, then reads the
/// plan, watch and facts files on the background executor, which answers:
/// when the arm returns, the reply is not there yet; once the background
/// work runs it is Iced's value for the same files.
#[gpui::test]
fn reads_do_not_touch_disk_on_the_ui_thread(cx: &mut TestAppContext) {
    use gitterm_core::{task_host::HostEffect, task_mcp::TaskControlOperation as Op};
    use std::task::Poll;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![], vec![]);
    let chats = Chats::new(&fixture, cx);
    let caller = chats.open(&fixture, Some("root-chat"), cx);
    let (owner, _sender) = fixture.start(cx);
    let (commands, mut receiver) = command_channel();
    let mut pending = commands
        .enqueue(Op::MergePlanGet, Some(caller.clone()))
        .unwrap();
    let envelope = receiver.try_recv().unwrap();
    let effect = HostEffect::MergePlanGet {
        reply: envelope.reply,
        caller: envelope.caller,
    };
    let mut poll = || {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        std::pin::Pin::new(&mut pending).poll(&mut context)
    };
    owner.update(cx, |owner, cx| {
        owner.execute_effect(effect, "merge_plan_get", cx)
    });
    assert!(poll().is_pending(), "the arm answered on the UI thread");
    cx.run_until_parked();
    let Poll::Ready(reply) = poll() else {
        panic!("the background read did not answer");
    };
    assert_eq!(
        reply.unwrap(),
        crate::task_host::merge_plan_for(&fixture.iced, &[], "root-chat")
    );
}

/// `plan_limits` and the task list's preset names are read on the
/// background executor too: the arm answers nothing until it runs, and
/// the answer is the synchronous functions' on the same files.
#[gpui::test]
fn plan_limits_and_preset_reads_leave_the_ui_thread(cx: &mut TestAppContext) {
    use gitterm_core::task_mcp::TaskControlOperation as Op;
    use std::task::Poll;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![], vec![]);
    let (owner, _sender) = fixture.start(cx);
    let presets = crate::task_host::preset_names(&fixture.iced);
    assert!(!presets.is_empty(), "the fixture has Iced presets");
    let expected = [
        (
            "plan_limits",
            Op::PlanLimits,
            crate::task_host::plan_limits(&fixture.iced).unwrap(),
        ),
        (
            "task_list",
            Op::List(req(serde_json::json!({}))),
            serde_json::json!({"tasks": [], "available_presets": presets}),
        ),
    ];
    let (commands, mut receiver) = command_channel();
    for (label, operation, expected) in expected {
        let mut pending = commands.enqueue(operation, None).unwrap();
        let envelope = receiver.try_recv().unwrap();
        let effect = owner
            .update(cx, |owner, _| {
                owner.host().dispatch_with_caller(envelope, None, None)
            })
            .expect("a read effect");
        let mut poll = || {
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            std::pin::Pin::new(&mut pending).poll(&mut context)
        };
        owner.update(cx, |owner, cx| owner.execute_effect(effect, label, cx));
        assert!(poll().is_pending(), "{label} answered on the UI thread");
        let mut answer = None;
        wait_until(cx, |_| match poll() {
            Poll::Ready(reply) => {
                answer = Some(reply);
                true
            }
            Poll::Pending => false,
        });
        assert_eq!(answer.unwrap().unwrap(), expected, "{label}");
    }
}

#[gpui::test]
fn html_render_requires_open_gpui_chat(cx: &mut TestAppContext) {
    use gitterm_core::task_mcp::TaskControlOperation as Op;
    use serde_json::json;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let chats = Chats::new(&fixture, cx);
    let caller = chats.open(&fixture, Some("root-chat"), cx);
    let (_owner, sender) = fixture.start(cx);
    let render = |html: &str| Op::HtmlRender(req(json!({"html": html, "height": 300})));
    let refused = "html_render requires an open local native Claude or ACP chat tab (Pi does not restore conversation history)";
    if !gitterm_core::content::html_render::NATIVE_HTML_SUPPORTED {
        let error = dispatch_as(cx, &sender, render("<p>x</p>"), Some(&caller)).unwrap_err();
        assert!(error.contains("requires GitTerm for macOS"), "{error}");
        return;
    }
    assert_eq!(
        dispatch_as(cx, &sender, render("<p>x</p>"), Some(&caller)).unwrap(),
        json!({"html_render": {"html": "<p>x</p>", "height": 300}})
    );
    for other in [None, Some("closed-chat")] {
        assert_eq!(
            dispatch_as(cx, &sender, render("<p>x</p>"), other),
            Err(refused.into()),
            "{other:?}"
        );
    }
    // Validation comes first, as in Iced.
    let invalid = dispatch_as(cx, &sender, render(" "), Some("closed-chat")).unwrap_err();
    assert_ne!(invalid, refused);
}

/// Two root chats in one workspace: each caller gets its own root's plan
/// (a worker's conversation resolves to its coordinator's root through the
/// store), in Iced's full response shape.
#[gpui::test]
fn merge_plan_scoped_to_root_chat(cx: &mut TestAppContext) {
    use gitterm_core::{
        chat_facts,
        chats::ChatBackend,
        merge_plan::{self, MergePlan},
        prs::{WatchFile, WatchedPr},
        task_mcp::TaskControlOperation as Op,
    };
    use serde_json::json;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    // root-a delegated a worker whose conversation is worker-a.
    let worker: Delegation = serde_json::from_value(json!({
        "delegation_id": "wa", "kind": "implement",
        "parent": {"session_uid": "uid-a", "chat_session_id": "root-a", "workspace": "A", "cwd": "/fixture/work"},
        "child": {"runner": "task_session", "task_id": "running", "task_session_id": "child-wa",
                  "conversation": {"backend": "claude", "session_id": "worker-a"}},
        "brief": "Fixture", "status": {"state": "running"},
        "created_at": "2026-11-20T12:00:00Z", "updated_at": "2026-11-20T12:01:00Z"
    }))
    .unwrap();
    fixture.write_iced(vec![task("running", TaskLifecycle::Running)], vec![worker]);
    let plans = merge_plan::plans_dir(&fixture.iced);
    let plan = |root: &str, entries: serde_json::Value| -> MergePlan {
        req(json!({"root_chat_id": root, "updated_at": "2026-11-20T10:00:00Z", "entries": entries}))
    };
    merge_plan::write_plan(
        &plans,
        &plan(
            "root-a",
            json!([
                {"repo": "acme/api", "number": 1},
                {"repo": "acme/api", "number": 2, "after": ["acme/api#1"], "note": "after one"},
            ]),
        ),
    )
    .unwrap();
    merge_plan::write_plan(
        &plans,
        &plan(
            "root-b",
            json!([{"repo": "acme/web", "number": 5, "hold": "waiting"}]),
        ),
    )
    .unwrap();
    // Watch: #1 polled open, #2 polled with an error only (no live state:
    // its row is unknown, not blocked).
    let mut watch = WatchFile::default();
    let mut one = WatchedPr::new(merge_plan::PrKey::new("acme/api", 1));
    one.snapshot.core = Some(req(json!({
        "title": "One", "state": "open", "url": "https://github.com/acme/api/pull/1",
        "author": "dev", "head_repo": "acme/api", "head_ref": "feat-one", "head_sha": "abc",
        "base_ref": "main", "mergeable": true, "mergeable_state": "clean", "requested_reviewers": 0
    })));
    one.checked_at = Some("2026-11-20T11:00:00Z".into());
    let mut two = WatchedPr::new(merge_plan::PrKey::new("acme/api", 2));
    two.checked_at = Some("2026-11-20T11:00:00Z".into());
    two.error = Some("rate limited".into());
    watch.prs.insert(one.key.id(), one);
    watch.prs.insert(two.key.id(), two);
    std::fs::write(
        WatchFile::path_in(&fixture.iced),
        serde_json::to_vec(&watch).unwrap(),
    )
    .unwrap();
    // Saved chat facts: root-a names a planned and an unplanned PR, its
    // worker another; root-b one of its own.
    let facts_dir = chat_facts::facts_dir(&fixture.iced);
    std::fs::create_dir_all(&facts_dir).unwrap();
    let facts = |chat: &str, numbers: &[(&str, u64)]| {
        let prs: Vec<_> = numbers
            .iter()
            .map(|(repo, n)| json!({"repo": repo, "number": n, "url": format!("https://github.com/{repo}/pull/{n}")}))
            .collect();
        std::fs::write(
            chat_facts::facts_path(&facts_dir, ChatBackend::Claude, chat).unwrap(),
            serde_json::to_vec(&json!({"prs": prs})).unwrap(),
        )
        .unwrap();
    };
    facts("root-a", &[("acme/api", 1), ("acme/api", 3)]);
    facts("worker-a", &[("acme/api", 4)]);
    facts("root-b", &[("acme/web", 6)]);

    let chats = Chats::new(&fixture, cx);
    let root_a = chats.open(&fixture, Some("root-a"), cx);
    let worker_a = chats.open(&fixture, Some("worker-a"), cx);
    let root_b = chats.open(&fixture, Some("root-b"), cx);
    let (_owner, sender) = fixture.start(cx);

    let live_one = json!({
        "title": "One", "state": "open", "head": "feat-one", "base": "main",
        "checks": "no checks", "failing_checks": [], "required_checks_passed": false,
        "review": "no review", "conflicting": false,
        "checked_at": "2026-11-20T11:00:00Z", "error": null,
    });
    let expected_a = json!({
        "root_chat_id": "root-a",
        "updated_at": "2026-11-20T10:00:00Z",
        "source": null,
        "next": "acme/api#1",
        "entries": [
            {
                "position": 1, "unordered_reason": null, "pr": "acme/api#1", "repo": "acme/api",
                "number": 1, "url": "https://github.com/acme/api/pull/1", "step": null, "after": [],
                "stacked_on": null, "hold": null, "method": "merge", "note": null,
                "status": "next", "waits_for": [], "live": live_one,
            },
            {
                "position": 2, "unordered_reason": null, "pr": "acme/api#2", "repo": "acme/api",
                "number": 2, "url": "https://github.com/acme/api/pull/2", "step": null,
                "after": ["acme/api#1"], "stacked_on": null, "hold": null, "method": "merge",
                "note": "after one", "status": "unknown", "waits_for": [],
                "live": {"checked_at": "2026-11-20T11:00:00Z", "error": "rate limited"},
            },
        ],
        "unplanned_prs": [
            {"pr": "acme/api#3", "repo": "acme/api", "number": 3, "url": "https://github.com/acme/api/pull/3", "live": null},
            {"pr": "acme/api#4", "repo": "acme/api", "number": 4, "url": "https://github.com/acme/api/pull/4", "live": null},
        ],
    });
    let expected_b = json!({
        "root_chat_id": "root-b",
        "updated_at": "2026-11-20T10:00:00Z",
        "source": null,
        "next": null,
        "entries": [{
            "position": 1, "unordered_reason": null, "pr": "acme/web#5", "repo": "acme/web",
            "number": 5, "url": "https://github.com/acme/web/pull/5", "step": null, "after": [],
            "stacked_on": null, "hold": "waiting", "method": "merge", "note": null,
            "status": "unknown", "waits_for": [], "live": null,
        }],
        "unplanned_prs": [
            {"pr": "acme/web#6", "repo": "acme/web", "number": 6, "url": "https://github.com/acme/web/pull/6", "live": null},
        ],
    });
    let get = |caller: &str, cx: &mut TestAppContext| {
        dispatch_as(cx, &sender, Op::MergePlanGet, Some(caller)).unwrap()
    };
    assert_eq!(get(&root_a, cx), expected_a);
    assert_eq!(get(&worker_a, cx), expected_a);
    assert_eq!(get(&root_b, cx), expected_b);
}

/// A worker whose parent names it only in a saved facts file (no store
/// link) resolves to that parent's root, as Iced's index ancestry does, and
/// a PR both chats saved is listed once, at its first occurrence.
#[test]
fn merge_plan_root_through_facts_only_link_and_prs_once() {
    use gitterm_core::{chat_facts, chats::ChatBackend};
    use serde_json::json;
    let iced = tempfile::tempdir().unwrap();
    let facts_dir = chat_facts::facts_dir(iced.path());
    std::fs::create_dir_all(&facts_dir).unwrap();
    let pr = |n: u64| json!({"repo": "acme/api", "number": n, "url": format!("https://github.com/acme/api/pull/{n}")});
    let write = |backend, chat: &str, facts: serde_json::Value| {
        std::fs::write(
            chat_facts::facts_path(&facts_dir, backend, chat).unwrap(),
            serde_json::to_vec(&facts).unwrap(),
        )
        .unwrap();
    };
    write(
        ChatBackend::Claude,
        "coord",
        json!({"children": [{"session_id": "worker", "kind": "worker"}], "prs": [pr(7)]}),
    );
    write(ChatBackend::Codex, "worker", json!({"prs": [pr(7), pr(8)]}));
    let unplanned = |n: u64| {
        json!({"pr": format!("acme/api#{n}"), "repo": "acme/api", "number": n,
               "url": format!("https://github.com/acme/api/pull/{n}"), "live": null})
    };
    for caller in ["worker", "coord"] {
        let value = crate::task_host::merge_plan_for(iced.path(), &[], caller);
        assert_eq!(value["root_chat_id"], json!("coord"), "{caller}: {value}");
        assert_eq!(
            value["unplanned_prs"],
            json!([unplanned(7), unplanned(8)]),
            "{caller}: {value}"
        );
    }
}

// ---------------------------------------------------------------------------
// H5b B9: fixture-mode store writes.

/// A fixture root under the fixture's temp base (`fx/`, marked) holding a
/// Git repository `fx/repo` with one commit on `main`, a plain directory
/// `fx/wt` standing in for a task worktree, and an unmarked repository
/// `outside-repo` beside it.
struct GitFixture {
    root: PathBuf,
    repo: PathBuf,
    worktree: PathBuf,
    outside: PathBuf,
}
impl GitFixture {
    fn new(fixture: &Fixture) -> Self {
        let base = fixture.iced.parent().unwrap();
        let root = base.join("fx");
        let repo = root.join("repo");
        let worktree = root.join("wt");
        let outside = base.join("outside-repo");
        for dir in [&repo, &worktree, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(
            root.join(gitterm_core::preview::service::FIXTURE_MARKER),
            b"",
        )
        .unwrap();
        for repo in [&repo, &outside] {
            git(repo, &["init", "-q", "-b", "main"]);
            std::fs::write(repo.join("README"), "fixture\n").unwrap();
            git(repo, &["add", "README"]);
            git(repo, &["commit", "-q", "-m", "fixture"]);
        }
        Self {
            root,
            repo,
            worktree,
            outside,
        }
    }
    fn start(
        &self,
        fixture: &Fixture,
        cx: &mut TestAppContext,
    ) -> (Entity<TaskHostOwner>, TaskControlSender) {
        let (config, sender) = fixture.config();
        let fixtures = vec![self.root.clone()];
        let owner = cx
            .update(|cx| TaskHostOwner::start_fixture(config, fixtures, cx))
            .unwrap();
        (owner, sender)
    }
}

/// Git with no user or system config, so the fixture repo is the same on
/// every machine.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Every file and directory under `dir`, with contents, mtimes and modes.
#[cfg(unix)]
fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>, SystemTime, u32)> {
    use std::os::unix::fs::PermissionsExt;
    let mut entries = vec![];
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let bytes = if meta.is_file() {
                std::fs::read(&path).unwrap()
            } else {
                if meta.is_dir() {
                    pending.push(path.clone());
                }
                vec![]
            };
            entries.push((
                path,
                bytes,
                meta.modified().unwrap(),
                meta.permissions().mode(),
            ));
        }
    }
    entries.sort();
    entries
}

/// A task whose worktree is `worktree`.
fn task_at(id: &str, lifecycle: TaskLifecycle, worktree: &Path) -> TaskRecord {
    use gitterm_core::tasks::{TaskWorktree, TaskWorktreeState};
    let mut task = task(id, lifecycle);
    task.worktree = TaskWorktree {
        state: TaskWorktreeState::Ready,
        path: Some(worktree.to_path_buf()),
    };
    task
}

/// An open worker delegation whose child is task `task_id`'s session `session`.
fn worker_delegation(id: &str, task_id: &str, session: &str) -> Delegation {
    serde_json::from_value(serde_json::json!({
        "delegation_id": id, "kind": "implement",
        "parent": {"session_uid": "parent", "chat_session_id": "parent-history", "workspace": "A", "cwd": "/fixture/work"},
        "child": {"runner": "task_session", "task_id": task_id, "task_session_id": session, "preset_name": "Codex", "model": "fixture-model"},
        "brief": "Fixture", "status": {"state": "running"},
        "created_at": "2026-11-20T12:00:00Z", "updated_at": "2026-11-20T12:01:00Z"
    }))
    .unwrap()
}

fn stored_task(owner: &Entity<TaskHostOwner>, id: &str, cx: &mut TestAppContext) -> TaskRecord {
    owner.read_with(cx, |owner, _| {
        owner
            .host()
            .store()
            .as_ref()
            .unwrap()
            .get(id)
            .unwrap()
            .clone()
    })
}

fn create_in(repo: &Path) -> TaskControlOperation {
    TaskControlOperation::Create(req(serde_json::json!({
        "title": "Fixture task", "objective": "Prepare a fixture worktree",
        "repository_path": repo.display().to_string(), "issue_key": "tru-1"
    })))
}

#[cfg(unix)]
#[gpui::test]
fn fixture_create_prepares_worktree_in_fixture_repo(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let git_fixture = GitFixture::new(&fixture);
    let (owner, sender) = git_fixture.start(&fixture, cx);
    let iced_before = tree(&fixture.iced);

    let created = dispatch(cx, &sender, create_in(&git_fixture.repo)).unwrap();
    let task_id = created["task_id"].as_str().unwrap().to_string();
    let path = PathBuf::from(created["worktree"]["path"].as_str().unwrap());
    assert_eq!(created["worktree"]["state"], "ready", "{created}");
    assert_eq!(created["lifecycle"], "ready", "{created}");
    assert_eq!(created["issue"]["key"], "TRU-1", "{created}");
    assert_eq!(created["created_by"]["session_id"], "caller", "{created}");
    // Under the fixture's worktree root, not the repo and not any config dir.
    assert!(
        path.starts_with(git_fixture.root.join("worktrees")),
        "{}",
        path.display()
    );
    let branch = created["branch"].as_str().unwrap();
    assert_eq!(
        git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        branch
    );
    assert!(git(&git_fixture.repo, &["worktree", "list"]).contains(path.to_str().unwrap()));
    // Saved in the preview's own store, on disk at the preview root.
    assert_eq!(stored_task(&owner, &task_id, cx).worktree.path, Some(path));
    let saved = gitterm_core::tasks::TaskStore::load(fixture.copy()).unwrap();
    assert!(saved.get(&task_id).is_some());
    // Iced's tree is untouched, and the preview's writes are not re-seeded away.
    assert_eq!(tree(&fixture.iced), iced_before);
    fixture.write_iced(vec![], vec![]);
    assert_eq!(
        owner.update(cx, |owner, cx| owner.refresh_seed(cx)),
        Reseed::Deferred
    );
    assert_eq!(task_ids(&owner, cx), [task_id]);
}

#[cfg(unix)]
#[gpui::test]
fn handoff_done_only_from_hosted_session(cx: &mut TestAppContext) {
    use gitterm_core::{task_mcp::TaskControlOperation as Op, tasks::DelegationStatus};
    use serde_json::json;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let git_fixture = GitFixture::new(&fixture);
    fixture.write_iced(
        vec![task_at(
            "worker",
            TaskLifecycle::Running,
            &git_fixture.worktree,
        )],
        vec![worker_delegation("d1", "worker", "child-d1")],
    );
    let chats = Chats::new(&fixture, cx);
    let hosted = chats.open_in(&fixture, Some("worker-chat"), &git_fixture.worktree, cx);
    let other = chats.open_in(&fixture, Some("other-chat"), &git_fixture.worktree, cx);
    chats.owner.update(cx, |owner, cx| {
        owner.chats[&hosted].update(cx, |pane, _| pane.bind_task_session("worker", "child-d1"));
    });
    let (owner, sender) = git_fixture.start(&fixture, cx);
    let iced_before = tree(&fixture.iced);
    let done = |summary: &str| {
        Op::UpdateHandoff(req(
            json!({"task_id": "worker", "summary": summary, "status": "done"}),
        ))
    };

    // Another pane, or no pane, cannot report done for the worker.
    for caller in [Some(other.as_str()), Some("closed"), None] {
        let refused = dispatch_as(cx, &sender, done("not mine"), caller).unwrap_err();
        assert!(
            refused.contains("only the worker's own GitTerm tab"),
            "{refused}"
        );
    }
    let delegation_status = |cx: &mut TestAppContext| {
        owner.read_with(cx, |owner, _| {
            let store = owner.host().store().as_ref().unwrap();
            store.delegation("d1").unwrap().status.clone()
        })
    };
    assert!(matches!(delegation_status(cx), DelegationStatus::Running));
    assert!(stored_task(&owner, "worker", cx).handoff.is_none());
    // Progress from anyone is only a note on the task.
    dispatch_as(
        cx,
        &sender,
        Op::UpdateHandoff(req(json!({"task_id": "worker", "summary": "note"}))),
        Some(&other),
    )
    .unwrap();
    assert!(matches!(delegation_status(cx), DelegationStatus::Running));

    // The pane hosting the worker's session completes the delegation.
    let value = dispatch_as(cx, &sender, done("finished"), Some(&hosted)).unwrap();
    assert_eq!(value["handoff_status"], "done", "{value}");
    assert_eq!(value["delegation_id"], "d1", "{value}");
    assert_eq!(value["handoff"]["summary"], "finished", "{value}");
    // The task records no session child-d1, so the caller is named, as Iced.
    assert_eq!(
        value["handoff"]["updated_by_session_id"],
        hosted.as_str(),
        "{value}"
    );
    assert!(matches!(delegation_status(cx), DelegationStatus::Completed));
    assert_eq!(
        stored_task(&owner, "worker", cx).lifecycle,
        TaskLifecycle::Completed
    );
    // Saved to the preview's copy only.
    let saved = gitterm_core::tasks::TaskStore::load(fixture.copy()).unwrap();
    assert_eq!(
        saved
            .get("worker")
            .unwrap()
            .handoff
            .as_ref()
            .unwrap()
            .summary,
        "finished"
    );
    assert_eq!(tree(&fixture.iced), iced_before);
}

/// Each write whose target is outside every fixture: a task worktree, a
/// repository, and a caller pane directory under the temp dir but unmarked.
#[cfg(unix)]
#[gpui::test]
fn non_fixture_target_denied(cx: &mut TestAppContext) {
    use gitterm_core::task_mcp::{TaskControlOperation as Op, TaskSessionEventRequest};
    use serde_json::json;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let git_fixture = GitFixture::new(&fixture);
    fixture.write_iced(
        vec![task_at(
            "outside",
            TaskLifecycle::Running,
            &git_fixture.outside,
        )],
        vec![],
    );
    let chats = Chats::new(&fixture, cx);
    let caller = chats.open_in(&fixture, Some("root-chat"), &git_fixture.outside, cx);
    chats.owner.update(cx, |owner, cx| {
        owner.chats[&caller].update(cx, |pane, _| pane.bind_task_session("outside", "s1"));
    });
    let (owner, sender) = git_fixture.start(&fixture, cx);
    let iced_before = tree(&fixture.iced);
    let preview_before = tree(fixture.roots.root());
    let outside_before = tree(&git_fixture.outside);
    let base = fixture.iced.parent().unwrap().to_path_buf();
    for (operation, label) in [
        (create_in(&git_fixture.outside), "task_create"),
        (
            Op::UpdateHandoff(req(json!({"task_id": "outside", "summary": "s"}))),
            "task_update_handoff",
        ),
        (
            Op::SessionEvent(TaskSessionEventRequest {
                task_id: "outside".into(),
                session_id: "s1".into(),
                event_type: "agent-turn-complete".into(),
            }),
            "notify",
        ),
        (
            Op::MergePlanSet(req(json!({"entries": [{"repo": "acme/api", "number": 1}]}))),
            "merge_plan_set",
        ),
    ] {
        assert_eq!(
            dispatch_as(cx, &sender, operation, Some(&caller)),
            Err(gitterm_core::preview::service::preview_notice(label)),
            "{label}"
        );
    }
    cx.run_until_parked();
    assert_eq!(tree(&fixture.iced), iced_before);
    assert_eq!(tree(fixture.roots.root()), preview_before);
    assert_eq!(tree(&git_fixture.outside), outside_before);
    assert!(!base.join("worktrees").exists());
    assert_eq!(
        stored_task(&owner, "outside", cx).lifecycle,
        TaskLifecycle::Running
    );
}

/// Plain preview over the same fixture: the gate and each write arm
/// itself answer the notice, with valid fixture targets, and write nothing.
#[cfg(unix)]
#[gpui::test]
fn plain_preview_still_refuses_all_writes(cx: &mut TestAppContext) {
    use gitterm_core::{
        preview::service::preview_notice,
        task_host::HostEffect,
        task_mcp::{TaskControlOperation as Op, TaskSessionEventRequest},
    };
    use serde_json::json;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let git_fixture = GitFixture::new(&fixture);
    fixture.write_iced(
        vec![task_at(
            "worker",
            TaskLifecycle::Running,
            &git_fixture.worktree,
        )],
        vec![worker_delegation("d1", "worker", "child-d1")],
    );
    let chats = Chats::new(&fixture, cx);
    let caller = chats.open_in(&fixture, Some("root-chat"), &git_fixture.worktree, cx);
    chats.owner.update(cx, |owner, cx| {
        owner.chats[&caller].update(cx, |pane, _| pane.bind_task_session("worker", "child-d1"));
    });
    let (owner, sender) = fixture.start(cx);
    let iced_before = tree(&fixture.iced);
    let preview_before = tree(fixture.roots.root());
    let fixture_before = tree(&git_fixture.root);
    let operations = || {
        [
            (create_in(&git_fixture.repo), "task_create"),
            (
                Op::UpdateHandoff(req(
                    json!({"task_id": "worker", "summary": "s", "status": "done"}),
                )),
                "task_update_handoff",
            ),
            (
                Op::SessionEvent(TaskSessionEventRequest {
                    task_id: "worker".into(),
                    session_id: "child-d1".into(),
                    event_type: "agent-turn-complete".into(),
                }),
                "notify",
            ),
            (
                Op::MergePlanSet(req(json!({"entries": [{"repo": "acme/api", "number": 1}]}))),
                "merge_plan_set",
            ),
        ]
    };
    for (operation, label) in operations() {
        assert_eq!(
            dispatch_as(cx, &sender, operation, Some(&caller)),
            Err(preview_notice(label)),
            "{label}"
        );
    }
    for (operation, label) in operations() {
        let (commands, mut receiver) = command_channel();
        let pending = commands.enqueue(operation, Some(caller.clone())).unwrap();
        let envelope = receiver.try_recv().unwrap();
        let (reply, caller) = (envelope.reply, envelope.caller);
        let effect = match envelope.operation {
            Op::Create(request) => HostEffect::Create {
                reply,
                caller,
                request,
            },
            Op::UpdateHandoff(request) => HostEffect::UpdateHandoff {
                reply,
                caller,
                request,
            },
            Op::SessionEvent(request) => HostEffect::TurnEnded {
                reply,
                task_id: request.task_id,
                session_id: request.session_id,
            },
            Op::MergePlanSet(request) => HostEffect::MergePlanSet {
                reply,
                caller,
                request,
            },
            other => panic!("not a store write: {other:?}"),
        };
        owner.update(cx, |owner, cx| owner.execute_effect(effect, label, cx));
        let answer = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(pending);
        assert_eq!(answer, Err(preview_notice(label)), "{label}");
    }
    cx.run_until_parked();
    assert_eq!(tree(&fixture.iced), iced_before);
    assert_eq!(tree(fixture.roots.root()), preview_before);
    assert_eq!(tree(&git_fixture.root), fixture_before);
    assert_eq!(
        stored_task(&owner, "worker", cx).lifecycle,
        TaskLifecycle::Running
    );
    assert!(!owner.read_with(cx, |owner, _| owns_in_flight_work(owner.host())));
}

#[cfg(unix)]
#[gpui::test]
fn merge_plan_set_writes_preview_state_only(cx: &mut TestAppContext) {
    use gitterm_core::{
        merge_plan::{self, MergePlan},
        task_mcp::TaskControlOperation as Op,
    };
    use serde_json::json;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let git_fixture = GitFixture::new(&fixture);
    // Iced already has a plan for this root; the preview neither reads
    // nor changes it in fixture mode.
    let iced_plans = merge_plan::plans_dir(&fixture.iced);
    let iced_plan: MergePlan = req(json!({
        "root_chat_id": "root-a", "updated_at": "2026-11-20T10:00:00Z",
        "entries": [{"repo": "acme/web", "number": 9}]
    }));
    merge_plan::write_plan(&iced_plans, &iced_plan).unwrap();
    let chats = Chats::new(&fixture, cx);
    let caller = chats.open_in(&fixture, Some("root-a"), &git_fixture.repo, cx);
    let (_owner, sender) = git_fixture.start(&fixture, cx);
    let iced_before = tree(&fixture.iced);

    let set = Op::MergePlanSet(req(json!({"entries": [
        {"repo": "acme/api", "number": 1},
        {"repo": "acme/api", "number": 2, "after": ["acme/api#1"]},
    ]})));
    let value = dispatch_as(cx, &sender, set, Some(&caller)).unwrap();
    assert_eq!(value["root_chat_id"], "root-a", "{value}");
    assert_eq!(value["kept_holds"], json!([]), "{value}");
    let prs: Vec<_> = value["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["pr"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(prs, ["acme/api#1", "acme/api#2"]);
    // The file is the preview's; Iced's dir is unchanged.
    let preview_plans = merge_plan::plans_dir(fixture.roots.root());
    let saved = merge_plan::load_plans(&preview_plans);
    assert_eq!(saved["root-a"].entries.len(), 2);
    assert_eq!(tree(&fixture.iced), iced_before);
    // merge_plan_get reads the plan the preview wrote.
    let got = dispatch_as(cx, &sender, Op::MergePlanGet, Some(&caller)).unwrap();
    assert_eq!(got["entries"], value["entries"]);
    // Iced's validation and caller errors still apply.
    let invalid = Op::MergePlanSet(req(json!({"entries": [
        {"repo": "acme/api", "number": 1}, {"repo": "acme/api", "number": 1},
    ]})));
    assert!(dispatch_as(cx, &sender, invalid, Some(&caller)).is_err());
    assert_eq!(
        merge_plan::load_plans(&preview_plans)["root-a"]
            .entries
            .len(),
        2
    );
}

/// The prepared worktree is replaced by a link out of the fixture before
/// the post-preparation recheck: the create is refused with the notice, the
/// task fails in the preview store, and the link's target is not touched.
#[cfg(unix)]
#[gpui::test]
fn path_swapped_after_prep_refused(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    let git_fixture = GitFixture::new(&fixture);
    let (owner, sender) = git_fixture.start(&fixture, cx);
    let elsewhere = fixture.iced.parent().unwrap().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let elsewhere_before = tree(&elsewhere);
    let moved = git_fixture.root.join("moved");
    owner.update(cx, |owner, _| {
        let (elsewhere, moved) = (elsewhere.clone(), moved.clone());
        owner.before_recheck = Some(Box::new(move |path: &Path| {
            std::fs::rename(path, &moved).unwrap();
            std::os::unix::fs::symlink(&elsewhere, path).unwrap();
        }));
    });

    assert_eq!(
        dispatch(cx, &sender, create_in(&git_fixture.repo)),
        Err(gitterm_core::preview::service::preview_notice(
            "task_create"
        ))
    );
    let tasks = owner.read_with(cx, |owner, _| {
        owner.host().store().as_ref().unwrap().tasks().to_vec()
    });
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    assert_ne!(task.lifecycle, TaskLifecycle::Ready, "{task:?}");
    assert_eq!(task.worktree.path, None, "{task:?}");
    assert_eq!(tree(&elsewhere), elsewhere_before);
}

// ---------------------------------------------------------------------------
// H5b B10: review and consult run a fake Codex in fixture mode.

/// A fake `codex` on its own PATH dir. Run `n` records `env-n`, `args-n`,
/// `stdin-n` and `pwd-n`, starts a `sleep` recording its pid in `child-n`
/// when `spawn-n` exists, waits while `hold-n` exists, then prints a
/// consult stream. `runs` holds one line per start.
struct FakeCodex {
    dir: tempfile::TempDir,
}
impl FakeCodex {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = r#"#!/bin/sh
echo run >> 'DIR/runs'
n=$(wc -l < 'DIR/runs' | tr -d ' ')
env > "DIR/env-$n"
printf '%s\n' "$@" > "DIR/args-$n"
cat > "DIR/stdin-$n"
pwd -P > "DIR/pwd-$n"
if [ -f "DIR/spawn-$n" ]; then
  sleep 600 >/dev/null 2>&1 &
  echo $! > "DIR/child-$n"
fi
while [ -f "DIR/hold-$n" ]; do sleep 0.05; done
echo '{"type":"thread.started","thread_id":"thread-'"$n"'"}'
echo '{"type":"turn.started"}'
echo '{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Fixture answer"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}'
"#
        .replace("DIR", &dir.path().display().to_string());
        let program = dir.path().join("codex");
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }
    /// The fake first, then the system tools and the real git.
    fn search_path(&self) -> std::ffi::OsString {
        let git = std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap();
        let git = PathBuf::from(String::from_utf8(git.stdout).unwrap().trim());
        std::env::join_paths([
            self.dir.path(),
            Path::new("/bin"),
            Path::new("/usr/bin"),
            git.parent().unwrap(),
        ])
        .unwrap()
    }
    fn runs(&self) -> usize {
        std::fs::read_to_string(self.dir.path().join("runs")).map_or(0, |runs| runs.lines().count())
    }
    fn hold(&self, run: usize) {
        std::fs::write(self.dir.path().join(format!("hold-{run}")), b"").unwrap();
    }
    fn release(&self, run: usize) {
        std::fs::remove_file(self.dir.path().join(format!("hold-{run}"))).unwrap();
    }
    fn env(&self, run: usize) -> std::collections::BTreeMap<String, String> {
        std::fs::read_to_string(self.dir.path().join(format!("env-{run}")))
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }
    fn pwd(&self, run: usize) -> PathBuf {
        let pwd = std::fs::read_to_string(self.dir.path().join(format!("pwd-{run}"))).unwrap();
        PathBuf::from(pwd.trim_end())
    }
    /// Run `run` starts a descendant; its pid, once recorded.
    fn spawn_descendant(&self, run: usize) {
        std::fs::write(self.dir.path().join(format!("spawn-{run}")), b"").unwrap();
    }
    fn descendant(&self, run: usize) -> Option<u32> {
        let pid = std::fs::read_to_string(self.dir.path().join(format!("child-{run}"))).ok()?;
        pid.ends_with('\n').then(|| pid.trim().parse().unwrap())
    }
    fn args(&self, run: usize) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join(format!("args-{run}")))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// A fixture host whose one Iced workspace is the fixture, with a chat
/// pane in the fixture repo and a fake Codex.
struct Runs {
    fixture: Fixture,
    git: GitFixture,
    _chats: Chats,
    caller: String,
    owner: Entity<TaskHostOwner>,
    sender: TaskControlSender,
    codex: FakeCodex,
}
impl Runs {
    fn start(max_concurrent: usize, cx: &mut TestAppContext) -> Self {
        Self::start_with(max_concurrent, |_| {}, cx)
    }
    /// [`Self::start`] after `seed` writes Iced's files.
    fn start_with(
        max_concurrent: usize,
        seed: impl FnOnce(&Runs0),
        cx: &mut TestAppContext,
    ) -> Self {
        cx.background_executor.allow_parking();
        let fixture = Fixture::new();
        let git = GitFixture::new(&fixture);
        std::fs::write(
            fixture.iced.join("settings.json"),
            serde_json::to_vec(
                &serde_json::json!({"review": {"max_concurrent_reviews": max_concurrent}}),
            )
            .unwrap(),
        )
        .unwrap();
        let chats = Chats::new(&fixture, cx);
        let workspace = serde_json::from_value(serde_json::json!({
            "name": "FX", "abbrev": "FX", "color": "blue",
            "dir": git.root, "repos": [git.repo], "tabs": []
        }))
        .unwrap();
        cx.update(|cx| {
            cx.global_mut::<crate::work_sidebar::IcedWorkSource>()
                .workspaces = vec![workspace]
        });
        let caller = chats.open_in(&fixture, Some("root-chat"), &git.repo, cx);
        seed(&Runs0 {
            fixture: &fixture,
            git: &git,
            caller: &caller,
        });
        let (owner, sender) = git.start(&fixture, cx);
        let codex = FakeCodex::new();
        owner.update(cx, |owner, _| {
            owner.codex_search_path = Some(codex.search_path())
        });
        Self {
            fixture,
            git,
            _chats: chats,
            caller,
            owner,
            sender,
            codex,
        }
    }
    fn consult_in(
        &self,
        checkout: &Path,
        cx: &mut TestAppContext,
    ) -> Result<serde_json::Value, String> {
        dispatch_as(
            cx,
            &self.sender,
            TaskControlOperation::RequestConsult(req(serde_json::json!({
                "brief": "What does this fixture hold?",
                "checkout": checkout,
            }))),
            Some(&self.caller),
        )
    }
    fn consult(&self, cx: &mut TestAppContext) -> String {
        let reply = self.consult_in(&self.git.repo, cx).unwrap();
        assert_eq!(reply["status"], "requested", "{reply}");
        reply["delegation_id"].as_str().unwrap().to_string()
    }
    fn status(&self, id: &str, cx: &mut TestAppContext) -> String {
        self.owner.read_with(cx, |owner, _| {
            owner
                .host()
                .store()
                .as_ref()
                .unwrap()
                .delegation(id)
                .unwrap()
                .status
                .label()
                .to_string()
        })
    }
    /// The delegations in the preview store's file, as saved.
    fn saved(&self) -> Vec<Delegation> {
        gitterm_core::tasks::TaskStore::load(self.fixture.copy())
            .unwrap()
            .delegations()
            .to_vec()
    }
    /// Shuts the host down when the run continuation at `stage` is reached.
    fn shut_down_at(&self, stage: &'static str, cx: &mut TestAppContext) {
        self.owner.update(cx, |owner, _| {
            owner.before_continuation = Some((stage, Box::new(|owner| owner.shutdown())))
        });
    }
    fn delegation_count(&self, cx: &mut TestAppContext) -> usize {
        self.owner.read_with(cx, |owner, _| {
            owner.host().store().as_ref().unwrap().delegations().len()
        })
    }
}

/// What a seed step sees before the host starts.
struct Runs0<'a> {
    fixture: &'a Fixture,
    git: &'a GitFixture,
    caller: &'a str,
}

/// Runs the executor until `done`, or fails after 15 seconds.
fn wait_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Lets a started fake run reach its hold and the host settle.
fn settle(cx: &mut TestAppContext) {
    for _ in 0..10 {
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(unix)]
#[gpui::test]
fn delegate_runs_fake_codex_with_scoped_env(cx: &mut TestAppContext) {
    let runs = Runs::start(2, cx);
    let iced_before = tree(&runs.fixture.iced);
    let reply = runs.consult_in(&runs.git.repo, cx).unwrap();
    assert_eq!(reply["queued"], false, "{reply}");
    assert_eq!(reply["provider"], "codex", "{reply}");
    let id = reply["delegation_id"].as_str().unwrap().to_string();
    wait_until(cx, |cx| runs.status(&id, cx) == "completed");
    let delegation = runs.owner.read_with(cx, |owner, _| {
        owner
            .host()
            .store()
            .as_ref()
            .unwrap()
            .delegation(&id)
            .unwrap()
            .clone()
    });
    assert_eq!(
        delegation.result.unwrap().handoff.unwrap().summary,
        "Fixture answer"
    );
    // Saved in the preview's own store; Iced's tree is untouched.
    let saved = gitterm_core::tasks::TaskStore::load(runs.fixture.copy()).unwrap();
    assert_eq!(saved.delegation(&id).unwrap().status.label(), "completed");
    assert_eq!(tree(&runs.fixture.iced), iced_before);

    let args = runs.codex.args(1);
    assert_eq!(&args[..3], ["exec", "--sandbox", "read-only"], "{args:?}");
    let env = runs.codex.env(1);
    let root = runs.fixture.roots.root();
    let home = root.join("provider-homes/home");
    assert_eq!(env["HOME"], home.display().to_string());
    assert_eq!(env["CODEX_HOME"], home.join(".codex").display().to_string());
    assert_eq!(
        env["GIT_CEILING_DIRECTORIES"],
        runs.git.root.parent().unwrap().display().to_string()
    );
    let git_vars: Vec<_> = env.keys().filter(|name| name.starts_with("GIT_")).collect();
    assert_eq!(git_vars, ["GIT_CEILING_DIRECTORIES"], "{env:?}");
    // Only the host's own GitTerm variables: its config root and its task
    // server, never an inherited (production) token.
    use gitterm_core::task_commands::mcp_protocol::{TASK_MCP_TOKEN_ENV, TASK_MCP_URL_ENV};
    let gitterm: Vec<_> = env
        .keys()
        .filter(|name| name.starts_with("GITTERM_"))
        .collect();
    assert_eq!(
        gitterm,
        [
            gitterm_core::config_paths::CONFIG_DIR_ENV,
            TASK_MCP_TOKEN_ENV,
            TASK_MCP_URL_ENV
        ],
        "{env:?}"
    );
    assert_eq!(
        env[gitterm_core::config_paths::CONFIG_DIR_ENV],
        root.display().to_string()
    );
    let token = runs.owner.read_with(cx, |owner, _| {
        owner.host().services().task_mcp().token().to_string()
    });
    assert_eq!(env[TASK_MCP_TOKEN_ENV], token);
    if let Ok(inherited) = std::env::var(TASK_MCP_TOKEN_ENV) {
        assert_ne!(env[TASK_MCP_TOKEN_ENV], inherited);
    }
    let endpoint = runs
        .owner
        .read_with(cx, |owner, _| owner.task_endpoint().unwrap());
    assert!(env[TASK_MCP_URL_ENV].starts_with(&endpoint), "{env:?}");
    // The Codex trust check reads the home the child got.
    gitterm_core::workers::validate_codex_trust_home(
        &home,
        &env.iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>(),
        |_| None,
    )
    .unwrap();
}

#[cfg(unix)]
#[gpui::test]
fn queued_start_rechecks_policy_trust_and_batch_capacity(cx: &mut TestAppContext) {
    use gitterm_core::plan_limits::{LimitWindow, PlanLimits, PlanLimitsFile, Provider};
    let runs = Runs::start(1, cx);
    runs.codex.hold(1);
    let first = runs.consult(cx);
    let second = runs.consult(cx);
    let third = runs.consult(cx);
    // Wait for the first start; the settle then shows no second one starts.
    wait_until(cx, |_| runs.codex.runs() == 1);
    settle(cx);
    assert_eq!(runs.codex.runs(), 1);
    assert_eq!(runs.status(&second, cx), "requested");
    assert_eq!(runs.status(&third, cx), "requested");
    // Codex's weekly window is used up before the slot frees: the queued
    // runs stay queued.
    let now = chrono::Utc::now().timestamp();
    let mut limits = PlanLimitsFile::default();
    limits.absorb(PlanLimits {
        provider: Provider::Codex,
        observed_at: now,
        local: Some(true),
        windows: vec![LimitWindow {
            label: "7d".into(),
            used_percent: Some(99.0),
            resets_at: Some(now + 86_400),
        }],
        alerts: vec![],
        plan_type: None,
        credits: None,
    });
    let limits_path = PlanLimitsFile::path_in(&runs.fixture.iced);
    limits.save(&limits_path).unwrap();
    runs.codex.release(1);
    wait_until(cx, |cx| runs.status(&first, cx) == "completed");
    settle(cx);
    assert_eq!(runs.codex.runs(), 1);
    assert_eq!(runs.status(&second, cx), "requested");
    assert_eq!(runs.status(&third, cx), "requested");
    // Once it frees, one slot starts one of the batch, oldest first.
    std::fs::remove_file(&limits_path).unwrap();
    runs.codex.hold(2);
    runs.owner
        .update(cx, |owner, cx| owner.start_queued_runs(cx));
    wait_until(cx, |_| runs.codex.runs() == 2);
    settle(cx);
    assert_eq!(runs.codex.runs(), 2);
    assert_eq!(runs.status(&third, cx), "requested");
    // Every start gets the same checked home.
    assert_eq!(
        runs.codex.env(2)["CODEX_HOME"],
        runs.codex.env(1)["CODEX_HOME"]
    );
    runs.codex.release(2);
    wait_until(cx, |cx| runs.status(&third, cx) == "completed");
    assert_eq!(runs.status(&second, cx), "completed");
    assert_eq!(runs.codex.runs(), 3);
}

#[cfg(unix)]
#[gpui::test]
fn fixture_checkout_with_outside_repo_refused(cx: &mut TestAppContext) {
    let runs = Runs::start(2, cx);
    // A worktree of the outside repo placed inside the fixture: its Git
    // admin dir lies outside.
    let linked = runs.git.root.join("outside-worktree");
    git(
        &runs.git.outside,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    let outside_before = tree(&runs.git.outside);
    for checkout in [&linked, &runs.git.outside] {
        assert_eq!(
            runs.consult_in(checkout, cx),
            Err(gitterm_core::preview::service::preview_notice(
                "consult_request"
            )),
            "{}",
            checkout.display()
        );
    }
    settle(cx);
    assert_eq!(runs.codex.runs(), 0);
    assert_eq!(runs.delegation_count(cx), 0);
    assert_eq!(tree(&runs.git.outside), outside_before);
}

#[cfg(unix)]
#[gpui::test]
fn run_path_swapped_after_check_refused(cx: &mut TestAppContext) {
    let runs = Runs::start(2, cx);
    let elsewhere = runs.fixture.iced.parent().unwrap().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let moved = runs.git.root.join("moved");
    runs.owner.update(cx, |owner, _| {
        let (elsewhere, moved) = (elsewhere.clone(), moved.clone());
        *owner.before_run_recheck.borrow_mut() = Some(Box::new(move |path: &Path| {
            std::fs::rename(path, &moved).unwrap();
            std::os::unix::fs::symlink(&elsewhere, path).unwrap();
        }));
    });
    assert_eq!(
        runs.consult_in(&runs.git.repo, cx),
        Err(gitterm_core::preview::service::preview_notice(
            "consult_request"
        ))
    );
    settle(cx);
    assert_eq!(runs.codex.runs(), 0);
    assert_eq!(runs.delegation_count(cx), 0);
}

#[cfg(unix)]
#[gpui::test]
fn mixed_allowed_and_denied_headless_queue(cx: &mut TestAppContext) {
    let runs = Runs::start(1, cx);
    // A second checkout in the fixture, a worktree of the listed repo.
    let other = runs.git.root.join("other");
    git(
        &runs.git.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            other.to_str().unwrap(),
        ],
    );
    runs.codex.hold(1);
    let first = runs.consult(cx);
    let reply = runs.consult_in(&other, cx).unwrap();
    let denied = reply["delegation_id"].as_str().unwrap().to_string();
    let allowed = runs.consult(cx);
    // The queued run's checkout now leads out of the fixture.
    let elsewhere = runs.fixture.iced.parent().unwrap().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::rename(&other, runs.git.root.join("other-moved")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &other).unwrap();
    runs.codex.release(1);
    wait_until(cx, |cx| runs.status(&allowed, cx) == "completed");
    assert_eq!(runs.status(&first, cx), "completed");
    // The denied one stays queued and untouched; the allowed one ran.
    assert_eq!(runs.status(&denied, cx), "requested");
    assert_eq!(runs.codex.runs(), 2);
}

#[cfg(unix)]
#[gpui::test]
fn imported_requested_run_inert_in_fixture_mode(cx: &mut TestAppContext) {
    // Iced's record looks like a run of this very chat in this fixture.
    let runs = Runs::start_with(
        2,
        |seed| {
            let imported: Delegation = serde_json::from_value(serde_json::json!({
                "delegation_id": "imported", "kind": "consult",
                "parent": {"session_uid": seed.caller, "workspace": "FX", "cwd": seed.git.repo},
                "child": {"runner": "codex_consult"},
                "brief": "Imported", "status": {"state": "requested"},
                "created_at": "2026-11-20T12:00:00Z", "updated_at": "2026-11-20T12:01:00Z"
            }))
            .unwrap();
            seed.fixture.write_iced(
                vec![
                    task("running", TaskLifecycle::Running),
                    task("queued", TaskLifecycle::Queued),
                ],
                vec![imported],
            );
        },
        cx,
    );
    let imported = |cx: &mut TestAppContext| {
        runs.owner.read_with(cx, |owner, _| {
            owner
                .host()
                .store()
                .as_ref()
                .unwrap()
                .delegation("imported")
                .unwrap()
                .clone()
        })
    };
    let before = imported(cx);
    let id = runs.consult(cx);
    wait_until(cx, |cx| runs.status(&id, cx) == "completed");
    settle(cx);
    assert_eq!(imported(cx), before);
    assert_eq!(runs.codex.runs(), 1);
    assert_eq!(
        stored_task(&runs.owner, "running", cx).lifecycle,
        TaskLifecycle::Running
    );
    assert_eq!(
        stored_task(&runs.owner, "queued", cx).lifecycle,
        TaskLifecycle::Queued
    );
    assert!(
        runs.owner
            .read_with(cx, |owner, _| owner.host().launch_queue().is_empty())
    );
}

/// A script at `path` that, once, moves `repo` aside and puts a symlink to
/// `outside` in its place.
#[cfg(unix)]
fn swap_script(path: &Path, prelude: &str, repo: &Path, outside: &Path, tail: &str) {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        "#!/bin/sh\n{prelude}\nif [ ! -L '{repo}' ]; then mv '{repo}' '{repo}-moved' && ln -s '{outside}' '{repo}'; fi\n{tail}\n",
        repo = repo.display(),
        outside = outside.display(),
    );
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Waits for `id` to leave requested and running.
fn settled_status(runs: &Runs, id: &str, cx: &mut TestAppContext) -> String {
    wait_until(cx, |cx| {
        !matches!(runs.status(id, cx).as_str(), "requested" | "running")
    });
    runs.status(id, cx)
}

#[cfg(unix)]
#[gpui::test]
fn fsmonitor_hook_never_runs_in_run_preflight(cx: &mut TestAppContext) {
    let runs = Runs::start(2, cx);
    let marker = runs.fixture.iced.parent().unwrap().join("fsmonitor-ran");
    // The checkout's own hook would put the outside repo in its place
    // during the runner's `git status`.
    let hook = runs.git.root.join("fsmonitor-hook");
    swap_script(
        &hook,
        &format!("touch '{}'", marker.display()),
        &runs.git.repo,
        &runs.git.outside,
        "exit 1",
    );
    git(
        &runs.git.repo,
        &["config", "core.fsmonitor", hook.to_str().unwrap()],
    );
    let outside_before = tree(&runs.git.outside);
    let id = runs.consult(cx);
    assert_eq!(settled_status(&runs, &id, cx), "completed");
    assert!(!marker.exists(), "the run's git preflight ran the hook");
    assert_eq!(runs.codex.runs(), 1);
    assert_eq!(runs.codex.pwd(1), runs.git.repo.canonicalize().unwrap());
    assert_eq!(tree(&runs.git.outside), outside_before);
}

#[cfg(unix)]
#[gpui::test]
fn checkout_swapped_during_preflight_refused(cx: &mut TestAppContext) {
    let runs = Runs::start(2, cx);
    // A `git` ahead of the real one swaps the checkout during the runner's
    // `git status`, as any hook it runs could.
    let wrapper = tempfile::tempdir().unwrap();
    let real_git = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    swap_script(
        &wrapper.path().join("git"),
        "for arg in \"$@\"; do [ \"$arg\" = status ] && swap=1; done\n[ -n \"$swap\" ] || exec '{git}' \"$@\""
            .replace("{git}", real_git.trim())
            .as_str(),
        &runs.git.repo,
        &runs.git.outside,
        &format!("exec '{}' \"$@\"", real_git.trim()),
    );
    let mut path = vec![wrapper.path().to_path_buf()];
    path.extend(std::env::split_paths(&runs.codex.search_path()));
    runs.owner.update(cx, |owner, _| {
        owner.codex_search_path = Some(std::env::join_paths(path).unwrap())
    });
    let outside_before = tree(&runs.git.outside);
    let id = runs.consult(cx);
    assert_eq!(settled_status(&runs, &id, cx), "failed");
    assert!(
        std::fs::symlink_metadata(&runs.git.repo)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the preflight swapped the checkout"
    );
    let status = runs.saved()[0].status.clone();
    assert_eq!(
        status,
        gitterm_core::tasks::DelegationStatus::Failed {
            message: gitterm_core::preview::service::preview_notice("Start delegation")
        }
    );
    assert_eq!(runs.codex.runs(), 0, "Codex started outside the fixture");
    assert_eq!(tree(&runs.git.outside), outside_before);
}

#[cfg(unix)]
#[gpui::test]
fn work_tree_moved_out_by_preflight_config_refused(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    let runs = Runs::start(2, cx);
    // A `git` ahead of the real one stands in for a config-driven program
    // in the runner's `git status`: once that status has run, it points the
    // checkout's work tree at the outside repository. The checkout and its
    // Git dirs stay where they were.
    let wrapper = tempfile::tempdir().unwrap();
    let real_git = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    let script = wrapper.path().join("git");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do [ \"$arg\" = status ] && moved=1; done\n\
             '{git}' \"$@\"\nstatus=$?\n\
             [ -n \"$moved\" ] && '{git}' -C '{repo}' config core.worktree '{outside}'\n\
             exit $status\n",
            git = real_git.trim(),
            repo = runs.git.repo.display(),
            outside = runs.git.outside.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = vec![wrapper.path().to_path_buf()];
    path.extend(std::env::split_paths(&runs.codex.search_path()));
    runs.owner.update(cx, |owner, _| {
        owner.codex_search_path = Some(std::env::join_paths(path).unwrap())
    });
    let outside_before = tree(&runs.git.outside);
    let id = runs.consult(cx);
    assert_eq!(settled_status(&runs, &id, cx), "failed");
    // The preflight did move the work tree.
    let top = git(&runs.git.repo, &["rev-parse", "--show-toplevel"]);
    assert_eq!(
        Path::new(top.trim_end()),
        runs.git.outside.canonicalize().unwrap()
    );
    assert_eq!(
        runs.saved()[0].status,
        gitterm_core::tasks::DelegationStatus::Failed {
            message: gitterm_core::preview::service::preview_notice("Start delegation")
        }
    );
    assert_eq!(
        runs.codex.runs(),
        0,
        "Codex started on the outside work tree"
    );
    assert_eq!(tree(&runs.git.outside), outside_before);
}

#[cfg(unix)]
#[gpui::test]
fn shutdown_during_pending_check_persists_nothing(cx: &mut TestAppContext) {
    let runs = Runs::start(2, cx);
    runs.shut_down_at("checked", cx);
    let reply = runs.consult_in(&runs.git.repo, cx);
    assert!(reply.is_err(), "{reply:?}");
    settle(cx);
    assert_eq!(runs.delegation_count(cx), 0);
    assert!(runs.saved().is_empty());
    assert_eq!(runs.codex.runs(), 0);
}

#[cfg(unix)]
#[gpui::test]
fn shutdown_during_queued_start_check_persists_nothing(cx: &mut TestAppContext) {
    let runs = Runs::start(1, cx);
    runs.codex.hold(1);
    let first = runs.consult(cx);
    let second = runs.consult(cx);
    settle(cx);
    runs.shut_down_at("queued start", cx);
    runs.codex.release(1);
    wait_until(cx, |cx| {
        runs.owner
            .read_with(cx, |owner, _| owner.before_continuation.is_none())
    });
    settle(cx);
    let saved = runs.saved();
    let status = |id: &str| {
        saved
            .iter()
            .find(|d| d.delegation_id == id)
            .unwrap()
            .status
            .label()
    };
    assert_eq!(status(&first), "completed");
    assert_eq!(status(&second), "requested");
    assert_eq!(runs.codex.runs(), 1);
}

#[cfg(unix)]
#[gpui::test]
fn shutdown_with_buffered_progress_persists_nothing(cx: &mut TestAppContext) {
    let runs = Runs::start(2, cx);
    runs.shut_down_at("progress", cx);
    let id = runs.consult(cx);
    wait_until(cx, |cx| {
        runs.owner
            .read_with(cx, |owner, _| owner.before_continuation.is_none())
    });
    settle(cx);
    // Neither the start nor the result reached the store.
    assert_eq!(runs.saved()[0].delegation_id, id);
    assert_eq!(runs.saved()[0].status.label(), "requested");
    assert_eq!(runs.status(&id, cx), "requested");
}

#[cfg(unix)]
#[gpui::test]
fn shutdown_kills_codex_descendants(cx: &mut TestAppContext) {
    let runs = Runs::start(2, cx);
    runs.codex.spawn_descendant(1);
    runs.codex.hold(1);
    let _id = runs.consult(cx);
    wait_until(cx, |_| runs.codex.descendant(1).is_some());
    let pid = runs.codex.descendant(1).unwrap().to_string();
    let alive = || {
        std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    assert!(alive());
    runs.owner.update(cx, |owner, _| owner.shutdown());
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive() {
        assert!(
            Instant::now() < deadline,
            "descendant {pid} outlived shutdown"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
#[gpui::test]
fn clean_filter_can_redirect_git_dir(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    let runs = Runs::start(2, cx);
    let repo = &runs.git.repo;
    // The fixture's own config names a clean filter for a tracked file; the
    // runner's `git status` runs it, and it points `.git` outside.
    let tracked = repo.join("filtered");
    std::fs::write(&tracked, "one\n").unwrap();
    std::fs::write(repo.join(".gitattributes"), "filtered filter=swap\n").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "filtered baseline"]);
    let filter = runs.git.root.join("clean-filter");
    std::fs::write(
        &filter,
        format!(
            "#!/bin/sh\nif [ ! -L .git ]; then mv .git .git-original; ln -s '{}' .git; fi\ncat\n",
            runs.git.outside.join(".git").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        repo,
        &["config", "filter.swap.clean", filter.to_str().unwrap()],
    );
    std::fs::write(&tracked, "two\n").unwrap();
    let id = runs.consult(cx);
    assert_eq!(settled_status(&runs, &id, cx), "failed");
    assert!(
        std::fs::symlink_metadata(repo.join(".git"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the preflight ran the clean filter"
    );
    assert_eq!(
        runs.codex.runs(),
        0,
        "Codex started with a Git dir outside the fixture"
    );
    assert_eq!(
        runs.saved()[0].status,
        gitterm_core::tasks::DelegationStatus::Failed {
            message: gitterm_core::preview::service::preview_notice("Start delegation")
        }
    );
}

#[cfg(unix)]
#[gpui::test]
fn slow_preflight_blocks_shutdown(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt;
    let runs = Runs::start(2, cx);
    // A `git` ahead of the real one that sleeps in the runner's `git status`.
    let wrapper = tempfile::tempdir().unwrap();
    let real_git = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    let marker = wrapper.path().join("in-status");
    let script = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do if [ \"$arg\" = status ]; then touch '{}'; sleep 4; fi; done\nexec '{}' \"$@\"\n",
        marker.display(),
        real_git.trim()
    );
    let program = wrapper.path().join("git");
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = vec![wrapper.path().to_path_buf()];
    path.extend(std::env::split_paths(&runs.codex.search_path()));
    runs.owner.update(cx, |owner, _| {
        owner.codex_search_path = Some(std::env::join_paths(path).unwrap())
    });
    let _id = runs.consult(cx);
    wait_until(cx, |_| marker.exists());
    let start = Instant::now();
    runs.owner.update(cx, |owner, _| owner.shutdown());
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "shutdown blocked the UI for {elapsed:?}"
    );
    assert_eq!(runs.codex.runs(), 0, "Codex started after shutdown");
}

/// A pane of task `ready` titled `title` in `folder`.
fn labelled_pane(folder: &Path, title: &str) -> crate::task_host::TaskPane {
    crate::task_host::TaskPane {
        task_id: "ready".into(),
        session: gitterm_core::task_host::OpenSession {
            session_id: Some("s1".into()),
            label: title.into(),
            workspace: "w".into(),
            is_active: true,
        },
        folder: Some(folder.to_path_buf()),
    }
}

/// `task_get ready`'s `open_sessions` labels.
fn open_session_labels(
    sender: &TaskControlSender,
    cx: &mut TestAppContext,
) -> Vec<serde_json::Value> {
    use gitterm_core::task_mcp::TaskControlOperation as Op;
    let got = dispatch(
        cx,
        sender,
        Op::Get(req(serde_json::json!({"task_id": "ready"}))),
    )
    .unwrap();
    got["open_sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["label"].clone())
        .collect()
}

/// Iced's steady-state tab label (`status.repo_name`) is the git root's
/// folder name, whichever folder inside the repo the session runs in; a
/// folder outside git keeps its own name. Until the background read
/// returns, the label is the pane title, as Iced's tab keeps its launch
/// title until its first status poll.
#[gpui::test]
fn open_sessions_label_is_the_repo_folder_name(cx: &mut TestAppContext) {
    use serde_json::json;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![task("ready", TaskLifecycle::Ready)], vec![]);
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("the-repo");
    let sub = repo.join("crates").join("inner");
    let plain = dir.path().join("plain-folder");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    git(&repo, &["init", "-q"]);
    let (owner, sender) = fixture.start(cx);
    owner.update(cx, |owner, cx| {
        owner.set_panes(
            vec![
                labelled_pane(&sub, "Codex \u{b7} Task"),
                labelled_pane(&plain, "Claude \u{b7} Task"),
            ],
            cx,
        )
    });
    // Nothing has run since the push, so the reads have not returned.
    assert_eq!(
        owner.read_with(cx, |owner, _| owner
            .labelled_panes()
            .iter()
            .map(|pane| pane.session.label.clone())
            .collect::<Vec<_>>()),
        ["Codex \u{b7} Task", "Claude \u{b7} Task"]
    );
    let expected = [json!("the-repo"), json!("plain-folder")];
    wait_until(cx, |cx| open_session_labels(&sender, cx) == expected);
}

/// A pane that leaves its folder drops the folder's label, so a pane that
/// returns reads it afresh, as a new Iced tab would.
#[gpui::test]
fn open_sessions_label_is_read_again_after_the_folder_is_left(cx: &mut TestAppContext) {
    use serde_json::json;
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![task("ready", TaskLifecycle::Ready)], vec![]);
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("later-a-repo");
    std::fs::create_dir_all(&folder).unwrap();
    let (owner, sender) = fixture.start(cx);
    let push = |panes: Vec<crate::task_host::TaskPane>, cx: &mut TestAppContext| {
        owner.update(cx, |owner, cx| owner.set_panes(panes, cx))
    };
    push(vec![labelled_pane(&folder, "T")], cx);
    wait_until(cx, |cx| {
        open_session_labels(&sender, cx) == [json!("later-a-repo")]
    });
    push(vec![], cx);
    assert!(owner.read_with(cx, |owner, _| owner.folder_labels.is_empty()));
    push(vec![labelled_pane(&folder, "T")], cx);
    assert_eq!(
        owner.read_with(cx, |owner, _| owner.labelled_panes()[0]
            .session
            .label
            .clone()),
        "T"
    );
}

#[cfg(unix)]
#[path = "task_host_launch_tests.rs"]
mod launch_tests;

#[cfg(unix)]
#[path = "task_host_delivery_tests.rs"]
mod delivery_tests;

// Slice 7 runs each scenario in a fresh process: no process-global HOME or
// config mutation races with the Preview/fixture tests in this binary.
#[cfg(unix)]
mod live_writes {
    use super::*;
    use gitterm_core::{preview::service::ServiceContext, tasks::TaskStore};
    use serde_json::json;

    const CHILD: &str = "GITTERM_LIVE_WRITE_TEST";

    fn child(name: &str) -> bool {
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return false;
        }
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let home = base.join("home");
        let root = base.join("v6");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&root).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("task_host_tests::live_writes::{name}"),
                "--nocapture",
            ])
            .env(CHILD, name)
            .env("HOME", &home)
            .env("GITTERM_V6_CONFIG_DIR", &root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child {name}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    fn fixture() -> (Fixture, GitFixture) {
        let root = PathBuf::from(std::env::var_os("GITTERM_V6_CONFIG_DIR").unwrap());
        assert!(root.is_absolute());
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(root.parent(), home.parent());
        let roots = Arc::new(
            Preview::validate(
                &root.parent().unwrap().join("preview"),
                ProtectedRoots::new(&home, [root.clone()]).unwrap(),
            )
            .unwrap(),
        );
        let fixture = Fixture {
            _dir: tempfile::tempdir().unwrap(),
            roots,
            iced: root,
        };
        fixture.write_iced(vec![], vec![]);
        let git = GitFixture::new(&fixture);
        (fixture, git)
    }

    fn start(
        fixture: &Fixture,
        cx: &mut TestAppContext,
    ) -> (Entity<TaskHostOwner>, TaskControlSender) {
        let (config, sender) = fixture.config();
        let lock = Arc::new(HostLock::acquire(&fixture.iced).unwrap());
        let services = Arc::new(
            ServiceContext::production(&fixture.iced, std::process::id().to_string())
                .unwrap()
                .for_gpui_live(),
        );
        let owner = cx
            .update(|cx| TaskHostOwner::start_live(config, lock, services, cx))
            .unwrap();
        // Exercise the same issuance path used by the shell's chat descriptor.
        assert_eq!(
            owner
                .read_with(cx, |owner, _| owner.claude_mcp_servers("caller"))
                .len(),
            2
        );
        (owner, sender)
    }

    fn create(
        cx: &mut TestAppContext,
        sender: &TaskControlSender,
        repo: &Path,
    ) -> serde_json::Value {
        dispatch(cx, sender, create_in(repo)).unwrap()
    }

    fn handoff(id: &str, summary: &str) -> TaskControlOperation {
        TaskControlOperation::UpdateHandoff(req(
            json!({"task_id": id, "summary": summary, "decisions": ["Use core"], "next_steps": ["Continue"], "blockers": []}),
        ))
    }

    fn caller_shell<'a>(
        fixture: &Fixture,
        owner: &Entity<TaskHostOwner>,
        cx: &'a mut TestAppContext,
    ) -> (Entity<crate::Shell>, &'a mut gpui::VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(crate::task_host::GpuiTaskHost(owner.clone()));
        });
        cx.add_window_view(|window, cx| {
            let mut model = crate::workspace_status::WorkspaceStatus::default();
            model.add(fixture.iced.clone(), Instant::now());
            let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, None, cx));
            crate::Shell::with_owner(
                std::rc::Rc::new(crate::data::LocalData(owner.clone())),
                owner,
                fixture.roots.root(),
                true,
                window,
                cx,
            )
            .unwrap()
        })
    }

    fn add_caller_chat(
        shell: &Entity<crate::Shell>,
        owner: &Entity<TaskHostOwner>,
        fixture: &Fixture,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<crate::chat::ChatPane> {
        let mut chat = None;
        shell.update_in(cx, |shell, window, cx| {
            let saved = crate::chat::SavedChat {
                uid: "history".into(),
                cwd: fixture.iced.clone(),
                provider: fixture.roots.provider(),
                session_id: None,
                records: vec![],
            };
            let view = cx.new(|cx| {
                crate::chat::ChatPane::history(
                    shell.owner.read(cx).chat_shutdown.clone(),
                    saved,
                    window,
                    cx,
                )
            });
            let target = view.read(cx).target();
            owner.read(cx).claude_mcp_servers(&target.session_uid);
            let id = crate::data::SessionId(target.tab_id as u64);
            let pane_id = crate::session_pane_id(id);
            let workspace = shell.active(cx).workspace.id;
            let pane = cx.new(|cx| {
                crate::pane::Pane::new(
                    pane_id.clone(),
                    "Chat".into(),
                    crate::pane::PaneKind::PreviewChat(view.clone()),
                    workspace,
                    shell.data.clone(),
                    window,
                    cx,
                )
            });
            shell.owner.update(cx, |owner, _| {
                owner.chats.insert(target.session_uid.clone(), view.clone());
            });
            let state = shell
                .workspaces
                .iter_mut()
                .find(|state| state.workspace.id == workspace)
                .unwrap();
            state.panes.insert(pane_id, pane);
            state.sessions.push(crate::data::Session {
                id,
                title: "Chat".into(),
                kind: crate::data::SessionKind::Chat,
            });
            chat = Some(view);
        });
        chat.unwrap()
    }

    #[gpui::test]
    fn closed_chat_refuses_create(cx: &mut TestAppContext) {
        if child("closed_chat_refuses_create") {
            return;
        }
        cx.background_executor.allow_parking();
        let (fixture, git) = fixture();
        let (owner, sender) = start(&fixture, cx);
        let (shell, cx) = caller_shell(&fixture, &owner, cx);
        let chat = add_caller_chat(&shell, &owner, &fixture, cx);
        let _other = add_caller_chat(&shell, &owner, &fixture, cx);
        let target = chat.read_with(cx, |chat, _| chat.target());
        shell.update_in(cx, |shell, window, cx| {
            shell.close_target_with(&target, true, window, cx)
        });
        // Retain a handle: authority ends at close, not at eventual entity drop.
        assert_eq!(
            dispatch_as(cx, &sender, create_in(&git.repo), Some(&target.session_uid)),
            Err("The calling GitTerm tab changed; request the delegation again".into())
        );
        assert!(
            TaskStore::load(fixture.source())
                .unwrap()
                .tasks()
                .is_empty()
        );
        owner.update(cx, |owner, _| owner.shutdown());
    }

    #[gpui::test]
    fn exited_chat_refuses_handoff(cx: &mut TestAppContext) {
        if child("exited_chat_refuses_handoff") {
            return;
        }
        cx.background_executor.allow_parking();
        let (fixture, git) = fixture();
        let (owner, sender) = start(&fixture, cx);
        let created = create(cx, &sender, &git.repo);
        let (shell, cx) = caller_shell(&fixture, &owner, cx);
        let chat = add_caller_chat(&shell, &owner, &fixture, cx);
        let caller = chat.read_with(cx, |chat, _| chat.target().session_uid);
        let before = std::fs::read(fixture.source()).unwrap();
        chat.update(cx, |chat, cx| {
            chat.receive_runtime_event(
                &chat.runtime_source(),
                gitterm_core::chat::runtime::RuntimeEvent::Harness(
                    gitterm_core::chat::types::HarnessEvent::ProcessExited { code: Some(0) },
                ),
                cx,
            )
        });
        assert_eq!(
            dispatch_as(
                cx,
                &sender,
                handoff(created["task_id"].as_str().unwrap(), "Stale"),
                Some(&caller)
            ),
            Err("The calling GitTerm tab changed; request the delegation again".into())
        );
        assert_eq!(std::fs::read(fixture.source()).unwrap(), before);
        owner.update(cx, |owner, _| owner.shutdown());
    }

    #[gpui::test]
    fn closing_one_chat_preserves_other_caller(cx: &mut TestAppContext) {
        if child("closing_one_chat_preserves_other_caller") {
            return;
        }
        cx.background_executor.allow_parking();
        let (fixture, git) = fixture();
        let (owner, sender) = start(&fixture, cx);
        let (shell, cx) = caller_shell(&fixture, &owner, cx);
        let first = add_caller_chat(&shell, &owner, &fixture, cx);
        let second = add_caller_chat(&shell, &owner, &fixture, cx);
        let target = first.read_with(cx, |chat, _| chat.target());
        let second = second.read_with(cx, |chat, _| chat.target().session_uid);
        shell.update_in(cx, |shell, window, cx| {
            shell.close_target_with(&target, true, window, cx)
        });
        assert!(dispatch_as(cx, &sender, create_in(&git.repo), Some(&target.session_uid)).is_err());
        let created = dispatch_as(cx, &sender, create_in(&git.repo), Some(&second)).unwrap();
        assert_eq!(
            dispatch_as(
                cx,
                &sender,
                handoff(created["task_id"].as_str().unwrap(), "Still open"),
                Some(&second)
            )
            .unwrap()["handoff"]["summary"],
            "Still open"
        );
        owner.update(cx, |owner, _| owner.shutdown());
    }

    #[gpui::test]
    fn chat_reset_and_workspace_stop_revoke_callers(cx: &mut TestAppContext) {
        if child("chat_reset_and_workspace_stop_revoke_callers") {
            return;
        }
        cx.background_executor.allow_parking();
        let (fixture, _) = fixture();
        let (owner, _) = start(&fixture, cx);
        let (shell, cx) = caller_shell(&fixture, &owner, cx);
        let chat = add_caller_chat(&shell, &owner, &fixture, cx);
        let caller = chat.read_with(cx, |chat, _| chat.target().session_uid);
        chat.update(cx, |chat, cx| {
            chat.receive_runtime_event(
                &chat.runtime_source(),
                gitterm_core::chat::runtime::RuntimeEvent::Harness(
                    gitterm_core::chat::types::HarnessEvent::ConversationReset { trigger: None },
                ),
                cx,
            )
        });
        owner.read_with(cx, |owner, _| {
            assert!(
                owner
                    .host()
                    .require_issued_caller(Some(&caller), "task_create")
                    .is_err()
            );
            // Explicit issuance, rather than restored metadata, grants authority again.
            owner.claude_mcp_servers(&caller);
            assert!(
                owner
                    .host()
                    .require_issued_caller(Some(&caller), "task_create")
                    .is_ok()
            );
        });
        shell.update_in(cx, |shell, window, cx| {
            let target = shell.active(cx).park_target(cx);
            shell.apply_park(
                gitterm_core::park::Effect::Park {
                    target,
                    mode: gitterm_core::park::Mode::Stop,
                    info: gitterm_core::park::Info::new("", String::new()),
                },
                window,
                cx,
            );
        });
        owner.read_with(cx, |owner, _| {
            assert!(
                owner
                    .host()
                    .require_issued_caller(Some(&caller), "task_create")
                    .is_err()
            );
        });
        owner.update(cx, |owner, _| owner.shutdown());
    }

    #[gpui::test]
    fn create_handoff_restart(cx: &mut TestAppContext) {
        if child("create_handoff_restart") {
            return;
        }
        cx.background_executor.allow_parking();
        let (fixture, git) = fixture();
        let (owner, sender) = start(&fixture, cx);
        let created = create(cx, &sender, &git.repo);
        let id = created["task_id"].as_str().unwrap();
        assert!(
            Path::new(created["worktree"]["path"].as_str().unwrap())
                .starts_with(fixture.iced.join("worktrees"))
        );
        let updated = dispatch(cx, &sender, handoff(id, "Resume here")).unwrap();
        assert_eq!(updated["handoff"]["summary"], "Resume here");
        owner.update(cx, |owner, _| owner.shutdown());
        // A saved process identity is history, even if its PID is currently alive.
        let mut core = TaskStore::load(fixture.source()).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        core.upsert_session(
            id,
            req(json!({
                "task_session_id": "imported-session", "label": "Saved terminal",
                "launch_identity": {"pid": std::process::id(), "birth": "placeholder"},
                "created_at": now, "updated_at": now
            })),
            &now,
        )
        .unwrap();
        let before = std::fs::read(fixture.source()).unwrap();
        let (restarted, sender) = start(&fixture, cx);
        let read = dispatch(
            cx,
            &sender,
            TaskControlOperation::Get(req(json!({"task_id": id}))),
        )
        .unwrap();
        assert_eq!(read["handoff"], updated["handoff"]);
        assert_eq!(read["task_id"], updated["task_id"]);
        assert_eq!(std::fs::read(fixture.source()).unwrap(), before);
        assert!(
            dispatch_as(
                cx,
                &sender,
                handoff(id, "Imported PID"),
                Some("imported-session")
            )
            .unwrap_err()
            .contains("calling GitTerm tab changed")
        );
        // Saved tasks are writable after admission; no launch/worker authority
        // is inferred from their history or from their former caller.
        dispatch(cx, &sender, handoff(id, "After restart")).unwrap();
        restarted.read_with(cx, |owner, _| {
            assert!(owner.host().launch_queue().is_empty());
            assert!(!owns_in_flight_work(owner.host()));
        });
        restarted.update(cx, |owner, _| owner.shutdown());
    }

    #[gpui::test]
    fn core_reads_live_file_and_custom_worktree_root(cx: &mut TestAppContext) {
        if child("core_reads_live_file_and_custom_worktree_root") {
            return;
        }
        cx.background_executor.allow_parking();
        let (fixture, git) = fixture();
        let custom = fixture.iced.parent().unwrap().join("custom-worktrees");
        std::fs::write(
            fixture.iced.join("settings.json"),
            serde_json::to_vec(&json!({"task_worktree_root": custom})).unwrap(),
        )
        .unwrap();
        let (owner, sender) = start(&fixture, cx);
        let created = create(cx, &sender, &git.repo);
        let core = TaskStore::load(TaskStore::path_for_config_root(&fixture.iced)).unwrap();
        let record = core.get(created["task_id"].as_str().unwrap()).unwrap();
        let mut expected = serde_json::to_value(record).unwrap();
        expected["open_sessions"] = json!([]);
        assert_eq!(created, expected);
        assert!(record.worktree.path.as_ref().unwrap().starts_with(&custom));
        assert!(
            super::git(&git.repo, &["worktree", "list"])
                .contains(record.worktree.path.as_ref().unwrap().to_str().unwrap())
        );
        owner.update(cx, |owner, _| owner.shutdown());
    }

    #[gpui::test]
    fn batch_uses_shared_create_with_ordered_partial_results(cx: &mut TestAppContext) {
        if child("batch_uses_shared_create_with_ordered_partial_results") {
            return;
        }
        cx.background_executor.allow_parking();
        let (fixture, git) = fixture();
        let (owner, _) = start(&fixture, cx);
        let servers = owner.read_with(cx, |owner, _| owner.claude_mcp_servers("caller"));
        let env = |key: &str| {
            servers[0]
                .env
                .iter()
                .find(|(name, _)| name == key)
                .unwrap()
                .1
                .clone()
        };
        let endpoint = env(gitterm_core::task_mcp::TASK_MCP_URL_ENV);
        let token = env(gitterm_core::task_mcp::TASK_MCP_TOKEN_ENV);
        let repo = git.repo.clone();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let client = McpClient::connect(&endpoint, &token);
            let tasks: Vec<_> = ["First", "", "Third"]
                .into_iter()
                .map(|title| {
                    json!({
                        "title": title, "objective": "Batch task", "repository_path": repo
                    })
                })
                .collect();
            send.send(client.call("task_create_batch", json!({"tasks": tasks})))
                .unwrap();
        });
        let result = pump_until(cx, &receive);
        let results = result["structuredContent"]["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        for (row, title, ok) in [(0, "First", true), (1, "", false), (2, "Third", true)] {
            assert_eq!(results[row]["title"], title);
            assert_eq!(results[row]["ok"], ok, "{result}");
        }
        let core = TaskStore::load(fixture.source()).unwrap();
        assert_eq!(core.tasks().len(), 2);
        assert_eq!(core.tasks()[0].title, "First");
        assert_eq!(core.tasks()[1].title, "Third");
        owner.update(cx, |owner, _| owner.shutdown());
    }

    #[gpui::test]
    fn forged_and_previous_host_callers_refused(cx: &mut TestAppContext) {
        if child("forged_and_previous_host_callers_refused") {
            return;
        }
        cx.background_executor.allow_parking();
        let (fixture, git) = fixture();
        let (owner, sender) = start(&fixture, cx);
        let created = create(cx, &sender, &git.repo);
        owner.read_with(cx, |owner, _| owner.claude_mcp_servers("former-caller"));
        let before = std::fs::read(fixture.source()).unwrap();
        for caller in [Some("forged"), None] {
            for operation in [
                create_in(&git.repo),
                handoff(created["task_id"].as_str().unwrap(), "Forged"),
            ] {
                let error = dispatch_as(cx, &sender, operation, caller).unwrap_err();
                assert!(!error.contains("not available in preview"), "{error}");
                assert!(
                    error.contains("calling GitTerm tab changed") || error.contains("caller"),
                    "{error}"
                );
            }
        }
        owner.update(cx, |owner, _| owner.shutdown());
        let (restarted, sender) = start(&fixture, cx);
        let error = dispatch_as(
            cx,
            &sender,
            handoff(created["task_id"].as_str().unwrap(), "Former host"),
            Some("former-caller"),
        )
        .unwrap_err();
        assert!(error.contains("calling GitTerm tab changed"), "{error}");
        assert_eq!(std::fs::read(fixture.source()).unwrap(), before);
        restarted.update(cx, |owner, _| owner.shutdown());
    }

    #[gpui::test]
    fn failed_save_preserves_file_and_memory(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt;
        if child("failed_save_preserves_file_and_memory") {
            return;
        }
        cx.background_executor.allow_parking();
        let (fixture, git) = fixture();
        let (owner, sender) = start(&fixture, cx);
        let created = create(cx, &sender, &git.repo);
        let id = created["task_id"].as_str().unwrap();
        dispatch(cx, &sender, handoff(id, "Last good handoff")).unwrap();
        let before = std::fs::read(fixture.source()).unwrap();
        let record = owner.read_with(cx, |owner, _| {
            owner
                .host()
                .store()
                .as_ref()
                .unwrap()
                .get(id)
                .unwrap()
                .clone()
        });
        let permissions = std::fs::metadata(&fixture.iced).unwrap().permissions();
        std::fs::set_permissions(&fixture.iced, std::fs::Permissions::from_mode(0o555)).unwrap();
        let failed_handoff = dispatch(cx, &sender, handoff(id, "Must not save"));
        let failed_create = dispatch(
            cx,
            &sender,
            TaskControlOperation::Create(req(json!({
                "title": "Cannot save", "objective": "Exercise the first store save", "repository_path": git.repo
            }))),
        );
        std::fs::set_permissions(&fixture.iced, permissions).unwrap();
        for result in [failed_handoff, failed_create] {
            let error = result.unwrap_err();
            assert!(error.contains("atomically write"), "{error}");
            assert!(error.contains("tasks.json"), "{error}");
        }
        assert_eq!(std::fs::read(fixture.source()).unwrap(), before);
        owner.read_with(cx, |owner, _| {
            let store = owner.host().store().as_ref().unwrap();
            assert_eq!(store.get(id).unwrap(), &record);
            assert_eq!(store.tasks().len(), 1);
        });
        dispatch(cx, &sender, handoff(id, "Writable again")).unwrap();
        owner.update(cx, |owner, _| owner.shutdown());
    }

    #[gpui::test]
    fn only_task_writes_open(cx: &mut TestAppContext) {
        use gitterm_core::preview::service::{RunnerEffect, preview_notice};
        if child("only_task_writes_open") {
            return;
        }
        cx.background_executor.allow_parking();
        let (fixture, git) = fixture();
        let (owner, sender) = start(&fixture, cx);
        let created = create(cx, &sender, &git.repo);
        let id = created["task_id"].as_str().unwrap();
        let before = std::fs::read(fixture.source()).unwrap();
        for (operation, label) in [
            (
                TaskControlOperation::LaunchSession(req(json!({"task_id": id}))),
                "task_launch_session",
            ),
            (
                TaskControlOperation::DelegateTask(req(
                    json!({"title": "Worker", "objective": "Do work", "repo": git.repo}),
                )),
                "delegate_task",
            ),
            (
                TaskControlOperation::RequestReview(req(
                    json!({"target": "uncommitted", "checkout": git.repo}),
                )),
                "review_request",
            ),
            (
                TaskControlOperation::RequestConsult(req(
                    json!({"brief": "Consult", "checkout": git.repo}),
                )),
                "consult_request",
            ),
            (
                TaskControlOperation::MergePlanSet(req(json!({"entries": []}))),
                "merge_plan_set",
            ),
        ] {
            assert_eq!(dispatch(cx, &sender, operation), Err(preview_notice(label)));
        }
        owner.read_with(cx, |owner, _| {
            for effect in [
                RunnerEffect::RestoreSession,
                RunnerEffect::AutomaticDelivery,
                RunnerEffect::QueuePromotion,
                RunnerEffect::NotifyIngress,
            ] {
                assert!(owner.host().services().runner(effect, None).is_err());
            }
            assert!(owner.host().launch_queue().is_empty());
            assert!(!owns_in_flight_work(owner.host()));
        });
        assert_eq!(std::fs::read(fixture.source()).unwrap(), before);
        owner.update(cx, |owner, _| owner.shutdown());
    }
}
