//! Remote identity DTOs and optional neutral transport/source adapters.
#[cfg(feature = "runtime")]
pub mod client;
pub mod config;
#[cfg(feature = "runtime")]
pub mod protocol;
#[cfg(feature = "runtime")]
pub mod source;
pub mod state;
