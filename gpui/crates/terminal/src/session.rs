//! Per-session I/O ownership. Parsing and nonblocking PTY writes never run on GPUI.
use crate::events::{EventProxy, SessionState};
use alacritty_terminal::{
    event::{Event, EventListener, OnResize, WindowSize},
    sync::FairMutex,
    term::Term,
    tty::{self, EventedPty},
    vte::ansi::Processor,
};
use parking_lot::Mutex;
use polling::{Event as PollEvent, Events, PollMode, Poller};
use std::{
    borrow::Cow,
    collections::VecDeque,
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

pub(crate) const INPUT_LIMIT: usize = 1024 * 1024;
const REPLY_RESERVE: usize = 64 * 1024;
const IO_BUDGET: usize = 64 * 1024;

struct Input {
    bytes: Cow<'static, [u8]>,
    offset: usize,
}
#[derive(Default)]
struct Queue {
    input: VecDeque<Input>,
    bytes: usize,
    resize: Option<WindowSize>,
}

pub(crate) struct IoControl {
    queue: Mutex<Queue>,
    poll: Arc<Poller>,
    stopped: AtomicBool,
    accepting: AtomicBool,
}
impl IoControl {
    pub fn input(&self, bytes: Cow<'static, [u8]>, reply: bool) -> bool {
        if !self.accepting.load(Ordering::Acquire) {
            return false;
        }
        let mut queue = self.queue.lock();
        let limit = INPUT_LIMIT + if reply { REPLY_RESERVE } else { 0 };
        // Bound message overhead as well as bytes; an empty write is a no-op.
        if !self.accepting.load(Ordering::Acquire)
            || queue.input.len() >= if reply { 4352 } else { 4096 }
            || bytes.len() > limit.saturating_sub(queue.bytes)
        {
            return false;
        }
        if bytes.is_empty() {
            return true;
        }
        queue.bytes += bytes.len();
        queue.input.push_back(Input { bytes, offset: 0 });
        drop(queue);
        let _ = self.poll.notify();
        true
    }
    pub fn resize(&self, size: WindowSize) {
        self.queue.lock().resize = Some(size);
        let _ = self.poll.notify();
    }
    pub fn is_closed(&self) -> bool {
        !self.accepting.load(Ordering::Acquire)
    }
    pub fn stop(&self) {
        self.accepting.store(false, Ordering::Release);
        self.stopped.store(true, Ordering::Release);
        let _ = self.poll.notify();
    }
    #[cfg(test)]
    pub fn queued_bytes(&self) -> usize {
        self.queue.lock().bytes
    }
}

pub(crate) struct Session {
    pub control: Arc<IoControl>,
    worker: Option<JoinHandle<()>>,
}
impl Session {
    pub fn start<T>(
        term: Arc<FairMutex<Term<EventProxy>>>,
        events: EventProxy,
        pty: T,
    ) -> io::Result<Self>
    where
        T: EventedPty + OnResize + Send + 'static,
    {
        let control = Arc::new(IoControl {
            queue: Mutex::new(Queue::default()),
            poll: Arc::new(Poller::new()?),
            stopped: AtomicBool::new(false),
            accepting: AtomicBool::new(true),
        });
        events.set_control(control.clone());
        let io = control.clone();
        let worker = std::thread::Builder::new()
            .name("terminal PTY".into())
            .spawn(move || {
                // Report unexpected failures as state, never unwind through the UI.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(pty, &term, &events, &io)
                }));
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => events.record_error(format!("PTY I/O: {error}")),
                    Err(_) => events.record_error("PTY worker failed".into()),
                }
                io.stop();
                let mut queue = io.queue.lock();
                queue.input.clear();
                queue.bytes = 0;
            })?;
        Ok(Self {
            control,
            worker: Some(worker),
        })
    }
    pub fn cancel(&mut self) {
        self.control.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn run<T: EventedPty + OnResize>(
    mut pty: T,
    term: &FairMutex<Term<EventProxy>>,
    events: &EventProxy,
    io: &IoControl,
) -> io::Result<()> {
    let mut interest = PollEvent::readable(0);
    // The worker owns the PTY until after deregistration, including error paths.
    unsafe {
        pty.register(&io.poll, interest, PollMode::Level)?;
    }
    let result = drive(&mut pty, term, events, io, &mut interest);
    let deregister = pty.deregister(&io.poll);
    result.and(deregister)
}

fn drive<T: EventedPty + OnResize>(
    pty: &mut T,
    term: &FairMutex<Term<EventProxy>>,
    events: &EventProxy,
    io: &IoControl,
    interest: &mut PollEvent,
) -> io::Result<()> {
    let mut polled = Events::new();
    let mut parser: Processor = Processor::new();
    let mut buffer = vec![0; IO_BUDGET];
    let mut buffered = 0;
    let mut child_exit = None;
    let mut exit_tail_bytes = 0;
    let mut writing: Option<Input> = None;
    loop {
        if io.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        // A short bounded retry also makes parser-lock contention cancellable.
        polled.clear();
        match io.poll.wait(&mut polled, Some(Duration::from_millis(5))) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => {
                result?;
            }
        }
        if let Some(size) = io.queue.lock().resize.take() {
            pty.on_resize(size);
        }
        if let Some(tty::ChildEvent::Exited(code)) = pty.next_child_event() {
            child_exit = Some(code);
            events.set_state(SessionState::Exited(code));
            io.accepting.store(false, Ordering::Release);
        }
        // Read and parse at most one budget per turn, even for an infinite writer.
        let mut read = 0;
        if buffered == 0 {
            match pty.reader().read(&mut buffer) {
                Ok(n) => {
                    buffered = n;
                    read = n;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                #[cfg(target_os = "linux")]
                Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
                Err(error) => return Err(error),
            }
        }
        if buffered > 0 {
            if let Some(mut terminal) = term.try_lock_unfair() {
                parser.advance(&mut *terminal, &buffer[..buffered]);
                if parser.sync_bytes_count() < buffered {
                    events.send_event(Event::Wakeup);
                }
                buffered = 0;
            }
        } else if let Some(mut terminal) = term.try_lock_unfair() {
            // Preserve Alacritty's synchronized-update timeout behavior.
            if parser
                .sync_timeout()
                .sync_timeout()
                .is_some_and(|t| t <= std::time::Instant::now())
            {
                parser.stop_sync(&mut *terminal);
                events.send_event(Event::Wakeup);
            }
        }
        if let Some(code) = child_exit {
            exit_tail_bytes += read;
            // The dead child's finite kernel tail fits in this allowance. A
            // descendant retaining the slave must not keep the reader forever.
            if buffered == 0 && (read == 0 || exit_tail_bytes >= INPUT_LIMIT) {
                events.set_state(SessionState::Exited(code));
                return Ok(());
            }
            continue;
        }
        let mut budget = IO_BUDGET;
        while budget > 0 {
            if writing.is_none() {
                writing = io.queue.lock().input.pop_front();
            }
            let Some(input) = &mut writing else {
                break;
            };
            let end = input.bytes.len().min(input.offset + budget);
            match pty.writer().write(&input.bytes[input.offset..end]) {
                Ok(0) => break,
                Ok(n) => {
                    input.offset += n;
                    budget -= n;
                    io.queue.lock().bytes -= n;
                    if input.offset == input.bytes.len() {
                        writing = None;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        let writable = writing.is_some() || !io.queue.lock().input.is_empty();
        if writable != interest.writable {
            interest.writable = writable;
            pty.reregister(&io.poll, *interest, PollMode::Level)?;
        }
    }
}

/// Alacritty's Unix PTY Drop sends HUP and waits indefinitely. Kill an owned,
/// unreaped child first so shells which ignore HUP cannot strand the worker.
#[cfg(unix)]
pub(crate) struct OwnedPty {
    pty: tty::Pty,
    reaped: bool,
}
#[cfg(unix)]
impl OwnedPty {
    pub fn new(pty: tty::Pty) -> Self {
        Self { pty, reaped: false }
    }
}
#[cfg(unix)]
impl Drop for OwnedPty {
    fn drop(&mut self) {
        if !self.reaped {
            // This is solely the child owned by this PTY, never a discovered process.
            unsafe {
                libc::kill(self.pty.child().id() as i32, libc::SIGKILL);
            }
        }
        // Pty::drop waits/reaps after this function returns.
    }
}
#[cfg(unix)]
impl tty::EventedReadWrite for OwnedPty {
    type Reader = <tty::Pty as tty::EventedReadWrite>::Reader;
    type Writer = <tty::Pty as tty::EventedReadWrite>::Writer;
    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        event: PollEvent,
        mode: PollMode,
    ) -> io::Result<()> {
        unsafe { self.pty.register(poll, event, mode) }
    }
    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        event: PollEvent,
        mode: PollMode,
    ) -> io::Result<()> {
        self.pty.reregister(poll, event, mode)
    }
    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.pty.deregister(poll)
    }
    fn reader(&mut self) -> &mut Self::Reader {
        self.pty.reader()
    }
    fn writer(&mut self) -> &mut Self::Writer {
        self.pty.writer()
    }
}
#[cfg(unix)]
impl EventedPty for OwnedPty {
    fn next_child_event(&mut self) -> Option<tty::ChildEvent> {
        let event = self.pty.next_child_event();
        if event.is_some() {
            self.reaped = true;
        }
        event
    }
}
#[cfg(unix)]
impl OnResize for OwnedPty {
    fn on_resize(&mut self, size: WindowSize) {
        self.pty.on_resize(size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, BackendSettings, test_support::wait};
    use std::fs::File;

    struct BrokenReader;
    impl Read for BrokenReader {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("injected reader failure"))
        }
    }
    struct BrokenPty {
        pty: OwnedPty,
        reader: BrokenReader,
        registration_error: bool,
    }
    impl tty::EventedReadWrite for BrokenPty {
        type Reader = BrokenReader;
        type Writer = File;
        unsafe fn register(
            &mut self,
            poll: &Arc<Poller>,
            event: PollEvent,
            mode: PollMode,
        ) -> io::Result<()> {
            if self.registration_error {
                return Err(io::Error::other("injected registration failure"));
            }
            unsafe { self.pty.register(poll, event, mode) }
        }
        fn reregister(
            &mut self,
            poll: &Arc<Poller>,
            event: PollEvent,
            mode: PollMode,
        ) -> io::Result<()> {
            self.pty.reregister(poll, event, mode)
        }
        fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
            self.pty.deregister(poll)
        }
        fn reader(&mut self) -> &mut BrokenReader {
            &mut self.reader
        }
        fn writer(&mut self) -> &mut File {
            self.pty.writer()
        }
    }
    impl EventedPty for BrokenPty {
        fn next_child_event(&mut self) -> Option<tty::ChildEvent> {
            self.pty.next_child_event()
        }
    }
    impl OnResize for BrokenPty {
        fn on_resize(&mut self, size: WindowSize) {
            self.pty.on_resize(size);
        }
    }

    #[test]
    fn pty_read_and_registration_errors_stop_worker_reap_child_and_persist() {
        for registration_error in [false, true] {
            let pty = tty::new(
                &tty::Options {
                    shell: Some(tty::Shell::new("/bin/sleep".into(), vec!["60".into()])),
                    ..Default::default()
                },
                WindowSize {
                    num_lines: 24,
                    num_cols: 80,
                    cell_width: 1,
                    cell_height: 1,
                },
                282,
            )
            .unwrap();
            let pid = pty.child().id() as i32;
            let mut backend = Backend::with_pty(
                BackendSettings::default(),
                BrokenPty {
                    pty: OwnedPty::new(pty),
                    reader: BrokenReader,
                    registration_error,
                },
            )
            .unwrap();
            wait(|| matches!(backend.state(), SessionState::Error(_)));
            let message = backend.status_message().unwrap();
            assert!(message.contains(if registration_error {
                "registration failure"
            } else {
                "reader failure"
            }));
            backend.take_events();
            assert_eq!(backend.status_message().as_deref(), Some(message.as_str()));
            assert!(!backend.paste("ignored"));
            backend.cancel();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
    }
}
