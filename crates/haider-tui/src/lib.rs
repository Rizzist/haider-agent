//! haider-tui — the Haider Code terminal UI.
//!
//! Visual spec: the `/tui` sim (next-diffforge) — panel-for-panel parity is
//! the acceptance bar. Widgets never hardcode colors or labels; everything
//! reads [`theme::Theme`] tokens and [`format`] helpers so the four themes
//! stay one identity. Tests live in `tests/` — never inline (workspace rule).

pub mod agent_metrics;
pub mod app;
pub mod boot;
pub mod branch;
pub mod browser;
pub mod cache_usage;
pub mod clipboard;
pub mod commands;
pub mod composer;
pub mod custom_commands;
pub mod demo_store;
pub mod format;
pub mod hooks;
pub mod identity;
pub mod link;
pub mod live;
pub mod mark;
pub mod md;
pub mod mock;
pub mod notify;
pub mod plain;
pub mod projection;
pub mod render;
pub mod runtime;
pub mod sanctum;
pub mod script;
pub mod select;
pub mod session;
pub mod settings;
pub mod stt_runtime;
pub mod style;
pub mod talk;
pub mod taskrows;
pub mod theme;
pub mod throughput;
pub mod wordmark;

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-tui";
