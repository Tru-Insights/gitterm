//! Recorded, display-only native chat. No session processes or configuration I/O.
pub mod events;
pub mod model;

#[cfg(target_os = "macos")]
extern crate gpui_kit as gpui;
#[cfg(target_os = "macos")]
mod view;
#[cfg(target_os = "macos")]
pub use view::{ChatView, run};
