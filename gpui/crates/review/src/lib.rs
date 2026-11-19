//! Native review from fixtures or local repositories, without persisted settings.
pub mod fixture;
pub mod model;
pub mod repo;

#[cfg(target_os = "macos")]
extern crate gpui_kit as gpui;
#[cfg(target_os = "macos")]
mod view;
#[cfg(target_os = "macos")]
pub use view::{ReviewView, init, run_fixture, run_repo};
