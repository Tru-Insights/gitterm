//! Shared plan-limit observations, notices and serialized read/save transitions.
//! Hosts own launch authority, provider counts, recipients and delivery acceptance.
//! Jobs require explicit paths; enabling runtime does not authorize service startup.
use crate::chat::types::RateLimitReading;
use crate::plan_limits::{self, PlanLimits, PlanLimitsFile};
use std::path::Path;
#[cfg(feature = "runtime")]
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const CODEX_READ_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub enum PlanLimitsEvent {
    /// A Codex read finished: the newest account-wide reading, if any.
    CodexRead(Option<Box<PlanLimits>>),
    PinChanged(Option<plan_limits::RoutingPin>),
    Saved(Result<(), String>),
}

#[derive(Debug, Default)]
pub struct PlanLimitsState {
    /// The last reading of each provider (`plan-limits.json`).
    file: PlanLimitsFile,
    codex_in_flight: bool,
    codex_read_at: Option<Instant>,
    /// A change not yet written.
    dirty: bool,
    save_in_flight: bool,
}

impl PlanLimitsState {
    pub fn file(&self) -> &PlanLimitsFile {
        &self.file
    }

    pub fn evaluation_for_launch(&self) -> plan_limits::PolicyEvaluation {
        plan_limits::evaluate_policy(&self.file, chrono::Utc::now().timestamp())
    }

    pub fn evaluate(&mut self, chats: &[String], now: i64) {
        if self.file.prune_history(now) {
            self.dirty = true;
        }
        let evaluation = plan_limits::evaluate_policy(&self.file, now);
        if self.file.pin != evaluation.pin {
            self.file.pin = evaluation.pin.clone();
            self.dirty = true;
        }
        if self.file.policy.as_ref() != Some(&evaluation.policy) {
            self.file.policy = Some(evaluation.policy);
            self.dirty = true;
        }
        self.queue_notices(chats, &evaluation.notices);
    }

    pub fn queue_notices(&mut self, chats: &[String], lines: &[String]) {
        let before = self.file.pending_notices.len();
        self.file
            .pending_notices
            .retain(|chat, _| chats.contains(chat));
        self.dirty |= before != self.file.pending_notices.len();
        if lines.is_empty() {
            return;
        }
        for chat in chats {
            let pending = self.file.pending_notices.entry(chat.clone()).or_default();
            if lines.iter().any(|line| line.starts_with("Routing:")) {
                let before = pending.len();
                pending.retain(|line| !line.starts_with("Routing:"));
                self.dirty |= before != pending.len();
            }
            for line in lines {
                if !pending.contains(line) {
                    pending.push(line.clone());
                    self.dirty = true;
                }
            }
        }
    }

    pub fn notice(&self, chat: &str, busy: bool) -> Option<String> {
        if busy {
            return None;
        }
        self.file.pending_notices.get(chat).map(|lines| {
            format!("GitTerm plan limits: {}\nRelay this heads-up to the user and follow `plan_limits` routing for new work. An explicit user model choice wins; never restart running workers to change routing.", lines.join("\n"))
        })
    }

    pub fn finish_notice(&mut self, chat: &str, accepted: bool) {
        if accepted {
            self.dirty |= self.file.pending_notices.remove(chat).is_some();
        } else {
            eprintln!("[plan-limits] coordinator {chat} did not accept the notice; retaining it for retry");
        }
    }

    /// The persisted readings from the config dir (a small read).
    pub fn load(config_dir: &Path) -> Self {
        Self {
            file: PlanLimitsFile::load(&PlanLimitsFile::path_in(config_dir)),
            ..Self::default()
        }
    }

    pub fn finish_save(&mut self, result: Result<(), String>, path: &Path) -> bool {
        self.save_in_flight = false;
        if let Err(error) = result {
            eprintln!("[plan-limits] not saved to {}: {error}", path.display());
            self.dirty = true;
            return false;
        }
        true
    }
    pub fn observe_claude_rate_limit(&mut self, reading: &RateLimitReading) -> bool {
        let mut limits = plan_limits::claude_limits(&reading.info, reading.observed_at);
        limits.local = Some(true);
        self.absorb(limits)
    }

    fn absorb(&mut self, limits: PlanLimits) -> bool {
        let changed = self.file.absorb(limits);
        self.dirty |= changed;
        changed
    }

    pub fn codex_read_due(&self, now: Instant) -> bool {
        self.codex_read_at
            .is_none_or(|at| now.duration_since(at) >= CODEX_READ_EVERY)
    }

    pub fn codex_read_in_flight(&self) -> bool {
        self.codex_in_flight
    }

    /// Record attempts even when source discovery failed, without starting a job.
    #[cfg(feature = "runtime")]
    pub fn begin_codex_read(
        &mut self,
        sessions_root: Option<PathBuf>,
        now: Instant,
    ) -> Option<CodexReadJob> {
        if self.codex_in_flight {
            return None;
        }
        self.codex_read_at = Some(now);
        let Some(sessions_root) = sessions_root else {
            eprintln!("[plan-limits] cannot find the home directory; Codex limits not read");
            return None;
        };
        self.codex_in_flight = true;
        Some(CodexReadJob { sessions_root })
    }

    /// Failure/absence clears the read slot while keeping the previous observation.
    pub fn finish_codex_read(&mut self, limits: Option<Box<PlanLimits>>) -> bool {
        self.codex_in_flight = false;
        limits.is_some_and(|limits| self.absorb(*limits))
    }

    pub fn set_pin(&mut self, pin: Option<plan_limits::RoutingPin>) -> bool {
        if self.file.pin == pin {
            return false;
        }
        self.file.pin = pin;
        self.dirty = true;
        true
    }

    /// Snapshot before handing the save to the runner. Later changes stay dirty.
    /// A successful completion permits immediate draining; a failure is retried
    /// by a later tick, never by the completion handler itself.
    #[cfg(feature = "runtime")]
    pub fn begin_save(&mut self, path: PathBuf) -> Option<SaveJob> {
        if !self.dirty || self.save_in_flight {
            return None;
        }
        self.dirty = false;
        self.save_in_flight = true;
        Some(SaveJob {
            file: self.file.clone(),
            path,
        })
    }
}

#[cfg(feature = "runtime")]
pub struct CodexReadJob {
    sessions_root: PathBuf,
}

#[cfg(feature = "runtime")]
impl CodexReadJob {
    pub async fn run(self) -> PlanLimitsEvent {
        let result = tokio::task::spawn_blocking(move || {
            plan_limits::latest_codex_limits(&self.sessions_root)
        })
        .await;
        let limits = match result {
            Ok(limits) => limits,
            Err(error) => {
                eprintln!("[plan-limits] Codex read task failed: {error}");
                None
            }
        };
        PlanLimitsEvent::CodexRead(limits.map(Box::new))
    }
}

#[cfg(feature = "runtime")]
pub struct SaveJob {
    file: PlanLimitsFile,
    path: PathBuf,
}

#[cfg(feature = "runtime")]
impl SaveJob {
    pub async fn run(self) -> PlanLimitsEvent {
        let result = tokio::task::spawn_blocking(move || self.file.save(&self.path))
            .await
            .unwrap_or_else(|error| Err(format!("plan limits save task failed: {error}")));
        PlanLimitsEvent::Saved(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_limits::Provider;
    fn now_unix() -> i64 {
        chrono::Utc::now().timestamp()
    }
    #[test]
    fn notices_are_deduplicated_per_chat_grouped_and_held_until_idle() {
        let mut state = PlanLimitsState::default();
        let chats = vec!["first".into(), "second".into()];
        let lines = vec!["hot".into(), "routing changed".into()];
        state.queue_notices(&chats, &lines);
        state.queue_notices(&chats, &lines);
        assert!(state.notice("first", true).is_none());
        let second = state.notice("second", false).unwrap();
        assert_eq!(second.matches("hot").count(), 1);
        assert!(second.contains("routing changed"));
        state.finish_notice("second", false);
        assert!(state.notice("second", false).is_some());
        state.finish_notice("second", true);
        assert!(state.notice("second", false).is_none());
        assert!(state.notice("first", false).is_some());
        state.finish_notice("first", true);
        assert!(state.notice("first", false).is_none());
        state.queue_notices(&chats, &["Routing: hold_claude".into()]);
        state.queue_notices(&chats, &["Routing: balanced".into()]);
        let prompt = state.notice("first", false).unwrap();
        assert!(!prompt.contains("hold_claude"));
        assert!(prompt.contains("Routing: balanced"));
    }

    #[test]
    fn setting_and_clearing_a_pin_queue_a_routing_nudge() {
        let mut state = PlanLimitsState::default();
        let chats = vec!["coordinator".into()];
        let pin = plan_limits::RoutingPin {
            provider: Provider::Codex,
            until: now_unix() + 3600,
        };
        state.queue_notices(
            &chats,
            &[plan_limits::pin_change_notice(
                Some(&pin),
                now_unix(),
                &plan_limits::evaluate_policy(&state.file, now_unix())
                    .report
                    .routing,
            )],
        );
        assert!(state
            .notice("coordinator", false)
            .unwrap()
            .contains("pin to Codex set"));
        state.queue_notices(
            &chats,
            &[plan_limits::pin_change_notice(
                None,
                now_unix(),
                &plan_limits::evaluate_policy(&state.file, now_unix())
                    .report
                    .routing,
            )],
        );
        let notice = state.notice("coordinator", false).unwrap();
        assert!(notice.contains("pin cleared."));
        assert!(!notice.contains("pin to Codex set"));
    }

    fn claude_reading(used: f64, now: i64) -> PlanLimits {
        let mut reading = plan_limits::claude_limits(
            &serde_json::json!({
                "unifiedWindows": {"seven_day": {"utilization": used / 100.0, "resetsAt": now + 5 * 86400}}
            }),
            now,
        );
        reading.local = Some(true);
        reading
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn pending_hot_and_hold_notices_survive_restart_and_acknowledgement() {
        for used in [35.0, 95.0] {
            let now = now_unix();
            let dir = tempfile::tempdir().unwrap();
            let path = PlanLimitsFile::path_in(dir.path());
            let mut state = PlanLimitsState::default();
            let chats = vec!["coordinator".into(), "other".into()];
            state.file.absorb(claude_reading(used, now));
            state.evaluate(&chats, now);
            assert!(state.notice("coordinator", true).is_none());
            let expected = state.notice("coordinator", false).unwrap();
            state.file.save(&path).unwrap();
            let mut resumed = PlanLimitsState::load(dir.path());
            resumed.evaluate(&chats, now);
            assert_eq!(resumed.notice("coordinator", false), Some(expected.clone()));
            resumed.finish_notice("coordinator", false);
            assert_eq!(resumed.notice("coordinator", false), Some(expected.clone()));
            resumed.finish_notice("coordinator", true);
            resumed.file.save(&path).unwrap();
            let mut acknowledged = PlanLimitsState::load(dir.path());
            acknowledged.evaluate(&chats, now);
            assert!(acknowledged.notice("coordinator", false).is_none());
            assert_eq!(acknowledged.notice("other", false), Some(expected));
        }
    }

    #[test]
    fn pin_under_hold_keeps_the_effective_checkpoint_and_handoff_notice() {
        let now = now_unix();
        let chats = vec!["coordinator".into()];
        let mut state = PlanLimitsState::default();
        state.file.absorb(claude_reading(95.0, now));
        state.evaluate(&chats, now);
        state.file.pin = Some(plan_limits::RoutingPin {
            provider: Provider::Claude,
            until: now + 3600,
        });
        state.evaluate(&chats, now);
        state.queue_notices(
            &chats,
            &[plan_limits::pin_change_notice(
                state.file.pin.as_ref(),
                now,
                &plan_limits::evaluate_policy(&state.file, now)
                    .report
                    .routing,
            )],
        );
        let prompt = state.notice("coordinator", false).unwrap();
        assert!(prompt.contains("start nothing new on Claude"));
        assert!(prompt.contains("writes checkpoint and handoff"));
        state.finish_notice("coordinator", true);
        state.file.pin = None;
        state.evaluate(&chats, now);
        state.queue_notices(
            &chats,
            &[plan_limits::pin_change_notice(
                None,
                now,
                &plan_limits::evaluate_policy(&state.file, now)
                    .report
                    .routing,
            )],
        );
        let cleared = state.notice("coordinator", false).unwrap();
        assert!(cleared.contains("pin cleared."));
        assert!(cleared.contains("writes checkpoint and handoff"));
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn notice_changes_during_an_in_flight_save_remain_dirty() {
        let chats = vec!["coordinator".into()];
        let mut state = PlanLimitsState::default();
        state.queue_notices(&chats, &["Routing: hold_codex".into()]);
        let snapshot = state.file.clone();
        state.dirty = false;
        state.save_in_flight = true;
        state.finish_notice("coordinator", true);
        let dir = tempfile::tempdir().unwrap();
        let path = PlanLimitsFile::path_in(dir.path());
        snapshot.save(&path).unwrap();
        assert!(state.finish_save(Ok(()), &path));
        assert!(state.dirty);
        state.file.save(&path).unwrap();
        assert!(PlanLimitsState::load(dir.path())
            .notice("coordinator", false)
            .is_none());
        state.queue_notices(&chats, &["Routing: balanced".into()]);
        state.dirty = false;
        state.queue_notices(&[], &[]);
        assert!(state.dirty);
        assert!(state.file.pending_notices.is_empty());
    }

    #[test]
    fn failed_save_restores_dirty_state_for_a_later_tick() {
        let mut state = PlanLimitsState {
            dirty: false,
            save_in_flight: true,
            ..PlanLimitsState::default()
        };

        assert!(!state.finish_save(Err("disk full".into()), Path::new("plan-limits.json")));
        assert!(state.dirty);
        assert!(!state.save_in_flight);
    }
    #[cfg(feature = "runtime")]
    #[test]
    fn read_slot_is_single_and_failed_read_preserves_observation_and_cadence() {
        let root = tempfile::tempdir().unwrap();
        let mut state = PlanLimitsState::default();
        state.absorb(claude_reading(40.0, now_unix()));
        let previous = state.file.clone();
        let at = Instant::now();
        assert!(state.codex_read_due(at));
        assert!(state
            .begin_codex_read(Some(root.path().to_path_buf()), at)
            .is_some());
        assert!(state
            .begin_codex_read(Some(root.path().to_path_buf()), at)
            .is_none());
        assert!(!state.codex_read_due(at + Duration::from_secs(59)));
        assert!(!state.finish_codex_read(None));
        assert!(!state.codex_read_in_flight());
        assert_eq!(state.file, previous);
        assert!(state.codex_read_due(at + CODEX_READ_EVERY));
        // Missing source discovery is an attempted poll, without an in-flight job.
        assert!(state
            .begin_codex_read(None, at + CODEX_READ_EVERY)
            .is_none());
        assert!(!state.codex_read_in_flight());
        assert!(!state.codex_read_due(at + CODEX_READ_EVERY));
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn runner_saves_snapshots_drains_concurrent_changes_and_retries_failures() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = PlanLimitsFile::path_in(dir.path());
        let chats = vec!["coordinator".into()];
        let mut state = PlanLimitsState::default();
        assert!(state.begin_save(path.clone()).is_none());
        state.queue_notices(&chats, &["Routing: hold_codex".into()]);
        let first = state.begin_save(path.clone()).unwrap();
        assert!(state.begin_save(path.clone()).is_none());
        state.finish_notice("coordinator", true);
        let PlanLimitsEvent::Saved(result) = runtime.block_on(first.run()) else {
            panic!("expected save completion")
        };
        assert!(result.is_ok());
        assert!(state.finish_save(result, &path));
        assert!(PlanLimitsState::load(dir.path())
            .notice("coordinator", false)
            .is_some());
        // The completion handler drains changes made after the first snapshot.
        let second = state.begin_save(path.clone()).unwrap();
        let PlanLimitsEvent::Saved(result) = runtime.block_on(second.run()) else {
            panic!("expected save completion")
        };
        assert!(result.is_ok());
        assert!(state.finish_save(result, &path));
        assert!(state.begin_save(path.clone()).is_none());
        assert!(PlanLimitsState::load(dir.path())
            .notice("coordinator", false)
            .is_none());

        state.queue_notices(&chats, &["Routing: balanced".into()]);
        let blocker = dir.path().join("a-file");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let failed = state.begin_save(blocker.join("limits.json")).unwrap();
        let PlanLimitsEvent::Saved(result) = runtime.block_on(failed.run()) else {
            panic!("expected save completion")
        };
        assert!(result.is_err());
        assert!(!state.finish_save(result, &path));
        // A later tick retries; failure completion itself does not spin.
        assert!(state.begin_save(path).is_some());
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn read_job_uses_explicit_source_and_newest_observation_wins() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut state = PlanLimitsState::default();
        let job = state
            .begin_codex_read(Some(dir.path().to_path_buf()), Instant::now())
            .unwrap();
        let PlanLimitsEvent::CodexRead(limits) = runtime.block_on(job.run()) else {
            panic!("expected read completion")
        };
        assert!(!state.finish_codex_read(limits));
        assert!(state.file.codex.is_none());
        let limits = plan_limits::codex_limits(
            &serde_json::json!({"limit_id":"codex","primary":{"used_percent":23,"window_minutes":300}}),
            200,
        )
        .unwrap();
        assert!(state.finish_codex_read(Some(Box::new(limits.clone()))));
        let mut older = limits.clone();
        older.observed_at -= 1;
        assert!(!state.finish_codex_read(Some(Box::new(older))));
        assert_eq!(state.file.codex, Some(limits));
    }
}
