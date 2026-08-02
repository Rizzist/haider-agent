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
    // B2b: branches are daemon truth — the picker lists, a name switches,
    // `new` forks at the active branch's last committed node.
    session_cmd(
        "branch",
        "Branches — list and switch; `new` forks at the last committed node",
        "[new|name]",
    ),
    // B4b registry extension (no sim counterpart): attachments are live
    // daemon-CAS truth — demo and feature-ungated daemons refuse honestly.
    session_cmd(
        "attach",
        "Attach an image — uploaded now, rides your next message",
        "<path>",
    ),
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
    // W5d addition (report §5.2): the sim has no providers screen, so this
    // entry is registry-extension, not sim parity.
    cmd(
        "providers",
        "Providers — registry truth: endpoints, models, defaults, health",
        "",
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteItem {
    Cmd(&'static CommandSpec),
    Arg {
        cmd: &'static str,
        /// OWNED (W5e-3): argument candidates are no longer all compile-time
        /// constants — `/model` and `/provider` rows come from the DISCOVERED
        /// catalog, `/account` from live aliases.
        value: String,
        desc: String,
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
    pub fn desc(&self) -> &str {
        match self {
            Self::Cmd(spec) => spec.desc,
            Self::Arg { desc, .. } => desc.as_str(),
        }
    }
}

/// Palette rows visible at once — a scroll WINDOW over the full match list
/// (sim max-height + internal scroll, tui.js:5353, 2710-2718), never a
/// truncation: selection wraps over every match and the window follows.
pub const PALETTE_MAX_ROWS: usize = 8;

/// Commands whose argument slot the palette can complete today.
///
/// W3c3 M3 adds `/login` (report §6.3: "`/login` argument slots") — the
/// one account command this release makes executable.
#[must_use]
pub fn has_arg_slots(name: &str) -> bool {
    // W5e-3 adds the three DISCOVERED slots. `/model` and `/provider` are
    // session-scoped commands; `/account` works anywhere.
    matches!(name, "theme" | "login" | "model" | "provider" | "account")
}

/// `/login`'s two argument slots: provider, then method. Slot 0 names the
/// REAL login roster — every provider whose add flow the account buttons run
/// today (B6b); slot 1 is the wire's method pair, both executable since
/// W5e-1 (loopback PKCE) and B6b (kimi's device grant).
fn login_args(slot: usize, fragment: &str) -> Vec<PaletteItem> {
    let candidates: &[(&'static str, &'static str)] = match slot {
        0 => &[
            ("anthropic", "Anthropic — Claude (oauth · api)"),
            ("openai", "OpenAI — ChatGPT (oauth · api)"),
            ("gemini", "Google — Gemini (api)"),
            ("kimi", "Moonshot — Kimi (oauth, device code)"),
        ],
        1 => &[
            ("api", "paste an API key (masked, stored in the OS vault)"),
            (
                "oauth",
                "browser sign-in — loopback PKCE (device code for kimi)",
            ),
        ],
        _ => &[],
    };
    candidates
        .iter()
        .filter(|(value, _)| value.starts_with(fragment))
        .map(|(value, desc)| PaletteItem::Arg {
            cmd: "login",
            value: (*value).to_owned(),
            desc: (*desc).to_owned(),
        })
        .collect()
}

/// `/theme`'s argument candidates, derived from the theme registry (sim
/// `argSlots("theme")`, tui.js:227 — value = key, desc = label).
fn theme_args(fragment: &str) -> Vec<PaletteItem> {
    crate::theme::ThemeKey::ALL
        .iter()
        .filter(|key| key.name().starts_with(fragment))
        .map(|key| PaletteItem::Arg {
            cmd: "theme",
            value: key.name().to_owned(),
            desc: key.theme().label.to_owned(),
        })
        .collect()
}

/// Live candidates for the argument slots that are no longer static
/// (W5e-3). Everything here is DAEMON TRUTH — providers and models come from
/// the discovered catalog, accounts from `account.list`. An empty vector
/// means "the daemon has not told us yet", which renders as no rows rather
/// than as invented ones.
#[derive(Debug, Default, Clone)]
pub struct DynamicSlots {
    /// `(provider, description)` for `/provider` and `/login`'s first slot.
    pub providers: Vec<(String, String)>,
    /// `(slug, description)` for `/model` — the ACTIVE provider's discovered
    /// models, already filtered to pickable and ordered by the provider's own
    /// priority.
    pub models: Vec<(String, String)>,
    /// `(alias, description)` for `/account`.
    pub accounts: Vec<(String, String)>,
}

fn dynamic_args(
    cmd: &'static str,
    candidates: &[(String, String)],
    fragment: &str,
) -> Vec<PaletteItem> {
    candidates
        .iter()
        .filter(|(value, _)| value.to_ascii_lowercase().starts_with(fragment))
        .map(|(value, desc)| PaletteItem::Arg {
            cmd,
            value: value.clone(),
            desc: desc.clone(),
        })
        .collect()
}

/// Palette rows for a composer query (the text after `/`). A single
/// unfinished token filters command names — but a FULLY-typed arg command
/// (exact single match with slots) jumps straight to its argument rows (sim
/// `getSuggestions` "lead" case, tui.js:243-247). After the space the
/// palette stays on the argument slot.
#[must_use]
pub fn palette_items(query: &str, in_session: bool, slots: &DynamicSlots) -> Vec<PaletteItem> {
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
            return match first.as_str() {
                "login" => login_args(0, ""),
                "model" => dynamic_args("model", &slots.models, ""),
                "provider" => dynamic_args("provider", &slots.providers, ""),
                "account" => dynamic_args("account", &slots.accounts, ""),
                _ => theme_args(""),
            };
        }
        return matches;
    }
    // Argument position. /theme has ONE slot; /login has two (provider,
    // then method); further args offer nothing.
    let done_args = if ends_space {
        rest.len()
    } else {
        rest.len().saturating_sub(1)
    };
    let fragment = if ends_space {
        String::new()
    } else {
        rest.last().copied().unwrap_or("").to_ascii_lowercase()
    };
    match first.as_str() {
        "theme" if done_args == 0 => theme_args(&fragment),
        "login" if done_args < 2 => login_args(done_args, &fragment),
        // W5e-3: fed from the discovered catalog / live account list.
        "model" if done_args == 0 => dynamic_args("model", &slots.models, &fragment),
        "provider" if done_args == 0 => dynamic_args("provider", &slots.providers, &fragment),
        "account" if done_args == 0 => dynamic_args("account", &slots.accounts, &fragment),
        _ => Vec::new(),
    }
}

/// The `/help` panel body — the sim's `HELP_TEXT` verbatim (tui.js:587-614),
/// including the `menus —` and `keys —` explainers.
pub const HELP_TEXT: &[&str] = &[
    "commands",
    "  /model [name]      switch model — fable-5 · gpt-5.6 · gemini-3 · qwen3",
    "  /provider [name]   anthropic · openai · gemini · kimi",
    "  /providers         provider registry — endpoints, models, defaults, health",
    "  /theme [name]      dawn · ivory · dark",
    "  /tree              session tree — main-line view, ⏎ opens forks, f forks at a node",
    "  /fork              fork the session at the current point",
    "  /branch [new|name] branches — numbered picker · direct switch · new forks at the last committed node",
    "  /sessions          list + switch sessions",
    "  /aura              Aura Mode — a voice/orchestrator session (spawns sessions, never codes) — demo only",
    "  /peers             reachability ladder — enrolled peers · sponsored SSH nodes · shell targets",
    "  /accounts          provider credentials — OAuth / API / HuggingFace / custom, pick the active",
    "  /account <alias>   switch the active account for its provider (tab-completes aliases)",
    "  /login <prov> <oauth|api>  add a provider account (OAuth loopback, API key, or custom URL)",
    "  /clear · /back     back to the main screen; typing there starts a fresh session",
    "  /compact           compact context now",
    "  /tokens            token panel — context by model (also ⌃G)",
    "  /hooks             trust third-party hooks — FULL-SCREEN startup gate (not a session card)",
    "  /voice             enable voice · pick STT / TTS providers (menu card) — demo only",
    "  /say <words>       speak a turn once voice is on (simulated STT) — demo only",
    "  /tools             core + custom tools · register with a dispatch mode (menu card) — demo only",
    "  /queue <steer|turn> mid-turn input mode — steer at safe boundary, or hold until turn end",
    "  /update            check for updates — FULL-SCREEN startup gate (harness-level)",
    "  /rename <name>     rename this session",
    "  /reset             reset the demo to the seed sessions",
    "menus — every card (permission · hook trust · update · recovery · voice · tools) is a typed menu:",
    "  answer by typing [n] ⏎, clicking, by id over RPC (menu.answer), or from Diff Forge web",
    "keys — ⏎ send · ⇧⏎ newline · esc interrupt / back · ⌃C launcher (quit from the launcher) · type / for the palette (↑↓ pick · tab complete · ⏎ run)",
];
