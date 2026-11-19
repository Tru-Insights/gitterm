mod backend;
mod clipboard;
mod colors;
mod events;
mod glyphs;
mod input;
mod launch;
mod mouse;
pub mod render;
mod reports;
mod services;
mod session;
mod settings;
#[cfg(test)]
mod test_support;
mod view;

pub use backend::{Backend, Command, LinkAction, MouseButton, RenderableContent, SearchMatch};
pub use colors::ColorPalette;
pub use events::{Action, PendingEvents, SessionState};
pub use launch::TerminalLaunch;
pub use services::{JobState, SearchResults, ServiceError, ServiceJob};
pub use settings::{BackendSettings, LayoutSize};
pub use view::{TerminalConfig, TerminalView};

#[cfg(test)]
mod launch_tests;

#[cfg(test)]
mod tool_tests;
