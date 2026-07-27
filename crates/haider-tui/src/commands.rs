//! The slash-command registry (sim `COMMANDS` parity) and help text. Every
//! command is PRESENT in the palette; ones whose machinery lands in later
//! waves execute as an honest flash note naming the wave.

/// One palette entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub desc: &'static str,
    pub arg_hint: &'static str,
    /// Only offered inside a session (sim `sessionOnly`).
    pub session_only: bool,
}

/// Sim-parity registry, same order.
pub const COMMANDS: &[CommandSpec] = &[
    cmd("help", "Show all commands", ""),
    session_cmd(
        "model",
        "Switch the active model for this session",
        "[name]",
    ),
    session_cmd("provider", "Switch provider — the model follows", "[name]"),
    cmd("theme", "Change the theme", "[dawn·ivory·dark]"),
    session_cmd(
        "tree",
        "Open the session tree — jump to or fork any node",
        "",
    ),
    session_cmd("fork", "Fork the session at the current point", ""),
    cmd("sessions", "List sessions — attach, or start fresh", ""),
    cmd(
        "aura",
        "Aura Mode — a voice/orchestrator session (spawns sessions, never codes)",
        "",
    ),
    cmd(
        "peers",
        "Peers — the reachability ladder: enrolled peers, sponsored nodes, shell targets",
        "",
    ),
    cmd(
        "accounts",
        "Accounts — provider credentials (OAuth / API), pick the active one",
        "",
    ),
    cmd(
        "account",
        "Switch the active account for its provider",
        "<alias>",
    ),
    cmd("login", "Add a provider account", "<provider> <oauth|api>"),
    cmd(
        "clear",
        "Clear back to the main screen; typing there starts a new session",
        "",
    ),
    cmd("back", "Back to the main screen — same as /clear", ""),
    session_cmd(
        "compact",
        "Compact context now — history stays in the tree",
        "",
    ),
    session_cmd("tokens", "Token panel — context by model (also ⌃G)", ""),
    session_cmd("hooks", "Hooks — review and trust third-party hooks", ""),
    session_cmd("voice", "Voice — enable and pick STT / TTS providers", ""),
    session_cmd(
        "say",
        "Speak a turn (simulated STT) — needs voice enabled",
        "<words>",
    ),
    session_cmd(
        "tools",
        "Tools — the core surface plus custom tools you register",
        "",
    ),
    session_cmd(
        "queue",
        "Mid-turn input mode — steer (safe boundary) or queue (after turn ends)",
        "<steer|turn>",
    ),
    session_cmd("update", "Check for updates — install as a menu card", ""),
    session_cmd("rename", "Rename this session", "<name>"),
    cmd("reset", "Reset the demo — restore the seed sessions", ""),
];

const fn cmd(name: &'static str, desc: &'static str, arg_hint: &'static str) -> CommandSpec {
    CommandSpec {
        name,
        desc,
        arg_hint,
        session_only: false,
    }
}

const fn session_cmd(
    name: &'static str,
    desc: &'static str,
    arg_hint: &'static str,
) -> CommandSpec {
    CommandSpec {
        name,
        desc,
        arg_hint,
        session_only: true,
    }
}

/// One palette row: a command, or an argument candidate for the current
/// slot (sim `getSuggestions`, tui.js:234-262). Only `/theme` carries an
/// executable slot today — the full arg-slot table lands with the daemon's
/// real command arguments. `Eq` matters: mouse hits carry the VALUE so a
/// stale hit map can never activate a different row (review r2 P2-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteItem {
    Cmd(&'static CommandSpec),
    Arg {
        cmd: &'static str,
        value: &'static str,
        desc: &'static str,
    },
}

impl PaletteItem {
    /// The fixed-width name-column text (sim `.cname`).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Cmd(spec) => format!("/{}", spec.name),
            Self::Arg { value, .. } => (*value).to_owned(),
        }
    }

    /// The dim description column (sim `.cdesc`).
    #[must_use]
    pub const fn desc(&self) -> &'static str {
        match self {
            Self::Cmd(spec) => spec.desc,
            Self::Arg { desc, .. } => desc,
        }
    }
}

/// Palette rows visible at once — a scroll WINDOW over the full match list
/// (sim max-height + internal scroll, tui.js:5353, 2710-2718), never a
/// truncation: selection wraps over every match and the window follows.
pub const PALETTE_MAX_ROWS: usize = 8;

/// Commands whose argument slot the palette can complete today.
#[must_use]
pub fn has_arg_slots(name: &str) -> bool {
    name == "theme"
}

/// `/theme`'s argument candidates, derived from the theme registry (sim
/// `argSlots("theme")`, tui.js:227 — value = key, desc = label).
fn theme_args(fragment: &str) -> Vec<PaletteItem> {
    crate::theme::ThemeKey::ALL
        .iter()
        .filter(|key| key.name().starts_with(fragment))
        .map(|key| PaletteItem::Arg {
            cmd: "theme",
            value: key.name(),
            desc: key.theme().label,
        })
        .collect()
}

/// Palette rows for a composer query (the text after `/`). A single
/// unfinished token filters command names — but a FULLY-typed arg command
/// (exact single match with slots) jumps straight to its argument rows (sim
/// `getSuggestions` "lead" case, tui.js:243-247). After the space the
/// palette stays on the argument slot.
#[must_use]
pub fn palette_items(query: &str, in_session: bool) -> Vec<PaletteItem> {
    let ends_space = query.ends_with(char::is_whitespace);
    let mut tokens = query.split_whitespace();
    let first = tokens.next().unwrap_or("").to_ascii_lowercase();
    let rest: Vec<&str> = tokens.collect();
    if !ends_space && rest.is_empty() {
        let matches: Vec<PaletteItem> = COMMANDS
            .iter()
            .filter(|spec| in_session || !spec.session_only)
            .filter(|spec| spec.name.starts_with(&first))
            .map(PaletteItem::Cmd)
            .collect();
        // Exact arg-command fully typed → its slot candidates, unfiltered.
        if matches.len() == 1
            && has_arg_slots(&first)
            && matches!(matches[0], PaletteItem::Cmd(spec) if spec.name == first)
        {
            return theme_args("");
        }
        return matches;
    }
    // Argument position. /theme has one slot; further args offer nothing.
    if first == "theme" {
        let done_args = if ends_space {
            rest.len()
        } else {
            rest.len().saturating_sub(1)
        };
        if done_args == 0 {
            let fragment = if ends_space {
                String::new()
            } else {
                rest.last().copied().unwrap_or("").to_ascii_lowercase()
            };
            return theme_args(&fragment);
        }
    }
    Vec::new()
}

/// The `/help` panel body — the sim's `HELP_TEXT` verbatim (tui.js:587-614),
/// including the `menus —` and `keys —` explainers.
pub const HELP_TEXT: &[&str] = &[
    "commands",
    "  /model [name]      switch model — fable-5 · gpt-5.6 · gemini-3 · qwen3",
    "  /provider [name]   anthropic · openai · google · local",
    "  /theme [name]      dawn · ivory · dark",
    "  /tree              session tree — main-line view, ⏎ opens forks, f forks at a node",
    "  /fork              fork the session at the current point",
    "  /sessions          list + switch sessions",
    "  /aura              Aura Mode — a voice/orchestrator session (spawns sessions, never codes)",
    "  /peers             reachability ladder — enrolled peers · sponsored SSH nodes · shell targets",
    "  /accounts          provider credentials — OAuth / API / HuggingFace / custom, pick the active",
    "  /account <alias>   switch the active account for its provider (tab-completes aliases)",
    "  /login <prov> <oauth|api>  add a provider account (OAuth loopback, API key, or custom URL)",
    "  /clear · /back     back to the main screen; typing there starts a fresh session",
    "  /compact           compact context now",
    "  /tokens            token panel — context by model (also ⌃G)",
    "  /hooks             trust third-party hooks — FULL-SCREEN startup gate (not a session card)",
    "  /voice             enable voice · pick STT / TTS providers (menu card)",
    "  /say <words>       speak a turn once voice is on (simulated STT)",
    "  /tools             core + custom tools · register with a dispatch mode (menu card)",
    "  /queue <steer|turn> mid-turn input mode — steer at safe boundary, or hold until turn end",
    "  /update            check for updates — FULL-SCREEN startup gate (harness-level)",
    "  /rename <name>     rename this session",
    "  /reset             reset the demo to the seed sessions",
    "menus — every card (permission · hook trust · update · recovery · voice · tools) is a typed menu:",
    "  answer by typing [n] ⏎, clicking, by id over RPC (menu.answer), or from Diff Forge web",
    "keys — ⏎ send · ⇧⏎ newline · esc interrupt / back · ⌃C launcher (quit from the launcher) · type / for the palette (↑↓ pick · tab complete · ⏎ run)",
];
