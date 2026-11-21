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
use gpui::{Entity, TestAppContext};
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
                iced_tasks: self.source(),
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
    let pending = sender.enqueue(operation, Some("caller".into())).unwrap();
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
fn every_b4_effect_answers_the_preview_notice(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![task("queued", TaskLifecycle::Queued)], vec![]);
    let (_owner, sender) = fixture.start(cx);
    for (operation, label) in [
        (TaskControlOperation::PlanLimits, "plan_limits"),
        (TaskControlOperation::MergePlanGet, "merge_plan_get"),
    ] {
        assert_eq!(
            dispatch(cx, &sender, operation),
            Err(format!("{label} · not available in preview"))
        );
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
