//! Frontend-independent workspace ordering and Git status transitions.
//!
//! The host retains the workspace catalog, selection,
//! and persistence. The optional runtime owns status jobs and remote transport;
//! hosts supply credential resolution and local path display.

pub mod attention_notifications;
#[cfg(unix)]
pub mod backup;
pub mod chat;
pub mod harness;
pub mod park;
pub mod preview;
pub mod session;
pub mod state_lock;
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
pub mod run;
pub mod task_commands;
pub mod tasks;
#[cfg(feature = "runtime")]
pub mod worker_process;

pub mod content;
pub mod file_page;
pub mod plans;

#[cfg(feature = "runtime")]
pub mod atomic_file;
pub mod limits;
pub mod plan_limits;
pub mod workers;

#[cfg(feature = "runtime")]
pub mod gh_identity;
#[cfg(feature = "runtime")]
pub mod log_server;
#[cfg(feature = "runtime")]
pub mod loopback;
#[cfg(feature = "runtime")]
pub mod task_host;
#[cfg(feature = "runtime")]
pub mod task_mcp;
#[cfg(feature = "runtime")]
pub mod task_worktree;

pub mod chat_facts;
pub mod chats;
pub mod find;
pub mod folder_picker;

pub mod work;
pub mod work_notes;

// Delegation services and runtime lifecycle authority; hosts execute effects.
#[cfg(feature = "runtime")]
pub mod checkout_name;
#[cfg(feature = "runtime")]
pub mod codex_review;
#[cfg(feature = "runtime")]
pub mod codex_runner;
#[cfg(feature = "runtime")]
pub mod delegation_request;
#[cfg(feature = "runtime")]
pub mod delegation_state;
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

// PR/issue/merge-plan leaf state and optional runtime services.
pub mod issues;
pub mod merge_plan;
pub mod page_navigation;
pub mod page_store;
pub mod prs;

pub mod settings;

pub mod usage;

pub mod remote;

pub mod agent_config;
pub mod config_paths;
pub mod workspace_config;

#[cfg(feature = "stt")]
pub mod dictation;

#[cfg(feature = "runtime")]
pub mod desktop_store;

pub mod editor;

#[cfg(feature = "runtime")]
pub mod shutdown;

#[cfg(feature = "runtime")]
pub mod chat_overview;
#[cfg(feature = "runtime")]
pub mod chat_summary;
