//! The slash-command registry (sim `COMMANDS` parity) and help text. Every
//! command is PRESENT in the palette; ones whose machinery lands in later
//! waves execute as an honest flash note naming the wave.

/// One palette entry.
#[derive(Debug, Clone, Copy)]
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

/// Filter the registry for a palette query (the composer text after `/`).
#[must_use]
pub fn palette_matches(query: &str, in_session: bool) -> Vec<&'static CommandSpec> {
    let needle = query
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    COMMANDS
        .iter()
        .filter(|spec| in_session || !spec.session_only)
        .filter(|spec| spec.name.starts_with(&needle))
        .collect()
}

/// The `/help` panel body (sim `HELP_TEXT` parity).
pub const HELP_TEXT: &[&str] = &[
    "commands",
    "  /model [name]      switch model — fable-5 · gpt-5.6 · gemini-3 · qwen3",
    "  /provider [name]   anthropic · openai · google · local",
    "  /theme [name]      dawn · ivory · dark",
    "  /tree              session tree — main-line view, ⏎ opens forks, f forks at a node",
    "  /fork              fork the session at the current point",
    "  /sessions          list + switch sessions",
    "  /aura              Aura Mode — a voice/orchestrator session",
    "  /peers             reachability ladder — peers · sponsored nodes · shell targets",
    "  /accounts          provider credentials — pick the active",
    "  /login <prov> <oauth|api>  add a provider account",
    "  /clear · /back     back to the main screen",
    "  /compact           compact context now",
    "  /tokens            token panel — context by model (also ⌃G)",
    "  /voice · /say      voice providers · speak a turn",
    "  /queue <steer|turn> mid-turn input mode",
    "  /reset             reset the demo to the seed sessions",
    "",
    "keys — ⏎ send · esc back/close · ⌃T theme · ⌃C quit · 1-3 attach a session",
];
