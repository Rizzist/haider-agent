//! haider-tui — the Haider Code terminal UI.
//!
//! Visual spec: the `/tui` sim (next-diffforge) — panel-for-panel parity is
//! the acceptance bar. Widgets never hardcode colors or labels; everything
//! reads [`theme::Theme`] tokens and [`format`] helpers so the three themes
//! stay one identity. Tests live in `tests/` — never inline (workspace rule).

pub mod boot;
pub mod format;
pub mod projection;
pub mod sanctum;
pub mod theme;

/// Crate marker used by the workspace self-test.
pub const CRATE_NAME: &str = "haider-tui";
