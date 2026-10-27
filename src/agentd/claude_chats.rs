//! Agent-owned native Claude chats (TRU-152).
//!
//! The agent is the durable host of every chat's `claude` process, the way
//! `sessions.rs` hosts terminal sessions: a `ClaudeChat` stream attaches to
//! a chat, and the stream ending only detaches it. A chat stops when a
//! client sends `Shutdown`, when its process exits, when it has sat idle
//! (no turn running, nothing awaiting the human) with nobody attached for
//! `ChatLimits::idle_timeout`, or with the agent.
//!
//! Every event a chat relays gets the next `seq` (from 1) and goes into a
//! bounded replay buffer, so a client that lost its connection attaches
//! again "after seq N" and misses nothing; when the events after N have
//! been evicted the attach reports a gap and the client reloads the
//! history instead. What is still in force (the session's ready / mode /
//! model / effort, the running turn's start, unanswered runtime requests)
//! is kept outside the buffer and redelivered on every attach, so a prompt
//! raised while nobody was watching can still be answered.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, mpsc};

use crate::harness::claude::{ClaudeSession, ClaudeSessionConfig};
use crate::harness::{HarnessCommand, HarnessEvent};

/// Replay buffer bounds per chat. A long turn streams a few thousand
/// events (text deltas dominate), so 8192 covers a turn or several; the
/// byte cap bounds a chat whose events are large (a Write tool call
/// carrying a whole file). Whichever bound is reached first evicts.
pub const DEFAULT_BUFFER_MAX_EVENTS: usize = 8192;
pub const DEFAULT_BUFFER_MAX_BYTES: usize = 8 * 1024 * 1024;
/// How long a chat may sit idle with nobody attached before the agent
/// stops it.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
/// Live fan-out depth per chat. A stream that falls further behind
/// catches up from the replay buffer.
const LIVE_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatLimits {
    pub buffer_max_events: usize,
    pub buffer_max_bytes: usize,
    pub idle_timeout: Duration,
}

impl Default for ChatLimits {
    fn default() -> Self {
        Self {
            buffer_max_events: DEFAULT_BUFFER_MAX_EVENTS,
            buffer_max_bytes: DEFAULT_BUFFER_MAX_BYTES,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }
}

/// One relayed event: its place in the chat and its JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct SeqEvent {
    pub seq: u64,
    pub json: Arc<str>,
    /// `ProcessExited`: the last event a chat relays.
    pub exited: bool,
}

/// The events after the requested seq are no longer buffered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap;

/// The most recent events of a chat, bounded by count and by bytes of
/// JSON. The newest event always stays, even when it alone is over the
/// byte bound.
pub struct EventBuffer {
    events: VecDeque<SeqEvent>,
    bytes: usize,
    max_events: usize,
    max_bytes: usize,
    /// The highest seq evicted so far (0: none yet).
    evicted_through: u64,
}

impl EventBuffer {
    pub fn new(max_events: usize, max_bytes: usize) -> Self {
        Self {
            events: VecDeque::new(),
            bytes: 0,
            max_events: max_events.max(1),
            max_bytes,
            evicted_through: 0,
        }
    }

    pub fn push(&mut self, event: SeqEvent) {
        self.bytes += event.json.len();
        self.events.push_back(event);
        while self.events.len() > 1
            && (self.events.len() > self.max_events || self.bytes > self.max_bytes)
        {
            let Some(old) = self.events.pop_front() else {
                break;
            };
            self.bytes -= old.json.len();
            self.evicted_through = old.seq;
        }
    }

    /// Every buffered event after `after_seq`, in order, or `Gap` when one
    /// of them has already been evicted.
    pub fn after(&self, after_seq: u64) -> Result<Vec<SeqEvent>, Gap> {
        if after_seq < self.evicted_through {
            return Err(Gap);
        }
        Ok(self
            .events
            .iter()
            .filter(|event| event.seq > after_seq)
            .cloned()
            .collect())
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

/// The process behind a chat. `ClaudeSession` in the agent; a fake in
/// tests.
pub trait ChatProcess: Send + Sync {
    fn send(&self, command: HarnessCommand) -> Result<(), String>;
}

impl ChatProcess for ClaudeSession {
    fn send(&self, command: HarnessCommand) -> Result<(), String> {
        ClaudeSession::send(self, command)
    }
}

/// Starts a chat's process; its events arrive on the receiver and end
/// with `ProcessExited`.
pub type ChatSpawner = Arc<
    dyn Fn(ClaudeSessionConfig) -> (Box<dyn ChatProcess>, mpsc::UnboundedReceiver<HarnessEvent>)
        + Send
        + Sync,
>;

/// Wall-clock source for activity times and the idle reap; injectable so
/// tests can move time.
pub type ChatClock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// What a chat is doing, for `ListClaudeChats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatActivity {
    Idle,
    TurnRunning,
    AwaitingInput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatInfo {
    pub chat_id: String,
    pub claude_session_id: Option<String>,
    pub cwd: String,
    pub activity: ChatActivity,
    pub created_unix_secs: u64,
    pub last_activity_unix_secs: u64,
    pub attached_clients: u32,
}

/// What a stream learns first about the chat it attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatOpened {
    pub chat_id: String,
    /// Joined a chat that was already running.
    pub attached: bool,
    pub claude_session_id: Option<String>,
    pub head_seq: u64,
    pub gap: bool,
}

/// Events still in force, kept outside the replay buffer.
#[derive(Default)]
struct Standing {
    ready: Option<SeqEvent>,
    permission_mode: Option<SeqEvent>,
    model: Option<SeqEvent>,
    effort: Option<SeqEvent>,
    turn: Option<SeqEvent>,
    /// Unanswered runtime requests by request id, oldest first.
    requests: Vec<(String, SeqEvent)>,
}

impl Standing {
    /// The standing events at or before `through`, oldest first: the ones
    /// a stream positioned at `through` would not get from the replay.
    fn through(&self, through: u64) -> Vec<SeqEvent> {
        let mut events: Vec<SeqEvent> = [
            &self.ready,
            &self.permission_mode,
            &self.model,
            &self.effort,
            &self.turn,
        ]
        .into_iter()
        .flatten()
        .chain(self.requests.iter().map(|(_, event)| event))
        .filter(|event| event.seq <= through)
        .cloned()
        .collect();
        events.sort_by_key(|event| event.seq);
        events
    }
}

struct ChatState {
    claude_session_id: Option<String>,
    head_seq: u64,
    buffer: EventBuffer,
    standing: Standing,
    attached: u32,
    last_activity: SystemTime,
    exited: bool,
    /// The idle reap asked the process to stop.
    stopping: bool,
}

impl ChatState {
    fn activity(&self) -> ChatActivity {
        if !self.standing.requests.is_empty() {
            ChatActivity::AwaitingInput
        } else if self.standing.turn.is_some() {
            ChatActivity::TurnRunning
        } else {
            ChatActivity::Idle
        }
    }

    fn track(&mut self, chat_id: &str, event: &HarnessEvent, seq_event: &SeqEvent) {
        match event {
            HarnessEvent::Ready { session_id, .. } => {
                self.learn_session_id(chat_id, session_id.as_deref());
                self.standing.ready = Some(seq_event.clone());
            }
            HarnessEvent::TurnStarted { session_id, .. } => {
                self.learn_session_id(chat_id, session_id.as_deref());
                self.standing.turn = Some(seq_event.clone());
            }
            HarnessEvent::TurnCompleted { .. } => {
                self.standing.turn = None;
                self.standing.requests.clear();
            }
            HarnessEvent::RuntimeRequest { request_id, .. } => {
                self.standing
                    .requests
                    .push((request_id.clone(), seq_event.clone()));
            }
            HarnessEvent::RuntimeRequestResolved { request_id } => {
                self.standing.requests.retain(|(id, _)| id != request_id);
            }
            HarnessEvent::PermissionModeChanged(_) => {
                self.standing.permission_mode = Some(seq_event.clone());
            }
            HarnessEvent::ModelChanged(_) => self.standing.model = Some(seq_event.clone()),
            HarnessEvent::EffortChanged { .. } => self.standing.effort = Some(seq_event.clone()),
            HarnessEvent::ProcessExited { .. } => {
                self.exited = true;
                self.standing.turn = None;
                self.standing.requests.clear();
            }
            _ => {}
        }
    }

    fn learn_session_id(&mut self, chat_id: &str, session_id: Option<&str>) {
        let Some(id) = session_id else {
            return;
        };
        if self.claude_session_id.as_deref() != Some(id) {
            eprintln!(
                "[agent] claude_chat {chat_id}: Claude session {id} (was {:?})",
                self.claude_session_id
            );
            self.claude_session_id = Some(id.to_string());
        }
    }
}

/// One agent-owned chat.
pub struct Chat {
    chat_id: String,
    cwd: String,
    created: SystemTime,
    process: Box<dyn ChatProcess>,
    state: Mutex<ChatState>,
    live: broadcast::Sender<SeqEvent>,
    clock: ChatClock,
}

impl Chat {
    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    /// Pass a client's command to the process. Any client may send any
    /// command; `Shutdown` stops the chat for everyone.
    ///
    /// A user message is first recorded in the event stream as
    /// `UserMessageEcho` (TRU-151), so every other attached client, and
    /// any that attaches later and replays it, shows it too. It is
    /// recorded before the process gets it, so the echo always comes ahead
    /// of the message's lifecycle events and the turn it starts.
    pub fn send(&self, command: HarnessCommand) -> Result<(), String> {
        match &command {
            HarnessCommand::Shutdown => eprintln!(
                "[agent] claude_chat {}: a client asked it to shut down",
                self.chat_id
            ),
            HarnessCommand::SendUserMessage(prompt) => {
                self.ingest(&HarnessEvent::UserMessageEcho {
                    id: prompt.id.clone(),
                    text: prompt.text.clone(),
                    image_count: prompt.images.len(),
                });
            }
            _ => {}
        }
        self.lock().last_activity = (self.clock)();
        self.process.send(command)
    }

    /// Buffered events after `after_seq`, for a stream that fell behind
    /// the live fan-out.
    pub fn events_after(&self, after_seq: u64) -> Result<Vec<SeqEvent>, Gap> {
        self.lock().buffer.after(after_seq)
    }

    pub fn head_seq(&self) -> u64 {
        self.lock().head_seq
    }

    /// Whether runtime request `request_id` is still waiting for an answer
    /// (raised, and not yet resolved by any client's answer or the turn's
    /// end).
    pub fn awaits_answer(&self, request_id: &str) -> bool {
        self.lock()
            .standing
            .requests
            .iter()
            .any(|(id, _)| id == request_id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ChatState> {
        self.state.lock().expect("claude chat state poisoned")
    }

    /// Number, buffer and fan out one event from the process.
    fn ingest(&self, event: &HarnessEvent) {
        let json: Arc<str> = match serde_json::to_string(event) {
            Ok(json) => json.into(),
            Err(err) => {
                eprintln!(
                    "[agent] claude_chat {}: could not encode {event:?}: {err}; not relayed",
                    self.chat_id
                );
                return;
            }
        };
        let mut state = self.lock();
        state.head_seq += 1;
        let seq_event = SeqEvent {
            seq: state.head_seq,
            json,
            exited: matches!(event, HarnessEvent::ProcessExited { .. }),
        };
        state.track(&self.chat_id, event, &seq_event);
        state.buffer.push(seq_event.clone());
        state.last_activity = (self.clock)();
        // Sent under the lock, so an attach (which subscribes under it
        // too) sees each event either in its replay or live, never both
        // and never neither. No receivers is fine: nobody is attached.
        let _ = self.live.send(seq_event);
    }

    fn info(&self) -> Option<ChatInfo> {
        let state = self.lock();
        if state.exited {
            return None;
        }
        Some(ChatInfo {
            chat_id: self.chat_id.clone(),
            claude_session_id: state.claude_session_id.clone(),
            cwd: self.cwd.clone(),
            activity: state.activity(),
            created_unix_secs: unix_secs(self.created),
            last_activity_unix_secs: unix_secs(state.last_activity),
            attached_clients: state.attached,
        })
    }

    /// Attach a stream: `None` when the chat has already exited.
    fn attach(self: &Arc<Self>, after_seq: Option<u64>, attached: bool) -> Option<ChatAttachment> {
        let mut state = self.lock();
        if state.exited {
            return None;
        }
        let live = self.live.subscribe();
        let head = state.head_seq;
        let (gap, replay, covered) = match after_seq {
            None => (false, Vec::new(), head),
            Some(after) if after > head => {
                eprintln!(
                    "[agent] claude_chat {}: a client resumed after seq {after}, past the head {head}; treating it as a gap",
                    self.chat_id
                );
                (true, Vec::new(), head)
            }
            Some(after) => match state.buffer.after(after) {
                Ok(replay) => (false, replay, after),
                Err(Gap) => {
                    eprintln!(
                        "[agent] claude_chat {}: events after seq {after} are no longer buffered (head {head}); the client reloads history",
                        self.chat_id
                    );
                    (true, Vec::new(), head)
                }
            },
        };
        let redelivered = state.standing.through(covered);
        state.attached += 1;
        state.last_activity = (self.clock)();
        let opened = ChatOpened {
            chat_id: self.chat_id.clone(),
            attached,
            claude_session_id: state.claude_session_id.clone(),
            head_seq: head,
            gap,
        };
        drop(state);
        Some(ChatAttachment {
            chat: self.clone(),
            opened,
            redelivered,
            replay,
            live,
            position: head,
        })
    }
}

/// A stream's hold on a chat. Dropping it detaches.
pub struct ChatAttachment {
    pub chat: Arc<Chat>,
    pub opened: ChatOpened,
    /// Standing events the stream's position already covers, oldest first.
    pub redelivered: Vec<SeqEvent>,
    /// Buffered events after the stream's `after_seq`, oldest first.
    pub replay: Vec<SeqEvent>,
    pub live: broadcast::Receiver<SeqEvent>,
    /// The seq the stream is at once `replay` is sent; live events at or
    /// before it are already covered.
    pub position: u64,
}

impl Drop for ChatAttachment {
    fn drop(&mut self) {
        let mut state = self.chat.lock();
        state.attached = state.attached.saturating_sub(1);
        state.last_activity = (self.chat.clock)();
        eprintln!(
            "[agent] claude_chat {}: a client detached ({} still attached)",
            self.chat.chat_id, state.attached
        );
    }
}

/// Every chat the agent is running.
pub struct ClaudeChatRegistry {
    chats: Mutex<HashMap<String, Arc<Chat>>>,
    limits: ChatLimits,
    spawner: ChatSpawner,
    clock: ChatClock,
}

impl ClaudeChatRegistry {
    /// Chats that run the real `claude`, on the system clock.
    pub fn new(limits: ChatLimits) -> Arc<Self> {
        Self::with_parts(
            limits,
            Arc::new(|config| {
                let (session, events) = ClaudeSession::spawn(config);
                (Box::new(session) as Box<dyn ChatProcess>, events)
            }),
            Arc::new(SystemTime::now),
        )
    }

    pub fn with_parts(limits: ChatLimits, spawner: ChatSpawner, clock: ChatClock) -> Arc<Self> {
        Arc::new(Self {
            chats: Mutex::new(HashMap::new()),
            limits,
            spawner,
            clock,
        })
    }

    pub fn limits(&self) -> ChatLimits {
        self.limits
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Chat>>> {
        self.chats.lock().expect("claude chat registry poisoned")
    }

    /// Start a chat for `config`, or, when `config.resume` names the Claude
    /// session of a running chat, attach to that one: one process per
    /// conversation. Must run inside a tokio runtime (the chat's event
    /// pump is a task).
    pub fn start_or_attach(
        self: &Arc<Self>,
        config: ClaudeSessionConfig,
        after_seq: Option<u64>,
    ) -> ChatAttachment {
        let mut chats = self.lock();
        if let Some(resume) = config.resume.as_deref() {
            let running = chats.values().find(|chat| {
                let state = chat.lock();
                !state.exited && state.claude_session_id.as_deref() == Some(resume)
            });
            if let Some(chat) = running.cloned() {
                if let Some(attachment) = chat.attach(after_seq, true) {
                    eprintln!(
                        "[agent] claude_chat {}: attached a client to running Claude session {resume}",
                        chat.chat_id
                    );
                    return attachment;
                }
            }
        }

        let chat_id = uuid::Uuid::new_v4().to_string();
        eprintln!(
            "[agent] claude_chat {chat_id}: starting claude in {} (model {:?}, mode {}, resume {:?})",
            config.cwd.display(),
            config.model,
            config.permission_mode,
            config.resume
        );
        let cwd = config.cwd.to_string_lossy().into_owned();
        // A resumed session keeps its id, so it is known from the start
        // and a second start for it attaches even before Claude answers.
        let claude_session_id = config.resume.clone();
        let (process, events) = (self.spawner)(config);
        let now = (self.clock)();
        let (live, _) = broadcast::channel(LIVE_CAPACITY);
        let chat = Arc::new(Chat {
            chat_id: chat_id.clone(),
            cwd,
            created: now,
            process,
            state: Mutex::new(ChatState {
                claude_session_id,
                head_seq: 0,
                buffer: EventBuffer::new(
                    self.limits.buffer_max_events,
                    self.limits.buffer_max_bytes,
                ),
                standing: Standing::default(),
                attached: 0,
                last_activity: now,
                exited: false,
                stopping: false,
            }),
            live,
            clock: self.clock.clone(),
        });
        let attachment = chat
            .attach(after_seq, false)
            .expect("a chat that has not started cannot have exited");
        chats.insert(chat_id, chat.clone());
        drop(chats);
        tokio::spawn(pump(Arc::downgrade(self), chat, events));
        attachment
    }

    /// Attach to a running chat by id; `None` when there is none (it
    /// stopped, or the agent restarted since).
    pub fn attach(&self, chat_id: &str, after_seq: Option<u64>) -> Option<ChatAttachment> {
        let chat = self.lock().get(chat_id).cloned()?;
        chat.attach(after_seq, true)
    }

    /// The running chats, oldest first.
    pub fn list(&self) -> Vec<ChatInfo> {
        let chats: Vec<Arc<Chat>> = self.lock().values().cloned().collect();
        let mut infos: Vec<ChatInfo> = chats.iter().filter_map(|chat| chat.info()).collect();
        infos.sort_by(|a, b| {
            a.created_unix_secs
                .cmp(&b.created_unix_secs)
                .then_with(|| a.chat_id.cmp(&b.chat_id))
        });
        infos
    }

    /// Ask every chat that has been idle with nobody attached for the idle
    /// timeout to stop. Returns their ids; each leaves the registry when
    /// its process exits.
    pub fn reap_idle(&self) -> Vec<String> {
        let now = (self.clock)();
        let chats: Vec<Arc<Chat>> = self.lock().values().cloned().collect();
        let mut stopped = Vec::new();
        for chat in chats {
            let idle_for = {
                let mut state = chat.lock();
                let idle = !state.exited
                    && !state.stopping
                    && state.attached == 0
                    && state.activity() == ChatActivity::Idle;
                let idle_for = now
                    .duration_since(state.last_activity)
                    .unwrap_or(Duration::ZERO);
                if !idle || idle_for < self.limits.idle_timeout {
                    continue;
                }
                state.stopping = true;
                idle_for
            };
            eprintln!(
                "[agent] claude_chat {}: idle with no clients for {}s; stopping it",
                chat.chat_id,
                idle_for.as_secs()
            );
            if let Err(err) = chat.process.send(HarnessCommand::Shutdown) {
                eprintln!(
                    "[agent] claude_chat {}: could not ask it to stop: {err}",
                    chat.chat_id
                );
            }
            stopped.push(chat.chat_id.clone());
        }
        stopped
    }

    fn remove(&self, chat_id: &str) {
        self.lock().remove(chat_id);
    }
}

/// Feed one chat's process events into the chat until the process exits,
/// then drop the chat from the registry.
async fn pump(
    registry: Weak<ClaudeChatRegistry>,
    chat: Arc<Chat>,
    mut events: mpsc::UnboundedReceiver<HarnessEvent>,
) {
    let mut exited = false;
    while let Some(event) = events.recv().await {
        exited = matches!(event, HarnessEvent::ProcessExited { .. });
        chat.ingest(&event);
        if exited {
            break;
        }
    }
    if !exited {
        eprintln!(
            "[agent] claude_chat {}: its event channel closed without an exit; ending it",
            chat.chat_id
        );
        chat.ingest(&HarnessEvent::ProcessExited { code: None });
    }
    eprintln!(
        "[agent] claude_chat {}: claude exited; chat removed",
        chat.chat_id
    );
    if let Some(registry) = registry.upgrade() {
        registry.remove(&chat.chat_id);
    }
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn event(seq: u64, json: &str) -> SeqEvent {
        SeqEvent {
            seq,
            json: json.into(),
            exited: false,
        }
    }

    fn seqs(events: &[SeqEvent]) -> Vec<u64> {
        events.iter().map(|event| event.seq).collect()
    }

    #[test]
    fn buffer_keeps_the_newest_events_by_count() {
        let mut buffer = EventBuffer::new(3, usize::MAX);
        for seq in 1..=5 {
            buffer.push(event(seq, "{}"));
        }
        assert_eq!(buffer.len(), 3);
        assert_eq!(seqs(&buffer.after(2).unwrap()), vec![3, 4, 5]);
        assert_eq!(seqs(&buffer.after(4).unwrap()), vec![5]);
        assert!(buffer.after(5).unwrap().is_empty());
        // Seq 2 was evicted: a client that saw only 1 has a gap.
        assert_eq!(buffer.after(1), Err(Gap));
        assert_eq!(buffer.after(0), Err(Gap));
    }

    #[test]
    fn buffer_keeps_the_newest_events_by_bytes() {
        let mut buffer = EventBuffer::new(100, 10);
        buffer.push(event(1, "aaaa"));
        buffer.push(event(2, "bbbb"));
        assert_eq!((buffer.len(), buffer.bytes()), (2, 8));
        buffer.push(event(3, "cccc"));
        assert_eq!((buffer.len(), buffer.bytes()), (2, 8));
        assert_eq!(buffer.after(0), Err(Gap));
        assert_eq!(seqs(&buffer.after(1).unwrap()), vec![2, 3]);
        // One event over the byte bound evicts everything older but stays.
        buffer.push(event(4, "dddddddddddddddddddd"));
        assert_eq!((buffer.len(), buffer.bytes()), (1, 20));
        assert_eq!(buffer.after(2), Err(Gap));
        assert_eq!(seqs(&buffer.after(3).unwrap()), vec![4]);
    }

    #[test]
    fn empty_buffer_never_reports_a_gap() {
        let buffer = EventBuffer::new(4, 1024);
        assert!(buffer.is_empty());
        assert_eq!(buffer.after(0), Ok(Vec::new()));
    }

    /// A process the test drives: it records commands, and the test feeds
    /// its events.
    struct FakeProcess {
        commands: mpsc::UnboundedSender<HarnessCommand>,
    }

    impl ChatProcess for FakeProcess {
        fn send(&self, command: HarnessCommand) -> Result<(), String> {
            self.commands
                .send(command)
                .map_err(|_| "fake process gone".to_string())
        }
    }

    struct Spawned {
        resume: Option<String>,
        events: mpsc::UnboundedSender<HarnessEvent>,
        commands: mpsc::UnboundedReceiver<HarnessCommand>,
    }

    struct Harness {
        registry: Arc<ClaudeChatRegistry>,
        spawned: Arc<Mutex<Vec<Spawned>>>,
        now: Arc<Mutex<SystemTime>>,
    }

    impl Harness {
        fn new(limits: ChatLimits) -> Self {
            let spawned: Arc<Mutex<Vec<Spawned>>> = Arc::default();
            let now = Arc::new(Mutex::new(UNIX_EPOCH + Duration::from_secs(1_000_000)));
            let spawner: ChatSpawner = {
                let spawned = spawned.clone();
                Arc::new(move |config: ClaudeSessionConfig| {
                    let (event_tx, event_rx) = mpsc::unbounded_channel();
                    let (command_tx, command_rx) = mpsc::unbounded_channel();
                    spawned.lock().unwrap().push(Spawned {
                        resume: config.resume.clone(),
                        events: event_tx,
                        commands: command_rx,
                    });
                    (
                        Box::new(FakeProcess {
                            commands: command_tx,
                        }) as Box<dyn ChatProcess>,
                        event_rx,
                    )
                })
            };
            let clock: ChatClock = {
                let now = now.clone();
                Arc::new(move || *now.lock().unwrap())
            };
            Self {
                registry: ClaudeChatRegistry::with_parts(limits, spawner, clock),
                spawned,
                now,
            }
        }

        fn spawn_count(&self) -> usize {
            self.spawned.lock().unwrap().len()
        }

        fn emit(&self, process: usize, event: HarnessEvent) {
            self.spawned.lock().unwrap()[process]
                .events
                .send(event)
                .unwrap();
        }

        fn advance(&self, by: Duration) {
            *self.now.lock().unwrap() += by;
        }

        fn commands(&self, process: usize) -> Vec<HarnessCommand> {
            let mut spawned = self.spawned.lock().unwrap();
            let mut out = Vec::new();
            while let Ok(command) = spawned[process].commands.try_recv() {
                out.push(command);
            }
            out
        }
    }

    fn config(resume: Option<&str>) -> ClaudeSessionConfig {
        ClaudeSessionConfig {
            cwd: PathBuf::from("/tmp"),
            model: Some("haiku".into()),
            permission_mode: "default".into(),
            effort: None,
            resume: resume.map(str::to_string),
            wire_log_dir: None,
            mcp_servers: Vec::new(),
            chrome: false,
        }
    }

    /// Wait for the pump to have ingested through `seq`.
    async fn settle(chat: &Chat, seq: u64) {
        for _ in 0..200 {
            if chat.head_seq() >= seq {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("chat stuck at seq {} waiting for {seq}", chat.head_seq());
    }

    async fn wait_until(mut done: impl FnMut() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition not reached");
    }

    fn turn_started(session_id: &str) -> HarnessEvent {
        HarnessEvent::TurnStarted {
            session_id: Some(session_id.into()),
            model: None,
        }
    }

    fn request(id: &str) -> HarnessEvent {
        HarnessEvent::RuntimeRequest {
            request_id: id.into(),
            tool_use_id: format!("tool-{id}"),
            kind: crate::harness::RuntimeRequestKind::Permission {
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                suggestions: Vec::new(),
                decision_reason: None,
            },
        }
    }

    fn decode(event: &SeqEvent) -> HarnessEvent {
        serde_json::from_str(&event.json).unwrap()
    }

    #[tokio::test]
    async fn a_start_for_a_running_session_attaches_instead_of_spawning() {
        let h = Harness::new(ChatLimits::default());
        let first = h.registry.start_or_attach(config(None), None);
        assert!(!first.opened.attached);
        assert_eq!(first.opened.claude_session_id, None);
        h.emit(
            0,
            HarnessEvent::Ready {
                session_id: None,
                permission_mode: Some("default".into()),
                models: Vec::new(),
                modes: Vec::new(),
            },
        );
        h.emit(0, turn_started("s1"));
        settle(&first.chat, 2).await;

        // The fresh chat registered its session id when Claude reported it.
        let second = h.registry.start_or_attach(config(Some("s1")), None);
        assert!(second.opened.attached);
        assert_eq!(second.opened.chat_id, first.opened.chat_id);
        assert_eq!(second.opened.claude_session_id.as_deref(), Some("s1"));
        assert_eq!(h.spawn_count(), 1, "no second process");
        assert_eq!(h.registry.list()[0].attached_clients, 2);

        // Another conversation is another process.
        let other = h.registry.start_or_attach(config(Some("s2")), None);
        assert!(!other.opened.attached);
        assert_eq!(h.spawn_count(), 2);
        assert_eq!(h.spawned.lock().unwrap()[1].resume.as_deref(), Some("s2"));
        // A resumed chat is findable by its id before Claude answers.
        let again = h.registry.start_or_attach(config(Some("s2")), None);
        assert!(again.opened.attached);
        assert_eq!(again.opened.chat_id, other.opened.chat_id);
        assert_eq!(h.spawn_count(), 2);
        assert_eq!(h.registry.list().len(), 2);
    }

    #[tokio::test]
    async fn reattach_replays_after_seq_and_redelivers_standing_events() {
        let h = Harness::new(ChatLimits::default());
        let first = h.registry.start_or_attach(config(None), None);
        let chat_id = first.opened.chat_id.clone();
        h.emit(
            0,
            HarnessEvent::Ready {
                session_id: None,
                permission_mode: Some("default".into()),
                models: Vec::new(),
                modes: Vec::new(),
            },
        ); // 1
        h.emit(0, turn_started("s1")); // 2
        h.emit(0, HarnessEvent::TextDelta("hi".into())); // 3
        h.emit(0, request("r1")); // 4
        h.emit(0, HarnessEvent::TextDelta("more".into())); // 5
        settle(&first.chat, 5).await;
        drop(first);
        assert_eq!(h.registry.list()[0].attached_clients, 0);
        assert_eq!(h.registry.list()[0].activity, ChatActivity::AwaitingInput);

        // Saw through 3: 4 and 5 replay; ready and the turn start are
        // redelivered (the request comes in the replay).
        let back = h.registry.attach(&chat_id, Some(3)).unwrap();
        assert!(!back.opened.gap);
        assert_eq!(back.opened.head_seq, 5);
        assert_eq!(seqs(&back.replay), vec![4, 5]);
        assert_eq!(seqs(&back.redelivered), vec![1, 2]);
        assert_eq!(back.position, 5);

        // Saw through 4: the request is redelivered, 5 replays.
        let back = h.registry.attach(&chat_id, Some(4)).unwrap();
        assert_eq!(seqs(&back.replay), vec![5]);
        assert_eq!(seqs(&back.redelivered), vec![1, 2, 4]);
        assert!(matches!(
            decode(&back.redelivered[2]),
            HarnessEvent::RuntimeRequest { request_id, .. } if request_id == "r1"
        ));

        // No position: no replay, everything standing.
        let fresh = h.registry.attach(&chat_id, None).unwrap();
        assert!(fresh.replay.is_empty() && !fresh.opened.gap);
        assert_eq!(seqs(&fresh.redelivered), vec![1, 2, 4]);

        // Answered: the request is no longer standing.
        h.emit(
            0,
            HarnessEvent::RuntimeRequestResolved {
                request_id: "r1".into(),
            },
        ); // 6
        settle(&fresh.chat, 6).await;
        let after = h.registry.attach(&chat_id, None).unwrap();
        assert_eq!(seqs(&after.redelivered), vec![1, 2]);
        assert_eq!(h.registry.list()[0].activity, ChatActivity::TurnRunning);
    }

    #[tokio::test]
    async fn reattach_past_the_buffer_reports_a_gap() {
        let h = Harness::new(ChatLimits {
            buffer_max_events: 2,
            ..ChatLimits::default()
        });
        let first = h.registry.start_or_attach(config(None), None);
        let chat_id = first.opened.chat_id.clone();
        h.emit(0, turn_started("s1"));
        h.emit(0, request("r1"));
        for n in 0..4 {
            h.emit(0, HarnessEvent::TextDelta(format!("{n}")));
        }
        settle(&first.chat, 6).await;
        drop(first);

        let back = h.registry.attach(&chat_id, Some(1)).unwrap();
        assert!(back.opened.gap);
        assert!(back.replay.is_empty());
        assert_eq!(back.position, 6);
        // The unanswered request survives eviction.
        assert_eq!(seqs(&back.redelivered), vec![1, 2]);

        let ok = h.registry.attach(&chat_id, Some(4)).unwrap();
        assert!(!ok.opened.gap);
        assert_eq!(seqs(&ok.replay), vec![5, 6]);

        let bogus = h.registry.attach(&chat_id, Some(99)).unwrap();
        assert!(bogus.opened.gap, "a position past the head is a gap");
    }

    #[tokio::test]
    async fn live_events_reach_every_attached_client() {
        let h = Harness::new(ChatLimits::default());
        let mut a = h.registry.start_or_attach(config(Some("s1")), None);
        let mut b = h.registry.start_or_attach(config(Some("s1")), None);
        h.emit(0, HarnessEvent::TextDelta("x".into()));
        let from_a = a.live.recv().await.unwrap();
        let from_b = b.live.recv().await.unwrap();
        assert_eq!(from_a, from_b);
        assert_eq!(from_a.seq, 1);
        // Either client's command reaches the one process.
        a.chat.send(HarnessCommand::Interrupt).unwrap();
        b.chat.send(HarnessCommand::Interrupt).unwrap();
        assert_eq!(h.commands(0).len(), 2);
    }

    #[tokio::test]
    async fn a_sent_message_is_echoed_in_order_and_replayed_on_reattach() {
        let h = Harness::new(ChatLimits::default());
        let mut sender = h.registry.start_or_attach(config(Some("s1")), None);
        let chat_id = sender.opened.chat_id.clone();
        let mut other = h.registry.attach(&chat_id, None).unwrap();
        h.emit(0, HarnessEvent::TextDelta("before".into())); // 1
        settle(&sender.chat, 1).await;

        let image = crate::harness::ImageAttachment {
            media_type: "image/png".into(),
            data: "aGk=".into(),
        };
        let prompt = crate::harness::UserPrompt {
            text: "look at this".into(),
            images: vec![image],
            id: Some("u1".into()),
        };
        sender
            .chat
            .send(HarnessCommand::SendUserMessage(prompt.clone()))
            .unwrap(); // 2
                       // The process still gets the whole message, images and all.
        assert_eq!(h.commands(0), vec![HarnessCommand::SendUserMessage(prompt)]);
        h.emit(
            0,
            HarnessEvent::MessageLifecycle {
                id: "u1".into(),
                state: crate::harness::MessageState::Queued,
            },
        ); // 3
        settle(&sender.chat, 3).await;

        let echo = HarnessEvent::UserMessageEcho {
            id: Some("u1".into()),
            text: "look at this".into(),
            image_count: 1,
        };
        for attachment in [&mut sender, &mut other] {
            let seen: Vec<SeqEvent> = (0..3)
                .map(|_| attachment.live.try_recv().unwrap())
                .collect();
            assert_eq!(seqs(&seen), vec![1, 2, 3]);
            assert_eq!(decode(&seen[1]), echo);
            assert!(matches!(
                decode(&seen[2]),
                HarnessEvent::MessageLifecycle { .. }
            ));
        }
        // Only the count of images is relayed, never their bytes.
        assert!(!sender.chat.events_after(1).unwrap()[0]
            .json
            .contains("aGk="));

        // A client that saw through 1 gets the echo again on reattach.
        drop(other);
        let back = h.registry.attach(&chat_id, Some(1)).unwrap();
        assert_eq!(seqs(&back.replay), vec![2, 3]);
        assert_eq!(decode(&back.replay[0]), echo);
        // It is not a standing event: a fresh attach (which reads the
        // transcript) does not get it redelivered.
        let fresh = h.registry.attach(&chat_id, None).unwrap();
        assert!(fresh
            .redelivered
            .iter()
            .all(|event| !matches!(decode(event), HarnessEvent::UserMessageEcho { .. })));
    }

    #[tokio::test]
    async fn idle_chats_with_no_clients_are_reaped_after_the_timeout() {
        let h = Harness::new(ChatLimits::default());
        let attachment = h.registry.start_or_attach(config(Some("s1")), None);
        h.emit(0, turn_started("s1")); // 1
        h.emit(0, request("r1")); // 2
        settle(&attachment.chat, 2).await;
        drop(attachment);

        // Awaiting input: never reaped, however long it waits.
        h.advance(DEFAULT_IDLE_TIMEOUT * 2);
        assert!(h.registry.reap_idle().is_empty());
        h.emit(
            0,
            HarnessEvent::RuntimeRequestResolved {
                request_id: "r1".into(),
            },
        );
        h.emit(
            0,
            HarnessEvent::TurnCompleted {
                status: crate::harness::TurnStatus::Completed,
                usage: serde_json::Value::Null,
                cost_usd: None,
            },
        );
        let chat = h
            .registry
            .attach(&h.registry.list()[0].chat_id, None)
            .unwrap();
        settle(&chat.chat, 4).await;

        // Idle, but a client is attached.
        h.advance(DEFAULT_IDLE_TIMEOUT * 2);
        assert!(h.registry.reap_idle().is_empty());
        drop(chat);

        // Idle and alone, but only just.
        h.advance(DEFAULT_IDLE_TIMEOUT - Duration::from_secs(1));
        assert!(h.registry.reap_idle().is_empty());
        h.advance(Duration::from_secs(1));
        assert_eq!(h.registry.reap_idle().len(), 1);
        assert_eq!(h.commands(0), vec![HarnessCommand::Shutdown]);
        // Asked once; it leaves the registry when the process exits.
        assert!(h.registry.reap_idle().is_empty());
        assert_eq!(h.registry.list().len(), 1);
        h.emit(0, HarnessEvent::ProcessExited { code: Some(0) });
        let registry = h.registry.clone();
        wait_until(move || registry.list().is_empty()).await;
    }

    #[tokio::test]
    async fn an_exited_chat_cannot_be_attached_and_a_resume_starts_anew() {
        let h = Harness::new(ChatLimits::default());
        let attachment = h.registry.start_or_attach(config(Some("s1")), None);
        let chat_id = attachment.opened.chat_id.clone();
        let mut live = attachment.chat.live.subscribe();
        attachment.chat.send(HarnessCommand::Shutdown).unwrap();
        assert_eq!(h.commands(0), vec![HarnessCommand::Shutdown]);
        h.emit(0, HarnessEvent::ProcessExited { code: Some(0) });
        assert!(live.recv().await.unwrap().exited);
        let registry = h.registry.clone();
        wait_until(move || registry.list().is_empty()).await;
        assert!(h.registry.attach(&chat_id, None).is_none());
        let again = h.registry.start_or_attach(config(Some("s1")), None);
        assert!(!again.opened.attached);
        assert_eq!(h.spawn_count(), 2);
    }
}
