//! haider-tui — the Haider Code terminal UI.
//!
//! Visual spec: the `/tui` sim (next-diffforge) — panel-for-panel parity is
//! the acceptance bar. Widgets never hardcode colors or labels; everything
//! reads [`theme::Theme`] tokens and [`format`] helpers so the three themes
//! stay one identity. Tests live in `tests/` — never inline (workspace rule).

pub mod app;
pub mod boot;
pub mod clipboard;
pub mod commands;
pub mod demo_store;
pub mod format;
pub mod mark;
pub mod mock;
pub mod plain;
pub mod projection;
pub mod render;
pub mod runtime;
pub mod sanctum;
pub mod script;
pub mod select;
pub mod session;
pub mod style;
pub mod theme;

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-tui";
