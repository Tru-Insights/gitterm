//! Workspace parking decisions over a projection of the host's sole catalog.
//!
//! Apply each effect synchronously to the roster it was decided against. The
//! host owns DTOs, processes, notes, snapshot cleanup and selection execution.
//! No effect can deliver an objective or construct a launch command.

use crate::session::{SavedIdentity, Target as SessionTarget};

/// Persisted matching deliberately uses name + directory, not runtime identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub name: String,
    pub dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub note: String,
    pub parked_at: String,
}

impl Info {
    pub fn new(note: &str, parked_at: String) -> Self {
        Self {
            note: note.trim().into(),
            parked_at,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Stop,
    KeepRunning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    NeedsYou,
    Finished,
    Failed,
}

impl WakeReason {
    pub fn for_attention(reason: crate::attention::AttentionReason) -> Option<Self> {
        use crate::attention::AttentionReason;
        match reason {
            AttentionReason::HumanInputRequired => Some(Self::NeedsYou),
            AttentionReason::CompletedUnread => Some(Self::Finished),
            AttentionReason::AgentFailed => Some(Self::Failed),
            AttentionReason::DelegationReady(_)
            | AttentionReason::DelegationFailed(_)
            | AttentionReason::DelegationBlocked(_) => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs you",
            Self::Finished => "finished",
            Self::Failed => "failed",
        }
    }
}

/// Workspace incarnation and its exact current tab bindings. A workspace stop
/// bypasses the per-tab Close policy, which would protect its last ordinary tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub uid: u64,
    pub tabs: Vec<SessionTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWorkspace {
    pub target: Target,
    pub key: Key,
    pub running: Option<Info>,
    pub remote_id: Option<String>,
    pub conversations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedWorkspace {
    pub key: Key,
    /// Includes legacy closed entries without a park note; excludes done ones.
    pub parked: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub open: Vec<OpenWorkspace>,
    pub closed: Vec<ClosedWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Park {
        uid: u64,
        mode: Mode,
        info: Info,
    },
    Return {
        uid: u64,
        woke: Option<WakeReason>,
    },
    Resume(Key),
    Done(Key),
    Close(u64),
    FilterRestore {
        remote_id: Option<String>,
        tabs: Vec<SavedIdentity>,
        active_tab: Option<usize>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Park {
        target: Target,
        mode: Mode,
        info: Info,
    },
    /// Running tabs and processes stay intact. Only manual Resume activates.
    Return {
        uid: u64,
        woke: Option<WakeReason>,
        activate: bool,
    },
    /// Remove this config, filter its saved tabs, then use the startup restore
    /// path. Identity selection remains session::SavedIdentity::open.
    RestoreStopped {
        index: usize,
    },
    /// Close snapshots and stop first, then mark the first parked key match.
    /// `finish` addresses the appended config only if no older parked match exists.
    Done {
        stop: Option<(Target, Info)>,
        finish: usize,
    },
    /// Close snapshots, park stopped, then re-run Done on the post-append roster.
    /// That lookup can stop another running workspace with the same key.
    Close {
        target: Target,
        key: Key,
    },
    FilteredRestore {
        indices: Vec<usize>,
        active_tab: Option<usize>,
    },
}

impl State {
    /// Resolve immediately before teardown; a stale incarnation or rebound tab
    /// must not stop its replacement. No separate session registry is kept.
    pub fn resolve(&self, target: &Target) -> Option<usize> {
        self.open.iter().position(|ws| &ws.target == target)
    }

    fn eligible(&self, uid: u64) -> Option<&OpenWorkspace> {
        self.open
            .iter()
            .find(|ws| ws.target.uid == uid)
            .filter(|ws| {
                ws.running.is_some()
                    || self.open.iter().filter(|ws| ws.running.is_none()).count() > 1
            })
    }

    fn running(&self, key: &Key) -> Option<&OpenWorkspace> {
        self.open
            .iter()
            .find(|ws| ws.running.is_some() && &ws.key == key)
    }

    fn parked(&self, key: &Key) -> Option<usize> {
        self.closed
            .iter()
            .position(|ws| ws.parked && &ws.key == key)
    }

    pub fn reduce(&self, intent: Intent) -> Effect {
        match intent {
            Intent::Park { uid, mode, info } => match self.eligible(uid) {
                Some(ws) => Effect::Park {
                    target: ws.target.clone(),
                    mode,
                    info,
                },
                None => Effect::None,
            },
            Intent::Close(uid) => match self.eligible(uid) {
                Some(ws) => Effect::Close {
                    target: ws.target.clone(),
                    key: ws.key.clone(),
                },
                None => Effect::None,
            },
            Intent::Return { uid, woke } => {
                if self
                    .open
                    .iter()
                    .any(|ws| ws.target.uid == uid && ws.running.is_some())
                {
                    Effect::Return {
                        uid,
                        woke,
                        activate: false,
                    }
                } else {
                    Effect::None
                }
            }
            Intent::Resume(key) => {
                if let Some(ws) = self.running(&key) {
                    Effect::Return {
                        uid: ws.target.uid,
                        woke: None,
                        activate: true,
                    }
                } else if let Some(index) = self.parked(&key) {
                    Effect::RestoreStopped { index }
                } else {
                    Effect::None
                }
            }
            Intent::Done(key) => {
                let stop = self.running(&key).map(|ws| {
                    (
                        ws.target.clone(),
                        ws.running.clone().expect("running workspace has park info"),
                    )
                });
                match self
                    .parked(&key)
                    .or_else(|| stop.as_ref().map(|_| self.closed.len()))
                {
                    Some(finish) => Effect::Done { stop, finish },
                    None => Effect::None,
                }
            }
            Intent::FilterRestore {
                remote_id,
                tabs,
                active_tab,
            } => {
                let mut indices = Vec::new();
                let mut kept_active = None;
                for (idx, tab) in tabs.iter().enumerate() {
                    if tab.chat_session_id.as_ref().is_some_and(|id| {
                        self.open
                            .iter()
                            .any(|ws| ws.remote_id == remote_id && ws.conversations.contains(id))
                    }) {
                        continue;
                    }
                    if active_tab == Some(idx) {
                        kept_active = Some(indices.len());
                    }
                    indices.push(idx);
                }
                Effect::FilteredRestore {
                    indices,
                    active_tab: kept_active,
                }
            }
        }
    }
}

/// Both stopped and running parks use Iced's closed-entry wire format.
pub fn parked_config<C>(
    mut config: crate::workspace_config::WorkspaceConfig<C>,
    info: crate::workspace_config::ParkedInfo,
) -> crate::workspace_config::WorkspaceConfig<C> {
    config.active = false;
    config.parked = Some(info);
    config
}

pub fn finish_parked<C>(
    configs: &mut [crate::workspace_config::WorkspaceConfig<C>],
    key: &Key,
    stamp: String,
) -> bool {
    if let Some(config) = configs.iter_mut().find(|c| c.is_parked() && key.matches(c)) {
        config.done_at = Some(stamp);
        true
    } else {
        false
    }
}

/// Clear Done on the first matching closed entry, making it parked again.
pub fn reopen_done<C>(
    configs: &mut [crate::workspace_config::WorkspaceConfig<C>],
    key: &Key,
) -> bool {
    if let Some(config) = configs
        .iter_mut()
        .find(|c| !c.active && c.done_at.is_some() && key.matches(c))
    {
        config.done_at = None;
        true
    } else {
        false
    }
}

impl Key {
    pub fn matches<C>(&self, config: &crate::workspace_config::WorkspaceConfig<C>) -> bool {
        self.name == config.name && self.dir == config.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State {
            open: (1..=3)
                .map(|uid| OpenWorkspace {
                    target: Target {
                        uid,
                        tabs: vec![SessionTarget {
                            tab_id: uid as usize,
                            session_uid: format!("caller-{uid}"),
                            task_id: Some("task".into()),
                            task_session_id: Some(format!("child-{uid}")),
                        }],
                    },
                    key: Key {
                        name: format!("work-{uid}"),
                        dir: "/fixture".into(),
                    },
                    running: None,
                    remote_id: None,
                    conversations: vec!["conversation".into()],
                })
                .collect(),
            closed: vec![],
        }
    }
    fn info() -> Info {
        Info::new("  note  ", "stamp".into())
    }

    #[test]
    fn park_return_restore_done_trace_and_last_bar_guard() {
        let mut state = state();
        let target = state.open[0].target.clone();
        let key = state.open[0].key.clone();
        for mode in [Mode::Stop, Mode::KeepRunning] {
            assert_eq!(
                state.reduce(Intent::Park {
                    uid: 1,
                    mode,
                    info: info()
                }),
                Effect::Park {
                    target: target.clone(),
                    mode,
                    info: info()
                }
            );
        }
        assert_eq!(info().note, "note");
        state.open[0].running = Some(info());
        assert_eq!(
            state.reduce(Intent::Return {
                uid: 1,
                woke: Some(WakeReason::NeedsYou)
            }),
            Effect::Return {
                uid: 1,
                woke: Some(WakeReason::NeedsYou),
                activate: false
            }
        );
        assert_eq!(
            state.reduce(Intent::Resume(key.clone())),
            Effect::Return {
                uid: 1,
                woke: None,
                activate: true
            }
        );
        assert_eq!(
            state.reduce(Intent::Done(key.clone())),
            Effect::Done {
                stop: Some((target.clone(), info())),
                finish: 0
            }
        );
        state.open.remove(0);
        state.closed.push(ClosedWorkspace {
            key: key.clone(),
            parked: true,
        });
        assert_eq!(
            state.reduce(Intent::Resume(key.clone())),
            Effect::RestoreStopped { index: 0 }
        );
        assert_eq!(
            state.reduce(Intent::Done(key)),
            Effect::Done {
                stop: None,
                finish: 0
            }
        );
        state.open[0].running = Some(info());
        assert_eq!(state.reduce(Intent::Close(3)), Effect::None);
        assert!(matches!(
            state.reduce(Intent::Close(2)),
            Effect::Close { .. }
        ));
        assert_eq!(state.reduce(Intent::Close(99)), Effect::None);
        assert_eq!(
            state.reduce(Intent::Return { uid: 3, woke: None }),
            Effect::None
        );
    }

    #[test]
    fn keys_choose_first_running_then_first_parked_and_close_requires_fresh_done() {
        let mut state = state();
        let key = state.open[0].key.clone();
        state.open[1].key = key.clone();
        state.open[1].running = Some(info());
        state.closed = vec![
            ClosedWorkspace {
                key: key.clone(),
                parked: false,
            },
            ClosedWorkspace {
                key: key.clone(),
                parked: true,
            },
        ];
        assert_eq!(
            state.reduce(Intent::Resume(key.clone())),
            Effect::Return {
                uid: 2,
                woke: None,
                activate: true
            }
        );
        assert_eq!(
            state.reduce(Intent::Close(1)),
            Effect::Close {
                target: state.open[0].target.clone(),
                key: key.clone()
            }
        );
        assert_eq!(
            state.reduce(Intent::Done(key.clone())),
            Effect::Done {
                stop: Some((state.open[1].target.clone(), info())),
                finish: 1
            }
        );
        state.open[1].key.dir.push_str("/different");
        assert_eq!(
            state.reduce(Intent::Resume(key)),
            Effect::RestoreStopped { index: 1 }
        );
    }

    #[test]
    fn teardown_target_revalidates_every_session_binding() {
        let state = state();
        let target = state.open[0].target.clone();
        assert_eq!(state.resolve(&target), Some(0));
        for field in 0..5 {
            let mut stale = target.clone();
            match field {
                0 => stale.uid += 10,
                1 => stale.tabs[0].tab_id += 10,
                2 => stale.tabs[0].session_uid.push('x'),
                3 => stale.tabs[0].task_id = None,
                _ => stale.tabs[0].task_session_id = None,
            }
            assert_eq!(state.resolve(&stale), None);
        }
    }

    #[test]
    fn restore_filter_maps_active_by_source_without_deduping_saved_tabs() {
        let mut state = state();
        state.open[0].running = Some(info());
        let tabs: Vec<_> = [Some("conversation"), Some("other"), Some("other"), None]
            .into_iter()
            .map(|id| SavedIdentity {
                chat_session_id: id.map(String::from),
                ..Default::default()
            })
            .collect();
        for (active, expected) in [
            (Some(0), None),
            (Some(1), Some(0)),
            (Some(2), Some(1)),
            (Some(99), None),
            (None, None),
        ] {
            assert_eq!(
                state.reduce(Intent::FilterRestore {
                    remote_id: None,
                    tabs: tabs.clone(),
                    active_tab: active
                }),
                Effect::FilteredRestore {
                    indices: vec![1, 2, 3],
                    active_tab: expected
                }
            );
        }
        assert_eq!(
            state.reduce(Intent::FilterRestore {
                remote_id: Some("remote".into()),
                tabs,
                active_tab: Some(0)
            }),
            Effect::FilteredRestore {
                indices: vec![0, 1, 2, 3],
                active_tab: Some(0)
            }
        );
    }
}
