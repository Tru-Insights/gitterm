//! Iced compatibility adapter for the shared notes scheduler and job.
//!
//! `main::new_work_notes` retains the auto-commit and isolation policy. This
//! adapter is the legacy Iced writer; preview frontends use core's explicit
//! writer API and receive no write authority from constructing a scheduler.
use std::path::Path;

pub use gitterm_core::work_notes::{
    commit_message, porcelain_paths, probe, push_failure_reason, status, too_large, CommitReport,
    Fingerprint, Job, JobResult, WorkNotes, MAX_FILE_BYTES, POLL_INTERVAL, PUSH_RETRY,
    PUSH_TIMEOUT, QUIET_PERIOD,
};

/// Only the Iced owner calls this, after its enabled scheduler issues a job.
/// Keep the existing caller signature while binding mutation authority to
/// today's home/Work root and refusing isolated launches.
pub fn run(root: &Path, job: Job) -> JobResult {
    // The config module is binary-owned; mirror its absolute, nonempty override
    // test here so this library facade never needs to discover config paths.
    let isolated = std::env::var_os("GITTERM_V6_CONFIG_DIR")
        .is_some_and(|raw| !raw.is_empty() && Path::new(&raw).is_absolute());
    let writer = dirs::home_dir()
        .filter(|home| root == home.join("Work"))
        .filter(|_| !isolated)
        .and_then(|_| {
            gitterm_core::work_notes::NotesWriter::authorize(root.to_path_buf(), true).ok()
        });
    gitterm_core::work_notes::run(root, job, writer.as_ref())
}
