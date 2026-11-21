//! TRU-274: starting a worker gathers its authority facts on Iced's UI
//! thread. A store of real Git worktrees and a window of real checkout
//! tabs, timed through the same fact gathering a worker launch runs, and
//! the targeted gather checked against the full one it replaced.
use super::task_authority::{
    fixture_worker_checkout, fixture_worker_source, pending_worker_fixture,
};
use super::*;
use gitterm::tasks::state::TaskAuthority;
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

/// The gather before TRU-274: a caller for every open tab.
fn full_callers(app: &App) -> Vec<WorkerCaller> {
    app.workspaces
        .iter()
        .flat_map(|ws| &ws.tabs)
        .filter_map(|tab| app.worker_authority_caller(&tab.session_uid).ok())
        .collect()
}

/// The gather before TRU-274: a checkout fact for every task's worktree.
fn full_checkouts(app: &App) -> Vec<WorkerCheckout> {
    app.task_host
        .store()
        .as_ref()
        .map(|store| {
            store
                .tasks()
                .iter()
                .filter_map(|task| task.worktree.path.as_ref())
                .filter_map(|path| worker_checkout_fact(path).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn is_worker_mutation(mutation: &TaskMutation) -> bool {
    matches!(
        mutation,
        TaskMutation::BeginWorkerPreparation { .. }
            | TaskMutation::CheckWorkerPreparation { .. }
            | TaskMutation::BindWorkerPreparation { .. }
            | TaskMutation::WorkerPrepared { .. }
            | TaskMutation::WorkerPreparationSaved { .. }
            | TaskMutation::ReuseWorkerCheckout { .. }
            | TaskMutation::WorkerCheckoutReuseSaved { .. }
    )
}

/// `reduce_task_mutation`'s facts as they were gathered before TRU-274.
fn full_mutation_facts(app: &App, mutation: &TaskMutation) -> TaskMutationFacts {
    let mut facts = app.mutation_facts(mutation);
    if is_worker_mutation(mutation) {
        facts.callers = full_callers(app);
    }
    if matches!(
        mutation,
        TaskMutation::WorkerPreparationSaved { .. } | TaskMutation::WorkerCheckoutReuseSaved { .. }
    ) && app.task_host.store().is_some()
    {
        // Saves have no destination of their own; the old gather was all of it.
        facts.checkouts = full_checkouts(app);
    }
    facts
}

/// The completion facts as they were gathered before TRU-274.
fn full_completion_facts(
    app: &App,
    completion: &TaskMutation,
    resolved: Option<&Path>,
) -> TaskMutationFacts {
    let mut facts = app.worker_completion_facts(completion, resolved);
    facts.callers = full_callers(app);
    facts
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
    let resolved = pending.resolved_worktree_path.as_deref();
    let (completion_before, full) = timed(|| full_completion_facts(&app, &completion, resolved));
    assert_eq!(full.callers.len(), TABS, "every checkout tab resolves");
    let (completion_after, facts) = timed(|| app.worker_completion_facts(&completion, resolved));
    assert_eq!(facts.callers, vec![caller.clone()]);
    assert_eq!(facts.checkouts, full.checkouts);
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
    let (saved_before, full) = timed(|| full_mutation_facts(&app, &saved));
    assert_eq!(
        full.checkouts.len(),
        WORKTREES + 1,
        "every task worktree resolves"
    );
    let (saved_after, facts) = timed(|| app.mutation_facts(&saved));
    assert_eq!(facts.callers, vec![caller.clone()]);
    assert_eq!(
        facts.checkouts,
        vec![WorkerCheckout {
            path: prepared.path.clone(),
            repository: prepared.repository.clone(),
        }]
    );
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
         completion facts (WorkerPrepared): before {} after {}\n\
         saved facts (WorkerPreparationSaved): before {} after {}\n\
         store complete_worktree_preparation: {}\n\
         start_delegated_worker (delegation insert + failure status): {}\n\
         store replace (one whole-store rewrite): {}",
        ms(completion_before),
        ms(completion_after),
        ms(saved_before),
        ms(saved_after),
        ms(complete_write),
        ms(start_writes),
        ms(replace_write),
    );
}

const STEPS: [&str; 5] = ["begin", "check", "bind", "prepared", "saved"];
const CASES: &[&str] = &[
    "matching caller",
    "missing caller",
    "foreign caller",
    "duplicate caller",
    "changed caller checkout",
    "stale worktree",
    "moved path",
    // Symlink creation is Unix-only here; Windows needs privileges.
    #[cfg(unix)]
    "symlinked path",
];

/// One worker preparation, reduced twice from the same store: by the Iced
/// app with the targeted gather, and by a second authority with the full
/// gather the app made before TRU-274.
struct Pair {
    _dir: tempfile::TempDir,
    app: App,
    full: TaskAuthority,
    uid: String,
    task_id: String,
    expected: CompletedWorktreePreparation,
    resolved: PathBuf,
}

fn pair() -> Pair {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut app = app_with_tabs(&root);
    add_task(&mut app, &root, &root);
    let source = root.join("source");
    std::fs::create_dir_all(&source).unwrap();
    fixture_worker_source(&source);
    app.workspaces[0].dir = source.clone();
    app.workspaces[0].location = WorkspaceLocation::Local {
        root: source.clone(),
    };
    app.workspaces[0].tabs[0].repo_path = source.clone();
    let uid = app.workspaces[0].tabs[0].session_uid.clone();
    let repository = app
        .worker_authority_caller(&uid)
        .unwrap()
        .repository
        .unwrap();
    let store = app.task_host.store_mut().as_mut().unwrap();
    let mut target = store.get("fixture-task").unwrap().clone();
    target.repository = repository.clone();
    for index in 0..3 {
        let name = format!("other-{index}");
        let path = root.join(&name);
        fixture_worker_checkout(&source, &path, &name);
        let mut task = target.clone();
        task.task_id = name.clone();
        task.branch = name;
        task.worktree.path = Some(path.clone());
        store.insert(task).unwrap();
        app.workspaces[0]
            .tabs
            .push(TabState::new(2000 + index, path));
    }
    target.task_id = "target".into();
    target.branch = "prepared".into();
    target.lifecycle = TaskLifecycle::Preparing;
    target.worktree = TaskWorktree {
        state: TaskWorktreeState::Preparing,
        path: None,
    };
    store.insert(target.clone()).unwrap();
    let resolved = root.join("prepared");
    fixture_worker_checkout(&source, &resolved, "prepared");
    let tasks = root.join("tasks.json");
    let copy = root.join("tasks-full.json");
    std::fs::copy(&tasks, &copy).unwrap();
    app.task_host
        .replace_store(Some(TaskStore::load(&tasks).unwrap()));
    let full = TaskAuthority::from(Some(TaskStore::load(&copy).unwrap()));
    Pair {
        _dir: dir,
        app,
        full,
        uid,
        task_id: target.task_id,
        expected: CompletedWorktreePreparation {
            repository,
            base: target.base,
            branch: target.branch,
            path: resolved.clone(),
        },
        resolved,
    }
}

fn perturb(pair: &mut Pair, case: &str) {
    let tabs = &mut pair.app.workspaces[0].tabs;
    match case {
        "matching caller" => {}
        "missing caller" => {
            tabs.remove(0);
        }
        "foreign caller" => {
            // Another open tab, in another checkout, now carries the
            // captured caller's UID.
            tabs.remove(0);
            tabs[0].session_uid = pair.uid.clone();
        }
        "duplicate caller" => {
            tabs[1].session_uid = pair.uid.clone();
        }
        "changed caller checkout" => {
            tabs[0].repo_path = tabs[1].repo_path.clone();
        }
        "stale worktree" => std::fs::remove_file(pair.resolved.join(".git")).unwrap(),
        "moved path" => {
            std::fs::rename(&pair.resolved, pair.resolved.with_extension("moved")).unwrap()
        }
        #[cfg(unix)]
        "symlinked path" => {
            let moved = pair.resolved.with_extension("moved");
            std::fs::rename(&pair.resolved, &moved).unwrap();
            std::os::unix::fs::symlink(&moved, &pair.resolved).unwrap();
        }
        _ => unreachable!("{case}"),
    }
}

/// Reduces one mutation on both sides and returns each outcome, Debug-printed
/// so error texts and every effect field are compared.
fn reduce_both(
    pair: &mut Pair,
    mutation: impl Fn() -> TaskMutation,
    completion: bool,
) -> (String, String, Result<TaskMutationAction, String>) {
    let resolved = Some(pair.resolved.as_path());
    let (targeted, full) = if completion {
        (
            pair.app.worker_completion_facts(&mutation(), resolved),
            full_completion_facts(&pair.app, &mutation(), resolved),
        )
    } else {
        (
            pair.app.mutation_facts(&mutation()),
            full_mutation_facts(&pair.app, &mutation()),
        )
    };
    let command = || TaskMutationCommand {
        reply: None,
        mutation: mutation(),
    };
    let new = pair
        .app
        .task_host
        .reduce_mutation(command(), &targeted)
        .action;
    let old = pair.full.reduce_mutation(command(), &full).action;
    (format!("{new:?}"), format!("{old:?}"), new)
}

#[test]
fn targeted_worker_facts_reduce_exactly_as_the_full_gather() {
    if isolated(
        "targeted_worker_facts_reduce_exactly_as_the_full_gather",
        "worker_facts",
    ) {
        return;
    }
    for case in CASES.iter().copied() {
        for perturb_at in STEPS {
            let mut pair = pair();
            let mut outcomes = Vec::new();
            let mut captured = None;
            for step in STEPS {
                if step == perturb_at {
                    perturb(&mut pair, case);
                }
                let id = match (step, captured) {
                    ("begin", _) => None,
                    (_, Some(id)) => Some(id),
                    // Nothing was captured: no later step has an operation.
                    (_, None) => break,
                };
                let id = || id.unwrap();
                let (uid, task_id, expected) = (
                    pair.uid.clone(),
                    pair.task_id.clone(),
                    pair.expected.clone(),
                );
                let (new, old, action) = match step {
                    "begin" => reduce_both(
                        &mut pair,
                        || TaskMutation::BeginWorkerPreparation {
                            caller: uid.clone(),
                        },
                        false,
                    ),
                    "check" => reduce_both(
                        &mut pair,
                        || TaskMutation::CheckWorkerPreparation { id: id() },
                        false,
                    ),
                    "bind" => reduce_both(
                        &mut pair,
                        || TaskMutation::BindWorkerPreparation {
                            id: id(),
                            task_id: task_id.clone(),
                            expected: expected.clone(),
                        },
                        false,
                    ),
                    "prepared" => reduce_both(
                        &mut pair,
                        || TaskMutation::WorkerPrepared {
                            id: id(),
                            result: Ok(expected.clone()),
                        },
                        true,
                    ),
                    _ => reduce_both(
                        &mut pair,
                        || TaskMutation::WorkerPreparationSaved {
                            id: id(),
                            result: Ok(()),
                        },
                        false,
                    ),
                };
                assert_eq!(new, old, "{case} (changed before {perturb_at}), {step}");
                match action {
                    Ok(TaskMutationAction::WorkerAuthorityChecked { id }) => captured = Some(id),
                    // Both sides save the same preparation, as the app would.
                    Ok(TaskMutationAction::PersistWorkerPreparation {
                        task_id, prepared, ..
                    }) => {
                        for store in [pair.app.task_host.store_mut(), pair.full.store_mut()] {
                            store
                                .as_mut()
                                .unwrap()
                                .complete_worktree_preparation(
                                    &task_id,
                                    prepared.clone(),
                                    "2000-01-02T00:00:00Z",
                                )
                                .unwrap();
                        }
                    }
                    _ => {}
                }
                outcomes.push(new);
            }
            // The matrix must reach both the launch and each refusal.
            let at = STEPS.iter().position(|step| *step == perturb_at).unwrap();
            let caller_case = case.contains("caller");
            let (step, needle) = match case {
                "matching caller" | "duplicate caller" => (4, "Ok(StartWorker"),
                "missing caller" if at == 0 => {
                    (0, "The calling GitTerm tab is no longer available")
                }
                // A caller already changed when captured is the caller.
                _ if caller_case && at == 0 => (4, "Ok(StartWorker"),
                _ if caller_case => (at, "caller, session, workspace authority"),
                // A destination changed before completion fails there.
                _ => (at.max(3), "destination checkout"),
            };
            assert!(
                outcomes[step].contains(needle),
                "{case} (changed before {perturb_at}): {outcomes:?}"
            );
        }
    }
}

const REUSE_STEPS: [&str; 3] = ["begin", "reuse", "saved"];

/// The same comparison for reusing an existing checkout: begin, reuse (the
/// reducer reads the replacement's checkout), then the reuse save (the
/// reducer reads the checkout of the `Reusing` subject, which the targeted
/// gather finds from the reducer's pending state).
#[test]
fn targeted_worker_facts_reduce_exactly_as_the_full_gather_on_checkout_reuse() {
    if isolated(
        "targeted_worker_facts_reduce_exactly_as_the_full_gather_on_checkout_reuse",
        "worker_facts",
    ) {
        return;
    }
    for case in CASES.iter().copied() {
        for perturb_at in REUSE_STEPS {
            let mut pair = pair();
            // Reuse the existing checkout of "other-0" instead of preparing one.
            pair.resolved = pair.resolved.with_file_name("other-0");
            let mut replacement = pair
                .app
                .task_host
                .store()
                .as_ref()
                .unwrap()
                .get("other-0")
                .unwrap()
                .clone();
            replacement.title = "Replacement work".into();
            replacement.updated_at = "2000-01-02T00:00:00Z".into();
            let mut outcomes = Vec::new();
            let mut captured = None;
            for step in REUSE_STEPS {
                if step == perturb_at {
                    perturb(&mut pair, case);
                }
                let id = match (step, captured) {
                    ("begin", _) => None,
                    (_, Some(id)) => Some(id),
                    (_, None) => break,
                };
                let uid = pair.uid.clone();
                let (new, old, action) = match step {
                    "begin" => reduce_both(
                        &mut pair,
                        || TaskMutation::BeginWorkerPreparation {
                            caller: uid.clone(),
                        },
                        false,
                    ),
                    "reuse" => reduce_both(
                        &mut pair,
                        || TaskMutation::ReuseWorkerCheckout {
                            id: id.unwrap(),
                            replacement: Box::new(replacement.clone()),
                        },
                        false,
                    ),
                    _ => reduce_both(
                        &mut pair,
                        || TaskMutation::WorkerCheckoutReuseSaved {
                            id: id.unwrap(),
                            result: Ok(()),
                        },
                        false,
                    ),
                };
                assert_eq!(new, old, "{case} (changed before {perturb_at}), {step}");
                match action {
                    Ok(TaskMutationAction::WorkerAuthorityChecked { id }) => captured = Some(id),
                    // Both sides save the same reuse, as the app would.
                    Ok(TaskMutationAction::PersistWorkerCheckoutReuse { replacement, .. }) => {
                        for store in [pair.app.task_host.store_mut(), pair.full.store_mut()] {
                            store
                                .as_mut()
                                .unwrap()
                                .replace(*replacement.clone())
                                .unwrap();
                        }
                    }
                    _ => {}
                }
                outcomes.push(new);
            }
            // The matrix must reach the launch and each refusal. A save whose
            // `Reusing` subject returned no checkout would refuse the
            // matching cases, so this fails if the targeted gather misses it.
            let at = REUSE_STEPS.iter().position(|s| *s == perturb_at).unwrap();
            let caller_case = case.contains("caller");
            let (step, needle) = match case {
                "matching caller" | "duplicate caller" => (2, "Ok(StartWorker"),
                "missing caller" if at == 0 => {
                    (0, "The calling GitTerm tab is no longer available")
                }
                _ if caller_case && at == 0 => (2, "Ok(StartWorker"),
                _ if caller_case => (at, "caller, session, workspace authority"),
                _ => (at.max(1), "checkout"),
            };
            assert!(
                outcomes.len() > step && outcomes[step].contains(needle),
                "{case} (changed before {perturb_at}): {outcomes:?}"
            );
        }
    }
}
