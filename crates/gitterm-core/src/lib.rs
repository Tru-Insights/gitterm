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

pub mod content;
pub mod plans;

#[cfg(feature = "runtime")]
pub mod atomic_file;
pub mod limits;
pub mod plan_limits;
pub mod workers;

#[cfg(feature = "runtime")]
pub mod gh_identity;
#[cfg(feature = "runtime")]
pub mod task_worktree;

pub mod chat_facts;
pub mod chats;
pub mod find;

// Delegation services; launch authority remains with the application owner.
#[cfg(feature = "runtime")]
pub mod checkout_name;
#[cfg(feature = "runtime")]
pub mod codex_review;
#[cfg(feature = "runtime")]
pub mod codex_runner;
#[cfg(feature = "runtime")]
pub mod delegation_request;
#[cfg(all(test, feature = "runtime"))]
mod delegation_test_support;
#[cfg(feature = "runtime")]
pub mod delegations;
#[cfg(feature = "runtime")]
pub mod review_request;
#[cfg(feature = "runtime")]
pub mod runner_guidelines;

#[cfg(feature = "runtime")]
pub mod guidelines;
#[cfg(feature = "runtime")]
pub mod turn_snapshots;
