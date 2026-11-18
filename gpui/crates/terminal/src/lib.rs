mod backend;
mod clipboard;
mod colors;
mod events;
mod glyphs;
mod input;
mod mouse;
pub mod render;
mod settings;
#[cfg(test)]
mod test_support;
mod view;

pub use backend::{Backend, Command, LinkAction, MouseButton, RenderableContent, SearchMatch};
pub use colors::ColorPalette;
pub use events::{Action, PendingEvents};
pub use settings::{BackendSettings, LayoutSize};
pub use view::{TerminalConfig, TerminalView};
