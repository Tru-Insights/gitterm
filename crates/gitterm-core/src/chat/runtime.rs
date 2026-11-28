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

pub use super::types::{CommandCorrelation, DeliveryOutcome};

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
    Closed,
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
    /// Whether this port emits ordered correlated delivery receipts.
    pub fn supports_correlation(&self) -> bool {
        self.supports_correlation
    }

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
                        cmd_rx.close();
                        while let Some(command) = cmd_rx.blocking_recv() {
                            reject_command(command, &events);
                        }
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
                            reject_queued(&mut cmd_rx, &events).await;
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

    /// ACP uses the Iced session loop and channel-acceptance semantics. The
    /// host retains this handle until the process and snapshots have joined.
    pub fn spawn_acp_runtime(
        prepare: impl FnOnce(CancellationToken) -> Result<crate::harness::acp_launch::Launch, String>
            + Send
            + 'static,
    ) -> (Self, mpsc::UnboundedReceiver<RuntimeEvent>) {
        let (commands, cmd_rx) = mpsc::unbounded_channel();
        let (tx, events) = mpsc::unbounded_channel();
        let snapshots_closed = CancellationToken::new();
        let close_token = snapshots_closed.clone();
        let (done, completion) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("acp-session".into())
            .spawn(move || {
                let sink = EventSink::Runtime(tx);
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match result {
                    Ok(rt) => {
                        rt.block_on(async move {
                            match prepare(close_token.clone()) {
                                Ok(launch) => {
                                    crate::harness::acp::runtime::run_session(launch, cmd_rx, sink)
                                        .await
                                }
                                Err(error) => {
                                    sink.emit(HarnessEvent::Error(error));
                                    sink.emit(HarnessEvent::ProcessExited { code: None });
                                }
                            }
                        });
                    }
                    Err(error) => {
                        sink.emit(HarnessEvent::Error(format!(
                            "could not start the ACP session runtime: {error}"
                        )));
                        sink.emit(HarnessEvent::ProcessExited { code: None });
                    }
                }
                let _ = done.send(());
            });
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(error) => {
                eprintln!("[acp] could not spawn session thread: {error}");
                None
            }
        };
        (
            Self {
                commands: CommandPort::Remote(commands),
                snapshots_closed,
                shutdown_on_drop: true,
                supports_correlation: false,
                pid: Default::default(),
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
            CommandPort::Closed => false,
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
            CommandPort::Remote(_) | CommandPort::Closed => {
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

    /// Stop an owned local process and join its I/O, snapshots and runtime.
    /// Remote ports retain their drop policy: detach unless explicitly owned.
    /// The caller supplies any timeout; cancelling this wait keeps the join
    /// handle on the session so it can still be awaited later.
    pub fn request_shutdown(&mut self) {
        if self.shutdown_on_drop {
            let _ = self.send(HarnessCommand::Shutdown);
        }
        self.commands = CommandPort::Closed;
        self.shutdown_on_drop = false;
    }

    pub async fn shutdown(&mut self) -> Result<(), String> {
        self.request_shutdown();
        self.completed().await
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
pub(crate) async fn reject_queued<C: Into<RuntimeCommand>>(
    commands: &mut mpsc::UnboundedReceiver<C>,
    events: &EventSink,
) {
    commands.close();
    reject_closed_queue(|cx| commands.poll_recv(cx), events).await;
}

async fn reject_closed_queue<C: Into<RuntimeCommand>>(
    mut poll_recv: impl FnMut(&mut std::task::Context<'_>) -> std::task::Poll<Option<C>>,
    events: &EventSink,
) {
    // A send can reserve acceptance before close and publish afterward. Only
    // Ready(None), not a temporarily empty queue, proves all sends are drained.
    while let Some(command) = std::future::poll_fn(&mut poll_recv).await {
        reject_command(command, events);
    }
}

fn reject_command(command: impl Into<RuntimeCommand>, events: &EventSink) {
    events.outcome(
        command.into().correlation,
        DeliveryOutcome::Rejected("the Claude session exited before executing the command".into()),
    );
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    #[tokio::test]
    async fn shutdown_joins_reader_writer_and_supervisor_teardown() {
        let (tx, mut rx) = mpsc::unbounded_channel::<RuntimeCommand>();
        let (done, completion) = oneshot::channel();
        let reader_done = Arc::new(AtomicBool::new(false));
        let writer_done = Arc::new(AtomicBool::new(false));
        let supervisor_done = Arc::new(AtomicBool::new(false));
        let flags = (
            reader_done.clone(),
            writer_done.clone(),
            supervisor_done.clone(),
        );
        let thread = std::thread::spawn(move || {
            assert_eq!(
                rx.blocking_recv().unwrap().command,
                HarnessCommand::Shutdown
            );
            let reader = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                flags.0.store(true, Ordering::SeqCst);
            });
            let writer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                flags.1.store(true, Ordering::SeqCst);
            });
            reader.join().unwrap();
            writer.join().unwrap();
            let _ = done.send(());
            // Completion notification can precede the thread's own teardown.
            std::thread::sleep(Duration::from_millis(20));
            flags.2.store(true, Ordering::SeqCst);
        });
        let mut session = Session {
            commands: CommandPort::Local(tx),
            snapshots_closed: CancellationToken::new(),
            shutdown_on_drop: true,
            supports_correlation: false,
            pid: Default::default(),
            completion: Some(completion),
            thread: Some(thread),
        };
        session.shutdown().await.unwrap();
        assert!(reader_done.load(Ordering::SeqCst));
        assert!(writer_done.load(Ordering::SeqCst));
        assert!(supervisor_done.load(Ordering::SeqCst));
        assert!(session.thread.is_none());
        assert!(session.send(HarnessCommand::Interrupt).is_err());
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_detaches_remote_port_without_stopping_chat() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut session = Session::from_port(tx, false);
        session.shutdown().await.unwrap();
        assert_eq!(rx.recv().await, None);
    }
}
