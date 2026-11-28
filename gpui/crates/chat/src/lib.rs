//! Native reducer adapter, row projection and fixture UI. No process or configuration I/O.
pub mod adapter;
mod appearance;
pub use appearance::{ChatAppearance, ChatPalette};
pub mod composer;
pub mod delegation;
pub mod events;
pub mod imported;
pub mod intents;
pub mod interactions;
mod markdown;
pub mod model;
pub use intents::{ChatIntent, PermissionDecision, QuestionAnswer};

#[cfg(target_os = "macos")]
mod cards;

#[cfg(target_os = "macos")]
extern crate gpui_kit as gpui;
#[cfg(target_os = "macos")]
mod view;
#[cfg(target_os = "macos")]
pub use view::{
    ChatView, IntentCallback, OpenCheckout, TranscriptScrollerState, VisualFactory, VisualHost,
    VisualMessage, VisualPlacement, VisualSurface, init, run, visual_link,
};

pub mod pickers;
pub mod review;
