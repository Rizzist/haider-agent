//! Shared slash-command catalog and palette projection.
//!
//! This is the one command-name authority used by both the terminal palette
//! and the daemon command door.  Keeping it below both surfaces prevents a
//! web or terminal client from growing a shadow command catalog.

use crate::{NeedsInputWire, ResponseBody};
use serde::{Deserialize, Serialize};

/// Which side owns the semantics of one built-in command.
///
/// `Unknown` is deliberately non-executable: an older client must never turn
/// a future ownership classification into a concrete action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandOwnershipWire {
    DaemonOperation,
    ClientView,
    #[serde(other)]
    Unknown,
}

/// One built-in command in the shared catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub desc: &'static str,
    pub arg_hint: &'static str,
    /// Only offered inside a session (sim `sessionOnly`).
    pub session_only: bool,
    pub ownership: CommandOwnershipWire,
}

const fn operation_cmd(
    name: &'static str,
    desc: &'static str,
    arg_hint: &'static str,
) -> CommandSpec {
    CommandSpec {
        name,
        desc,
        arg_hint,
        session_only: false,
        ownership: CommandOwnershipWire::DaemonOperation,
    }
}

const fn session_operation_cmd(
    name: &'static str,
    desc: &'static str,
    arg_hint: &'static str,
) -> CommandSpec {
    CommandSpec {
        name,
        desc,
        arg_hint,
        session_only: true,
        ownership: CommandOwnershipWire::DaemonOperation,
    }
}

const fn client_cmd(name: &'static str, desc: &'static str, arg_hint: &'static str) -> CommandSpec {
    CommandSpec {
        name,
        desc,
        arg_hint,
        session_only: false,
        ownership: CommandOwnershipWire::ClientView,
    }
}

const fn session_client_cmd(
    name: &'static str,
    desc: &'static str,
    arg_hint: &'static str,
) -> CommandSpec {
    CommandSpec {
        name,
        desc,
        arg_hint,
        session_only: true,
        ownership: CommandOwnershipWire::ClientView,
    }
}

/// The authoritative built-in catalog, in palette order.
pub const COMMANDS: &[CommandSpec] = &[
    client_cmd("help", "Show all commands", ""),
    operation_cmd(
        "model",
        "Pick a model — every provider, full-screen search",
        "[query]",
    ),
    session_operation_cmd("provider", "Switch provider — the model follows", "[name]"),
    session_operation_cmd(
        "effort",
        "Reasoning effort — bare /effort opens the ladder picker",
        "[level|default]",
    ),
    session_operation_cmd("fast", "Toggle fast mode (supported pairs only)", ""),
    client_cmd(
        "theme",
        "Change the theme — bare /theme opens the picker",
        "[system·light·dark·desert·oasis]",
    ),
    session_client_cmd(
        "tree",
        "Open the session tree — jump to or fork any node",
        "",
    ),
    session_client_cmd("fork", "Fork the session at the current point", ""),
    session_client_cmd(
        "branch",
        "Branches — list and switch; `new` forks at the last committed node",
        "[new|name]",
    ),
    session_client_cmd(
        "undo",
        "Undo the latest durable file checkpoint",
        "[last|id]",
    ),
    session_client_cmd(
        "redo",
        "Redo the latest undone file checkpoint",
        "[last|id]",
    ),
    session_client_cmd(
        "checkpoints",
        "List durable file checkpoints for this branch",
        "",
    ),
    session_client_cmd(
        "rollback",
        "Roll back every file edit from one turn",
        "[current|previous|run-id]",
    ),
    session_client_cmd(
        "attach",
        "Attach a file — image or UTF-8 text, uploaded now, rides your next message",
        "<path>",
    ),
    client_cmd("sessions", "List sessions — attach, or start fresh", ""),
    client_cmd(
        "aura",
        "Aura Mode — a voice/orchestrator session (spawns sessions, never codes)",
        "",
    ),
    client_cmd(
        "peer",
        "List live peers or send: /peer <name> <message>",
        "<name> <message>",
    ),
    client_cmd(
        "ssh",
        "SSH profiles — list, scope, or open a remote shell",
        "[scope all|none|name,…|shell name]",
    ),
    client_cmd("shells", "List local and SSH terminal sessions", ""),
    client_cmd(
        "accounts",
        "Accounts — provider credentials (OAuth / API), pick the active one",
        "",
    ),
    client_cmd(
        "account",
        "Switch the active account for its provider",
        "<alias>",
    ),
    client_cmd(
        "providers",
        "Providers — registry truth: endpoints, models, defaults, health",
        "",
    ),
    client_cmd(
        "usage",
        "Usage — cross-provider limit meters, costs, local stats",
        "[provider]",
    ),
    client_cmd("login", "Add a provider account", "<provider> <oauth|api>"),
    client_cmd(
        "clear",
        "Clear back to the main screen; typing there starts a new session",
        "",
    ),
    client_cmd("back", "Back to the main screen — same as /clear", ""),
    session_operation_cmd(
        "compact",
        "Compact context now — history stays in the tree",
        "",
    ),
    session_client_cmd("tokens", "Token panel — context by model (also ⌃G)", ""),
    session_client_cmd(
        "history",
        "Recall durable prompts — newest first (also esc esc)",
        "[number]",
    ),
    session_client_cmd("hooks", "Hooks — review and trust third-party hooks", ""),
    session_client_cmd("voice", "Voice — enable and pick STT / TTS providers", ""),
    session_client_cmd(
        "say",
        "Speak a turn (simulated STT) — needs voice enabled",
        "<words>",
    ),
    session_client_cmd(
        "talk",
        "Dictate into the composer — ⏎ sends, esc cancels, typing keeps it",
        "[setup·wave]",
    ),
    session_client_cmd(
        "tools",
        "Tools — the core surface plus custom tools you register",
        "",
    ),
    session_client_cmd(
        "queue",
        "Mid-turn input mode — steer, subturn (next tool), or queue (turn end)",
        "<steer|subturn|turn>",
    ),
    session_client_cmd(
        "graph",
        "Convergence Graph — the pinned run: nodes, gates, evidence",
        "[pin]",
    ),
    client_cmd(
        "workflows",
        "Workflows — typed pipe DAGs: signatures, node chains, source",
        "",
    ),
    client_cmd(
        "loom",
        "Loom — Agent Types: capability-scoped specialists · @type spawns one",
        "",
    ),
    session_client_cmd("update", "Check for and install a production update", ""),
    session_operation_cmd("rename", "Rename this session", "<name>"),
    client_cmd("reset", "Reset the demo — restore the seed sessions", ""),
];

/// One palette row: a built-in, an argument candidate, or a custom command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteItem {
    Cmd(&'static CommandSpec),
    Arg {
        cmd: &'static str,
        value: String,
        desc: String,
    },
    Custom {
        name: String,
        desc: String,
    },
}

impl PaletteItem {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Cmd(spec) => format!("/{}", spec.name),
            Self::Arg { value, .. } => value.clone(),
            Self::Custom { name, .. } => format!("/{name}"),
        }
    }

    #[must_use]
    pub fn desc(&self) -> &str {
        match self {
            Self::Cmd(spec) => spec.desc,
            Self::Arg { desc, .. } | Self::Custom { desc, .. } => desc,
        }
    }

    #[must_use]
    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom { .. })
    }
}

/// Palette rows visible at once.
pub const PALETTE_MAX_ROWS: usize = 8;

/// Dynamic catalog inputs supplied by the requesting surface's current view.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDynamicSlotsWire {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_commands: Vec<(String, String)>,
}

impl CommandDynamicSlotsWire {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
            && self.models.is_empty()
            && self.accounts.is_empty()
            && self.efforts.is_empty()
            && self.custom_commands.is_empty()
    }
}

#[must_use]
pub fn command_spec(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|spec| spec.name == name)
}

#[must_use]
pub fn has_arg_slots(name: &str) -> bool {
    matches!(name, "login" | "provider" | "account")
}

#[must_use]
pub fn offers_arg_completions(name: &str) -> bool {
    matches!(name, "theme" | "model" | "usage" | "effort") || has_arg_slots(name)
}

fn login_args(slot: usize, provider: &str, fragment: &str) -> Vec<PaletteItem> {
    const API: &[(&str, &str)] = &[(
        "api",
        "paste an API key (masked, stored in the daemon vault)",
    )];
    const OAUTH: &[(&str, &str)] = &[(
        "oauth",
        "browser sign-in — loopback PKCE (device code for kimi/grok)",
    )];
    const API_AND_OAUTH: &[(&str, &str)] = &[
        (
            "api",
            "paste an API key (masked, stored in the daemon vault)",
        ),
        (
            "oauth",
            "browser sign-in — loopback PKCE (device code for kimi/grok)",
        ),
    ];
    const CUSTOM: &[(&str, &str)] = &[(
        "api",
        "name + base URL + optional API key (masked, stored in the daemon vault)",
    )];

    let candidates: &[(&str, &str)] = match (slot, provider) {
        (0, _) => &[
            ("anthropic", "Anthropic — Claude (oauth · api)"),
            ("openai", "OpenAI — ChatGPT (oauth · api)"),
            ("gemini", "Google — Gemini (api)"),
            ("kimi", "Moonshot — Kimi (oauth, device code)"),
            ("grok", "xAI — Grok (oauth, device code)"),
            ("xai", "xAI — Grok (api)"),
            ("deepseek", "DeepSeek (api)"),
            (
                "custom",
                "Custom server — name + base URL + optional API key",
            ),
        ],
        (1, "anthropic" | "openai") => API_AND_OAUTH,
        (1, "kimi" | "grok") => OAUTH,
        (1, "custom") => CUSTOM,
        // Named API-only builtins, plus configured custom providers typed
        // directly. OAuth is authoritative and finite; API providers can be
        // added through `provider.configure`, so an unknown name gets the
        // API-key route rather than a fictitious OAuth choice.
        (1, _) => API,
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

fn theme_args(fragment: &str) -> Vec<PaletteItem> {
    const THEMES: &[(&str, &str)] = &[
        ("system", "follow the terminal · auto light / dark"),
        ("light", "Light"),
        ("dark", "Dark"),
        ("desert", "Desert"),
        ("water", "Water"),
        ("oasis", "Oasis"),
    ];
    THEMES
        .iter()
        .filter(|(value, _)| value.starts_with(fragment))
        .map(|(value, desc)| PaletteItem::Arg {
            cmd: "theme",
            value: (*value).to_owned(),
            desc: (*desc).to_owned(),
        })
        .collect()
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

/// Palette rows for a composer query (the text after `/`).
#[must_use]
pub fn palette_items(
    query: &str,
    in_session: bool,
    slots: &CommandDynamicSlotsWire,
) -> Vec<PaletteItem> {
    let ends_space = query.ends_with(char::is_whitespace);
    let mut tokens = query.split_whitespace();
    let first = tokens.next().unwrap_or("").to_ascii_lowercase();
    let rest: Vec<&str> = tokens.collect();
    if !ends_space && rest.is_empty() {
        let mut matches: Vec<PaletteItem> = COMMANDS
            .iter()
            .filter(|spec| in_session || !spec.session_only)
            .filter(|spec| spec.name.starts_with(&first))
            .map(PaletteItem::Cmd)
            .collect();
        matches.extend(
            slots
                .custom_commands
                .iter()
                .filter(|(name, _)| name.starts_with(&first))
                .map(|(name, desc)| PaletteItem::Custom {
                    name: name.clone(),
                    desc: desc.clone(),
                }),
        );
        if matches.len() == 1
            && has_arg_slots(&first)
            && matches!(matches[0], PaletteItem::Cmd(spec) if spec.name == first)
        {
            return match first.as_str() {
                "login" => login_args(0, "", ""),
                "model" => dynamic_args("model", &slots.models, ""),
                "provider" => dynamic_args("provider", &slots.providers, ""),
                "account" => dynamic_args("account", &slots.accounts, ""),
                _ => theme_args(""),
            };
        }
        return matches;
    }
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
        "login" if done_args < 2 => {
            login_args(done_args, rest.first().copied().unwrap_or(""), &fragment)
        }
        "model" if done_args == 0 => dynamic_args("model", &slots.models, &fragment),
        "provider" if in_session && done_args == 0 => {
            dynamic_args("provider", &slots.providers, &fragment)
        }
        "account" if done_args == 0 => dynamic_args("account", &slots.accounts, &fragment),
        "effort" if in_session && done_args == 0 => {
            dynamic_args("effort", &slots.efforts, &fragment)
        }
        "usage" if done_args == 0 => dynamic_args("usage", &slots.providers, &fragment),
        _ => Vec::new(),
    }
}

/// Growable catalog row vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandCatalogItemKindWire {
    BuiltIn,
    Argument,
    Custom,
    #[serde(other)]
    Unknown,
}

/// One rendered catalog row. Optional row-specific fields are additive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandCatalogItemWire {
    pub kind: CommandCatalogItemKindWire,
    pub ownership: CommandOwnershipWire,
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_only: Option<bool>,
}

impl CommandCatalogItemWire {
    fn from_palette_item(item: PaletteItem, in_session: bool) -> Self {
        match item {
            PaletteItem::Cmd(spec) => Self {
                kind: CommandCatalogItemKindWire::BuiltIn,
                // At the launcher `/model` chooses client-local defaults;
                // only an attached session has daemon truth to mutate.
                ownership: if spec.name == "model" && !in_session {
                    CommandOwnershipWire::ClientView
                } else {
                    spec.ownership
                },
                label: format!("/{}", spec.name),
                description: spec.desc.to_owned(),
                name: Some(spec.name.to_owned()),
                value: None,
                arg_hint: (!spec.arg_hint.is_empty()).then(|| spec.arg_hint.to_owned()),
                session_only: Some(spec.session_only),
            },
            PaletteItem::Arg { cmd, value, desc } => Self {
                kind: CommandCatalogItemKindWire::Argument,
                ownership: command_spec(cmd).map_or(CommandOwnershipWire::Unknown, |spec| {
                    if spec.name == "model" && !in_session {
                        CommandOwnershipWire::ClientView
                    } else {
                        spec.ownership
                    }
                }),
                label: value.clone(),
                description: desc,
                name: Some(cmd.to_owned()),
                value: Some(value),
                arg_hint: None,
                session_only: command_spec(cmd).map(|spec| spec.session_only),
            },
            PaletteItem::Custom { name, desc } => Self {
                kind: CommandCatalogItemKindWire::Custom,
                // The supplied custom-command slot is a client-local prompt
                // expansion. The daemon may list it, but must not assert or
                // execute semantics it was not given.
                ownership: CommandOwnershipWire::ClientView,
                label: format!("/{name}"),
                description: desc,
                name: Some(name),
                value: None,
                arg_hint: None,
                session_only: None,
            },
        }
    }
}

/// Projects the exact shared palette rows into wire-owned values.
#[must_use]
pub fn command_catalog_items(
    query: &str,
    in_session: bool,
    slots: &CommandDynamicSlotsWire,
) -> Vec<CommandCatalogItemWire> {
    palette_items(query, in_session, slots)
        .into_iter()
        .map(|item| CommandCatalogItemWire::from_palette_item(item, in_session))
        .collect()
}

/// Result of `command.invoke`.
///
/// An unknown result is never executable. `Receipt` nests the canonical
/// operation response instead of creating a second receipt vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandInvokeOutcomeWire {
    Receipt {
        receipt: Box<ResponseBody>,
    },
    Parked {
        needs_input: NeedsInputWire,
    },
    ClientOwned {
        command: String,
    },
    Unsupported {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(other)]
    Unknown,
}
