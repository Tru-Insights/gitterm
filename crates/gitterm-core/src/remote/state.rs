//! Pure remote connection authority. The host executes effects; credentials and
//! transport errors never enter this state. Nothing in the Iced host calls it yet.
//!
//! Identity is the exact configured id + endpoint. The wire protocol supplies no
//! authenticated server UUID/key, so this cannot detect a server replaced behind
//! an unchanged endpoint. Names, capabilities and home directories are not identity.
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use crate::content::source::SourcePath;

/// Existing transport DTO, also available without the runtime feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgentHandshake {
    pub remote_id: String,
    pub endpoint: String,
    pub agent_name: String,
    pub agent_version: String,
    pub protocol_version: u32,
    pub capabilities: Vec<String>,
    /// Opaque agent user's home; empty from agents predating this field.
    pub home_dir: String,
}

/// Safe domain failures, never raw transport messages or credential references.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    InvalidIdentity,
    IdentityChanged,
    NotConfigured,
    HandshakeFailed,
}

/// No auth, token reference or client config. Endpoints containing URL credentials,
/// queries or fragments are refused rather than retained in Debug/state/effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    remote_id: String,
    endpoint: String,
}
impl Identity {
    pub fn new(remote_id: String, endpoint: String) -> Result<Self, Failure> {
        let url = url::Url::parse(&endpoint).map_err(|_| Failure::InvalidIdentity)?;
        if remote_id.trim().is_empty()
            || !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Failure::InvalidIdentity);
        }
        Ok(Self {
            remote_id,
            endpoint,
        })
    }
    pub fn remote_id(&self) -> &str {
        &self.remote_id
    }
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub identity: Identity,
    pub name: String,
}

// Process-wide tickets also reject results from a removed/recreated State.
static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    pub identity: Identity,
    generation: u64,
}
impl Stamp {
    fn new(identity: Identity) -> Self {
        // A compare/exchange loop supports both host toolchains; fetch_update
        // is deprecated on stable 1.99, while its replacement is absent on 1.95.
        let generation = loop {
            let current = NEXT_ATTEMPT.load(Ordering::Relaxed);
            let next = current
                .checked_add(1)
                .expect("remote connection attempt generation exhausted");
            if NEXT_ATTEMPT
                .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break current;
            }
        };
        Self {
            identity,
            generation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Disconnected,
    Connecting,
    Connected(RemoteAgentHandshake),
    Error(Failure),
}

#[derive(Debug)]
pub struct Connection {
    pub target: Target,
    pub status: Status,
    pub last_attempt: Option<Instant>,
    pub last_connected: Option<Instant>,
    current: Option<Stamp>,
    pending: bool,
}
impl Connection {
    pub fn handshake(&self) -> Option<&RemoteAgentHandshake> {
        match &self.status {
            Status::Connected(handshake) => Some(handshake),
            _ => None,
        }
    }
    /// Unknown names, order and duplicates remain exactly as returned by the agent.
    pub fn has_capability(&self, capability: &str) -> bool {
        self.handshake()
            .is_some_and(|h| h.capabilities.iter().any(|c| c == capability))
    }
    pub fn home(&self) -> Option<SourcePath> {
        self.handshake()
            .filter(|h| !h.home_dir.is_empty())
            .map(|h| SourcePath::Remote(h.home_dir.clone()))
    }
}

#[derive(Debug)]
pub enum Intent {
    /// Register defaults or update a display name, without changing identity.
    Register(Target),
    Ensure {
        remote_id: String,
        force: bool,
        now: Instant,
    },
    /// Map all unstructured client errors to HandshakeFailed at the host boundary.
    /// Do not pass raw error strings or classify them by parsing their messages.
    HandshakeFinished {
        stamp: Stamp,
        result: Result<RemoteAgentHandshake, HandshakeFailed>,
        now: Instant,
    },
    Disconnect {
        remote_id: String,
    },
    Remove {
        remote_id: String,
    },
}

/// Intentionally carries no strings from a transport or credential resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakeFailed;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Render,
    /// Resolve credentials outside the reducer, only after matching BOTH id and
    /// endpoint against current host config. Return this exact stamp unchanged.
    Handshake(Stamp),
    /// Accepted completion only. The adapter must:
    /// - refresh files/git on success only for the active matching remote;
    /// - close only the matching saved-id connect form, only on success;
    /// - notify the matching picker on success OR failure (clear awaiting_home);
    /// - request running chats only with the fresh persistent-chat capability.
    ///
    /// Delayed dispatch must first check State::is_current; focus cannot retarget it.
    Completed {
        stamp: Stamp,
        connected: bool,
    },
    Rejected {
        remote_id: String,
        reason: Failure,
    },
}

#[derive(Debug, Default)]
pub struct State {
    connections: BTreeMap<String, Connection>,
    // Authority tombstones, not a config cache: removing/re-adding an id cannot
    // evade its endpoint binding. A different machine needs a new saved id.
    bindings: BTreeMap<String, Identity>,
}
impl State {
    pub fn connection(&self, remote_id: &str) -> Option<&Connection> {
        self.connections.get(remote_id)
    }
    pub fn is_current(&self, stamp: &Stamp) -> bool {
        self.connection(stamp.identity.remote_id())
            .is_some_and(|c| c.current.as_ref() == Some(stamp))
    }
    pub fn accepts(&self, stamp: &Stamp) -> bool {
        self.is_current(stamp)
            && self
                .connection(stamp.identity.remote_id())
                .is_some_and(|c| c.pending)
    }
    pub fn reduce(&mut self, intent: Intent) -> Vec<Effect> {
        match intent {
            Intent::Register(target) => {
                let id = target.identity.remote_id().to_string();
                if self
                    .bindings
                    .get(&id)
                    .is_some_and(|bound| *bound != target.identity)
                {
                    return vec![Effect::Rejected {
                        remote_id: id,
                        reason: Failure::IdentityChanged,
                    }];
                }
                self.bindings.insert(id.clone(), target.identity.clone());
                if let Some(connection) = self.connections.get_mut(&id) {
                    if connection.target == target {
                        return Vec::new();
                    }
                    connection.target = target;
                } else {
                    self.connections.insert(
                        id,
                        Connection {
                            target,
                            status: Status::Disconnected,
                            last_attempt: None,
                            last_connected: None,
                            current: None,
                            pending: false,
                        },
                    );
                }
                vec![Effect::Render]
            }
            Intent::Ensure {
                remote_id,
                force,
                now,
            } => {
                let Some(connection) = self.connections.get_mut(&remote_id) else {
                    return vec![Effect::Rejected {
                        remote_id,
                        reason: Failure::NotConfigured,
                    }];
                };
                if !force && matches!(connection.status, Status::Connecting | Status::Connected(_))
                {
                    return Vec::new();
                }
                let stamp = Stamp::new(connection.target.identity.clone());
                connection.status = Status::Connecting;
                connection.last_attempt = Some(now);
                // Preserve the current handler's reset on an actual retry.
                connection.last_connected = None;
                connection.current = Some(stamp.clone());
                connection.pending = true;
                vec![Effect::Render, Effect::Handshake(stamp)]
            }
            Intent::HandshakeFinished { stamp, result, now } => {
                if !self.accepts(&stamp) {
                    return Vec::new();
                }
                if result.as_ref().is_ok_and(|h| {
                    h.remote_id != stamp.identity.remote_id || h.endpoint != stamp.identity.endpoint
                }) {
                    // Do not consume the legitimate outstanding attempt.
                    return Vec::new();
                }
                let connection = self
                    .connections
                    .get_mut(stamp.identity.remote_id())
                    .unwrap();
                connection.pending = false;
                connection.last_attempt = Some(now);
                let connected = result.is_ok();
                connection.status = match result {
                    Ok(handshake) => {
                        connection.last_connected = Some(now);
                        Status::Connected(handshake)
                    }
                    Err(HandshakeFailed) => Status::Error(Failure::HandshakeFailed),
                };
                vec![Effect::Render, Effect::Completed { stamp, connected }]
            }
            Intent::Disconnect { remote_id } => {
                let Some(connection) = self.connections.get_mut(&remote_id) else {
                    return Vec::new();
                };
                connection.current = None;
                connection.pending = false;
                connection.status = Status::Disconnected;
                vec![Effect::Render]
            }
            Intent::Remove { remote_id } => {
                if self.connections.remove(&remote_id).is_some() {
                    vec![Effect::Render]
                } else {
                    Vec::new()
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
