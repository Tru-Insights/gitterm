//! Live startup (`crate::run`) calls `install_live` once, after installing
//! the live task host and before opening chat panes.
//! The context MUST be the host's original production context (including its
//! MCP namespace), not a newly constructed production context. No install
//! means Preview; changing policy never widens an already-open pane.
use anyhow::{Result, ensure};
use gitterm_core::preview::{Preview, service::ServiceContext};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) enum ChatLaunchPolicy {
    Preview(Arc<Preview>),
    Live { services: ServiceContext },
}
struct Installed(ChatLaunchPolicy);
impl gpui::Global for Installed {}

/// Live startup's hook. Rejects both Preview and fixture contexts.
pub(crate) fn install_live(services: ServiceContext, cx: &mut gpui::App) -> Result<()> {
    ensure!(
        !services.is_preview(),
        "Live chat requires production services"
    );
    ensure!(
        cx.try_global::<Installed>().is_none(),
        "Chat launch policy already installed"
    );
    cx.set_global(Installed(ChatLaunchPolicy::Live { services }));
    Ok(())
}

pub(super) fn current(preview: Arc<Preview>, cx: &gpui::App) -> ChatLaunchPolicy {
    cx.try_global::<Installed>()
        .map(|installed| installed.0.clone())
        .unwrap_or(ChatLaunchPolicy::Preview(preview))
}
