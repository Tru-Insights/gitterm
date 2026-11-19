//! Process-owned status jobs. No domain state, clock, timeout or freshness policy.

use crate::status::{GitStatusSnapshot, StatusCommand};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobId(u64);

#[derive(Debug, Clone)]
pub enum StatusOutcome {
    Completed {
        job: JobId,
        snapshot: GitStatusSnapshot,
    },
    Cancelled {
        job: JobId,
        owner: usize,
    },
}

struct Job {
    owner: usize,
    pending: bool,
    abort: Option<AbortHandle>,
    monitor: JoinHandle<()>,
}

struct Jobs {
    next: u64,
    owners: HashSet<usize>,
    jobs: HashMap<JobId, Job>,
    output: Option<mpsc::UnboundedSender<StatusOutcome>>,
}

/// Retain this at application scope, independently of panes and subscriptions.
/// Handles are supplied by the host, which must keep its executor alive through teardown.
/// Owner IDs must be unique for the process lifetime, as with host tab IDs.
pub struct StatusRuntime {
    inner: Arc<Mutex<Jobs>>,
    teardown: tokio::sync::Mutex<()>,
}

/// The sole result receiver; removed owners' queued snapshots are discarded here too.
pub struct StatusReceiver {
    input: mpsc::UnboundedReceiver<StatusOutcome>,
    inner: Arc<Mutex<Jobs>>,
}

impl StatusReceiver {
    pub async fn recv(&mut self) -> Option<StatusOutcome> {
        while let Some(outcome) = self.input.recv().await {
            if let StatusOutcome::Completed { snapshot, .. } = &outcome {
                if !self.inner.lock().unwrap().owners.contains(&snapshot.tab_id) {
                    continue;
                }
            }
            return Some(outcome);
        }
        None
    }
}

impl StatusRuntime {
    pub fn new() -> (Self, StatusReceiver) {
        let (output, input) = mpsc::unbounded_channel();
        let inner = Arc::new(Mutex::new(Jobs {
            next: 0,
            owners: HashSet::new(),
            jobs: HashMap::new(),
            output: Some(output),
        }));
        (
            Self {
                inner: inner.clone(),
                teardown: tokio::sync::Mutex::new(()),
            },
            StatusReceiver { input, inner },
        )
    }

    /// SchedulePoll remains the host's existing maintenance input, never a new timer.
    pub fn execute(&self, handle: &Handle, owner: usize, command: StatusCommand) -> Option<JobId> {
        let StatusCommand::CollectStatus { root } = command else {
            return None;
        };
        self.submit(handle, owner, root, true, |handle, owner, root| {
            handle.spawn_blocking(move || collect_snapshot(owner, root))
        })
    }

    /// The adapter retains remote transport, credentials and DTO conversion. Both
    /// local and remote jobs use the same correlation, cancellation and result path.
    pub fn collect_remote(
        &self,
        handle: &Handle,
        owner: usize,
        root: PathBuf,
        collect: impl Future<Output = GitStatusSnapshot> + Send + 'static,
    ) -> Option<JobId> {
        self.submit(handle, owner, root, false, |handle, _, _| {
            handle.spawn(collect)
        })
    }

    fn submit(
        &self,
        handle: &Handle,
        owner: usize,
        root: PathBuf,
        blocking: bool,
        start: impl FnOnce(&Handle, usize, PathBuf) -> JoinHandle<GitStatusSnapshot>,
    ) -> Option<JobId> {
        let mut jobs = self.inner.lock().unwrap();
        jobs.output.as_ref()?;
        jobs.jobs.retain(|_, job| !job.monitor.is_finished());
        jobs.next = jobs.next.checked_add(1).expect("status job IDs exhausted");
        let id = JobId(jobs.next);
        jobs.owners.insert(owner);
        let worker = start(handle, owner, root.clone());
        let abort = (!blocking).then(|| worker.abort_handle());
        let inner = self.inner.clone();
        let monitor = handle.spawn(async move {
            let snapshot = worker.await.unwrap_or_else(|err| GitStatusSnapshot {
                tab_id: owner,
                repo_path: root,
                repo_name: "unknown".into(),
                branch_name: "main".into(),
                is_git_repo: true,
                staged: Vec::new(),
                unstaged: Vec::new(),
                untracked: Vec::new(),
                error: Some(format!("git status task failed: {err}")),
            });
            let mut jobs = inner.lock().unwrap();
            if let Some(job) = jobs.jobs.get_mut(&id) {
                if job.pending {
                    job.pending = false;
                    if let Some(output) = &jobs.output {
                        let _ = output.send(StatusOutcome::Completed { job: id, snapshot });
                    }
                }
            }
        });
        jobs.jobs.insert(
            id,
            Job {
                owner,
                pending: true,
                abort,
                monitor,
            },
        );
        Some(id)
    }

    /// Exactly one outcome wins under the job lock. Local Git is not hard-cancelled:
    /// its monitor remains owned until the blocking child returns, then discards output.
    pub fn cancel(&self, id: JobId) -> bool {
        cancel(&mut self.inner.lock().unwrap(), id)
    }

    pub fn remove_owner(&self, owner: usize) {
        let mut jobs = self.inner.lock().unwrap();
        jobs.owners.remove(&owner);
        let ids: Vec<_> = jobs
            .jobs
            .iter()
            .filter(|(_, j)| j.owner == owner)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            cancel(&mut jobs, id);
        }
    }

    /// Call after host catalog changes; hiding/switching panes keeps owners live.
    pub fn retain_owners(&self, owners: &HashSet<usize>) {
        let removed: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .owners
            .difference(owners)
            .copied()
            .collect();
        for owner in removed {
            self.remove_owner(owner);
        }
        self.inner
            .lock()
            .unwrap()
            .jobs
            .retain(|_, job| !job.monitor.is_finished());
    }

    /// Stop accepting jobs, cancel pending outcomes and close the result channel.
    /// Nonblocking fallback for host drop; use teardown to account for running Git.
    pub fn close(&self) {
        let mut jobs = self.inner.lock().unwrap();
        let ids: Vec<_> = jobs.jobs.keys().copied().collect();
        for id in ids {
            cancel(&mut jobs, id);
        }
        jobs.owners.clear();
        jobs.output.take();
    }

    /// Explicit shutdown waits for started blocking Git instead of claiming that
    /// aborting a Tokio join stopped its child. There is deliberately no timeout.
    pub async fn teardown(&self) {
        // Repeated close requests must also wait for the first drain.
        let _teardown = self.teardown.lock().await;
        self.close();
        let monitors: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .jobs
            .drain()
            .map(|(_, job)| job.monitor)
            .collect();
        for monitor in monitors {
            let _ = monitor.await;
        }
    }
}

impl Drop for StatusRuntime {
    fn drop(&mut self) {
        self.close();
    }
}

fn cancel(jobs: &mut Jobs, id: JobId) -> bool {
    let Some(job) = jobs.jobs.get_mut(&id) else {
        return false;
    };
    if !job.pending {
        return false;
    }
    job.pending = false;
    if let Some(abort) = &job.abort {
        abort.abort();
    }
    if let Some(output) = &jobs.output {
        let _ = output.send(StatusOutcome::Cancelled {
            job: id,
            owner: job.owner,
        });
    }
    true
}

pub fn collect_snapshot(tab_id: usize, root: PathBuf) -> GitStatusSnapshot {
    let status = crate::git_status::collect_repo_status(&root);
    GitStatusSnapshot {
        tab_id,
        repo_path: status.root,
        repo_name: status.repo_name,
        branch_name: status.branch_name,
        is_git_repo: status.is_git_repo,
        staged: status.staged,
        unstaged: status.unstaged,
        untracked: status.untracked,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{GitStatusState, StatusEvent};
    use std::time::Instant;
    use tokio::sync::oneshot;

    fn executor() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    fn snapshot(branch: &str) -> GitStatusSnapshot {
        GitStatusSnapshot {
            tab_id: 7,
            repo_path: "/fixture".into(),
            repo_name: "fixture".into(),
            branch_name: branch.into(),
            is_git_repo: true,
            staged: vec![],
            unstaged: vec![],
            untracked: vec![],
            error: None,
        }
    }

    // A real blocking worker models a Git child that has already started. It
    // cannot be aborted by cancelling its async join or removing its owner.
    fn blocking_job(
        runtime: &StatusRuntime,
        handle: &Handle,
    ) -> (JobId, std::sync::mpsc::Sender<()>) {
        let (started, wait_started) = std::sync::mpsc::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        let id = runtime
            .submit(handle, 7, "/fixture".into(), true, |handle, _, _| {
                handle.spawn_blocking(move || {
                    started.send(()).unwrap();
                    wait_release.recv().unwrap();
                    snapshot("late")
                })
            })
            .unwrap();
        wait_started.recv().unwrap();
        (id, release)
    }

    #[test]
    fn cancellation_has_one_outcome_and_discards_late_blocking_result() {
        let executor = executor();
        let (runtime, mut results) = StatusRuntime::new();
        let (id, release) = blocking_job(&runtime, executor.handle());
        assert!(runtime.cancel(id));
        assert!(!runtime.cancel(id));
        executor.block_on(async {
            assert!(matches!(results.recv().await, Some(StatusOutcome::Cancelled { job, owner: 7 }) if job == id));
            release.send(()).unwrap();
            runtime.teardown().await;
            assert!(results.recv().await.is_none());
        });
    }

    #[test]
    fn removed_owner_discards_running_and_queued_results() {
        let executor = executor();
        executor.block_on(async {
            let (runtime, mut results) = StatusRuntime::new();
            let (id, release) = blocking_job(&runtime, executor.handle());
            runtime.remove_owner(7);
            assert!(matches!(results.recv().await, Some(StatusOutcome::Cancelled { job, .. }) if job == id));
            release.send(()).unwrap();
            runtime.teardown().await;
            assert!(results.recv().await.is_none());

            let (runtime, mut results) = StatusRuntime::new();
            runtime.collect_remote(executor.handle(), 7, "/fixture".into(), async { snapshot("queued") }).unwrap();
            // Wait for the actual monitor, leaving its completion queued.
            loop {
                if runtime.inner.lock().unwrap().jobs.values().all(|j| !j.pending) { break; }
                tokio::task::yield_now().await;
            }
            runtime.remove_owner(7);
            runtime.teardown().await;
            assert!(results.recv().await.is_none());
        });
    }

    #[test]
    fn both_completion_orders_apply_in_arrival_order_including_errors() {
        let executor = executor();
        executor.block_on(async {
            for reversed in [false, true] {
                for error_first_request in [false, true] {
                    let (runtime, mut results) = StatusRuntime::new();
                    let mut releases = Vec::new();
                    let mut ids = Vec::new();
                    for branch in ["first", "second"] {
                        let (tx, rx) = oneshot::channel();
                        releases.push(Some(tx));
                        ids.push(
                            runtime
                                .collect_remote(
                                    executor.handle(),
                                    7,
                                    "/fixture".into(),
                                    async move {
                                        rx.await.unwrap();
                                        let mut result = snapshot(branch);
                                        if error_first_request && branch == "first" {
                                            result.error = Some("transport failure".into());
                                        }
                                        result
                                    },
                                )
                                .unwrap(),
                        );
                    }
                    assert_ne!(ids[0], ids[1]);
                    let mut state = GitStatusState::new("fixture".into(), true, Instant::now());
                    let order = if reversed { [1, 0] } else { [0, 1] };
                    for index in order {
                        releases[index].take().unwrap().send(()).unwrap();
                        let Some(StatusOutcome::Completed { job, snapshot }) = results.recv().await
                        else {
                            panic!("expected completion")
                        };
                        assert_eq!(job, ids[index]);
                        state.reduce(StatusEvent::Completed(snapshot), Instant::now());
                        assert!(!runtime.cancel(job));
                    }
                    assert_eq!(
                        state.branch_name,
                        if reversed && !error_first_request {
                            "first"
                        } else {
                            "second"
                        }
                    );
                    assert_eq!(
                        state.git_status_error.is_some(),
                        reversed && error_first_request
                    );
                    runtime.teardown().await;
                    assert!(results.recv().await.is_none());
                }
            }
        });
    }

    #[test]
    fn teardown_cancels_remote_and_waits_for_started_git() {
        let executor = executor();
        let (runtime, mut results) = StatusRuntime::new();
        let (local, release) = blocking_job(&runtime, executor.handle());
        let remote = runtime
            .collect_remote(
                executor.handle(),
                8,
                "/opaque".into(),
                std::future::pending(),
            )
            .unwrap();
        executor.block_on(async {
            let teardown = runtime.teardown();
            tokio::pin!(teardown);
            // One poll closes submissions and cancels outcomes, but must wait
            // for the already-running blocking operation.
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(teardown.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            assert!(runtime
                .execute(
                    executor.handle(),
                    9,
                    StatusCommand::CollectStatus {
                        root: "/unused".into()
                    }
                )
                .is_none());
            let mut cancelled = HashSet::new();
            for _ in 0..2 {
                let Some(StatusOutcome::Cancelled { job, .. }) = results.recv().await else {
                    panic!("expected cancellation")
                };
                cancelled.insert(job);
            }
            assert_eq!(cancelled, HashSet::from([local, remote]));
            let second_teardown = runtime.teardown();
            tokio::pin!(second_teardown);
            assert!(std::future::poll_fn(|cx| {
                std::task::Poll::Ready(second_teardown.as_mut().poll(cx))
            })
            .await
            .is_pending());
            release.send(()).unwrap();
            teardown.await;
            second_teardown.await;
            assert!(runtime.inner.lock().unwrap().jobs.is_empty());
            assert!(results.recv().await.is_none());
        });
    }

    #[test]
    fn local_command_uses_shared_collector_and_drop_closes_receiver() {
        let executor = executor();
        executor.block_on(async {
            let (runtime, mut results) = StatusRuntime::new();
            assert!(runtime
                .execute(
                    executor.handle(),
                    7,
                    StatusCommand::SchedulePoll {
                        after: std::time::Duration::from_secs(5)
                    }
                )
                .is_none());
            let root = PathBuf::from("/nonexistent-gitterm-v6-runtime-fixture");
            let expected = collect_snapshot(7, root.clone());
            let id = runtime
                .execute(executor.handle(), 7, StatusCommand::CollectStatus { root })
                .unwrap();
            let Some(StatusOutcome::Completed { job, snapshot }) = results.recv().await else {
                panic!("expected local completion")
            };
            assert_eq!(job, id);
            assert_eq!(snapshot.repo_path, expected.repo_path);
            assert_eq!(snapshot.branch_name, expected.branch_name);
            assert_eq!(snapshot.is_git_repo, expected.is_git_repo);
            drop(runtime);
            assert!(results.recv().await.is_none());
        });
    }
}
