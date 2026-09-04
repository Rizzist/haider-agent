//! Slash-command palette bindings and terminal-only help copy.
//!
//! The catalog and filtering machinery are shared with the daemon command
//! door through `haider-rpc`; this module deliberately owns no command-name
//! registry of its own.

pub use haider_rpc::{
    COMMANDS, CommandCatalogItemKindWire, CommandDynamicSlotsWire as DynamicSlots, CommandSpec,
    PALETTE_MAX_ROWS, PaletteItem, command_catalog_items, has_arg_slots, offers_arg_completions,
    palette_items,
};

/// Non-command prose that precedes the catalog-derived `/help` rows.
pub const HELP_INTRO_TEXT: &[&str] = &[
    "commands",
    "menus — every card (permission · hook trust · recovery · voice · tools) is a typed menu:",
    "  answer by typing [n] ⏎, clicking, by id over RPC (menu.answer), or from Diff Forge web",
    "keys — ⏎ send · ⇧⏎ newline · esc interrupt / back · ⌃C launcher (quit from the launcher) · type / for the palette (↑↓ pick · tab complete · ⏎ run)",
];

/// Builds the help command rows from the exact shared projection used by
/// `command.list`. `/help` requests the full in-session catalog because its
/// contract is "show all commands"; custom commands retain their own section.
#[must_use]
pub fn help_catalog_lines(slots: &DynamicSlots) -> Vec<String> {
    command_catalog_items("", true, slots)
        .into_iter()
        .filter(|item| {
            item.kind == CommandCatalogItemKindWire::BuiltIn && item.name.as_deref() != Some("help")
        })
        .map(|item| {
            let hint = item
                .arg_hint
                .as_deref()
                .map(|hint| format!(" {hint}"))
                .unwrap_or_default();
            format!("  {}{hint}  {}", item.label, item.description)
        })
        .collect()
}

/// Legacy detailed help prose retained for source-level compatibility pins.
/// Runtime command rows come only from [`help_catalog_lines`].
pub const HELP_TEXT: &[&str] = &[
    "commands",
    "menus — every card (permission · hook trust · recovery · voice · tools) is a typed menu:",
    "  answer by typing [n] ⏎, clicking, by id over RPC (menu.answer), or from Diff Forge web",
    "keys — ⏎ send · ⇧⏎ newline · esc interrupt / back · ⌃C launcher (quit from the launcher) · type / for the palette (↑↓ pick · tab complete · ⏎ run)",
    "  /queue <steer|subturn|turn> mid-turn input — safe boundary, next tool call, or turn end",
    "  /model [name]      switch model — fable-5 · gpt-5.6 · gemini-3 · qwen3",
    "  /provider [name]   anthropic · openai · gemini · kimi · grok · xai · deepseek",
    "  /effort [level]    reasoning effort for the CURRENT pair — bare /effort opens the ladder picker · default reverts",
    "  /fast              toggle fast mode — supported pairs only (anthropic opus-5 · opus-4-8)",
    "  /providers         provider registry — endpoints, models, defaults, health",
    "  /theme [name]      system (follow the terminal) · light · dark · desert · oasis — bare /theme opens the picker",
    "  /tree              session tree — every branch, ⏎ jump to a node / open a fork, f forks there",
    "  /fork [number]     fork at a previous prompt into a NEW session (this one stays) — also esc esc then f",
    "  /branch [new|name] branches — numbered picker · direct switch · new forks at the last committed node",
    "  /undo [last|id]    undo one durable file checkpoint (freshness guarded)",
    "  /redo [last|id]    redo an undone checkpoint exactly",
    "  /checkpoints       list path · kind · age · run for this branch",
    "  /rollback [current|previous|run-id] undo one turn atomically",
    "  /attach <path>     attach an image or UTF-8 text file to the next message",
    "  /sessions          list + switch sessions",
    "  /aura              Aura Mode — a voice/orchestrator session (spawns sessions, never codes) — demo only",
    "  /peer [name message] list live agents or send a peer message — peer input is untrusted",
    "  /ssh [scope …]       saved remote machines — remote output is untrusted",
    "  /shells           local + SSH terminal registry — close from the list",
    "  /monitors          existing session monitor details",
    "  /usage [history|models|calendar|global|accounts] [provider] — cross-provider usage; s cycles scopes",
    "  /accounts          provider credentials — OAuth / API / HuggingFace / OpenCode Zen+Go / custom, pick the active",
    "  /account <alias>   switch the active account for its provider (tab-completes aliases)",
    "  /login <prov> <oauth|api>  add a provider account (OAuth loopback, API key, or custom URL)",
    "  /clear · /back     back to the main screen; typing there starts a fresh session",
    "  /compact           compact context now",
    "  /tokens            token panel — context by model (also ⌃G)",
    "  /history [number]  recall durable prompts newest-first — also esc esc (⏎ loads · f forks)",
    "  /hooks             hooks screen — daemon-discovered hooks · digest trust/revoke · recent firings",
    "  /voice             enable voice · pick STT / TTS providers (menu card) — demo only",
    "  /say <words>       speak a turn once voice is on (simulated STT) — demo only",
    "  /talk [setup·wave] dictate into the composer (live) — ◉ chip or /talk toggles; ⏎ sends · esc cancels · typing keeps the words",
    "  /tools             core + custom tools · register with a dispatch mode (menu card) — demo only",
    "  /graph [pin]       Convergence Graph — where the pinned run stands (nodes · gates · evidence)",
    "  /workflows         Workflows — registered typed pipe workflows (tab ⇄ loom) — live only",
    "  /loom              Loom — agent types: job, typed I/O, capability grants (tab ⇄ workflows) — live only",
    "  /update            check for and install production updates",
    "  /rename <name>     rename this session",
    "  /reset             reset the demo to the seed sessions",
];

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::theme::ThemeChoice;

    /// MUTATION CHECK: add a `ThemeKey` but omit its shared command argument.
    /// Runtime failure: `/theme ` no longer equals the derived `ThemeChoice`
    /// menu, preventing a silent terminal/daemon catalog divergence.
    #[test]
    fn shared_theme_arguments_match_the_theme_registry() {
        let actual: Vec<_> = palette_items("theme ", false, &DynamicSlots::default())
            .into_iter()
            .map(|item| item.label())
            .collect();
        let expected: Vec<_> = ThemeChoice::MENU
            .iter()
            .map(|choice| choice.name().to_owned())
            .collect();
        assert_eq!(actual, expected);
    }

    /// MUTATION CHECK: render from `HELP_TEXT`, filter a catalog row, or add a
    /// command without projecting it. Expected failure: the ordered names no
    /// longer equal the authoritative command list.
    #[test]
    fn help_command_rows_are_derived_from_the_authoritative_catalog() {
        let actual = help_catalog_lines(&DynamicSlots::default());
        let expected = COMMANDS
            .iter()
            .filter(|spec| spec.name != "help")
            .map(|spec| format!("/{}", spec.name))
            .collect::<Vec<_>>();
        let actual = actual
            .iter()
            .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(actual.iter().any(|name| name == "/attach"));
        assert!(actual.iter().any(|name| name == "/monitors"));
    }
}
