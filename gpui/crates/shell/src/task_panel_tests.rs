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
            iced_tasks: self.source(),
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
        dir: Some(DIR.into()),
        selected: None,
        general_sessions: 1,
        compact: false,
        open_sessions: open.iter().map(|id| target(id)).collect(),
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
        let model = rail(store, Some(Path::new(DIR)), RailFilter::All);
        let mut ids: Vec<_> = model.rows.iter().map(|t| t.task_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["seeded-a", "seeded-b"]);
        assert!(
            rail(store, Some(Path::new("/elsewhere")), RailFilter::All)
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
        let all = rail(store, Some(Path::new(DIR)), RailFilter::All);
        assert_eq!((all.active, all.needs_you), (2, 1));
        assert_eq!(all.rows[0].task_id, "input", "attention sorts first");
        let needs = rail(store, Some(Path::new(DIR)), RailFilter::NeedsYou);
        assert_eq!(needs.rows.len(), 1);
        let archived = rail(store, Some(Path::new(DIR)), RailFilter::Archived);
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
    assert_eq!(
        verbs(&running, true),
        [
            Verb::ReviewChanges,
            Verb::PushBranch,
            Verb::OpenDraftPr,
            Verb::Stop,
            Verb::Archive { enabled: false },
        ]
    );
    // Stop needs an open session of the task.
    assert!(!verbs(&running, false).contains(&Verb::Stop));
    assert!(!row_archivable(&running, false));

    let queued = task("queued", TaskLifecycle::Queued);
    assert_eq!(
        verbs(&queued, false),
        [Verb::CancelQueue, Verb::Archive { enabled: false }]
    );
    assert!(!delete_opener(&queued));

    let mut settled = with_worktree(task("settled", TaskLifecycle::Completed), path.clone());
    settled.delivery.pr_url = Some("https://example.invalid/pr/1".into());
    assert_eq!(
        verbs(&settled, false),
        [
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
    assert_eq!(verbs(&archived, false), [Verb::Archive { enabled: false }]);
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
