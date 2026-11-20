//! Native reducer adapter, row projection and fixture UI. No process or configuration I/O.
pub mod adapter;
pub mod events;
pub mod intents;
pub mod model;
pub use intents::{ChatIntent, PermissionDecision, QuestionAnswer};

#[cfg(target_os = "macos")]
mod cards;

#[cfg(target_os = "macos")]
extern crate gpui_kit as gpui;
#[cfg(target_os = "macos")]
mod view;
#[cfg(target_os = "macos")]
pub use view::{ChatView, IntentCallback, init, run};
