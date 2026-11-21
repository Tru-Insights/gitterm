//! TRU-274: starting a worker gathers its authority facts on Iced's UI
//! thread. A store of real Git worktrees and a window of real checkout
//! tabs, timed through the same fact gathering a worker launch runs.
use super::task_authority::{fixture_worker_checkout, pending_worker_fixture};
use super::*;
use std::time::Duration;

const WORKTREES: usize = 40;
const TABS: usize = 20;
const RUNS: usize = 5;

/// The median of `RUNS` timed calls, and the last call's result.
fn timed<T>(mut run: impl FnMut() -> T) -> (Duration, T) {
    let mut times = Vec::new();
    let mut last = None;
    for _ in 0..RUNS {
        let start = Instant::now();
        last = Some(run());
        times.push(start.elapsed());
    }
    times.sort();
    (times[RUNS / 2], last.unwrap())
}

fn ms(duration: Duration) -> String {
    format!("{:.2}ms", duration.as_secs_f64() * 1000.0)
}

/// `WORKTREES` Ready tasks, each in its own worktree of the caller's
/// repository, and `TABS` open tabs (the caller first) in those checkouts.
fn add_worktrees_and_tabs(app: &mut App, dir: &Path) -> Vec<PathBuf> {
    let source = app.workspaces[0].dir.clone();
    let template = app
        .task_host
        .store()
        .as_ref()
        .unwrap()
        .get("fixture-task")
        .unwrap()
        .clone();
    let mut paths = Vec::new();
    for index in 0..WORKTREES {
        let name = format!("bench-{index}");
        let path = dir.join("worktrees").join(&name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        fixture_worker_checkout(&source, &path, &name);
        let path = std::fs::canonicalize(path).unwrap();
        let mut task = template.clone();
        task.task_id = name.clone();
        task.branch = name;
        task.worktree.path = Some(path.clone());
        task.worktree.state = TaskWorktreeState::Ready;
        app.task_host
            .store_mut()
            .as_mut()
            .unwrap()
            .insert(task)
            .unwrap();
        paths.push(path);
    }
    for (index, path) in paths.iter().take(TABS - 1).enumerate() {
        let tab = TabState::new(1000 + index, path.clone());
        app.workspaces[0].tabs.push(tab);
    }
    assert_eq!(
        app.workspaces.iter().map(|ws| ws.tabs.len()).sum::<usize>(),
        TABS + 2,
        "the caller's workspace holds TABS tabs; two fixture workspaces hold one each"
    );
    paths
}

#[test]
fn worker_start_facts_gathering_benchmark() {
    if isolated("worker_start_facts_gathering_benchmark", "worker_facts") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_tabs(dir.path());
    add_task(&mut app, dir.path(), dir.path());
    let (pending, prepared, response, _receiver) = pending_worker_fixture(&mut app, dir.path());
    add_worktrees_and_tabs(&mut app, dir.path());
    let worker = pending.worker.clone().unwrap();
    let id = worker.authority;
    let uid = worker.caller.session_uid.clone();
    let caller = app.worker_authority_caller(&uid).unwrap();

    // The completion: WorkerPrepared, reduced with the completion facts.
    let completion = TaskMutation::WorkerPrepared {
        id,
        result: Ok(prepared.clone().into()),
    };
    let (completion_time, facts) = timed(|| {
        app.worker_completion_facts(&completion, pending.resolved_worktree_path.as_deref())
    });
    assert!(facts.callers.contains(&caller));
    let effect = app
        .task_host
        .reduce_mutation(
            TaskMutationCommand {
                reply: None,
                mutation: completion,
            },
            &facts,
        )
        .action;
    let Ok(TaskMutationAction::PersistWorkerPreparation {
        task_id, prepared, ..
    }) = effect
    else {
        panic!("completion: {effect:?}")
    };

    // The store write the completion asks for.
    let store = app.task_host.store_mut().as_mut().unwrap();
    let start = Instant::now();
    store
        .complete_worktree_preparation(&task_id, prepared.clone(), "2000-01-02T00:00:00Z")
        .unwrap();
    let complete_write = start.elapsed();
    let tasks = store.tasks().len();

    // WorkerPreparationSaved, reduced with reduce_task_mutation's facts.
    let saved = TaskMutation::WorkerPreparationSaved { id, result: Ok(()) };
    let (saved_time, facts) = timed(|| app.mutation_facts(&saved));
    assert!(facts.callers.contains(&caller));
    assert!(facts.checkouts.iter().any(|checkout| {
        checkout.path == prepared.path && checkout.repository == prepared.repository
    }));
    let effect = app.reduce_task_mutation(saved).unwrap();
    assert!(
        matches!(&effect, TaskMutationAction::StartWorker { task_id: started, .. } if started == &task_id),
        "{effect:?}"
    );

    // The worker start: one delegation write, then the launch (the fixture's
    // preset is unavailable, so it records the failure: a second write).
    let start = Instant::now();
    let _task = app.start_delegated_worker(effect, worker, pending.control_reply.clone().unwrap());
    let start_writes = start.elapsed();
    let error = iced::futures::executor::block_on(response).unwrap_err();
    assert!(
        error.contains("selected agent preset is unavailable"),
        "{error}"
    );
    let store = app.task_host.store().as_ref().unwrap();
    assert_eq!(store.get(&task_id).unwrap().lifecycle, TaskLifecycle::Ready);
    assert_eq!(store.delegations().len(), 1);

    // One more whole-store rewrite, the size of every write above.
    let mut task = store.get(&task_id).unwrap().clone();
    task.objective = "rewritten".into();
    let store = app.task_host.store_mut().as_mut().unwrap();
    let (replace_write, ()) = timed(|| store.replace(task.clone()).unwrap());

    println!(
        "[TRU-274 benchmark] {WORKTREES} worktrees, {TABS} tabs, {tasks} tasks, median of {RUNS}:\n\
         completion facts (WorkerPrepared): {}\n\
         saved facts (WorkerPreparationSaved): {}\n\
         store complete_worktree_preparation: {}\n\
         start_delegated_worker (delegation insert + failure status): {}\n\
         store replace (one whole-store rewrite): {}",
        ms(completion_time),
        ms(saved_time),
        ms(complete_write),
        ms(start_writes),
        ms(replace_write),
    );
}
