//! Neutral harness contracts.

pub mod transcript;

pub mod claude_parser;

#[cfg(feature = "runtime")]
pub mod claude;

#[cfg(feature = "runtime")]
pub mod local_launch;

pub mod acp;
pub mod pi;

#[cfg(feature = "runtime")]
pub mod acp_launch;

#[cfg(feature = "runtime")]
pub mod remote_chat;
