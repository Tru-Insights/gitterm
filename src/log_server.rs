//! Compatibility facade: the log server lives in `gitterm_core::log_server`
//! so both apps can host it. The root supplies the instance identity.
pub use gitterm_core::log_server::*;

/// Start the log server for this process's instance (`config::instance_id`).
pub async fn start_server(state: ServerState) {
    gitterm_core::log_server::start_server(state, crate::config::instance_id()).await;
}
