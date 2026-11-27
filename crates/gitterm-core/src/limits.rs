//! Shared plan-limit observations, Usage badge and serialized read/save transitions.
//! Hosts own launch authority and provider counts; limits never prompt chats.
//! Jobs require explicit paths; enabling runtime does not authorize service startup.
use crate::chat::types::RateLimitReading;
use crate::plan_limits::{self, PlanLimits, PlanLimitsFile};
use std::path::Path;
#[cfg(feature = "runtime")]
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Same local rollout source for both frontends; never a credential probe.
#[cfg(feature = "runtime")]
pub fn local_codex_sessions() -> Option<PathBuf> {
    dirs::home_dir().map(|home| crate::usage::UsageSources::under(&home).codex_sessions)
}

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
        self.dirty |= self
            .file
            .usage_badge
            .observe(&evaluation.policy, &evaluation.report, now);
        if self.file.policy.as_ref() != Some(&evaluation.policy) {
            self.file.policy = Some(evaluation.policy);
            self.dirty = true;
        }
        self.queue_notices(chats, &evaluation.notices);
    }

    /// Legacy host API: plan-limit messages must never enter a chat queue.
    pub fn queue_notices(&mut self, _chats: &[String], _lines: &[String]) {
        self.dirty |= !self.file.pending_notices.is_empty();
        self.file.pending_notices.clear();
    }

    pub fn notice(&self, _chat: &str, _busy: bool) -> Option<String> {
        None
    }

    pub fn finish_notice(&mut self, _chat: &str, _accepted: bool) {
        self.queue_notices(&[], &[]);
    }

    /// The persisted readings from the config dir (a small read).
    pub fn load(config_dir: &Path) -> Self {
        Self {
            file: PlanLimitsFile::load(&PlanLimitsFile::path_in(config_dir)),
            // Rewrite legacy files without their pending chat prompts.
            dirty: true,
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

    /// Iced's periodic observation, including failed-source attempt cadence.
    #[cfg(feature = "runtime")]
    pub fn begin_scheduled_codex_read(&mut self, now: Instant) -> Option<CodexReadJob> {
        if !self.codex_read_due(now) {
            return None;
        }
        self.begin_codex_read(local_codex_sessions(), now)
    }

    /// Shared completion transition. True asks the host to drain another
    /// serialized save; failed writes wait for a later poll to retry.
    pub fn complete_event(&mut self, event: PlanLimitsEvent, path: &Path, now: i64) -> bool {
        let changed = match event {
            PlanLimitsEvent::CodexRead(limits) => self.finish_codex_read(limits),
            PlanLimitsEvent::Saved(result) => return self.finish_save(result, path),
            PlanLimitsEvent::PinChanged(pin) => self.set_pin(pin),
        };
        if changed {
            self.evaluate(&[], now);
        }
        changed
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
    fn no_notice_kind_can_be_queued_or_delivered_to_any_chat() {
        let mut state = PlanLimitsState::default();
        let chats = vec!["first".into(), "second".into()];
        let lines = vec![
            "running hot".into(),
            "urgent".into(),
            "Both plans are running hot".into(),
            "Routing: hold_claude".into(),
            "weekly window reset; usage is unknown".into(),
            "pin cleared".into(),
        ];
        state.queue_notices(&chats, &lines);
        state.queue_notices(&chats, &lines);
        for chat in chats {
            for busy in [false, true] {
                assert!(state.notice(&chat, busy).is_none());
            }
            state.finish_notice(&chat, false);
            state.finish_notice(&chat, true);
        }
        assert!(state.file.pending_notices.is_empty());
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

    #[test]
    fn legacy_pending_notices_are_dropped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = PlanLimitsFile::path_in(dir.path());
        std::fs::write(
            &path,
            br#"{"pin":{"provider":"codex","until":1800003600},"pending_notices":{"first":["hot"],"second":["Routing: hold_codex"]}}"#,
        )
        .unwrap();
        let loaded = PlanLimitsFile::load(&path);
        assert!(loaded.pending_notices.is_empty());
        assert_eq!(
            loaded.pin.as_ref().map(|pin| pin.provider),
            Some(Provider::Codex)
        );
        let mut resumed = PlanLimitsState::load(dir.path());
        assert!(resumed.file.pending_notices.is_empty());
        resumed.evaluate(&["first".into(), "second".into()], now_unix());
        assert!(resumed.notice("first", false).is_none());
        assert!(resumed.notice("second", false).is_none());
        assert!(!serde_json::to_string(resumed.file())
            .unwrap()
            .contains("pending_notices"));
    }

    #[test]
    fn hot_hold_pin_and_reset_evaluations_never_queue_chat_notices() {
        let now = now_unix();
        let chats = vec!["first".into(), "second".into()];
        for used in [35.0, 95.0] {
            let mut state = PlanLimitsState::default();
            state.absorb(claude_reading(used, now));
            let mut codex = claude_reading(used, now);
            codex.provider = Provider::Codex;
            state.absorb(codex);
            assert!(!plan_limits::evaluate_policy(state.file(), now)
                .notices
                .is_empty());
            state.evaluate(&chats, now);
            state.set_pin(Some(plan_limits::RoutingPin {
                provider: Provider::Claude,
                until: now + 3600,
            }));
            state.evaluate(&chats, now);
            if used >= 95.0 {
                assert_eq!(
                    state.evaluation_for_launch().report.routing.state,
                    plan_limits::RoutingState::Conserve
                );
            }
            state.set_pin(None);
            state.evaluate(&chats, now);
            let mut reset = claude_reading(used, now + 60);
            reset.windows[0].used_percent = None;
            state.absorb(reset);
            state.evaluate(&chats, now + 60);
            assert!(state.file.pending_notices.is_empty());
            for chat in &chats {
                assert!(state.notice(chat, false).is_none());
            }
        }
    }

    #[test]
    fn drifting_hot_estimates_keep_one_badge_and_never_prompt_chats() {
        let now = now_unix();
        let mut state = PlanLimitsState::default();
        let chats = vec!["first".into(), "second".into()];
        let mut runouts = Vec::new();
        for minute in 0..120 {
            let at = now + minute * 60;
            state.absorb(claude_reading(35.0 + minute as f64 * 0.01, at));
            state.evaluate(&chats, at);
            let report = plan_limits::evaluate_policy(state.file(), at);
            runouts.push(
                report
                    .report
                    .claude
                    .windows
                    .iter()
                    .find(|w| w.label == "7d")
                    .unwrap()
                    .days_left,
            );
            assert_eq!(
                state.file.usage_badge.level,
                if minute == 0 {
                    plan_limits::UsageBadgeLevel::OnTrack
                } else {
                    plan_limits::UsageBadgeLevel::Hot
                }
            );
            assert!(state.file.pending_notices.is_empty());
            for chat in &chats {
                assert!(state.notice(chat, false).is_none());
            }
        }
        assert_ne!(runouts.first(), runouts.last());
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
        let mut state = PlanLimitsState::default();
        assert!(state.begin_save(path.clone()).is_none());
        state.set_pin(Some(plan_limits::RoutingPin {
            provider: Provider::Codex,
            until: now_unix() + 3600,
        }));
        let first = state.begin_save(path.clone()).unwrap();
        assert!(state.begin_save(path.clone()).is_none());
        state.set_pin(None);
        let PlanLimitsEvent::Saved(result) = runtime.block_on(first.run()) else {
            panic!("expected save completion")
        };
        assert!(result.is_ok());
        assert!(state.finish_save(result, &path));
        assert!(PlanLimitsState::load(dir.path()).file.pin.is_some());
        // The completion handler drains changes made after the first snapshot.
        let second = state.begin_save(path.clone()).unwrap();
        let PlanLimitsEvent::Saved(result) = runtime.block_on(second.run()) else {
            panic!("expected save completion")
        };
        assert!(result.is_ok());
        assert!(state.finish_save(result, &path));
        assert!(state.begin_save(path.clone()).is_none());
        assert!(PlanLimitsState::load(dir.path()).file.pin.is_none());

        state.set_pin(Some(plan_limits::RoutingPin {
            provider: Provider::Claude,
            until: now_unix() + 3600,
        }));
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
