//! Exercise the actual root config module without starting the app.
#![allow(dead_code)]

#[path = "../src/config.rs"]
mod config;

mod theme {
    pub enum AppTheme {
        Dark,
        Light,
    }
}

mod tab {
    pub use gitterm_core::chat::types::AgentBackendConfig;
}
