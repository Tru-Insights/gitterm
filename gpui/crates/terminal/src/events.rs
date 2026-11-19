use crate::session::IoControl;
use alacritty_terminal::event::{Event, EventListener};
use parking_lot::Mutex;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Clone, Default)]
pub enum Action {
    Shutdown,
    ChangeTitle(String),
    ClipboardStore(String),
    ClipboardLoad(Arc<dyn Fn(&str) -> String + Sync + Send>),
    OpenUrl(String),
    #[default]
    Ignore,
}

/// One slot per UI effect. No output bytes or growing event backlog live here.
#[derive(Default)]
pub struct PendingEvents {
    pub title: Option<String>,
    pub clipboard_store: Option<String>,
    pub bell: bool,
    pub exited: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SessionState {
    #[default]
    Running,
    Exited(Option<i32>),
    Error(String),
    Cancelled,
}
impl SessionState {
    pub fn message(&self) -> Option<String> {
        match self {
            Self::Running => None,
            Self::Exited(Some(code)) => Some(format!("Shell exited (status {code})")),
            Self::Exited(None) => Some("Shell exited (signal / status unavailable)".into()),
            Self::Error(error) => {
                Some(format!("Terminal error: {error} (exit status unavailable)"))
            }
            Self::Cancelled => Some("Session closed".into()),
        }
    }
}

#[derive(Default)]
struct Shared {
    control: OnceLock<Arc<IoControl>>,
    state: Mutex<SessionState>,
    input_rejected: AtomicBool,
    dirty: AtomicBool,
    effects_dirty: AtomicBool,
    output_ready: AtomicBool,
    revision: AtomicU64,
    pending: Mutex<PendingEvents>,
}

#[derive(Clone, Default)]
pub(crate) struct EventProxy(Arc<Shared>);

impl EventProxy {
    pub fn set_control(&self, control: Arc<IoControl>) {
        let _ = self.0.control.set(control);
    }
    pub fn state(&self) -> SessionState {
        self.0.state.lock().clone()
    }
    pub fn set_state(&self, state: SessionState) {
        let mut current = self.0.state.lock();
        if *current != SessionState::Running {
            return;
        }
        *current = state;
        drop(current);
        self.queue_effect(|pending| pending.exited = true);
        self.mark_dirty();
    }
    pub fn reject_input(&self, rejected: bool) {
        self.0.input_rejected.store(rejected, Ordering::Release);
        self.mark_dirty();
    }
    pub fn input_rejected(&self) -> bool {
        self.0.input_rejected.load(Ordering::Acquire)
    }
    pub fn is_dirty(&self) -> bool {
        self.0.dirty.load(Ordering::Acquire) || self.0.effects_dirty.load(Ordering::Acquire)
    }
    pub fn clear_dirty(&self) {
        self.0.dirty.store(false, Ordering::Release);
    }
    pub fn output_ready(&self) -> bool {
        self.0.output_ready.load(Ordering::Acquire)
    }
    pub fn revision(&self) -> u64 {
        self.0.revision.load(Ordering::Acquire)
    }
    /// Advance under the terminal lock whenever grid text/coordinates can change.
    /// Keep paint notification separate, including during deferred startup.
    pub fn invalidate(&self) {
        self.0.revision.fetch_add(1, Ordering::AcqRel);
    }
    pub fn mark_dirty(&self) {
        self.0.dirty.store(true, Ordering::Release);
    }
    pub fn record_error(&self, error: String) {
        self.set_state(SessionState::Error(error.clone()));
        if let Some(control) = self.0.control.get() {
            control.stop();
        }
        self.queue_effect(|pending| pending.error = Some(error));
        self.mark_dirty();
    }
    fn queue_effect(&self, update: impl FnOnce(&mut PendingEvents)) {
        let mut pending = self.0.pending.lock();
        update(&mut pending);
        self.0.effects_dirty.store(true, Ordering::Release);
    }
    pub fn take(&self) -> PendingEvents {
        let mut pending = self.0.pending.lock();
        self.0.effects_dirty.store(false, Ordering::Release);
        std::mem::take(&mut *pending)
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        // Called under the terminal lock, including on the parser thread. Replies
        // bypass UI delivery entirely, so hiding/pausing the view cannot lose them.
        match event {
            Event::PtyWrite(text) => {
                if let Some(control) = self.0.control.get() {
                    if !control.input(text.into_bytes().into(), true)
                        && !control.is_closed()
                        && self.state() == SessionState::Running
                    {
                        self.record_error("terminal reply queue exhausted".into());
                    }
                } else {
                    self.record_error("terminal reply before PTY initialization".into());
                }
            }
            Event::Title(title) => self.queue_effect(|pending| pending.title = Some(title)),
            Event::ResetTitle => self.queue_effect(|pending| pending.title = Some(String::new())),
            Event::Bell => self.queue_effect(|pending| pending.bell = true),
            Event::ClipboardStore(_, text) => {
                self.queue_effect(|pending| pending.clipboard_store = Some(text))
            }
            Event::ChildExit(code) => self.set_state(SessionState::Exited(Some(code))),
            Event::Exit => self.set_state(SessionState::Exited(None)),
            Event::Wakeup => self.0.output_ready.store(true, Ordering::Release),
            // Config keeps Alacritty's copy-only OSC52 policy. No clipboard reads.
            Event::ClipboardLoad(..) => {}
            _ => {}
        }
        self.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ui_effects_coalesce_and_survive_snapshot_dirty_reset() {
        let proxy = EventProxy::default();
        for i in 0..10_000 {
            proxy.send_event(Event::Title(i.to_string()));
        }
        proxy.send_event(Event::Bell);
        // A title can arrive between view event delivery and snapshot sync.
        proxy.clear_dirty();
        assert!(proxy.is_dirty());
        let pending = proxy.take();
        assert_eq!(pending.title.as_deref(), Some("9999"));
        assert!(pending.bell);
        assert!(!proxy.is_dirty());
    }
}
