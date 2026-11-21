//! Core's Codex trust cases, run under GPUI's resolved toml (the root
//! workspace runs the same file under its pinned toml 0.8.2).
#![cfg(target_os = "macos")]

// Core's file keeps core's (2021) formatting.
#[rustfmt::skip]
#[path = "../../../../crates/gitterm-core/tests/codex_trust.rs"]
mod cases;
