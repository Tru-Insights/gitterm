//! H5b B8a: the task rail, Overview verbs and modals on a preview task host
//! seeded from a temp stand-in for Iced's store. No real config, git or process.
use super::*;
use crate::task_host::{GpuiTaskHost, HostConfig, Ingress, TaskHostOwner, owns_in_flight_work};
use gitterm_core::{
    preview::{Preview, ProtectedRoots},
    task_commands::runtime::command_channel,
    tasks::{
        AttemptState, ExecutorTarget, TaskAttention, TaskAttentionReason, TaskExecutionAttempt,
        TaskStoreDocument,
    },
};
use gpui::{TestAppContext, VisualTestContext};
use std::{cell::RefCell, rc::Rc};

/// The fixture workspace directory `crate::tasks::fixture_task` records.
const DIR: &str = "/tmp/fixture";

struct Fixture {
    dir: tempfile::TempDir,
    roots: Arc<Preview>,
    iced: PathBuf,
}
use std::sync::Arc;
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let iced = base.join("iced");
        std::fs::create_dir(&iced).unwrap();
        let protected = ProtectedRoots::new(&base.join("fixture-home"), [iced.clone()]).unwrap();
        let roots = Arc::new(Preview::validate(&base.join("preview"), protected).unwrap());
        Self { dir, roots, iced }
    }
    fn source(&self) -> PathBuf {
        self.iced.join("tasks.json")
    }
    fn copy(&self) -> PathBuf {
        self.roots.root().join("tasks.json")
    }
    fn write_iced(&self, tasks: Vec<TaskRecord>) {
        let document = TaskStoreDocument {
            tasks,
            ..Default::default()
        };
        std::fs::write(self.source(), serde_json::to_vec(&document).unwrap()).unwrap();
    }
    /// A real directory for a Ready worktree, so it is not "missing".
    fn worktree(&self, name: &str) -> PathBuf {
        let path = self.dir.path().canonicalize().unwrap().join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }
    fn start(&self, cx: &mut TestAppContext) -> Entity<TaskHostOwner> {
        let (sender, receiver) = command_channel();
        let config = HostConfig {
            roots: self.roots.clone(),
            iced_config: self.iced.clone(),
            ingress: Ingress::Channel(sender, receiver),
        };
        let owner = cx.update(|cx| TaskHostOwner::start(config, cx)).unwrap();
        cx.update(|cx| cx.set_global(GpuiTaskHost(owner.clone())));
        owner
    }
}

fn task(id: &str, lifecycle: TaskLifecycle) -> TaskRecord {
    let mut task = crate::tasks::fixture_task(id, &format!("Task {id}"), &format!("task/{id}"));
    task.lifecycle = lifecycle;
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

fn with_worktree(mut task: TaskRecord, path: PathBuf) -> TaskRecord {
    task.worktree.state = TaskWorktreeState::Ready;
    task.worktree.path = Some(path);
    task
}

fn with_issue(mut task: TaskRecord) -> TaskRecord {
    task.issue = Some(
        serde_json::from_value(serde_json::json!({"provider": "linear", "key": "TRU-274"}))
            .unwrap(),
    );
    task
}

fn target(task_id: &str) -> Target {
    Target {
        tab_id: 7,
        session_uid: format!("uid-{task_id}"),
        task_id: Some(task_id.into()),
        task_session_id: Some(format!("session-{task_id}")),
    }
}

fn inputs(open: &[&str]) -> Inputs {
    Inputs {
        workspace_name: "Fixture".into(),
        location: Location::local(DIR),
        selected: None,
        general_sessions: 1,
        compact: false,
        open_sessions: open.iter().map(|id| target(id)).collect(),
        titles: Default::default(),
    }
}

/// Renders the three surfaces the shell mounts, for click-level checks.
struct Surfaces {
    panel: Entity<TaskPanel>,
    inputs: Rc<RefCell<Inputs>>,
}
impl gpui::Render for Surfaces {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        cx.observe(&self.panel, |_, _, cx| cx.notify()).detach();
        let inputs = self.inputs.borrow().clone();
        div()
            .size_full()
            .flex()
            .child(
                div()
                    .w(px(360.))
                    .child(rail_view(&self.panel, inputs.clone(), cx)),
            )
            .child(overview_view(&self.panel, inputs.clone(), cx))
            .children(modal_view(&self.panel, inputs, cx))
    }
}

type Outcomes = Rc<RefCell<Vec<TaskPanelOutcome>>>;

fn surfaces(
    cx: &mut TestAppContext,
    inputs: Inputs,
) -> (
    Entity<TaskPanel>,
    Rc<RefCell<Inputs>>,
    &mut VisualTestContext,
    Outcomes,
) {
    cx.update(gpui_component::init);
    let outcomes = Outcomes::default();
    let shared = Rc::new(RefCell::new(inputs));
    let mut panel = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|_| TaskPanel::default());
        let sink = outcomes.clone();
        cx.subscribe(&view, move |_, _, outcome: &TaskPanelOutcome, _| {
            sink.borrow_mut().push(outcome.clone())
        })
        .detach();
        panel = Some(view.clone());
        let root = cx.new(|_| Surfaces {
            panel: view,
            inputs: shared.clone(),
        });
        gpui_component::Root::new(root, window, cx)
    });
    draw(cx);
    (panel.unwrap(), shared, cx, outcomes)
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    cx.run_until_parked();
}

fn click(selector: &str, cx: &mut VisualTestContext) {
    draw(cx);
    let bounds = cx
        .debug_bounds(Box::leak(selector.to_owned().into_boxed_str()))
        .unwrap_or_else(|| panic!("{selector} is not visible"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw(cx);
}

fn visible(selector: &str, cx: &mut VisualTestContext) -> bool {
    draw(cx);
    cx.debug_bounds(Box::leak(selector.to_owned().into_boxed_str()))
        .is_some()
}

fn notice(panel: &Entity<TaskPanel>, cx: &mut VisualTestContext) -> Option<String> {
    panel.read_with(cx, |panel, _| panel.notice.clone())
}

#[gpui::test]
fn task_rows_from_host_snapshot(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    fixture.write_iced(vec![
        task("seeded-a", TaskLifecycle::Ready),
        task("seeded-b", TaskLifecycle::Completed),
    ]);
    let _owner = fixture.start(cx);
    // Iced's file changes after the seed; the rail keeps the host's copy
    // (the owner re-seeds only on its own poll).
    fixture.write_iced(vec![task("real-file-only", TaskLifecycle::Ready)]);
    cx.update(|cx| {
        let store = host(cx).unwrap().store().as_ref().unwrap();
        assert_eq!(store.path(), fixture.copy());
        let model = rail(store, &Location::local(DIR), RailFilter::All);
        let mut ids: Vec<_> = model.rows.iter().map(|t| t.task_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["seeded-a", "seeded-b"]);
        assert!(
            rail(store, &Location::local("/elsewhere"), RailFilter::All)
                .rows
                .is_empty()
        );
    });
    let (_, _, cx, outcomes) = surfaces(cx, inputs(&[]));
    assert!(visible("task-row-seeded-a", cx));
    assert!(!visible("task-row-real-file-only", cx));
    click("task-row-seeded-a", cx);
    assert_eq!(
        outcomes.borrow().as_slice(),
        [TaskPanelOutcome::Select("seeded-a".into())]
    );
}

#[gpui::test]
fn task_write_intents_show_notice(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let ready = fixture.worktree("ready");
    // A recorded worktree that is gone makes this task "finished" too.
    let failed = with_worktree(
        task("failed", TaskLifecycle::Failed),
        fixture.dir.path().join("gone"),
    );
    fixture.write_iced(vec![
        failed,
        with_issue(with_worktree(
            task("linked", TaskLifecycle::Ready),
            ready.clone(),
        )),
        with_worktree(task("unlinked", TaskLifecycle::Ready), ready.clone()),
        task("queued", TaskLifecycle::Queued),
        with_worktree(task("running", TaskLifecycle::Running), ready),
    ]);
    let owner = fixture.start(cx);
    let source = std::fs::read(fixture.source()).unwrap();
    let copy = std::fs::read(fixture.copy()).unwrap();
    let open = inputs(&["running"]);
    let (panel, _, cx, outcomes) = surfaces(cx, open.clone());
    let expect = |write: Write, label: &str, cx: &mut VisualTestContext| {
        panel.update(cx, |panel, cx| panel.write(write.clone(), &open, cx));
        assert_eq!(
            notice(&panel, cx).as_deref(),
            Some(format!("{label} · not available in preview").as_str()),
            "{write:?}"
        );
    };
    expect(Write::ArchiveFailed, "Close task", cx);
    expect(Write::ArchiveFinished, "Close task", cx);
    expect(Write::Archive("linked".into()), "Close task", cx);
    expect(
        Write::Publish {
            task_id: "linked".into(),
            action: TaskPublish::PushBranch,
        },
        "Publish",
        cx,
    );
    expect(Write::Stop("running".into()), "Close session", cx);
    expect(Write::CancelQueue("queued".into()), "Save task", cx);

    // A task without an issue asks first (a read), then publishes on confirm.
    panel.update(cx, |panel, cx| {
        panel.notice = None;
        panel.write(
            Write::Publish {
                task_id: "unlinked".into(),
                action: TaskPublish::OpenDraftPr,
            },
            &open,
            cx,
        )
    });
    assert!(notice(&panel, cx).is_none());
    assert!(visible("task-publish-cancel", cx));
    click("task-publish-confirm", cx);
    assert_eq!(
        notice(&panel, cx).as_deref(),
        Some("Publish · not available in preview")
    );
    click("task-publish-cancel", cx);
    assert!(!panel.read_with(cx, |panel, _| panel.modal_open()));

    // Delete: the inspection is refused, the confirm reaches the notice.
    panel.update(cx, |panel, cx| panel.open_delete("linked", &open, cx));
    panel.read_with(cx, |panel, _| match &panel.modal {
        Some(Modal::Delete {
            record_only,
            inspection,
            ..
        }) => {
            assert!(!record_only);
            assert_eq!(inspection, "Inspect worktree · not available in preview");
        }
        _ => panic!("delete confirmation should be open"),
    });
    click("task-delete-confirm", cx);
    assert_eq!(
        notice(&panel, cx).as_deref(),
        Some("Remove worktree · not available in preview")
    );
    click("task-delete-keep", cx);
    panel.update(cx, |panel, cx| panel.open_delete("failed", &open, cx));
    assert!(matches!(
        panel.read_with(cx, |panel, _| panel.modal.as_ref().map(|m| matches!(
            m,
            Modal::Delete {
                record_only: true,
                ..
            }
        ))),
        Some(true)
    ));
    draw(cx);
    let bounds = cx.debug_bounds("task-delete-confirm").unwrap();
    assert!(bounds.size.width > px(0.));
    click("task-delete-confirm", cx);
    assert_eq!(
        notice(&panel, cx).as_deref(),
        Some("Remove worktree · not available in preview")
    );
    click("task-delete-keep", cx);

    // Create: Iced's validation first, then the refusal.
    panel.update_in(cx, |panel, window, cx| panel.open_create(&open, window, cx));
    click("task-create-submit", cx);
    assert_eq!(
        panel.read_with(cx, |panel, _| match &panel.modal {
            Some(Modal::Create(form)) => form.error.clone(),
            _ => None,
        }),
        Some("Title is required".into())
    );
    panel.update_in(cx, |panel, window, cx| {
        let Some(Modal::Create(form)) = &panel.modal else {
            panic!("create form should be open")
        };
        let (title, objective) = (form.title.clone(), form.objective.clone());
        title.update(cx, |input, cx| input.set_value("Outcome", window, cx));
        objective.update(cx, |input, cx| input.set_value("Do the work", window, cx));
    });
    click("task-create-submit", cx);
    assert_eq!(
        notice(&panel, cx).as_deref(),
        Some("Create task · not available in preview")
    );
    click("task-create-cancel", cx);
    assert!(!panel.read_with(cx, |panel, _| panel.modal_open()));

    // Stop and close on a running task session's tab.
    panel.update(cx, |panel, cx| {
        panel.open_stop_and_close(target("running"), cx)
    });
    click("task-stop-and-close", cx);
    assert_eq!(
        notice(&panel, cx).as_deref(),
        Some("Close session · not available in preview")
    );
    click("task-close-keep", cx);
    assert!(!panel.read_with(cx, |panel, _| panel.modal_open()));

    // Iced's running-tab rule, from the host's record.
    cx.update(|_, cx| {
        let running = target("running");
        assert!(close_needs_confirm(&running, &running, false, cx));
        let settled = target("linked");
        assert!(!close_needs_confirm(&settled, &settled, false, cx));
    });

    // Nothing was written, queued or started.
    assert_eq!(std::fs::read(fixture.source()).unwrap(), source);
    assert_eq!(std::fs::read(fixture.copy()).unwrap(), copy);
    owner.read_with(cx, |owner, _| {
        assert!(!owns_in_flight_work(owner.host()));
        let store = owner.host().store().as_ref().unwrap();
        assert_eq!(
            store.get("queued").unwrap().lifecycle,
            TaskLifecycle::Queued
        );
        assert_eq!(
            store.get("failed").unwrap().lifecycle,
            TaskLifecycle::Failed
        );
    });
    assert!(
        outcomes
            .borrow()
            .iter()
            .all(|outcome| matches!(outcome, TaskPanelOutcome::Notice(_)))
    );
}

#[gpui::test]
fn task_filter_chips_match_iced(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let mut input = task("input", TaskLifecycle::Ready);
    input.attention = TaskAttention::raised(
        TaskAttentionReason::RequiresInput,
        &TaskAttention::default(),
        "2026-01-01T00:00:00Z",
    );
    let mut archived = task("archived", TaskLifecycle::Archived);
    archived.archived_at = Some("2026-01-02T00:00:00Z".into());
    let calm = task("calm", TaskLifecycle::Ready);
    fixture.write_iced(vec![input, archived, calm]);
    let _owner = fixture.start(cx);
    let (panel, _, cx, _) = surfaces(cx, inputs(&[]));
    cx.update(|_, cx| {
        let store = host(cx).unwrap().store().as_ref().unwrap();
        let all = rail(store, &Location::local(DIR), RailFilter::All);
        assert_eq!((all.active, all.needs_you), (2, 1));
        assert_eq!(all.rows[0].task_id, "input", "attention sorts first");
        let needs = rail(store, &Location::local(DIR), RailFilter::NeedsYou);
        assert_eq!(needs.rows.len(), 1);
        let archived = rail(store, &Location::local(DIR), RailFilter::Archived);
        assert_eq!(archived.rows[0].task_id, "archived");
        assert!(all.failed.is_empty() && all.finished.is_empty());
    });
    for chip in [
        "task-filter-all",
        "task-filter-needs-you",
        "task-filter-archived",
    ] {
        assert!(visible(chip, cx), "{chip}");
    }
    // Hidden at zero, as Iced.
    assert!(!visible("task-archive-failed", cx));
    assert!(!visible("task-archive-finished", cx));
    click("task-filter-needs-you", cx);
    assert_eq!(panel.read_with(cx, |p, _| p.filter), RailFilter::NeedsYou);
    assert!(visible("task-row-input", cx));
    assert!(!visible("task-row-calm", cx));
    click("task-filter-archived", cx);
    assert!(visible("task-row-archived", cx));
    // Archived rows offer no Archive.
    assert!(!visible("task-row-archive-archived", cx));
    click("task-filter-all", cx);
    assert!(visible("task-row-archive-calm", cx));
}

#[test]
fn task_controls_hidden_disabled_like_iced() {
    let path = PathBuf::from("/tmp/fixture-worktree");
    let running = with_worktree(task("running", TaskLifecycle::Running), path.clone());
    let start = |continuing| Verb::Start {
        continuing,
        enabled: true,
    };
    assert_eq!(
        verbs(&running, &inputs(&["running"])),
        [
            start(true),
            Verb::GoToSession("session-running".into()),
            Verb::ReviewChanges,
            Verb::PushBranch,
            Verb::OpenDraftPr,
            Verb::Stop,
            Verb::Archive { enabled: false },
        ]
    );
    // Stop needs an open session of the task.
    assert!(!verbs(&running, &inputs(&[])).contains(&Verb::Stop));
    assert!(!row_archivable(&running, false));

    let queued = task("queued", TaskLifecycle::Queued);
    let no_worktree = Verb::Start {
        continuing: false,
        enabled: false,
    };
    assert_eq!(
        verbs(&queued, &inputs(&[])),
        [
            no_worktree.clone(),
            Verb::CancelQueue,
            Verb::Archive { enabled: false }
        ]
    );
    assert!(!delete_opener(&queued));

    let mut settled = with_worktree(task("settled", TaskLifecycle::Completed), path.clone());
    settled.delivery.pr_url = Some("https://example.invalid/pr/1".into());
    assert_eq!(
        verbs(&settled, &inputs(&[])),
        [
            start(false),
            Verb::ReviewChanges,
            Verb::PushBranch,
            Verb::Archive { enabled: true },
        ]
    );
    assert!(row_archivable(&settled, false));
    assert!(
        !row_archivable(&settled, true),
        "compact rail hides Archive"
    );
    assert!(delete_opener(&settled));

    let archived = with_worktree(task("archived", TaskLifecycle::Archived), path);
    assert_eq!(
        verbs(&archived, &inputs(&[])),
        [no_worktree, Verb::Archive { enabled: false }]
    );
    assert!(!row_archivable(&archived, false));
    assert!(delete_opener(&archived));

    // A running task's tab asks before closing; a settled one does not.
    let current = target("running");
    let state = |task_live| session::State {
        target: current.clone(),
        control: session::TurnControl::Terminal,
        task_live,
        last_tab: false,
    };
    let close = session::Intent::Close {
        target: current.clone(),
        confirmed: false,
    };
    assert!(matches!(
        state(true).reduce(close.clone()),
        session::Effect::ConfirmClose(_)
    ));
    assert!(matches!(
        state(false).reduce(close),
        session::Effect::Close(_)
    ));
}

/// The h7 shell over an isolated Iced fixture, its preview task host seeded
/// with `tasks` (given the shell workspace's directory).
fn shell_with_tasks(
    cx: &mut TestAppContext,
    tasks: impl FnOnce(&Path) -> Vec<TaskRecord>,
) -> (
    Fixture,
    tempfile::TempDir,
    Entity<crate::Shell>,
    Entity<TaskPanel>,
    &mut VisualTestContext,
) {
    let (root, shell, cx) = crate::h7_tests::fixture(cx);
    let fixture = Fixture::new();
    fixture.write_iced(tasks(&root.path().join("work")));
    fixture.start(cx);
    let panel =
        cx.update(|window, cx| shell.update(cx, |shell, cx| shell.ensure_task_panel(window, cx)));
    draw(cx);
    (fixture, root, shell, panel, cx)
}

fn in_dir(mut task: TaskRecord, dir: &Path) -> TaskRecord {
    task.workspace.location = WorkspaceLocationIdentity::Local {
        directory: dir.to_owned(),
    };
    task
}

fn run_tabs(shell: &Entity<crate::Shell>, cx: &mut VisualTestContext) -> usize {
    cx.read(|cx| shell.read(cx).bottom.read(cx).input.tabs.len())
}

/// F1: Iced consumes background commands while a task prompt is up.
#[gpui::test]
fn task_modal_blocks_background_shortcuts(cx: &mut TestAppContext) {
    let (_fixture, _root, shell, panel, cx) = shell_with_tasks(cx, |_| vec![]);
    panel.update(cx, |panel, cx| {
        panel.open_stop_and_close(target("running"), cx)
    });
    draw(cx);
    cx.update(|window, cx| {
        let context = shell.update(cx, |shell, cx| shell.menu_context(window, cx));
        assert!(!context.contains("IcedMain"), "{context}");
    });
    cx.simulate_keystrokes("cmd-shift-r");
    draw(cx);
    assert_eq!(run_tabs(&shell, cx), 0, "the run shortcut reached run_repo");
    // Dismissed, the same chord runs again (it is the modal that blocks it).
    panel.update(cx, |panel, cx| panel.dismiss(cx));
    draw(cx);
    cx.update(|window, cx| {
        let context = shell.update(cx, |shell, cx| shell.menu_context(window, cx));
        assert!(context.contains("IcedMain"), "{context}");
    });
    cx.simulate_keystrokes("cmd-shift-r");
    draw(cx);
    assert_eq!(run_tabs(&shell, cx), 1);
}

/// F2: an archived row selects its task, from the host snapshot.
#[gpui::test]
fn archived_rail_row_selects_task(cx: &mut TestAppContext) {
    let (_fixture, _root, shell, _panel, cx) = shell_with_tasks(cx, |work| {
        let mut archived = in_dir(task("archived", TaskLifecycle::Archived), work);
        archived.archived_at = Some("2026-01-02T00:00:00Z".into());
        vec![archived]
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.activate_rail_task("archived", window, cx)
        })
    });
    draw(cx);
    cx.read(|cx| assert_eq!(shell.read(cx).selected_task.as_deref(), Some("archived")));
    // A task outside the workspace's rail is not selected.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.activate_rail_task("absent", window, cx)
        })
    });
    cx.read(|cx| assert_eq!(shell.read(cx).selected_task.as_deref(), Some("archived")));
}

/// F3: a remote workspace's tasks match on remote id and root, as Iced.
#[test]
fn remote_workspace_rail_matches_remote_and_root() {
    let remote = |id: &str, remote_id: &str, root: &str| {
        let mut task = task(id, TaskLifecycle::Ready);
        task.workspace.location = WorkspaceLocationIdentity::RemoteAgent {
            remote_id: remote_id.into(),
            root: root.into(),
        };
        task
    };
    let location = Location::Remote {
        remote_id: "agent-1".into(),
        root: "/srv/repo".into(),
    };
    assert!(in_workspace(
        &remote("a", "agent-1", "/srv/repo"),
        &location
    ));
    assert!(!in_workspace(
        &remote("b", "agent-2", "/srv/repo"),
        &location
    ));
    assert!(!in_workspace(
        &remote("c", "agent-1", "/srv/other"),
        &location
    ));
    assert!(!in_workspace(
        &task("local", TaskLifecycle::Ready),
        &location
    ));
    // A local workspace matches its root as well as its directory.
    let local = Location::Local {
        dir: "/tmp/elsewhere".into(),
        root: DIR.into(),
    };
    assert!(in_workspace(&task("local", TaskLifecycle::Ready), &local));
    assert!(!in_workspace(&remote("a", "agent-1", "/srv/repo"), &local));
}

/// F4: Escape closes each task modal before background navigation and
/// gives focus back to the shell.
#[gpui::test]
fn escape_dismisses_task_modals(cx: &mut TestAppContext) {
    let (_fixture, root, shell, panel, cx) = shell_with_tasks(cx, |work| {
        vec![with_worktree(
            in_dir(task("ready", TaskLifecycle::Ready), work),
            work.to_owned(),
        )]
    });
    let shell_focus = cx.read(|cx| shell.read(cx).focus.clone());
    let work = root.path().join("work");
    let open = Inputs {
        location: Location::local(&work),
        ..inputs(&[])
    };
    type Open<'a> = &'a dyn Fn(&mut TaskPanel, &mut Window, &mut Context<TaskPanel>);
    let opens: [Open; 4] = [
        &|panel, window, cx| panel.open_create(&open, window, cx),
        &|panel, _, cx| panel.open_stop_and_close(target("ready"), cx),
        &|panel, _, cx| panel.open_delete("ready", &open, cx),
        &|panel, _, cx| {
            panel.modal = Some(Modal::Publish {
                task_id: "ready".into(),
                action: TaskPublish::PushBranch,
                error: None,
            });
            cx.notify();
        },
    ];
    for (index, open_modal) in opens.iter().enumerate() {
        panel.update_in(cx, |panel, window, cx| open_modal(panel, window, cx));
        draw(cx);
        assert!(
            panel.read_with(cx, |panel, _| panel.modal_open()),
            "modal {index}"
        );
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert!(
            !panel.read_with(cx, |panel, _| panel.modal_open()),
            "modal {index}"
        );
        cx.update(|window, _| assert!(shell_focus.is_focused(window), "modal {index}"));
    }
}

/// F5: the create form goes through the core reducer's validation.
#[gpui::test]
fn create_uses_core_validation(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    fixture.write_iced(vec![]);
    let _owner = fixture.start(cx);
    let open = inputs(&[]);
    let (panel, _, cx, _) = surfaces(cx, open.clone());
    let form_error = |cx: &mut VisualTestContext| {
        panel.read_with(cx, |panel, _| match &panel.modal {
            Some(Modal::Create(form)) => form.error.clone(),
            _ => None,
        })
    };
    let fill = |issue: &str, custom_base: Option<&str>, cx: &mut VisualTestContext| {
        panel.update_in(cx, |panel, window, cx| {
            let Some(Modal::Create(form)) = &mut panel.modal else {
                panic!("create form should be open")
            };
            // As a pick in the base picker does (Iced `base_touched`).
            form.base_custom = custom_base.is_some();
            form.base_touched = true;
            let fields = [
                (form.title.clone(), "Outcome"),
                (form.objective.clone(), "Do the work"),
                (form.issue.clone(), issue),
                (form.custom_base.clone(), custom_base.unwrap_or_default()),
            ];
            for (field, value) in fields {
                field.update(cx, |input, cx| input.set_value(value, window, cx));
            }
        });
    };
    panel.update_in(cx, |panel, window, cx| panel.open_create(&open, window, cx));
    fill("", Some("  "), cx);
    click("task-create-submit", cx);
    assert_eq!(
        form_error(cx).as_deref(),
        Some("Base reference is required")
    );
    assert!(
        notice(&panel, cx).is_none(),
        "no preview notice before validation"
    );
    fill("tru-105", None, cx);
    click("task-create-submit", cx);
    assert_eq!(
        form_error(cx).as_deref(),
        Some("Linear issue must look like TRU-105")
    );
    assert!(notice(&panel, cx).is_none());
    fill("TRU-105", Some(" origin/main "), cx);
    click("task-create-submit", cx);
    assert_eq!(
        notice(&panel, cx).as_deref(),
        Some("Create task · not available in preview")
    );
}

/// F6: a Ready task with an open session reads Iced's session copy.
#[test]
fn rail_state_label_reads_open_sessions() {
    use gitterm_core::tasks::{ObjectiveDeliveryState, TaskSessionRecord};
    let mut ready = task("ready", TaskLifecycle::Ready);
    let session = |delivery| TaskSessionRecord {
        task_session_id: "session-ready".into(),
        label: "Claude".into(),
        harness: None,
        conversation: None,
        objective_delivery: delivery,
        launch_identity: None,
        created_at: ready.created_at.clone(),
        updated_at: ready.created_at.clone(),
    };
    assert_eq!(
        state_line(&ready, &inputs(&["ready"])),
        "Ready · not started"
    );
    for (delivery, expected) in [
        (
            ObjectiveDeliveryState::NotDelivered,
            "Session open · no brief",
        ),
        (ObjectiveDeliveryState::Delivered, "Session open · briefed"),
        (
            ObjectiveDeliveryState::Unknown,
            "Session open · delivery unknown",
        ),
    ] {
        ready.sessions = vec![session(delivery)];
        assert_eq!(state_line(&ready, &inputs(&["ready"])), expected);
        // Another task's open session does not count.
        assert_eq!(
            state_line(&ready, &inputs(&["other"])),
            "Ready · not started"
        );
    }
}

/// T09c: Iced `task_base_choices`' own cases.
#[test]
fn base_choices_prefer_develop_then_main_then_current() {
    let branch = |name: &str, current| (name.to_owned(), current);
    let (choices, selected) = base_choices(&[
        branch("feature/zeta", false),
        branch("main", true),
        branch("develop", false),
    ]);
    assert_eq!(selected, "develop");
    assert_eq!(
        choices,
        ["develop", "main", "feature/zeta", "HEAD", CUSTOM_BASE]
    );
    let (_, selected) = base_choices(&[branch("main", false), branch("release", true)]);
    assert_eq!(selected, "main");
    let (choices, selected) = base_choices(&[branch("release", true), branch("Alpha", false)]);
    assert_eq!(selected, "release");
    assert_eq!(choices, ["release", "Alpha", "HEAD", CUSTOM_BASE]);
    let (choices, selected) = base_choices(&[]);
    assert_eq!(selected, "HEAD");
    assert_eq!(choices, ["HEAD", CUSTOM_BASE]);
}

fn git(dir: &Path, args: &[&str]) {
    let status = gitterm_core::git_status::git_command()
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// T09c: the form reads the repository's local branches (a git read) and
/// selects Iced's preferred base; a failed read keeps `HEAD`.
#[gpui::test]
fn create_form_lists_local_branches(cx: &mut TestAppContext) {
    cx.background_executor.allow_parking();
    let fixture = Fixture::new();
    fixture.write_iced(vec![]);
    fixture.start(cx);
    let repo = fixture.worktree("repo");
    git(&repo, &["init", "-q", "-b", "feature"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "fixture"]);
    for name in ["main", "develop", "zeta"] {
        git(&repo, &["branch", name]);
    }
    let refs_before = std::fs::read_dir(repo.join(".git/refs/heads"))
        .unwrap()
        .count();
    let (panel, _, cx, _) = surfaces(cx, inputs(&[]));
    let base = |panel: &Entity<TaskPanel>, cx: &mut VisualTestContext| {
        panel.read_with(cx, |panel, cx| match &panel.modal {
            Some(Modal::Create(form)) => (
                form.branches_loading,
                form.base.read(cx).selected_value().cloned(),
            ),
            _ => panic!("create form should be open"),
        })
    };
    let open = Inputs {
        location: Location::local(&repo),
        ..inputs(&[])
    };
    panel.update_in(cx, |panel, window, cx| panel.open_create(&open, window, cx));
    draw(cx);
    assert_eq!(base(&panel, cx), (false, Some("develop".to_owned())));
    assert_eq!(
        std::fs::read_dir(repo.join(".git/refs/heads"))
            .unwrap()
            .count(),
        refs_before
    );
    // Not a repository: Iced's pre-load choices stay.
    let plain = fixture.worktree("plain");
    let open = Inputs {
        location: Location::local(&plain),
        ..inputs(&[])
    };
    panel.update_in(cx, |panel, window, cx| panel.open_create(&open, window, cx));
    draw(cx);
    assert_eq!(base(&panel, cx), (false, Some("HEAD".to_owned())));
}

fn with_conversation(mut task: TaskRecord) -> TaskRecord {
    task.sessions.push(
        serde_json::from_value(serde_json::json!({
            "task_session_id": "worker-1",
            "label": "Codex",
            "conversation": {"backend": "codex", "session_id": "conversation-1"},
            "objective_delivery": "unknown",
            "created_at": task.created_at,
            "updated_at": task.updated_at,
        }))
        .unwrap(),
    );
    task
}

/// T03/T03a/T04: Start with / Resume / Go to session. Opening the picker and
/// focusing a session are reads; launches stop at the preview notice.
#[gpui::test]
fn start_resume_and_go_to_session_like_iced(cx: &mut TestAppContext) {
    let fixture = Fixture::new();
    let ready = fixture.worktree("ready");
    fixture.write_iced(vec![
        with_conversation(with_worktree(
            task("stopped", TaskLifecycle::Stopped),
            ready.clone(),
        )),
        with_worktree(task("fresh", TaskLifecycle::Interrupted), ready.clone()),
        with_worktree(task("running", TaskLifecycle::Running), ready),
    ]);
    fixture.start(cx);
    let source = std::fs::read(fixture.source()).unwrap();
    let copy = std::fs::read(fixture.copy()).unwrap();
    let open = inputs(&["running"]);
    let (panel, shared, cx, outcomes) = surfaces(cx, open.clone());
    let select = |id: &str, cx: &mut VisualTestContext| {
        shared.borrow_mut().selected = Some(id.into());
        panel.update(cx, |panel, cx| {
            panel.notice = None;
            cx.notify()
        });
        outcomes.borrow_mut().clear();
        draw(cx);
    };
    let launch = Some("Launch session · not available in preview");

    select("stopped", cx);
    assert!(visible("task-details", cx) && visible("task-sessions", cx));
    assert!(!visible("task-go-to-session", cx), "no open session");
    click("task-start", cx);
    assert_eq!(
        outcomes.borrow().as_slice(),
        [TaskPanelOutcome::Pick("stopped".into())]
    );
    // A recorded conversation resumes: a launch.
    click("task-resume", cx);
    assert_eq!(notice(&panel, cx).as_deref(), launch);

    // No conversation: Iced's Resume falls back to the picker.
    select("fresh", cx);
    click("task-resume", cx);
    assert_eq!(
        outcomes.borrow().as_slice(),
        [TaskPanelOutcome::Pick("fresh".into())]
    );
    assert!(notice(&panel, cx).is_none());
    // The picker's choice is a launch.
    panel.update(cx, |panel, cx| {
        panel.write(
            Write::Launch {
                task_id: "fresh".into(),
                preset: None,
            },
            &open,
            cx,
        )
    });
    assert_eq!(notice(&panel, cx).as_deref(), launch);

    select("running", cx);
    assert!(!visible("task-resume", cx));
    click("task-go-to-session", cx);
    assert_eq!(
        outcomes.borrow().as_slice(),
        [TaskPanelOutcome::FocusSession("session-running".into())]
    );
    outcomes.borrow_mut().clear();
    click("task-session-0", cx);
    assert_eq!(
        outcomes.borrow().as_slice(),
        [TaskPanelOutcome::FocusSession("session-running".into())]
    );
    assert_eq!(std::fs::read(fixture.source()).unwrap(), source);
    assert_eq!(std::fs::read(fixture.copy()).unwrap(), copy);
}

/// The Overview's detail rows, as Iced's `view_task_detail` lists them.
#[test]
fn overview_detail_rows_match_iced() {
    let mut settled = with_issue(with_worktree(
        task("settled", TaskLifecycle::Completed),
        PathBuf::from("/tmp/fixture-worktree"),
    ));
    settled.delivery.commit_sha = Some("0123456789abcdef".into());
    settled.last_error = Some("fixture error".into());
    let rows = detail_rows(&settled, &inputs(&[]));
    let labels: Vec<_> = rows.iter().map(|(label, _)| *label).collect();
    assert_eq!(
        labels,
        [
            "STATE",
            "DELIVERY",
            "ISSUE",
            "BASE",
            "BRANCH",
            "WORKTREE",
            "CREATED BY",
            "STOP WHEN",
            "LAST ERROR",
        ]
    );
    let value = |label| rows.iter().find(|(l, _)| *l == label).unwrap().1.clone();
    assert_eq!(value("DELIVERY"), "0123456789 · no PR");
    assert_eq!(value("ISSUE"), "TRU-274 ↗");
    assert_eq!(value("BRANCH"), "task/settled");
    assert_eq!(value("WORKTREE"), "/tmp/fixture-worktree");
    assert_eq!(value("STOP WHEN"), "Tests pass");

    // An active task adds Iced's live rows, from the stored snapshot.
    let running = task("running", TaskLifecycle::Running);
    let rows = detail_rows(&running, &inputs(&[]));
    let value = |label| rows.iter().find(|(l, _)| *l == label).map(|r| r.1.clone());
    assert_eq!(value("WORKTREE").as_deref(), Some("Not prepared"));
    assert_eq!(value("PHASE").as_deref(), Some("Working"));
    assert_eq!(value("LAST ACTIVITY").as_deref(), Some("Not observed yet"));
    assert_eq!(value("CHANGED FILES").as_deref(), Some("Not checked yet"));
    assert!(value("ELAPSED").is_some());
    assert!(value("VERIFICATION").is_some());
}

/// T04: Go to session focuses the open tab carrying the task session; the
/// picker's Continue is a launch and stops at the notice.
#[gpui::test]
fn go_to_session_focuses_the_open_tab(cx: &mut TestAppContext) {
    let (_fixture, _root, shell, panel, cx) = shell_with_tasks(cx, |work| {
        vec![with_worktree(
            in_dir(task("ready", TaskLifecycle::Ready), work),
            work.to_owned(),
        )]
    });
    let id = cx.update(|_, cx| {
        let state = shell.read(cx).active(cx);
        let id = state.sessions[0].id;
        let pane = state.panes[&crate::session_pane_id(id)].clone();
        pane.update(cx, |pane, _| {
            pane.session = Some(Target {
                task_id: Some("ready".into()),
                task_session_id: Some("session-ready".into()),
                ..target("ready")
            })
        });
        id
    });
    shell.update(cx, |shell, _| shell.selected_task = None);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.focus_task_session("session-ready", window, cx)
        })
    });
    draw(cx);
    cx.read(|cx| {
        let shell = shell.read(cx);
        assert_eq!(shell.active(cx).active_session, Some(id));
        assert_eq!(shell.selected_task.as_deref(), Some("ready"));
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.focus_task_session("gone-1234567", window, cx)
        })
    });
    assert_eq!(
        notice(&panel, cx).as_deref(),
        Some("Task session gone-123 is no longer open")
    );
    // Start with opens the task's session picker; its Terminal row launches.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.open_task_picker("ready", window, cx))
    });
    draw(cx);
    assert!(cx.read(|cx| shell.read(cx).session_picker.is_some()));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.select_picker_outcome(
                &crate::session_picker::SessionPickerOutcome::Continue {
                    task_id: "ready".into(),
                    preset: None,
                },
                "Terminal",
                window,
                cx,
            )
        })
    });
    assert_eq!(
        notice(&panel, cx).as_deref(),
        Some("Launch session · not available in preview")
    );
}
