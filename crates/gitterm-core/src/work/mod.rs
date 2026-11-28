//! Work rules and read-only services shared by frontends.

pub mod documents;
pub mod pr_actions;
pub mod summary;

pub mod state;

pub mod catalog;

#[cfg(feature = "runtime")]
pub mod pr_live;

#[cfg(feature = "runtime")]
pub mod pr_discovery;

pub mod pr_created;

#[cfg(feature = "runtime")]
pub mod worktrees;
