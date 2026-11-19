//! Frontend-independent workspace ordering and Git status transitions.
//!
//! The host retains the workspace catalog, selection,
//! persistence and remote transport. The optional runtime owns status jobs.

pub mod chat;
pub mod harness;
pub mod status;
pub mod workspace;

#[cfg(feature = "runtime")]
pub mod git_diff;
#[cfg(feature = "runtime")]
pub mod git_status;
#[cfg(feature = "runtime")]
pub mod runtime;

pub mod git_facts;
pub mod review;
pub mod tasks;
#[cfg(feature = "runtime")]
pub mod worker_process;

#[cfg(feature = "runtime")]
pub mod atomic_file;
pub mod limits;
pub mod plan_limits;
pub mod workers;
