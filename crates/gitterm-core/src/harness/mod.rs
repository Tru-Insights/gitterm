//! Neutral harness contracts.

pub mod transcript;

pub mod claude_parser;

#[cfg(feature = "runtime")]
pub mod claude;

pub mod acp;
pub mod pi;
