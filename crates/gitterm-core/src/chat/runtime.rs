//! Local chat process supervision, independent of a frontend executor.
//! The process loop owns initialization, commands, shutdown and child wait.
//! Its thread stays alive through snapshot cleanup after ProcessExited.
use crate::chat::types::{HarnessCommand, HarnessEvent};
use crate::harness::claude::{run_session_program, Launch};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub struct Session {
    commands: mpsc::UnboundedSender<HarnessCommand>,
    snapshots_closed: CancellationToken,
    shutdown_on_drop: bool,
    pid: Arc<AtomicU32>,
    completion: Option<oneshot::Receiver<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Preparation is a host port, invoked at the old session-thread boundary.
    /// It supplies all launch inputs, including a recorder sharing this token.
    pub fn spawn(
        prepare: impl FnOnce(CancellationToken) -> Result<Launch, String> + Send + 'static,
    ) -> (Self, mpsc::UnboundedReceiver<HarnessEvent>) {
        let snapshots_closed = CancellationToken::new();
        let close_token = snapshots_closed.clone();
        let (commands, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, events) = mpsc::unbounded_channel();
        let (done_tx, completion) = oneshot::channel();
        let pid = Arc::new(AtomicU32::new(0));
        let session_pid = pid.clone();
        let thread = std::thread::Builder::new()
            .name("claude-session".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ev_tx.send(HarnessEvent::Error(format!(
                            "could not start the Claude session runtime: {e}"
                        )));
                        let _ = ev_tx.send(HarnessEvent::ProcessExited { code: None });
                        return;
                    }
                };
                rt.block_on(async move {
                    match prepare(close_token.clone()) {
                        Ok(launch) => {
                            run_session_program(launch, cmd_rx, ev_tx, close_token, session_pid)
                                .await
                        }
                        Err(msg) => {
                            eprintln!("[claude-harness] {msg}");
                            let _ = ev_tx.send(HarnessEvent::Error(msg));
                            let _ = ev_tx.send(HarnessEvent::ProcessExited { code: None });
                        }
                    }
                });
                drop(rt);
                let _ = done_tx.send(());
            });
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(e) => {
                eprintln!("[claude-harness] could not spawn session thread: {e}");
                None
            }
        };
        (
            Self {
                commands,
                snapshots_closed,
                shutdown_on_drop: true,
                pid,
                completion: Some(completion),
                thread,
            },
            events,
        )
    }

    /// Explicit root transport port. Remote attach/reconnect remains in the
    /// host, which owns its executor and completion. No local process is owned.
    pub fn from_port(
        commands: mpsc::UnboundedSender<HarnessCommand>,
        shutdown_on_drop: bool,
    ) -> Self {
        Self {
            commands,
            snapshots_closed: CancellationToken::new(),
            shutdown_on_drop,
            pid: Default::default(),
            completion: None,
            thread: None,
        }
    }

    pub fn send(&self, command: HarnessCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "the Claude session has exited".to_string())
    }
    pub fn close_turn_snapshots(&self) {
        self.snapshots_closed.cancel();
    }
    pub fn local_pid(&self) -> Option<u32> {
        match self.pid.load(Ordering::Relaxed) {
            0 => None,
            pid => Some(pid),
        }
    }

    /// Wait for the entire local loop, final snapshot cleanup, and runtime
    /// teardown. ProcessExited alone does not establish this boundary.
    /// Drop sends shutdown but never blocks a frontend on this join.
    pub async fn completed(&mut self) -> Result<(), String> {
        if let Some(done) = self.completion.as_mut() {
            let _ = done.await;
        }
        self.completion = None;
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| "the Claude session thread panicked".to_string())?;
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.shutdown_on_drop {
            let _ = self.commands.send(HarnessCommand::Shutdown);
        }
    }
}

#[cfg(all(test, unix))]
#[path = "runtime_tests.rs"]
mod tests;
