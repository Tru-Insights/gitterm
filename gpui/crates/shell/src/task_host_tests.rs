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
    token: String,
    session: Option<String>,
}
impl McpClient {
    fn connect(endpoint: &str, token: &str) -> Self {
        let address = endpoint
            .strip_prefix("http://")
            .and_then(|rest| rest.strip_suffix("/mcp"))
            .unwrap()
            .to_string();
        let mut client = Self {
            address,
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
            "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    let endpoint = owner.read_with(&app, |owner, _| owner.task_endpoint().unwrap());
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
    let address = endpoint
        .strip_prefix("http://")
        .and_then(|rest| rest.strip_suffix("/mcp"))
        .unwrap();
    assert!(std::net::TcpStream::connect(address).is_err());
    HostLock::acquire(&fixture.roots).unwrap();
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
    let error = HostLock::acquire(&fixture.roots).err().unwrap().to_string();
    assert!(error.contains("already hosts tasks"), "{error}");
    // A second host fails before seeding or binding anything.
    fixture.write_iced(vec![task("changed", TaskLifecycle::Ready)], vec![]);
    let (config, _) = fixture.config();
    let error = cx
        .update(|cx| TaskHostOwner::start(config, cx))
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("already hosts tasks"), "{error}");
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
    let _lock = HostLock::acquire(&roots).unwrap();
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
    assert!(HostLock::acquire(&fixture.roots).is_err());
    // SIGKILL: no destructor runs, only the kernel releases the lock.
    child.kill().unwrap();
    child.wait().unwrap();
    HostLock::acquire(&fixture.roots).unwrap();
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
        let saved = crate::chat::SavedChat {
            uid: "saved".into(),
            cwd: fixture.iced.parent().unwrap().join("work"),
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
