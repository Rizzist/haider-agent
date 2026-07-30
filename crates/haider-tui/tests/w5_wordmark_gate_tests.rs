//! The v0.0.15 swallowed-first-key fix: the graphics-capability query runs
//! ONLY on terminals whose environment proves they answer it. A terminal
//! that never answers leaves the query's stdio reader to consume the user's
//! next keystroke — the field probe lost the leading `/` of `/accounts` and
//! started a session named "accounts" instead.
#![allow(clippy::expect_used)]

use haider_tui::wordmark::graphics_terminal_likely;
use std::collections::HashMap;

fn env_of(pairs: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<&'static str, &'static str> = pairs.into_iter().collect();
    move |name: &str| map.get(name).map(|&value| value.to_owned())
}

/// MUTATION CHECK: make `graphics_terminal_likely` return `true`
/// unconditionally (restoring the v0.0.15 always-query behavior). Expected
/// runtime failure: the plain-xterm assertions below — exactly the
/// environments where the query eats the first keystroke.
/// Verified by revert on 2026-07-30.
#[test]
fn plain_terminals_never_query_graphics_capability() {
    // The field-bug environments: nothing proves an answering terminal.
    let plain = env_of(vec![("TERM", "xterm-256color")]);
    assert!(
        !graphics_terminal_likely(&plain),
        "plain xterm must not be queried — the reader eats the first key"
    );
    let empty = env_of(vec![]);
    assert!(!graphics_terminal_likely(&empty));
    let tmux = env_of(vec![("TERM", "screen-256color"), ("TMUX", "/tmp/tmux-1")]);
    assert!(!graphics_terminal_likely(&tmux));
}

#[test]
fn graphics_terminals_are_recognized() {
    for pairs in [
        vec![("KITTY_WINDOW_ID", "1"), ("TERM", "xterm-kitty")],
        vec![("TERM", "xterm-kitty")],
        vec![("TERM_PROGRAM", "iTerm.app"), ("TERM", "xterm-256color")],
        vec![("TERM_PROGRAM", "WezTerm")],
        vec![("WEZTERM_EXECUTABLE", "/usr/bin/wezterm")],
        vec![("TERM", "xterm-ghostty")],
        vec![("KONSOLE_VERSION", "23.08.0")],
    ] {
        assert!(
            graphics_terminal_likely(&env_of(pairs.clone())),
            "must recognize {pairs:?}"
        );
    }
}
