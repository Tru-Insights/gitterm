//! Frontend-independent workspace ordering and Git status transitions.
//!
//! The host retains the workspace catalog, selection,
//! persistence and remote transport. The optional runtime owns status jobs.

pub mod status;
pub mod workspace;

#[cfg(feature = "runtime")]
pub mod git_diff;
#[cfg(feature = "runtime")]
pub mod git_status;
#[cfg(feature = "runtime")]
pub mod runtime;

pub mod tasks;
#[cfg(feature = "runtime")]
pub mod worker_process;
