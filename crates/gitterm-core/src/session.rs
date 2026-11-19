//! Session identity and explicit close/turn-stop decisions shared by frontends.
//!
//! The host projects facts from its sole session catalog for each intent and
//! executes the returned effect synchronously against the verified target.
//! Process handles, UUID generation, task admission and serializers stay in
//! the host. Interruption never predicts the process's eventual state.

/// Persisted bindings, independent of the config DTO's serialization contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedIdentity {
    pub session_uid: Option<String>,
    pub task_id: Option<String>,
    pub task_session_id: Option<String>,
    pub chat_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub session_uid: String,
    pub task_id: Option<String>,
    pub task_session_id: Option<String>,
    pub chat_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub identity: Identity,
    /// An older saved tab needs its allocated caller UID written once.
    pub needs_persist: bool,
}

impl SavedIdentity {
    /// Open/restore only selects identity. It cannot deliver an objective or
    /// choose a startup command. Resolve this before constructing launch env.
    pub fn open(self, fresh_uid: impl FnOnce() -> String) -> Opened {
        let needs_persist = self.session_uid.is_none();
        Opened {
            identity: Identity {
                session_uid: self.session_uid.unwrap_or_else(fresh_uid),
                task_id: self.task_id,
                task_session_id: self.task_session_id,
                chat_session_id: self.chat_session_id,
            },
            needs_persist,
        }
    }
}

/// A particular runtime incarnation, not an index or a conversation ID.
/// Restoring the same persisted bindings creates a different runtime tab ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub tab_id: usize,
    pub session_uid: String,
    pub task_id: Option<String>,
    pub task_session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnControl {
    Terminal,
    /// Interrupt a native harness; its completion event settles turn state.
    Harness,
    /// The legacy task-handle path also marks the turn stopped immediately.
    TaskHandle,
}

/// Current facts borrowed conceptually from the host's sole catalog. No
/// separate registry or process state is maintained by this reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub target: Target,
    pub control: TurnControl,
    pub task_live: bool,
    pub last_tab: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Close { target: Target, confirmed: bool },
    StopTurn { target: Target },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    ConfirmClose(Target),
    /// Explicit closure: the host shuts down remote chats before dropping
    /// their handles. This is distinct from detach or turn interruption.
    Close(Target),
    InterruptHarness(Target),
    CancelTaskTurn(Target),
}

impl State {
    pub fn reduce(&self, intent: Intent) -> Effect {
        let target = match &intent {
            Intent::Close { target, .. } | Intent::StopTurn { target } => target,
        };
        if target != &self.target {
            return Effect::None;
        }
        match intent {
            Intent::Close { target, confirmed } => {
                if self.target.task_id.is_some() {
                    if self.task_live && !confirmed {
                        Effect::ConfirmClose(target)
                    } else {
                        // The host creates a replacement tab if needed.
                        Effect::Close(target)
                    }
                } else if self.last_tab {
                    Effect::None
                } else {
                    Effect::Close(target)
                }
            }
            Intent::StopTurn { target } => match self.control {
                TurnControl::Terminal => Effect::None,
                TurnControl::Harness => Effect::InterruptHarness(target),
                TurnControl::TaskHandle => Effect::CancelTaskTurn(target),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State {
            target: Target {
                tab_id: 7,
                session_uid: "caller".into(),
                task_id: Some("task".into()),
                task_session_id: Some("child".into()),
            },
            control: TurnControl::Harness,
            task_live: true,
            last_tab: true,
        }
    }

    #[test]
    fn open_preserves_all_bindings_and_only_allocates_missing_caller() {
        let old = SavedIdentity::default().open(|| "fresh".into());
        assert!(old.needs_persist);
        assert_eq!(old.identity.session_uid, "fresh");
        let current = SavedIdentity {
            session_uid: Some("caller".into()),
            task_id: Some("task".into()),
            task_session_id: Some("child".into()),
            chat_session_id: Some("conversation".into()),
        }
        .open(|| panic!("restoring must not allocate another caller"));
        assert!(!current.needs_persist);
        assert_eq!(
            current.identity,
            Identity {
                session_uid: "caller".into(),
                task_id: Some("task".into()),
                task_session_id: Some("child".into()),
                chat_session_id: Some("conversation".into()),
            }
        );
    }

    #[test]
    fn close_confirmation_and_stop_have_distinct_effects() {
        let mut state = state();
        let target = state.target.clone();
        assert_eq!(
            state.reduce(Intent::Close {
                target: target.clone(),
                confirmed: false
            }),
            Effect::ConfirmClose(target.clone())
        );
        assert_eq!(
            state.reduce(Intent::Close {
                target: target.clone(),
                confirmed: true
            }),
            Effect::Close(target.clone())
        );
        assert_eq!(
            state.reduce(Intent::StopTurn {
                target: target.clone()
            }),
            Effect::InterruptHarness(target.clone())
        );
        state.control = TurnControl::TaskHandle;
        assert_eq!(
            state.reduce(Intent::StopTurn {
                target: target.clone()
            }),
            Effect::CancelTaskTurn(target.clone())
        );
        state.control = TurnControl::Terminal;
        assert_eq!(
            state.reduce(Intent::StopTurn {
                target: target.clone()
            }),
            Effect::None
        );
        state.task_live = false;
        assert_eq!(
            state.reduce(Intent::Close {
                target,
                confirmed: false
            }),
            Effect::Close(state.target.clone())
        );
        state.target.task_id = None;
        assert_eq!(
            state.reduce(Intent::Close {
                target: state.target.clone(),
                confirmed: true
            }),
            Effect::None
        );
        state.last_tab = false;
        assert_eq!(
            state.reduce(Intent::Close {
                target: state.target.clone(),
                confirmed: false
            }),
            Effect::Close(state.target.clone())
        );
    }

    #[test]
    fn stale_target_never_closes_or_interrupts_a_replacement() {
        let state = state();
        for field in 0..4 {
            let mut stale = state.target.clone();
            match field {
                0 => stale.tab_id += 1,
                1 => stale.session_uid = "other".into(),
                2 => stale.task_id = Some("other".into()),
                _ => stale.task_session_id = Some("other".into()),
            }
            assert_eq!(
                state.reduce(Intent::Close {
                    target: stale.clone(),
                    confirmed: true
                }),
                Effect::None
            );
            assert_eq!(
                state.reduce(Intent::StopTurn { target: stale }),
                Effect::None
            );
        }
    }
}
