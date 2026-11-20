//! Local chat process supervision, independent of a frontend executor.
//! The process loop owns initialization, commands, shutdown and child wait.
//! Its thread stays alive through snapshot cleanup after ProcessExited.
use crate::chat::types::{HarnessCommand, HarnessEvent};
use crate::harness::claude::{run_correlated_session_program, Launch};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Opaque caller-owned identity, scoped to one session. Never sent to the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandCorrelation(pub u64);

/// Actual stdin delivery, independent of channel acceptance or provider acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Written,
    Rejected(String),
    Uncertain(String),
}

/// Internal events only: these have no phone wire or transcript representation.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeEvent {
    Harness(HarnessEvent),
    CommandOutcome {
        correlation: CommandCorrelation,
        outcome: DeliveryOutcome,
    },
}

pub(crate) struct RuntimeCommand {
    pub command: HarnessCommand,
    pub correlation: Option<CommandCorrelation>,
}

impl From<HarnessCommand> for RuntimeCommand {
    fn from(command: HarnessCommand) -> Self {
        Self {
            command,
            correlation: None,
        }
    }
}

pub(crate) enum EventSink {
    Harness(mpsc::UnboundedSender<HarnessEvent>),
    Runtime(mpsc::UnboundedSender<RuntimeEvent>),
}

impl EventSink {
    pub(crate) fn emit(&self, event: HarnessEvent) {
        match self {
            Self::Harness(tx) => {
                let _ = tx.send(event);
            }
            Self::Runtime(tx) => {
                let _ = tx.send(RuntimeEvent::Harness(event));
            }
        }
    }

    pub(crate) fn outcome(
        &self,
        correlation: Option<CommandCorrelation>,
        outcome: DeliveryOutcome,
    ) {
        if let (Self::Runtime(tx), Some(correlation)) = (self, correlation) {
            let _ = tx.send(RuntimeEvent::CommandOutcome {
                correlation,
                outcome,
            });
        }
    }
}

enum CommandPort {
    Local(mpsc::UnboundedSender<RuntimeCommand>),
    Remote(mpsc::UnboundedSender<HarnessCommand>),
}

pub struct Session {
    commands: CommandPort,
    snapshots_closed: CancellationToken,
    shutdown_on_drop: bool,
    supports_correlation: bool,
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
        let (tx, events) = mpsc::unbounded_channel();
        (
            Self::spawn_with_events(prepare, EventSink::Harness(tx)),
            events,
        )
    }

    /// Spawn with internal delivery events on the same ordered path as harness events.
    pub fn spawn_runtime(
        prepare: impl FnOnce(CancellationToken) -> Result<Launch, String> + Send + 'static,
    ) -> (Self, mpsc::UnboundedReceiver<RuntimeEvent>) {
        let (tx, events) = mpsc::unbounded_channel();
        (
            Self::spawn_with_events(prepare, EventSink::Runtime(tx)),
            events,
        )
    }

    fn spawn_with_events(
        prepare: impl FnOnce(CancellationToken) -> Result<Launch, String> + Send + 'static,
        events: EventSink,
    ) -> Self {
        let supports_correlation = matches!(&events, EventSink::Runtime(_));
        let snapshots_closed = CancellationToken::new();
        let close_token = snapshots_closed.clone();
        let (commands, mut cmd_rx) = mpsc::unbounded_channel();
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
                        events.emit(HarnessEvent::Error(format!(
                            "could not start the Claude session runtime: {e}"
                        )));
                        reject_queued(&mut cmd_rx, &events);
                        events.emit(HarnessEvent::ProcessExited { code: None });
                        return;
                    }
                };
                rt.block_on(async move {
                    match prepare(close_token.clone()) {
                        Ok(launch) => {
                            run_correlated_session_program(
                                launch,
                                cmd_rx,
                                events,
                                close_token,
                                session_pid,
                            )
                            .await
                        }
                        Err(msg) => {
                            eprintln!("[claude-harness] {msg}");
                            events.emit(HarnessEvent::Error(msg));
                            reject_queued(&mut cmd_rx, &events);
                            events.emit(HarnessEvent::ProcessExited { code: None });
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
        Self {
            commands: CommandPort::Local(commands),
            snapshots_closed,
            shutdown_on_drop: true,
            supports_correlation,
            pid,
            completion: Some(completion),
            thread,
        }
    }

    /// Explicit root transport port. Remote attach/reconnect remains in the
    /// host, which owns its executor and completion. No local process is owned.
    pub fn from_port(
        commands: mpsc::UnboundedSender<HarnessCommand>,
        shutdown_on_drop: bool,
    ) -> Self {
        Self {
            commands: CommandPort::Remote(commands),
            snapshots_closed: CancellationToken::new(),
            shutdown_on_drop,
            supports_correlation: false,
            pid: Default::default(),
            completion: None,
            thread: None,
        }
    }

    /// Ok means channel acceptance only. It is never a stdin write receipt.
    pub fn send(&self, command: HarnessCommand) -> Result<(), String> {
        let accepted = match &self.commands {
            CommandPort::Local(tx) => tx.send(command.into()).is_ok(),
            CommandPort::Remote(tx) => tx.send(command).is_ok(),
        };
        accepted
            .then_some(())
            .ok_or_else(|| "the Claude session has exited".to_string())
    }

    /// Attach a caller identity. Read the matching RuntimeEvent for actual delivery.
    /// Requires spawn_runtime; remote ports cannot supply local I/O receipts.
    pub fn send_correlated(
        &self,
        command: HarnessCommand,
        correlation: CommandCorrelation,
    ) -> Result<(), String> {
        if !self.supports_correlation {
            return Err("correlated writes require Session::spawn_runtime".into());
        }
        match &self.commands {
            CommandPort::Local(tx) => tx
                .send(RuntimeCommand {
                    command,
                    correlation: Some(correlation),
                })
                .map_err(|_| "the Claude session has exited".to_string()),
            CommandPort::Remote(_) => {
                Err("remote sessions do not support correlated writes".into())
            }
        }
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
            let _ = self.send(HarnessCommand::Shutdown);
        }
    }
}

#[cfg(all(test, unix))]
#[path = "runtime_tests.rs"]
mod tests;

/// Closing first makes every accepted but unexecuted command definitively rejected.
pub(crate) fn reject_queued<C: Into<RuntimeCommand>>(
    commands: &mut mpsc::UnboundedReceiver<C>,
    events: &EventSink,
) {
    commands.close();
    while let Ok(command) = commands.try_recv() {
        events.outcome(
            command.into().correlation,
            DeliveryOutcome::Rejected(
                "the Claude session exited before executing the command".into(),
            ),
        );
    }
}
