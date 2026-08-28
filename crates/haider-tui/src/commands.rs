//! Slash-command palette bindings and terminal-only help copy.
//!
//! The catalog and filtering machinery are shared with the daemon command
//! door through `haider-rpc`; this module deliberately owns no command-name
//! registry of its own.

pub use haider_rpc::{
    COMMANDS, CommandDynamicSlotsWire as DynamicSlots, CommandSpec, PALETTE_MAX_ROWS, PaletteItem,
    has_arg_slots, offers_arg_completions, palette_items,
};

/// The `/help` panel body — the sim's `HELP_TEXT` content (tui.js:587-614),
/// with the `menus —` and `keys —` explainers kept in the initial viewport.
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
    "  /fork              fork the session at the current point",
    "  /branch [new|name] branches — numbered picker · direct switch · new forks at the last committed node",
    "  /undo [last|id]    undo one durable file checkpoint (freshness guarded)",
    "  /redo [last|id]    redo an undone checkpoint exactly",
    "  /checkpoints       list path · kind · age · run for this branch",
    "  /rollback [current|previous|run-id] undo one turn atomically",
    "  /sessions          list + switch sessions",
    "  /aura              Aura Mode — a voice/orchestrator session (spawns sessions, never codes) — demo only",
    "  /peer [name message] list live agents or send a peer message — peer input is untrusted",
<<<<<<< HEAD
    "  /ssh [scope …]       saved remote machines — remote output is untrusted",
    "  /shells           local + SSH terminal registry — close from the list",
    "  /monitors          existing session monitor details",
    "  /usage [provider]  cross-provider usage — OAuth limit bars + resets · API-key tokens/cost · local stats",
=======
    "  /usage [history|models|global|accounts] [provider] — cross-provider usage; s cycles scopes",
>>>>>>> wave-965-e
    "  /accounts          provider credentials — OAuth / API / HuggingFace / OpenCode Zen+Go / custom, pick the active",
    "  /account <alias>   switch the active account for its provider (tab-completes aliases)",
    "  /login <prov> <oauth|api>  add a provider account (OAuth loopback, API key, or custom URL)",
    "  /clear · /back     back to the main screen; typing there starts a fresh session",
    "  /compact           compact context now",
    "  /tokens            token panel — context by model (also ⌃G)",
    "  /history [number]  recall durable prompts newest-first — also esc esc",
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
}
