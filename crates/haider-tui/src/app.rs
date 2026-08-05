//! The app model + reducer: one owner of all TUI state, driven by a single
//! event stream (research rec 3/6). Rendering reads this model; nothing else
//! mutates it. The reducer is pure enough to test headlessly.

use crate::commands::{PALETTE_MAX_ROWS, PaletteItem, has_arg_slots, palette_items};
use crate::identity::UiGeneration;
use crate::mock::seed_session_states;
use crate::projection::SessionProjection;
use crate::sanctum::SanctumTier;
use crate::script::{AuraState, ChipDisplayState, ChipPrefill, ChipSeed, TALK_PHRASE};
use crate::theme::{ThemeChoice, ThemeKey};
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{MenuId, SessionId};
use haider_protocol::menu::{
    AnswerVia, Menu, MenuAnswer, MenuCloseReason, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::state::{HarnessStatus, RunState};
use haider_protocol::{DeliveryMode, EventPayload};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;

/// Sim `autoBlurb` (tui.js:401-406): strip a leading slash-command token,
/// keep the first seven words, cap at 46 chars, capitalize the first letter.
#[must_use]
pub fn auto_blurb(text: &str) -> String {
    let body: String = if text.starts_with('/') {
        text.split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        text.to_owned()
    };
    let joined = body
        .split_whitespace()
        .take(7)
        .collect::<Vec<_>>()
        .join(" ");
    let truncated = if joined.chars().count() > 46 {
        let cut: String = joined.chars().take(46).collect();
        format!("{}…", cut.trim_end())
    } else {
        joined
    };
    let mut chars = truncated.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "New session".to_owned(),
    }
}

/// Sim session-name slug (tui.js:2014-2016): first 3 words, joined by `-`,
/// lowercased, `[a-z0-9-]` only, max 28 chars, fallback `session`.
#[must_use]
pub fn slug_name(text: &str) -> String {
    let joined = text
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    let slug: String = joined
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .take(28)
        .collect();
    if slug.is_empty() {
        "session".to_owned()
    } else {
        slug
    }
}

/// The shell builtins the demo VFS serves locally — instant, NO model turn
/// (sim `SHELL_CMDS`, tui.js:1993-2008).
pub const SHELL_CMDS: [&str; 6] = ["ls", "dir", "pwd", "cd", "mkdir", "touch"];

/// The demo VFS seed (sim tui.js:418-426).
#[must_use]
pub fn vfs_seed() -> BTreeMap<String, Vec<String>> {
    let entry = |dir: &str, names: &[&str]| {
        (
            dir.to_owned(),
            names.iter().map(|n| (*n).to_owned()).collect(),
        )
    };
    BTreeMap::from([
        entry(
            "~/dev",
            &[
                "diffforge/",
                "enterprise-suite/",
                "haider-code/",
                "notes.md",
            ],
        ),
        entry(
            "~/dev/diffforge",
            &["cloud/", "cellular/", "web/", "README.md"],
        ),
        entry(
            "~/dev/diffforge/cloud",
            &["src/", "tests/", "docs/", "Cargo.toml"],
        ),
        entry("~/dev/diffforge/cellular", &["src/", "pbx/", "Cargo.toml"]),
        entry("~/dev/diffforge/web", &["src/", "public/", "package.json"]),
        entry(
            "~/dev/enterprise-suite",
            &["services/", "web/", "infra/", "README.md"],
        ),
        entry("~/dev/haider-code", &["PROPOSAL.md", "research/"]),
    ])
}

/// Sim `resolvePath` (tui.js:444-462): `~` roots, `.` no-ops, `..` pops
/// with a one-segment floor; empty targets default to `~/dev`.
#[must_use]
pub fn resolve_path(arg: &str, cwd: &str) -> String {
    if arg.is_empty() {
        return "~/dev".to_owned();
    }
    if arg.starts_with('~') {
        let segments: Vec<&str> = arg.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return "~".to_owned();
        }
        return segments.join("/");
    }
    let mut base: Vec<String> = cwd
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    for segment in arg.split('/').filter(|s| !s.is_empty()) {
        match segment {
            "." => {}
            ".." => {
                if base.len() > 1 {
                    base.pop();
                }
            }
            other => base.push(other.to_owned()),
        }
    }
    base.join("/")
}

/// Unknown dirs list `src/ README.md` (sim tui.js:448).
fn default_listing() -> Vec<String> {
    vec!["src/".to_owned(), "README.md".to_owned()]
}

/// Sim `runShell` (tui.js:444-462) against the demo VFS. Returns the
/// output line and, for `cd`, the retargeted working dir.
#[must_use]
pub fn run_shell(
    line: &str,
    cwd: &str,
    vfs: &mut BTreeMap<String, Vec<String>>,
) -> (String, Option<String>) {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
    let arg = parts.next().unwrap_or("");
    match cmd.as_str() {
        "ls" | "dir" => {
            let entries = vfs.get(cwd).cloned().unwrap_or_else(default_listing);
            (entries.join("  "), None)
        }
        "pwd" => (cwd.to_owned(), None),
        "cd" => {
            let target = resolve_path(arg, cwd);
            (format!("→ {target}"), Some(target))
        }
        "mkdir" | "touch" => {
            if arg.is_empty() {
                return (format!("usage: {cmd} <name>"), None);
            }
            let entry = if cmd == "mkdir" {
                format!("{arg}/")
            } else {
                arg.to_owned()
            };
            let listing = vfs.entry(cwd.to_owned()).or_insert_with(default_listing);
            if listing.contains(&entry) {
                (format!("{entry} already exists"), None)
            } else {
                listing.push(entry.clone());
                (format!("created {entry}"), None)
            }
        }
        other => (format!("unknown: {other}"), None),
    }
}

/// Which screen is showing (sim: boot | main | session | sub | aura).
/// The subagent view's target chip lives in [`AppModel::view_path`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Boot,
    Launcher,
    Session,
    Subagent,
    Aura,
    /// `/tree` — the session tree's main-line view (sim tui.js:3366-3430,
    /// W7b port). Branch forks land with the branch wave; until then the
    /// view lists the main line's turns and compaction nodes.
    Tree,
    /// `/tools` live (W8b) — a read-only view of the daemon's canonical
    /// tool inventory + remembered session grants. Never a menu.
    Tools,
    /// `/accounts` — the sim's harness-owned credential list
    /// (tui.js:3588-3688), backed by `account.list` in live mode.
    Accounts,
    /// `/providers` — registry truth (report §5.2). The sim has NO such
    /// screen: this layout is owner-directed, provisional until the
    /// v0.0.15 install-probe sign-off.
    Providers,
    /// `/hooks` (H4) — the daemon's hook discovery + digest trust for the
    /// active session's workspace, plus the session's journaled hook
    /// firings. Live-only truth: demo renders a sim-honest empty state.
    Hooks,
    /// `/usage` (U2) — the cross-provider usage report: OAuth limit bars,
    /// API-key token/cost counters, journal-derived local stats. Backed by
    /// U1's `usage.report` read in live mode; demo renders an honest
    /// empty state (usage is daemon truth, never fabricated).
    Usage,
}

/// Sim `AUTH_LABEL` (tui.js:145): the badge text per auth method.
#[must_use]
pub fn auth_label(method: haider_protocol::credential::AuthMethod) -> &'static str {
    match method {
        haider_protocol::credential::AuthMethod::OAuth => "oauth",
        haider_protocol::credential::AuthMethod::ApiKey => "api key",
    }
}

/// One `/accounts` row (sim seedAccounts shape, tui.js:146-154), projected
/// from `account.list` descriptors in live mode or the demo seed.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountRow {
    pub alias: String,
    pub provider: String,
    pub method: haider_protocol::credential::AuthMethod,
    pub identity: String,
    /// `Ok` renders the sim's literal `active`; Limited/Expired/Revoked are
    /// additive W5 status vocabulary with their own snapshots.
    pub status: haider_protocol::credential::CredentialStatus,
    pub selected: bool,
    pub base_url: Option<String>,
}

impl AccountRow {
    /// Projects one `account.list` descriptor into a screen row.
    #[must_use]
    pub fn from_descriptor(descriptor: &haider_protocol::credential::CredentialDescriptor) -> Self {
        Self {
            alias: descriptor.alias.as_str().to_owned(),
            provider: descriptor.provider.clone(),
            method: descriptor.auth_method,
            identity: descriptor.identity.clone(),
            status: descriptor.status.clone(),
            selected: descriptor.active,
            base_url: descriptor.base_url.clone(),
        }
    }
}

/// The `/accounts` screen state. OPTIMISTIC SELECTION IS FORBIDDEN (report
/// §5.1): the dot moves only when a correlated daemon result or a
/// newer-revision snapshot applies — never on click.
#[derive(Debug, Default)]
pub struct AccountsState {
    pub rows: Vec<AccountRow>,
    /// Management revision the rows were read at; an older reply is DROPPED.
    pub revision: Option<u64>,
    /// Last account action (sim `acctMsg`), shown under the head line.
    pub message: Option<String>,
    /// In-flight `account.set_active` target. While `Some`, the rows do not
    /// move and further selects are refused (one at a time).
    pub pending_select: Option<String>,
    /// W10b: an armed removal awaiting Enter (x armed it; esc disarms).
    pub pending_remove: Option<String>,
    /// Keyboard highlight over the flattened selectable rows (W5
    /// accessibility extension — separately goldened).
    pub cursor: usize,
    /// P1 MASK LAW (the U2 owner addendum extended): row identities and
    /// the shared device-section labels render MASKED unless this is set.
    /// `r` toggles it for the CURRENT visit only — the one door in
    /// ([`AppModel::enter_accounts`]) and the esc exit both reset to
    /// masked, so the screen never OPENS revealed (the U2 ⌃C lesson: the
    /// enter-door reset covers exits that bypass `exit_accounts`).
    pub revealed: bool,
}

impl AppModel {
    /// Whether the connected daemon serves a method family. Demo mode
    /// answers everything locally, so it is always capable there.
    #[must_use]
    pub fn daemon_serves(&self, feature: &str) -> bool {
        self.mode.fabricates_locally() || self.daemon_features.contains(feature)
    }

    /// Whether the LIVE daemon serves D1's device-credential discovery
    /// (D2). Deliberately NOT [`Self::daemon_serves`] — that predicate is
    /// demo-true, and the demo has no device to probe: the section is
    /// sim-honestly ABSENT there, exactly like an ungated daemon (no
    /// notice either way — discovery is an enhancement).
    #[must_use]
    pub fn device_discovery_available(&self) -> bool {
        !self.mode.fabricates_locally()
            && self
                .daemon_features
                .contains(haider_rpc::FEATURE_ACCOUNT_DEVICE_DISCOVERY_V1)
    }

    /// Whether the connected daemon's provider registry lists `provider`
    /// (B6b). Some adapters ship WITHOUT a feature bit (Gemini, B6a), so
    /// `provider.list` truth is their capability signal: boot always issues
    /// the list, so live mode holds a snapshot before the accounts screen
    /// can be clicked. Demo answers from its own seed registry, so it is
    /// always capable there — the same shape as [`Self::daemon_serves`].
    #[must_use]
    pub fn daemon_lists_provider(&self, provider: &str) -> bool {
        self.mode.fabricates_locally()
            || self
                .providers
                .providers
                .iter()
                .any(|summary| summary.provider == provider)
    }

    /// The honest refusal for a method this daemon does not serve — names
    /// the stale daemon rather than letting the request fail obscurely.
    #[must_use]
    pub fn stale_daemon_note(&self, what: &str) -> String {
        match &self.daemon_version {
            Some(version) => format!(
                "· {what} needs a newer daemon (running v{version}) — restart it to pick up this release"
            ),
            None => format!("· {what} is not served by the connected daemon"),
        }
    }
}

impl AccountsState {
    /// Applies an `account.list` snapshot, gated on revision monotonicity.
    /// `None` revisions (older daemons) always apply.
    pub fn apply_snapshot(&mut self, rows: Vec<AccountRow>, revision: Option<u64>) -> bool {
        if let (Some(current), Some(new)) = (self.revision, revision)
            && new < current
        {
            return false;
        }
        self.rows = rows;
        if revision.is_some() {
            self.revision = revision;
        }
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
        true
    }
}

/// The "found on this device" section (D2): metadata-only credential
/// candidates the DAEMON discovered in first-party CLI stores (D1's
/// `account.device_candidates`). LIVE-ONLY truth: the demo never populates
/// it (the sim has no device to probe — sim-honest absent), an ungated
/// daemon is never asked. Import installs NOTHING locally — the daemon
/// re-reads the store itself and the new account lands via the normal
/// `account.list` refresh chained to the receipt.
#[derive(Debug, Default)]
pub struct DeviceCandidatesState {
    /// The daemon's last discovery report. Refreshed on SCREEN ENTRY only
    /// (no polling) — freshness hints are hints, not live meters.
    pub candidates: Vec<haider_rpc::DeviceCredentialCandidateWire>,
    /// The daemon's honest configured-off state — never an empty-device
    /// claim (D1's wire contract).
    pub discovery_disabled: bool,
    /// In-flight `account.import_device` candidate id. One at a time; the
    /// correlated receipt or failure clears it — never a render.
    pub pending_import: Option<String>,
    /// Last import outcome, rendered inside the section on BOTH screens
    /// that show it (the section is shared, so the message travels with
    /// it rather than with one screen's message slot).
    pub message: Option<String>,
}

impl DeviceCandidatesState {
    /// Applies a discovery report (screen-entry refresh). Wholesale
    /// replacement: candidate ids are daemon-derived and opaque, so there
    /// is nothing to merge.
    pub fn apply(
        &mut self,
        candidates: Vec<haider_rpc::DeviceCredentialCandidateWire>,
        discovery_disabled: bool,
    ) {
        self.candidates = candidates;
        self.discovery_disabled = discovery_disabled;
    }

    /// How many candidates are actually importable — the numbered,
    /// selectable rows. Unsupported rows render dim + inert and are
    /// deliberately NOT in this count.
    #[must_use]
    pub fn supported_len(&self) -> usize {
        self.candidates
            .iter()
            .filter(|candidate| candidate.import_supported)
            .count()
    }

    /// The `index`th SUPPORTED candidate's opaque id — the digit/⏎/click
    /// coordinate. Numbering skips unsupported rows by construction, so a
    /// key can never land an inert row's id.
    #[must_use]
    pub fn supported_id(&self, index: usize) -> Option<String> {
        self.candidates
            .iter()
            .filter(|candidate| candidate.import_supported)
            .nth(index)
            .map(|candidate| candidate.candidate.clone())
    }
}

/// The `/accounts` OAuth add card (sim authFlow, tui.js:3629-3682): a
/// total-flow overlay above the add row. LIVE: the daemon's loopback PKCE
/// flow drives the phases; DEMO: `[1]` simulates the authorize exactly like
/// the sim's "authorize in browser (simulated)".
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthAddCard {
    /// Wire provider (`openai-oauth` / `anthropic-oauth`) in live mode;
    /// the demo completes under the sim's provider names instead.
    pub provider: String,
    pub title: &'static str,
    pub alias: String,
    /// Client-side attempt identity (the login-card discipline, TUI6.3):
    /// every driver reply must correlate to it or die silently.
    pub attempt: u64,
    pub phase: OAuthAddPhase,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OAuthAddPhase {
    /// `account.oauth_start` is in flight.
    Starting,
    /// The browser owns the authorize; the loopback waits.
    WaitingBrowser { url: String, origin: String },
    /// B6b device-code grant (Kimi): NOTHING is listening on a loopback —
    /// the user enters the code at the verification URL and the daemon
    /// polls the token endpoint. Device-honest copy, never the
    /// "your browser opened…" loopback line (B2b-m3 polish c).
    WaitingDevice { url: String, origin: String },
    /// Callback consumed; the daemon is exchanging the code.
    Exchanging,
    /// `account.add` is committing the ready reference.
    Adding,
    /// Terminal public failure; `[2]`/esc closes.
    Failed { message: String },
}

/// Which custom-provider field owns the keystrokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomField {
    Name,
    Origin,
    Model,
}

/// Where the `+ Custom (OpenAI-compatible)` card is in its flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomPhase {
    /// Typing name/origin (also the retype state after a failure — the
    /// error line renders above the still-editable fields).
    Editing { error: Option<String> },
    /// `provider.configure` is in flight.
    Submitting,
}

/// The `+ Custom (OpenAI-compatible)` card (sim tui.js:3629-3682).
///
/// The DEMO card is the sim's verbatim MenuBox — info lines and a fixed
/// `[1] add http://127.0.0.1:8000/v1 (demo)`. The EDITABLE name/origin
/// fields are the live extension (report §4.4: "custom provider rows are
/// created/edited through provider.configure" — the sim only fabricates).
#[derive(Debug)]
pub struct CustomProviderCard {
    /// Provider id — doubles as the row name (`custom`, `local-llama`).
    pub name: String,
    /// OpenAI-compatible base URL (`http://127.0.0.1:8000/v1`).
    pub origin: String,
    /// The served model id, free-form (`llama3.1:8b`) — an enabled create
    /// REQUIRES a model inventory and a default (daemon law, W5g-5), so
    /// the card asks for the one the server actually serves.
    pub model: String,
    pub focus: CustomField,
    pub phase: CustomPhase,
    /// Attempt identity (the card discipline): every driver reply must
    /// correlate to it or die silently.
    pub attempt: u64,
    /// W10b: editing an EXISTING provider — identity fields (name, origin)
    /// are locked; only the model line is typed (`provider.configure`
    /// update semantics: supplied identity must match exactly).
    pub edit: bool,
    /// G4a: an auth-None (keyless) preset — the configure carries
    /// `auth_requirement: none` and commit SKIPS the key card, going
    /// straight to model discovery.
    pub keyless: bool,
}

/// The `/providers` screen state (report §5.2). Same daemon-truth law as
/// `/accounts`: a default-model change applies only on the correlated,
/// revision-gated reply.
#[derive(Debug, Default)]
pub struct ProvidersState {
    pub providers: Vec<haider_rpc::ProviderSummaryWire>,
    pub revision: Option<u64>,
    pub message: Option<String>,
    /// In-flight `account.set_default_model`: (provider, model).
    pub pending_default: Option<(String, String)>,
    pub cursor: usize,
    /// W10b: an armed removal awaiting Enter (x armed it; esc disarms).
    pub pending_remove: Option<String>,
    /// F2b: the roster's scroll offset in lines. RENDER is the single
    /// scroll authority (the transcript's law): the frame writes the true
    /// max and reconciles this offset against it, so resizes and roster
    /// growth can never bank invisible debt.
    pub scroll: std::cell::Cell<u16>,
    /// The frame-written maximum scroll (lines beyond the viewport).
    pub scroll_max: std::cell::Cell<u16>,
    /// Armed by a cursor move: the next frame scrolls the cursor's
    /// provider block into view, then clears the latch.
    pub follow_cursor: std::cell::Cell<bool>,
    /// P1 MASK LAW: the only identity this screen renders is the shared
    /// device-section's `account_label` — masked unless this is set. Same
    /// per-visit `r` semantics as `/accounts`/`/usage`: reset on the one
    /// door in ([`AppModel::enter_providers`]) and on the esc exit.
    pub revealed: bool,
}

impl ProvidersState {
    /// Applies a `provider.list` snapshot, gated on revision monotonicity.
    pub fn apply_snapshot(
        &mut self,
        providers: Vec<haider_rpc::ProviderSummaryWire>,
        revision: u64,
    ) -> bool {
        if let Some(current) = self.revision
            && revision < current
        {
            return false;
        }
        self.providers = providers;
        self.revision = Some(revision);
        if self.cursor >= self.providers.len() {
            self.cursor = self.providers.len().saturating_sub(1);
        }
        true
    }

    /// Applies a committed default-model change (one provider summary).
    pub fn apply_default_set(
        &mut self,
        summary: haider_rpc::ProviderSummaryWire,
        revision: u64,
    ) -> bool {
        if self
            .pending_default
            .as_ref()
            .is_some_and(|(provider, _)| *provider == summary.provider)
        {
            self.pending_default = None;
        }
        if let Some(current) = self.revision
            && revision < current
        {
            return false;
        }
        if let Some(slot) = self
            .providers
            .iter_mut()
            .find(|existing| existing.provider == summary.provider)
        {
            *slot = summary;
        }
        self.revision = Some(revision);
        true
    }

    /// Merges one provider's refreshed summary — its discovered catalog
    /// (W5f-2d). Upserts (found → replace; absent → push) under the same
    /// revision gate as the snapshot.
    pub fn apply_models_refresh(
        &mut self,
        summary: haider_rpc::ProviderSummaryWire,
        revision: u64,
    ) -> bool {
        if let Some(current) = self.revision
            && revision < current
        {
            return false;
        }
        if let Some(slot) = self
            .providers
            .iter_mut()
            .find(|existing| existing.provider == summary.provider)
        {
            *slot = summary;
        } else {
            self.providers.push(summary);
        }
        self.revision = Some(revision);
        true
    }

    /// The provider-DECLARED context window for `model`, when discovery
    /// carried one (W5g-1). `None` means the provider declared nothing —
    /// the caller keeps its current figure rather than inventing a number.
    #[must_use]
    pub fn declared_window(&self, provider: &str, model: &str) -> Option<u64> {
        self.providers
            .iter()
            .find(|summary| summary.provider == provider)
            .and_then(|summary| {
                summary
                    .model_details
                    .iter()
                    .find(|detail| detail.name == model)
            })
            .and_then(|detail| detail.context_window)
    }
}

/// One `/usage` provider group: a provider and the report indices of its
/// accounts, both in REPORT order (daemon truth — never re-sorted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageGroup {
    pub provider: String,
    /// Indices into [`UsageState::report`]'s `accounts`.
    pub accounts: Vec<usize>,
}

/// The `/usage` screen state (U2). The report is U1's `usage.report`
/// snapshot CONSUMED whole — meter windows, typed unavailability, local
/// counters; nothing here re-derives or fabricates a reading.
#[derive(Debug, Default)]
pub struct UsageState {
    /// The last committed `usage.report` snapshot. `None` until the first
    /// reply lands (live) — the demo never fabricates one.
    pub report: Option<haider_protocol::usage::UsageReportV1>,
    /// A read is in flight (screen entry / `r`).
    pub fetching: bool,
    /// The last read's typed failure — rendered on the screen, never a
    /// bare flash; a later good reply clears it.
    pub error: Option<String>,
    /// `/usage <provider>`: case-insensitive PREFIX filter over provider
    /// names (`anthropic` catches `anthropic-oauth`). `None` shows all.
    pub filter: Option<String>,
    /// Cursor over the FILTERED provider groups (↑/↓, F2b follow).
    pub cursor: usize,
    /// Per-provider selected account (←/→ tabs). Clamped lazily against
    /// the group at read time so a shrunken report can never index out.
    pub tabs: std::collections::BTreeMap<String, usize>,
    /// Identities render MASKED unless this is set (U2 owner addendum —
    /// streamer-friendly by default). `r` toggles it for the CURRENT
    /// visit only: the one door in ([`AppModel::enter_usage`]) and the esc
    /// exit both reset to masked, so a screen never OPENS revealed.
    pub revealed: bool,
    /// F2b scroll discipline: RENDER is the single scroll authority — the
    /// frame writes the true max and reconciles this offset against it.
    pub scroll: std::cell::Cell<u16>,
    pub scroll_max: std::cell::Cell<u16>,
    /// Armed by a cursor move: the next frame scrolls the cursor's group
    /// header into view, then clears the latch.
    pub follow_cursor: std::cell::Cell<bool>,
}

impl UsageState {
    /// Install a committed `usage.report` snapshot. The ONLY writer of
    /// `report`; clears the in-flight mark and any stale error.
    pub fn apply_report(&mut self, report: haider_protocol::usage::UsageReportV1) {
        self.report = Some(report);
        self.fetching = false;
        self.error = None;
        let groups = self.groups().len();
        if self.cursor >= groups {
            self.cursor = groups.saturating_sub(1);
        }
    }

    /// A `usage.report` read failed: the typed message lands on the
    /// screen. A held snapshot stays visible under the error line —
    /// clearly older truth, never a fabrication.
    pub fn read_failed(&mut self, message: &str) {
        self.fetching = false;
        self.error = Some(message.to_owned());
    }

    /// The FILTERED provider groups, report order preserved: accounts
    /// grouped by provider (first-seen order), the `/usage <provider>`
    /// prefix filter applied case-insensitively.
    #[must_use]
    pub fn groups(&self) -> Vec<UsageGroup> {
        let mut groups: Vec<UsageGroup> = Vec::new();
        let Some(report) = &self.report else {
            return groups;
        };
        let filter = self.filter.as_deref().unwrap_or("");
        for (index, account) in report.accounts.iter().enumerate() {
            if !filter.is_empty()
                && !account
                    .provider
                    .to_ascii_lowercase()
                    .starts_with(&filter.to_ascii_lowercase())
            {
                continue;
            }
            if let Some(group) = groups
                .iter_mut()
                .find(|group| group.provider == account.provider)
            {
                group.accounts.push(index);
            } else {
                groups.push(UsageGroup {
                    provider: account.provider.clone(),
                    accounts: vec![index],
                });
            }
        }
        groups
    }

    /// The selected tab WITHIN `group` — the ←/→ choice clamped to the
    /// group's current width (a shrunken report can never index out).
    #[must_use]
    pub fn selected_tab(&self, group: &UsageGroup) -> usize {
        self.tabs
            .get(&group.provider)
            .copied()
            .unwrap_or(0)
            .min(group.accounts.len().saturating_sub(1))
    }
}

/// A chip's pending question (the amber `?` / recovery `⌁`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipQuestion {
    pub recovery: bool,
    pub text: String,
    pub options: Vec<String>,
    pub resolved: bool,
}

/// One subagent chip — the sim's recursive tree node (§2). Each chip owns
/// its own [`SessionProjection`]: "a child is the same object".
#[derive(Debug)]
pub struct ChipModel {
    pub agent: String,
    /// The roster index the callsign was claimed at (persistence guard 3:
    /// the reload's honour-roll restore reads every chip's `ros`).
    pub ros: Option<u64>,
    pub callsign: String,
    pub hon: &'static str,
    pub full: String,
    pub name: String,
    pub model: String,
    pub device: String,
    pub state: ChipDisplayState,
    pub tokens: u64,
    /// The child's own session id, from the manifest `coordinates`
    /// (`child_session_id` — the key W6d attaches the chip view by). The
    /// S4 row's token join reads it against the roster's session-summary
    /// truth; `None` (older daemon, demo seeds) never joins — a figure is
    /// never guessed off another row.
    pub child_session: Option<String>,
    /// Epoch-ms the child spawned: the `AgentSpawned` envelope's
    /// `committed_at_ms` on live streams, the local wall clock at chip
    /// creation in demo mode (the demo fabricates locally). `None` renders
    /// no elapsed segment — never a guess (S4).
    pub spawned_at_ms: Option<u64>,
    /// Epoch-ms of the LATEST child-attributed event this chip applied.
    /// [`Self::note_event_at`] stops advancing it at the terminal
    /// transition, so `last − spawned` IS the frozen final duration (the
    /// S4 live-tick vs frozen-final law).
    pub last_event_at_ms: Option<u64>,
    pub question: Option<ChipQuestion>,
    pub closed: bool,
    pub removing: bool,
    pub children: Vec<ChipModel>,
    pub transcript: SessionProjection,
}

impl ChipModel {
    #[must_use]
    pub fn from_seed(seed: ChipSeed) -> Self {
        let mut transcript = SessionProjection::new();
        for prefill in &seed.prefill {
            match prefill {
                ChipPrefill::Note(text) => transcript.push_note(text.clone()),
                ChipPrefill::Agent(text) => {
                    transcript.apply(&EventPayload::Item(
                        haider_protocol::item::ItemEvent::Completed {
                            item_id: haider_protocol::ids::ItemId::new(format!(
                                "{}-seed-a",
                                seed.agent
                            )),
                            item: haider_protocol::item::TurnItem::AgentMessage {
                                text: text.clone(),
                            },
                        },
                    ));
                }
                ChipPrefill::ToolOk { name, desc, meta } => {
                    transcript.apply(&EventPayload::Item(
                        haider_protocol::item::ItemEvent::Completed {
                            item_id: haider_protocol::ids::ItemId::new(format!(
                                "{}-seed-t",
                                seed.agent
                            )),
                            item: haider_protocol::item::TurnItem::ToolCall {
                                call_id: format!("{}-seed-t", seed.agent),
                                name: name.clone(),
                                args: serde_json::json!({ "desc": desc, "meta": meta }),
                                status: haider_protocol::item::ToolStatus::Completed,
                            },
                        },
                    ));
                }
            }
        }
        Self {
            agent: seed.agent,
            ros: seed.ros,
            callsign: seed.callsign,
            hon: seed.hon,
            full: seed.full,
            name: seed.name,
            model: seed.model,
            device: seed.device,
            state: seed.state,
            tokens: seed.tokens,
            // Seeded chips carry no time base or child session: the mock's
            // pre-seeded history has no honest spawn instant, so the row
            // simply shows no elapsed. The demo driver's LIVE ChipAdd arm
            // stamps `spawned_at_ms` at creation instead.
            child_session: None,
            spawned_at_ms: None,
            last_event_at_ms: None,
            question: None,
            closed: false,
            removing: false,
            children: Vec::new(),
            transcript,
        }
    }

    /// A chip built from a live `AgentSpawned` manifest (W3c3, report R11
    /// cut 2). The manifest is the ONLY source: `callsign` is display-only
    /// identity (§5.1 — never an address), `model_profile` is the model
    /// line, and `placement` names the device. The chip starts IDLE because
    /// `AgentChipState` is the sole chip-state authority; nothing here
    /// guesses at a running state the stream has not reported.
    #[must_use]
    pub fn from_manifest(manifest: &haider_protocol::agent::AgentManifest) -> Self {
        let callsign = manifest.callsign.clone().unwrap_or_default();
        // The honorific/full name are roster facts, not wire fields: when a
        // callsign is one we minted, re-derive its pair; otherwise carry the
        // bare callsign with no honorific rather than inventing one.
        let roster = crate::script::ROSTER
            .iter()
            .find(|(name, _, _)| *name == callsign);
        let device = match &manifest.placement {
            haider_protocol::agent::Placement::Local => local_device_name().to_owned(),
            haider_protocol::agent::Placement::Device { device } => device.as_str().to_owned(),
        };
        Self {
            agent: manifest.agent.as_str().to_owned(),
            ros: None,
            callsign: callsign.clone(),
            hon: roster.map_or("", |(_, hon, _)| *hon),
            full: roster.map_or(callsign, |(_, _, full)| (*full).to_owned()),
            // The W6a manifest carries the delegated task as a persisted
            // display label (research W6b checklist item 4).
            name: manifest.task.clone(),
            model: manifest.model_profile.clone(),
            device,
            state: ChipDisplayState::Idle,
            tokens: 0,
            // S4: the child's session id rides the manifest's reserved
            // `coordinates` blob (`delegation.rs` writes it for the W6d
            // chip-view attach). Absent or non-string → no join, honestly.
            child_session: manifest
                .coordinates
                .as_ref()
                .and_then(|coordinates| coordinates.get("child_session_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            spawned_at_ms: None,
            last_event_at_ms: None,
            question: None,
            closed: false,
            removing: false,
            children: Vec::new(),
            transcript: SessionProjection::new(),
        }
    }

    /// The chip's question card, per the sim's `chipMenu` gate
    /// (tui.js:2360-2364): open only while the chip is `input_required`/
    /// `error` AND holds an UNRESOLVED question. A closed chip has its
    /// question force-resolved, so its view shows the composer again even
    /// though the protocol Menu in its projection is still open.
    #[must_use]
    pub fn question_menu(&self) -> Option<&Menu> {
        if self.closed
            || !matches!(
                self.state,
                ChipDisplayState::InputRequired | ChipDisplayState::Error
            )
            || self.question.as_ref().is_none_or(|q| q.resolved)
        {
            return None;
        }
        self.transcript.open_menu()
    }

    /// `chipIsLive` (tui.js:286): not closed, state ∉ {done, error}.
    #[must_use]
    pub fn is_live(&self) -> bool {
        !self.closed && !matches!(self.state, ChipDisplayState::Done | ChipDisplayState::Error)
    }

    /// S4: this chip's clock is stopped — closed, or a terminal state.
    /// Deliberately the negation of [`Self::is_live`]: the elapsed figure
    /// freezes exactly where the tree stops counting the chip as live, so
    /// the two laws can never disagree.
    #[must_use]
    pub fn elapsed_frozen(&self) -> bool {
        !self.is_live()
    }

    /// Record one child-attributed event instant (envelope
    /// `committed_at_ms` on live streams, local wall clock in the demo).
    /// Monotone max — replayed/out-of-order envelopes never rewind it —
    /// and REFUSED once the chip is terminal: the terminal transition is
    /// where the clock stops, so `last − spawned` stays the frozen final
    /// (S4 law). Callers note the instant BEFORE applying a state flip, so
    /// the terminal envelope's own timestamp is the end of the measure.
    pub fn note_event_at(&mut self, at_ms: u64) {
        if self.elapsed_frozen() {
            return;
        }
        self.last_event_at_ms = Some(self.last_event_at_ms.map_or(at_ms, |held| held.max(at_ms)));
    }

    /// One law for state flips that must carry time (S4): note the event
    /// instant while the chip is still live, THEN apply the state — a
    /// terminal flip freezes the clock at its own timestamp. Both demo
    /// arms and the live `AgentChipState` reducer go through here so the
    /// freeze can never drift between paths.
    pub fn set_state_at(&mut self, state: ChipDisplayState, at_ms: u64) {
        self.note_event_at(at_ms);
        self.state = state;
    }

    /// The S4 row's elapsed figure, in ms:
    ///
    /// * live chip → `now − spawned`, ticking on the shared anim clock;
    /// * terminal/closed chip → `last event − spawned`, frozen (the
    ///   [`Self::note_event_at`] gate stopped the clock at the terminal
    ///   transition);
    /// * no spawn instant → `None` — the segment is dropped, never a
    ///   fabricated `0s`.
    ///
    /// Saturating both ways: clock skew renders `0s`, never a panic or a
    /// wrapped figure.
    #[must_use]
    pub fn elapsed_ms(&self, now_ms: u64) -> Option<u64> {
        let spawned = self.spawned_at_ms?;
        if self.elapsed_frozen() {
            Some(
                self.last_event_at_ms
                    .unwrap_or(spawned)
                    .saturating_sub(spawned),
            )
        } else {
            Some(now_ms.saturating_sub(spawned))
        }
    }

    /// `chipDisplayState` (tui.js:2810-2811): a live chip that is NOT
    /// input_required with a live descendant displays `waiting`.
    #[must_use]
    pub fn display_state(&self) -> ChipDisplayState {
        if self.is_live()
            && self.state != ChipDisplayState::InputRequired
            && tree_live_count(&self.children) > 0
        {
            ChipDisplayState::Waiting
        } else {
            self.state
        }
    }

    /// `chipActivity` (tui.js:2825-2833), truncated at 52 chars + `…`.
    #[must_use]
    pub fn activity(&self) -> String {
        if self.closed {
            return "closing · leaves in 5s".to_owned();
        }
        if self.state == ChipDisplayState::InputRequired
            && let Some(question) = &self.question
            && !question.resolved
        {
            return truncate_activity(&question.text);
        }
        let live_children = tree_live_count(&self.children);
        if self.display_state() == ChipDisplayState::Waiting && live_children > 0 {
            let plural = if live_children > 1 {
                "children"
            } else {
                "child"
            };
            return format!("waiting on {live_children} {plural}");
        }
        if self.state == ChipDisplayState::Done {
            return "report ready".to_owned();
        }
        if self.state == ChipDisplayState::Thinking {
            return "thinking…".to_owned();
        }
        // Sim: NO entries → `starting…`; an entry with empty text → `…`
        // (tui.js:2377-2380).
        let last = self.transcript.entries().last().map(|entry| match entry {
            crate::projection::TranscriptEntry::Item(block) => match &block.item {
                haider_protocol::item::TurnItem::ToolCall { name, args, .. } => {
                    let desc = args
                        .get("desc")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    format!("{name} {desc}")
                }
                haider_protocol::item::TurnItem::AgentMessage { text } => text.clone(),
                _ => String::new(),
            },
            crate::projection::TranscriptEntry::User { text, .. } => text.clone(),
            crate::projection::TranscriptEntry::Note { text } => text.clone(),
            crate::projection::TranscriptEntry::Error { text } => format!("✗ {text}"),
            crate::projection::TranscriptEntry::Shell { cmd, .. } => format!("$ {cmd}"),
        });
        match last {
            Some(text) if text.is_empty() => "…".to_owned(),
            Some(text) => truncate_activity(&text),
            None => "starting…".to_owned(),
        }
    }
}

fn truncate_activity(text: &str) -> String {
    if text.chars().count() > 52 {
        format!("{}…", text.chars().take(52).collect::<String>())
    } else {
        text.to_owned()
    }
}

/// `treeLiveCount` (tui.js:286-329): live chips, recursively.
#[must_use]
/// The chips-level half of the close lifecycle (§2.5): flags + the parent
/// transcript note — shared by the attached surface (`close_chip_state`)
/// and background routing, so both speak one law. Returns `was_live`, or
/// `None` when the chip is unknown or already closed.
pub fn close_chip_core(
    chips: &mut [ChipModel],
    projection: &mut SessionProjection,
    agent: &str,
) -> Option<bool> {
    let chip = find_chip_mut(chips, agent)?;
    if chip.closed {
        return None;
    }
    let was_live = chip.is_live();
    chip.closed = true;
    chip.removing = true;
    if let Some(question) = &mut chip.question {
        question.resolved = true;
    }
    projection.push_note(format!(
        "· subagent {} {} closed — leaving the tree in 5s",
        chip.callsign, chip.hon
    ));
    Some(was_live)
}

pub fn tree_live_count(chips: &[ChipModel]) -> usize {
    chips
        .iter()
        .map(|chip| usize::from(chip.is_live()) + tree_live_count(&chip.children))
        .sum()
}

/// The S4 row's token figure — the child's TOTAL, truth-ordered, or `None`
/// (the segment is dropped; unknown is never rendered as zero):
///
/// 1. the chip transcript's own durable context footprint (`/tokens` panel
///    truth — the two surfaces share the first source so they cannot
///    disagree);
/// 2. the chip's accumulated counter when it has actually accrued (the
///    demo driver's `ChipTokens` feed; live streams never feed it, so a
///    live chip's honest `0` falls through instead of rendering);
/// 3. the roster join: the manifest's `child_session_id` against the
///    session rows' summary/projection truth ([`known token truth law:
///    crate::session::SessionState::known_tokens`]). Children are full
///    sessions, and `session.list` is the only live wire that carries a
///    child's token total today — the parent stream mirrors no child
///    `Usage` (verified against the daemon's delegation mirror, S4 notes).
///
/// Join-correctness law: the lookup is BY THE CHIP'S OWN recorded id,
/// exact-match — never positional, never by callsign — so a wrong child
/// can never wear another child's tokens.
#[must_use]
pub fn chip_row_tokens(sessions: &[crate::session::SessionState], chip: &ChipModel) -> Option<u64> {
    if let Some(footprint) = chip.transcript.latest_footprint() {
        return Some(footprint.used_tokens);
    }
    if chip.tokens > 0 {
        return Some(chip.tokens);
    }
    let child_session = chip.child_session.as_deref()?;
    sessions
        .iter()
        .find(|row| row.id.as_str() == child_session)
        .and_then(crate::session::SessionState::known_tokens)
}

/// Any non-closed chip whose DISPLAYED state pulses in the sim (running /
/// tool maroon · input-required amber, tui.js:4823-4834), recursively.
/// Waiting (◔), done and error are deliberately still.
fn chips_animated(chips: &[ChipModel]) -> bool {
    chips.iter().any(|chip| {
        (!chip.closed
            && matches!(
                chip.display_state(),
                ChipDisplayState::Running
                    | ChipDisplayState::Tool
                    | ChipDisplayState::InputRequired
            ))
            || chips_animated(&chip.children)
    })
}

/// A tool row still in flight (sim ToolRow `.glyph` while
/// `$status === "running"`, tui.js:4524-4530). Scanned from the tail —
/// a live tool is always recent.
fn streaming_tool_live(entries: &[crate::projection::TranscriptEntry]) -> bool {
    use haider_protocol::item::{ToolStatus, TurnItem};
    entries.iter().rev().any(|entry| {
        matches!(
            entry,
            crate::projection::TranscriptEntry::Item(block)
                if matches!(
                    &block.item,
                    TurnItem::ToolCall { status: ToolStatus::InProgress | ToolStatus::Pending, .. }
                )
        )
    })
}

/// Find a chip anywhere in the tree.
#[must_use]
pub fn find_chip<'t>(chips: &'t [ChipModel], agent: &str) -> Option<&'t ChipModel> {
    for chip in chips {
        if chip.agent == agent {
            return Some(chip);
        }
        if let Some(found) = find_chip(&chip.children, agent) {
            return Some(found);
        }
    }
    None
}

/// Recovery text for a typed login failure (W3c3 M3, report §6.3: "typed
/// `restage_required`/`busy` result handling").
///
/// The daemon's STABLE CODE decides what to say; `message` is human detail
/// and is never load-bearing. Every one of these leaves the card in Entry
/// with an EMPTY buffer, so the user's next act is a retype — the key is
/// never held across a retry.
#[must_use]
pub fn login_recovery(code: &str, message: &str) -> String {
    match code {
        // The staged secret expired (or was already claimed) before the
        // login committed: stage a FRESH one — there is nothing to resend.
        haider_rpc::ERROR_CODE_RESTAGE_REQUIRED => {
            "the staged key expired before it was committed — type it again".to_owned()
        }
        // Retryable: the account actor is mid-transaction.
        haider_rpc::ERROR_CODE_BUSY | haider_rpc::ERROR_CODE_OVERLOADED => {
            "the daemon is busy — press ⏎ to try again".to_owned()
        }
        // The provider rejected the key itself.
        haider_rpc::ERROR_CODE_UNAUTHORIZED => {
            "the provider rejected this key — check it and type it again".to_owned()
        }
        // This connection may not stage secrets (not same-UID / no Control).
        haider_rpc::ERROR_CODE_PERMISSION_DENIED => {
            "this connection may not stage secrets — run haider as the profile owner".to_owned()
        }
        // No vault on this platform (W3c's vault gate is macOS).
        haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED => {
            "no credential vault on this platform — API login lands with the file vault".to_owned()
        }
        // Committed a descriptor whose secret cannot be found.
        haider_rpc::ERROR_CODE_CREDENTIAL_MISSING => {
            "the stored credential is gone — type the key again to re-commit".to_owned()
        }
        _ if message.is_empty() => format!("login failed ({code})"),
        _ => format!("login failed ({code}) — {message}"),
    }
}

/// The transcript note one `AgentReport` becomes (W3c3, report R11 cut 2:
/// "maps `AgentReport` only to report summary/verification content" —
/// never to chip STATE, which stays `AgentChipState`'s alone). The
/// vocabulary matches the `ChildResult` row the transcript already renders
/// (`render.rs`: `└ subagent report — {summary}`).
#[must_use]
pub fn report_note(report: &haider_protocol::agent::ChildReport) -> String {
    use haider_protocol::agent::ReportVerification;
    let verdict = match report.verified {
        ReportVerification::Verified => "verified",
        ReportVerification::Red => "red",
        ReportVerification::Waived => "waived",
        ReportVerification::Unverified => "unverified",
    };
    format!("└ subagent report ({verdict}) — {}", report.summary)
}

/// A chip's display name: callsign + honorific when one is claimed, the
/// bare callsign otherwise (§5.1: display identity, never an address).
#[must_use]
pub fn chip_display_name(chip: &ChipModel) -> String {
    if chip.hon.is_empty() {
        chip.callsign.clone()
    } else {
        format!("{} {}", chip.callsign, chip.hon)
    }
}

/// The parent-timeline row one `AgentMessaged` journal fact becomes (S3) —
/// the messaged marker between the `ChildSpawn` and `ChildResult` rows,
/// same dim note voice as [`report_note`]. The delivery kind rides the
/// tail subtly (`steer` — landed inside the running child turn · `queued`
/// — started a fresh child turn). Callsign resolution is display-only: an
/// agent with no chip (or an unclaimed callsign) keeps its opaque id
/// rather than inventing a name.
#[must_use]
pub fn messaged_note(chips: &[ChipModel], fact: &haider_protocol::agent::AgentMessaged) -> String {
    let who = find_chip(chips, fact.agent.as_str())
        .filter(|chip| !chip.callsign.is_empty())
        .map_or_else(|| fact.agent.as_str().to_owned(), chip_display_name);
    let delivery = match fact.delivery {
        haider_protocol::agent::AgentMessageDelivery::DeliveredSteer => "steer",
        haider_protocol::agent::AgentMessageDelivery::DeliveredQueued => "queued",
    };
    // The preview is a bounded single fact — one timeline row, so its
    // newlines flatten (the daemon's 200-char bound already applied).
    let preview = fact.preview.replace(['\n', '\r'], " ");
    format!("→ messaged {who} · {preview} · {delivery}")
}

pub fn find_chip_mut<'t>(chips: &'t mut [ChipModel], agent: &str) -> Option<&'t mut ChipModel> {
    for chip in chips {
        if chip.agent == agent {
            return Some(chip);
        }
        if let Some(found) = find_chip_mut(&mut chip.children, agent) {
            return Some(found);
        }
    }
    None
}

/// The root→chip path (breadcrumb + view addressing).
#[must_use]
pub fn path_to_chip(chips: &[ChipModel], agent: &str) -> Option<Vec<String>> {
    for chip in chips {
        if chip.agent == agent {
            return Some(vec![chip.agent.clone()]);
        }
        if let Some(mut path) = path_to_chip(&chip.children, agent) {
            path.insert(0, chip.agent.clone());
            return Some(path);
        }
    }
    None
}

/// Remove a chip (and its subtree) wherever it sits.
pub fn remove_chip(chips: &mut Vec<ChipModel>, agent: &str) -> bool {
    if let Some(index) = chips.iter().position(|chip| chip.agent == agent) {
        chips.remove(index);
        return true;
    }
    chips
        .iter_mut()
        .any(|chip| remove_chip(&mut chip.children, agent))
}

/// The short host name (uname nodename up to the first dot) — the sim's
/// `this-mac` made real (owner ask). Cached; the placeholder survives
/// only if the kernel offers nothing.
#[must_use]
pub fn local_device_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let uname = rustix::system::uname();
        let node = uname.nodename().to_string_lossy();
        let short = node.split('.').next().unwrap_or("").trim().to_lowercase();
        if short.is_empty() {
            "this-mac".to_owned()
        } else {
            short
        }
    })
}

/// A render-resolved jump anchor (B2b-m3, research §Q3): the durable
/// `{branch, node}` identity a tree node-row activation arms. The renderer
/// resolves it — node → display entry → logical line → wrapped row — with
/// its OWN width/prefix sums, in the same frame that paints the result.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingJump {
    pub branch: Option<haider_protocol::ids::BranchId>,
    pub node: haider_protocol::ids::NodeId,
}

/// One typed `/tree` row (B2b-m3, research §Q3: value-carrying coordinates
/// — never a bare string or an ordinal). `Eq` matters twice: mouse hits
/// carry the VALUE so a stale hit can never activate a replaced row, and
/// key activation re-reads the freshly built rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    /// The VIEWED branch's header (sim `{type:"branch"}` row). `None` =
    /// the root/main branch.
    Branch {
        branch: Option<haider_protocol::ids::BranchId>,
        label: String,
    },
    /// One user/compaction node on the viewed branch. `coords` carries the
    /// durable `{node_id, node_seq}` when the journal committed them
    /// (live); demo entries have no node identity and carry `None`, so
    /// `f`/jump refuse honestly instead of guessing.
    Node {
        branch: Option<haider_protocol::ids::BranchId>,
        coords: Option<(haider_protocol::ids::NodeId, u64)>,
        label: String,
    },
    /// A fork marker immediately under its EXACT fork node — ⏎ drills.
    Fork {
        branch: haider_protocol::ids::BranchId,
        label: String,
    },
}

impl TreeRow {
    /// The rendered row text (styling is the renderer's).
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Branch { label, .. } | Self::Node { label, .. } | Self::Fork { label, .. } => {
                label
            }
        }
    }
}

/// The `/tree` screen's VIEWED branch, validated against the registry — an
/// id the session never installed falls back to the root (sim: unknown
/// treeBranchId → the root branch).
#[must_use]
pub fn tree_viewed(model: &AppModel) -> Option<haider_protocol::ids::BranchId> {
    model
        .tree_view
        .as_ref()
        .filter(|id| model.branch_state.contains(id))
        .cloned()
}

/// Branch header vocabulary shared by the viewed-branch row and the fork
/// markers: `{name} · N turns · X tok` from the branch's OWN surfaces.
fn tree_branch_meta(projection: &SessionProjection) -> String {
    let turns = projection.user_row_count();
    let tokens = projection
        .latest_footprint()
        .map_or_else(|| projection.context_tokens(), |fp| fp.used_tokens);
    format!(
        "{turns} turn{} · {} tok",
        if turns == 1 { "" } else { "s" },
        crate::format::fmt_tok(tokens)
    )
}

/// `/tree` rows for the VIEWED branch (sim treeRows, tui.js:2327-2337):
/// the branch header, then one node row per user turn / ⊟ compaction in
/// that branch's transcript, each fork marker IMMEDIATELY under its exact
/// fork node. Markers whose fork node has no display row yet (an ancestry
/// this view never materialized) trail at the end rather than being
/// guessed under a nearby row.
#[must_use]
pub fn tree_rows(model: &AppModel) -> Vec<TreeRow> {
    use crate::projection::TranscriptEntry;
    let viewed = tree_viewed(model);
    let Some(projection) = model.branch_projection(viewed.as_ref()) else {
        return Vec::new();
    };
    let active = model.branch_state.active().cloned();
    let name = viewed.as_ref().map_or("main", |id| {
        model
            .branch_state
            .descriptor(id)
            .map_or("?", |descriptor| descriptor.name.as_str())
    });
    // ● follows the session's ACTIVE branch (law) — the viewed branch may
    // be another one entirely.
    let dot = if viewed == active { "●" } else { "○" };
    let mut label = format!("{dot} {name} · {}", tree_branch_meta(projection));
    if viewed.is_some() {
        label.push_str(" · esc up to parent");
    }
    let mut rows = vec![TreeRow::Branch {
        branch: viewed.clone(),
        label,
    }];
    // Fork markers for the viewed branch, keyed by their exact fork node.
    let mut forks: Vec<&haider_protocol::branch::BranchDescriptor> = model
        .branch_state
        .descriptors()
        .filter(|descriptor| descriptor.source_branch_id == viewed)
        .collect();
    let mut push_forks_at =
        |rows: &mut Vec<TreeRow>, node: Option<&haider_protocol::ids::NodeId>| {
            forks.retain(|descriptor| {
                let here = node.is_some_and(|node| &descriptor.fork_node_id == node);
                if here {
                    let meta = model
                        .branch_state
                        .view(&descriptor.branch_id)
                        .map_or_else(String::new, |view| {
                            format!(" · {}", tree_branch_meta(&view.projection))
                        });
                    rows.push(TreeRow::Fork {
                        branch: descriptor.branch_id.clone(),
                        label: format!("  │   └⑂ {}{meta} · ⏎ open", descriptor.name),
                    });
                }
                !here
            });
        };
    for (index, entry) in projection.entries().iter().enumerate() {
        let label = match entry {
            TranscriptEntry::User { text, .. } => {
                let mut text = text.replace(['\n', '\r'], " ");
                if text.chars().count() > 58 {
                    text = text.chars().take(58).collect::<String>() + "…";
                }
                format!("  ├─ ❯ {text}")
            }
            TranscriptEntry::Item(block) => {
                let haider_protocol::item::TurnItem::ContextCompaction {
                    tokens_before,
                    tokens_after,
                    ..
                } = &block.item
                else {
                    continue;
                };
                let detail = match (tokens_before, tokens_after) {
                    (Some(before), Some(after)) => format!(
                        "⊟ compacted {} → {}",
                        crate::format::fmt_tok(*before),
                        crate::format::fmt_tok(*after)
                    ),
                    _ => "⊟ context compacted".to_owned(),
                };
                format!("  ├─ {detail}")
            }
            _ => continue,
        };
        // The durable coordinates riding this display row, when the
        // journal committed them (demo rows honestly carry none).
        let coords = projection.node_of_entry(index).and_then(|node| {
            model
                .branch_state
                .node_seq(node)
                .map(|seq| (node.clone(), seq))
        });
        let node = projection.node_of_entry(index).cloned();
        rows.push(TreeRow::Node {
            branch: viewed.clone(),
            coords,
            label,
        });
        push_forks_at(&mut rows, node.as_ref());
    }
    // Unanchored markers trail honestly (never guessed under another row).
    for descriptor in forks {
        let meta = model
            .branch_state
            .view(&descriptor.branch_id)
            .map_or_else(String::new, |view| {
                format!(" · {}", tree_branch_meta(&view.projection))
            });
        rows.push(TreeRow::Fork {
            branch: descriptor.branch_id.clone(),
            label: format!("  │   └⑂ {}{meta} · ⏎ open", descriptor.name),
        });
    }
    rows
}

/// The `/tree` breadcrumb: branch names root → viewed (sim treeCrumb,
/// tui.js:2339-2345), cycle-guarded against a corrupt source chain.
#[must_use]
pub fn tree_crumb(model: &AppModel) -> Vec<String> {
    let mut crumb = Vec::new();
    let mut cursor = tree_viewed(model);
    let mut hops = model.branch_state.named_count() + 1;
    while hops > 0 {
        hops -= 1;
        match cursor {
            None => {
                crumb.insert(0, "main".to_owned());
                break;
            }
            Some(id) => match model.branch_state.descriptor(&id) {
                Some(descriptor) => {
                    crumb.insert(0, descriptor.name.clone());
                    cursor = descriptor.source_branch_id.clone();
                }
                None => break,
            },
        }
    }
    crumb
}

pub fn flatten_chips(chips: &[ChipModel]) -> Vec<(usize, &ChipModel)> {
    let mut rows = Vec::new();
    fn walk<'t>(chips: &'t [ChipModel], depth: usize, rows: &mut Vec<(usize, &'t ChipModel)>) {
        for chip in chips {
            rows.push((depth, chip));
            walk(&chip.children, depth + 1, rows);
        }
    }
    walk(chips, 0, &mut rows);
    rows
}

/// One controlled-session row on the aura stage.
#[derive(Debug, Clone)]
pub struct AuraAgentRow {
    pub name: String,
    pub device: String,
    pub state: ChipDisplayState,
    pub activity: String,
}

/// The aura orchestrator surface (§3 — demo-local; sim seedVoiceSession,
/// tui.js:121-138). Exiting the screen does NOT reset this state.
#[derive(Debug)]
pub struct AuraModel {
    /// true = gpt-realtime (native duplex); false = composed STT·LLM·TTS.
    pub realtime: bool,
    pub muted: bool,
    pub state: AuraState,
    pub roster: Vec<AuraAgentRow>,
    pub log: Vec<String>,
    pub transcript: SessionProjection,
    /// Per-run counter for unique stream item ids.
    pub runs: u64,
}

impl AuraModel {
    #[must_use]
    pub fn seed() -> Self {
        let mut transcript = SessionProjection::new();
        transcript.set_voice_live(true);
        transcript.apply(&EventPayload::Item(
            haider_protocol::item::ItemEvent::Completed {
                item_id: haider_protocol::ids::ItemId::new("aura-seed"),
                item: haider_protocol::item::TurnItem::AgentMessage {
                    text: "Aura online. I orchestrate sessions across your devices — I don't write code myself. Say or type what to spin up.".to_owned(),
                },
            },
        ));
        transcript.set_voice_live(false);
        Self {
            realtime: true,
            muted: false,
            state: AuraState::Idle,
            roster: vec![AuraAgentRow {
                name: "billing-service".to_owned(),
                device: "workstation".to_owned(),
                state: ChipDisplayState::Done,
                activity: "webhook tests green".to_owned(),
            }],
            log: vec![
                "spawned billing-service on workstation".to_owned(),
                "ran cargo test -p billing — 216 passed".to_owned(),
            ],
            transcript,
            runs: 0,
        }
    }

    /// `VOICE_ENGINES[engine].label` (tui.js:121-138).
    #[must_use]
    pub const fn engine_label(&self) -> &'static str {
        if self.realtime {
            "gpt-realtime-2"
        } else {
            "whisper → gpt-5.6 → openai"
        }
    }

    /// `VOICE_ENGINES[engine].kind`.
    #[must_use]
    pub const fn engine_kind(&self) -> &'static str {
        if self.realtime {
            "native duplex"
        } else {
            "STT·LLM·TTS"
        }
    }
}

impl Default for AuraModel {
    fn default() -> Self {
        Self::seed()
    }
}

/// Which of the login card's two fields owns the keystrokes.
///
/// §5.3: the alias is a visible, editable field, not a hidden
/// auto-generated name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginFocus {
    Alias,
    Key,
}

/// The daemon's alias grammar (`normalize_account_alias`):
/// `[a-z0-9][a-z0-9._-]{0,63}`. Checked client-side so the card can refuse
/// a submit the daemon would bounce; the daemon remains the authority.
#[must_use]
pub fn account_alias_ok(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte))
}

/// A character's place in the alias grammar, as typed: uppercase folds to
/// lowercase, anything outside `[a-z0-9._-]` is dropped at the keyboard.
fn alias_char(c: char) -> Option<char> {
    let c = c.to_ascii_lowercase();
    (c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')).then_some(c)
}

/// The smallest free alias on `base`: `base`, then `base-2`, `base-3`, …
/// against the CURRENT management snapshot (§5.3). The daemon recanonizes
/// and rechecks at commit, so a concurrent client losing this race gets a
/// typed error, never a silent overwrite of intent.
#[must_use]
pub fn smallest_free_alias(base: &str, rows: &[AccountRow]) -> String {
    let taken = |candidate: &str| rows.iter().any(|row| row.alias == candidate);
    let mut candidate = base.to_owned();
    let mut suffix = 1u32;
    while taken(&candidate) {
        suffix += 1;
        candidate = format!("{base}-{suffix}");
    }
    candidate
}

/// The `/login <provider> api` masked key card (W3c3 M3 — report R10).
///
/// SECRET HYGIENE is the whole point of this type existing instead of
/// reusing the composer:
///
/// * the key lives in a [`Zeroizing`] buffer that wipes on drop, on
///   submit, and on cancel;
/// * `Debug` is REDACTED, so `{:?}` on the whole `AppModel` (panic
///   teardown, a stray log) cannot print it;
/// * the renderer is given the LENGTH, never the text, so no frame — and
///   therefore no snapshot, no scrollback, no selection copy — can carry
///   it;
/// * nothing in it reaches the composer's input ring, the per-surface
///   drafts, or the demo store's DTO.
pub struct LoginCard {
    /// Provider being logged into (`anthropic`).
    pub provider: String,
    /// The visible, editable credential alias (§5.3) — prefilled with the
    /// smallest free `«provider»-api[-N]`, or the slash command's token.
    pub alias: String,
    /// Which field the next keystroke lands in. The KEY by default: the
    /// prefill makes the alias correct without a keystroke in the common
    /// path.
    pub focus: LoginFocus,
    /// The typed key. Never rendered, never persisted, never logged.
    secret: zeroize::Zeroizing<String>,
    pub stage: LoginStage,
    /// The CURRENT stage issuance's ATTEMPT IDENTITY (TUI6.3 fix 1,
    /// re-scoped by TUI6.5 / review r5): minted at card open AND
    /// RE-MINTED at every submit — the identity is per-STAGE-ISSUANCE,
    /// not per-card. r5's probe proved card-scoped identity unsound: a
    /// timeout cleared the driver binding but the retype revived the
    /// SAME id, so the timed-out stage's late reply passed both gates
    /// and minted the stale vault reference. A retry is a NEW issuance;
    /// the moment it is minted, the previous issuance's id is dead
    /// forever (never reused, never re-bound). It is carried end-to-end
    /// — the queued [`AppRequest::LoginApi`], the driver's binding, the
    /// link's request context, and every reply gate — and closing the
    /// card retires the current issuance.
    pub attempt: u64,
}

/// Where the login card is in its two-transaction flow (stage → login).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStage {
    /// Typing the key.
    Entry,
    /// `vault.stage` + `account.login_api` are in flight; the local copy of
    /// the key is ALREADY wiped (it lives only in the staged frame).
    Submitting,
    /// A typed failure, carrying its recovery text.
    Failed(String),
    /// Committed: the descriptor's identity, for the confirmation row.
    Done(String),
}

impl std::fmt::Debug for LoginCard {
    /// Redacted by construction (the W3c2 precedent: `SecretWire`'s Debug
    /// was mutation-killed TWICE for exactly this). The length is omitted
    /// too — a key's length is itself a hint.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoginCard")
            .field("provider", &self.provider)
            .field("alias", &self.alias)
            .field("secret", &"<redacted>")
            .field("stage", &self.stage)
            .finish()
    }
}

impl LoginCard {
    #[must_use]
    pub fn new(provider: String, alias: String, attempt: u64) -> Self {
        Self {
            provider,
            alias,
            focus: LoginFocus::Key,
            secret: zeroize::Zeroizing::new(String::new()),
            stage: LoginStage::Entry,
            attempt,
        }
    }

    /// One typed character into the ALIAS field, grammar-filtered at the
    /// keyboard (uppercase folds; illegal characters vanish).
    pub fn alias_push(&mut self, c: char) {
        if self.accepts_input()
            && let Some(c) = alias_char(c)
        {
            self.alias.push(c);
        }
    }

    pub fn alias_backspace(&mut self) {
        if self.accepts_input() {
            self.alias.pop();
        }
    }

    /// How many MASK GLYPHS to draw — the only thing the renderer learns.
    #[must_use]
    pub fn masked_len(&self) -> usize {
        self.secret.chars().count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.secret.is_empty()
    }

    /// True while the card accepts typing — entry, and after a FAILURE
    /// (the recovery text says "type it again" / "press ⏎ to try again",
    /// and a card that refused the retry it advertises is a dead end —
    /// review P2-1). `Submitting` and `Done` are closed to input.
    #[must_use]
    pub const fn accepts_input(&self) -> bool {
        matches!(self.stage, LoginStage::Entry | LoginStage::Failed(_))
    }

    /// Append one typed character. Non-printing keys never reach here.
    pub fn push(&mut self, c: char) {
        if self.accepts_input() {
            self.secret.push(c);
        }
    }

    /// Bracketed paste — the whole clipboard at once (keys are pasted far
    /// more often than typed).
    pub fn push_str(&mut self, text: &str) {
        if self.accepts_input() {
            self.secret.push_str(text.trim());
        }
    }

    pub fn backspace(&mut self) {
        if self.accepts_input() {
            self.secret.pop();
        }
    }

    /// TAKE the key for staging: the card's copy is emptied in the same
    /// move, so between here and the wire there is exactly one live copy.
    fn take_secret(&mut self) -> haider_rpc::SecretWire {
        let taken = std::mem::replace(&mut self.secret, zeroize::Zeroizing::new(String::new()));
        haider_rpc::SecretWire::new(taken.as_str())
    }
}

/// Which runtime is driving this model (W3c3 M2).
///
/// The reducer is source-agnostic by design. What is NOT source-agnostic
/// is ONE question, asked at every site where the reducer would otherwise
/// invent session state: **may this model fabricate locally?** See
/// [`Self::fabricates_locally`] — in live mode the daemon owns the
/// sessions, their rows, their transcripts and their cards, so anything
/// minted here would have to be reconciled with the truth that follows,
/// and reconciliation is where duplicate rows and un-closable cards come
/// from.
///
/// Every branch in the reducer, exhaustively (W3c3.1, review D2-1 — the
/// previous charter named three, there were four, and one of the three it
/// named was not a reducer branch at all):
///
/// | Site | Demo | Live |
/// |---|---|---|
/// | mid-turn composer submit | local queue / steer row | `SubmitText` |
/// | launcher composer submit | `new_session` | `CreateSession` |
/// | voice submit (`submit_voice`) | local ◉ row + note | `CreateSession` / `SubmitText` |
/// | Esc mid-turn | local `Cancelled` + note | `Interrupt`; the committed `RunState` paints it |
/// | `/reset` | reseeds the demo world | honest flash |
/// | `/voice`, `/tools`, `/say` | local card | honest flash |
/// | `/compact` | local `turn_active` + demo beat | honest flash |
/// | `enter_aura` (`/aura`, ◉ Aura row) | the aura stage | honest flash |
/// | ◉ talk hold | local `listening` + demo timer | honest flash |
/// | subagent submit | `ChipSubmit` (scripted beat) | `ChipSubmit` → `agent.message` (S3); feature-gated honest flash |
/// | subagent close | `ChipClose` | honest flash |
/// | shell builtins (`ls` · `cd` …) | the demo VFS | honest flash |
/// | `/sessions` | honest stub (the sim's screen is unbuilt) | real listing + open |
///
/// The last row is the one INVERSION: demo refuses and live acts, because
/// what demo refuses there is a sim surface this port has not built, not a
/// fabrication. Every row above it is the same shape — demo may invent
/// local state, live may not.
///
/// Menu ANSWER coordinates are deliberately absent: they are not a reducer
/// decision. The reducer emits one source-neutral [`OutboundAnswer`] and
/// `LiveDriver::drain_answers` / `DemoDriver` supply their own
/// coordinates.
///
/// The default is [`Self::Demo`] so the entire pre-W3c3 corpus keeps its
/// exact meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeMode {
    #[default]
    Demo,
    Live,
}

impl RuntimeMode {
    /// May the reducer invent session state — a row, a transcript line, a
    /// card — that no committed envelope carries?
    ///
    /// This is the ONE question behind every mode branch in the reducer.
    /// It is expressed as a predicate rather than an identity check so
    /// every site asks the same question. Call sites use BOTH polarities
    /// (a demo-only path guards with the positive form; a live refusal
    /// guards with the negative), so a bare grep count is NOT an audit —
    /// the audit is the exhaustive `AppRequest` match in the live driver's
    /// `handle_request`, where a new variant is a compile error, plus the
    /// per-surface gate tests in `w3c31_r2_tests` (review r2 NF-4).
    ///
    /// ONE site reads the mode and is NOT about fabrication:
    /// [`AppModel::launcher_rows`], which is a DISPLAY policy (the sim's
    /// three rows in demo, the reachable digit span live). It is named
    /// here so the enumeration stays total — the previous charter's claim
    /// that nothing else compared `RuntimeMode` was falsified by the same
    /// round that wrote it (W3c3.1 r2, P2-C).
    #[must_use]
    pub const fn fabricates_locally(self) -> bool {
        matches!(self, Self::Demo)
    }
}

/// Side effects the reducer requests from the runtime (the reducer itself
/// never performs IO).
#[derive(Debug, Clone, PartialEq)]
pub enum AppRequest {
    /// W8b: live `!` shell escape — one exact command for the SESSION
    /// daemon's workspace (receipt-backed `shell.exec`; zero provider
    /// requests). Never demo vocabulary.
    ShellExec { command: String },
    /// W10b: durable account removal (receipt-backed `account.remove`).
    AccountRemove { alias: String },
    /// W10b: durable custom-provider removal (`provider.remove`) — the
    /// daemon refuses builtins and account-referenced providers with
    /// typed reasons; the client never pre-judges.
    ProviderRemove { provider: String },
    /// W8b: `/tools` live — read the daemon's canonical tool inventory.
    ToolsRefresh,
    /// `/hooks` live (H4): read the daemon's hook discovery for `cwd` —
    /// workspace + profile truth. The cwd is CAPTURED AT ISSUANCE (the B2b
    /// capture law): a later screen or session switch cannot retarget the
    /// listing.
    HooksRefresh { cwd: String },
    /// A trust (`trusted == true`) or revoke (`false`) for one digest —
    /// receipted daemon commands (H3's R2 pattern). The receipt installs
    /// NOTHING locally: the driver chains a fresh `hooks.list` and daemon
    /// truth moves the rows (the branch discipline).
    HooksTrust { digest: String, trusted: bool },
    /// Run a respond() turn for user text. `voice` turns skip the script's
    /// UserMessage (the reducer already pushed the ◉ row); `title` asks the
    /// driver to schedule the 1.5 s auto-title micro-call, which names the
    /// session INSIDE its callback (sim tui.js:1219-1227, review P2-12).
    SubmitText {
        text: String,
        voice: bool,
        title: bool,
        /// The branch CAPTURED AT ISSUANCE (B2b, research risk 4): a
        /// submit queued before a later `/branch` switch still lands on
        /// the branch it was typed on — never re-read from mutable
        /// active-branch state downstream. `None` = legacy/main.
        branch: Option<haider_protocol::ids::BranchId>,
        /// Ready attachment blocks taken from the draft at ISSUANCE (B4b)
        /// — same capture law as `branch`: a later `/branch` switch or a
        /// chip removal cannot retarget what this submit carries.
        attachments: Vec<haider_protocol::tool::AttachmentBlock>,
    },
    /// `/attach <path>` (B4b): read + magic-sniff the file. Filesystem IO
    /// is SHELL-owned (the reducer never performs IO), uniform with
    /// [`Self::CopySelection`]: the runtime reads bounded bytes, then
    /// either flashes the honest refusal or hands the bytes back through
    /// [`AppModel::begin_attachment_upload`].
    AttachRead { path: String },
    /// Upload one attachment's bytes into the daemon CAS (B4b) — the
    /// receipt-free `artifact.put` (content-addressed, naturally
    /// idempotent; deliberately NO command id and never outboxed).
    /// `upload`/`surface` are CLIENT-side identity only: the wire carries
    /// the bytes, the link's request context carries these back so the
    /// reply completes the chip on the ISSUING draft even after a
    /// surface switch.
    AttachUpload {
        upload: u64,
        surface: DraftKey,
        bytes: ArtifactBytes,
    },
    /// Cancel EVERY session's and every chip's arms and clear all demo
    /// token meters — a GLOBAL reset, not a polite stop (renamed from
    /// `StopScripts`, review TUI4.1 D3-4: the old name undersold the
    /// blast radius). Pushed only by [`AppModel::fresh_session`] — the
    /// `/reset` teardown and the scratch surface's fresh start. Aura
    /// deliberately survives (sim tui.js:1950-1955); `/reset` resets it
    /// separately via [`Self::ResetAura`].
    ResetAllSessions,
    /// Esc mid-turn: stop the playing script; the reducer already settled
    /// the projection into idle(i) (sim interrupt, tui.js:1551-1567).
    /// `branch` is captured at issuance (B2b) — client-side identity; the
    /// wire cancel pins the run by its `run_id`.
    Interrupt {
        branch: Option<haider_protocol::ids::BranchId>,
    },
    /// Manual `/compact` (sim tui.js:1791-1806). `branch` is captured at
    /// issuance (B2b): a later switch cannot retarget the compaction.
    Compact {
        branch: Option<haider_protocol::ids::BranchId>,
    },
    /// `/branch new [name]` (B2b): fork the session at EXACT captured
    /// coordinates — session, source branch, and the source's last
    /// committed node/seq from the fork-coordinate tracker. Receipt
    /// correlation installs nothing; the daemon's `BranchCreated` journal
    /// fact is the only branch materializer.
    BranchCreate {
        session: haider_protocol::ids::SessionId,
        source_branch: Option<haider_protocol::ids::BranchId>,
        fork_node_id: haider_protocol::ids::NodeId,
        fork_seq: u64,
        name: Option<String>,
    },
    /// A drag selection finished (owner item 9): the RUNTIME extracts the
    /// selected text from its last-drawn frame and copies it (pbcopy, then
    /// OSC 52 — see [`crate::clipboard`]). A request because the reducer
    /// never sees the rendered buffer; headless tests assert the request
    /// itself.
    CopySelection,
    /// TUI5 items 4+5: copy MODEL-known text (the composer selection on
    /// ⌃C or drag-release). Unlike [`Self::CopySelection`] the reducer
    /// already holds the exact text, so it travels in the request; the
    /// runtime runs the same pbcopy + OSC 52 + honest-flash path.
    CopyText(String),
    /// The ◉ talk hold started — fire the canned phrase after 1300 ms.
    Talk,
    /// Steer/message a subagent (respondChip, §2.4). Demo: a full turn on
    /// the CHIP's state machine (scripted beats). Live (S3): the driver
    /// rides S1's `agent.message` wire — the daemon chooses steer vs
    /// queued and its receipt flashes the delivery kind; the transcript
    /// rows arrive as journal facts, nothing painted locally.
    ChipSubmit { agent: String, text: String },
    /// Close a chip (✕ / the docs-recovery close arm): lifecycle flags are
    /// the reducer's; the driver owns the 5 s removal + resume timers.
    ChipClose { agent: String },
    /// Run an aura orchestrate turn (§3.4).
    AuraSubmit { text: String, voice: bool },
    /// The aura hold-to-talk: 1100 ms listening, then the canned phrase.
    AuraTalk,
    /// `/reset` reseeded the aura — bump its script guard.
    ResetAura,
    /// The masked login card was submitted (W3c3 M3, report R10): stage
    /// the secret over the non-journaled vault RPC, then commit the login.
    /// The raw key rides HERE and nowhere else — it never enters the
    /// composer, a draft, the input ring, a transcript row or the store.
    LoginApi {
        /// The card's attempt identity (TUI6.3 fix 1) — the driver binds
        /// its stage/login correlation to this, and a retire kills it.
        attempt: u64,
        provider: String,
        alias: Option<String>,
        /// The wire's own secret carrier: zeroized on drop and REDACTED in
        /// `Debug`, so `AppRequest`'s derived `Debug` — and any panic
        /// teardown that prints it — cannot leak the key. (W3c2's review
        /// mutation-killed an un-redacted `SecretWire` Debug TWICE; reusing
        /// that type is how this lane inherits the guarantee instead of
        /// re-earning it.)
        secret: haider_rpc::SecretWire,
    },
    /// The login card CLOSED while its attempt might still be queued or
    /// in flight (TUI6.3 fix 1): the driver retires every trace of the
    /// attempt — pending stage correlation, the login command id, the
    /// deadline — so late replies are ignored, silently (the user already
    /// saw the cancel). Queued-but-undrained `LoginApi` requests for the
    /// attempt were already dropped by `close_login_card` before this is
    /// pushed.
    LoginRetired { attempt: u64 },
    /// LIVE launcher submit (W3c3, report R11 cut 4): ask the daemon to
    /// create a session for `text`. Deliberately NOT accompanied by a row,
    /// a session id, a screen flip or a turn: in live mode there is no
    /// local truth to fabricate, so the launcher shows nothing new until
    /// `session.create` answers. The demo path (`new_session`) is the
    /// opposite by design — its world IS local.
    CreateSession { text: String },
    /// The strict gap law fired (W3c3, report R11 cut 2): reduction STOPPED
    /// for `session` with its cursor still at `after_seq`, and NOTHING later
    /// may be applied until the driver reattaches from there. The demo
    /// driver never produces gaps and ignores this; `LiveDriver` reattaches.
    Reattach {
        session: haider_protocol::ids::SessionId,
        after_seq: u64,
    },
    /// Fetch/refresh the `/accounts` rows (`account.list`). Pushed on
    /// entering the screen; the demo driver answers from the seed list.
    AccountsRefresh,
    /// Read the daemon's device-credential discovery report (D2,
    /// `account.device_candidates`). Pushed on SCREEN ENTRY only —
    /// `/accounts` and `/providers`, both feature-gated by the reducer —
    /// never polled. Unreachable in demo by that gate.
    DeviceCandidatesRefresh,
    /// Import one discovered candidate by its opaque daemon-derived id
    /// (D2, `account.import_device`). Receipted + durable: the daemon
    /// re-reads the local store itself — no credential bytes ride this
    /// request, and NOTHING is installed locally on the reply; the new
    /// account lands via the chained `account.list` refresh.
    DeviceImport { candidate: String },
    /// `account.set_active` for the clicked/entered row. The model already
    /// holds `pending_select` — the dot moves only when the driver's reply
    /// applies (optimism forbidden, report §5.1).
    AccountSetActive { alias: String },
    /// Fetch/refresh the `/providers` summaries (`provider.list`).
    ProvidersRefresh,
    /// Fetch/refresh the `/usage` snapshot (U1's `usage.report`). A READ —
    /// pushed on screen entry and by `r`, live-only vocabulary: the demo
    /// opens an honest empty state and never fabricates a meter.
    UsageRefresh,
    /// `account.set_default_model` under the expected-revision CAS. The
    /// default marker moves only on the correlated reply.
    SetDefaultModel {
        provider: String,
        model: String,
        expected_revision: u64,
    },
    /// F2a: receipted live-session model selection (`session.select_model`)
    /// — the picker's ⏎ on an attached session. The provider always rides
    /// along (a picker row IS a model × provider pair); the identity pair
    /// moves only on the correlated RESOLVED reply.
    SelectModel {
        session: SessionId,
        model: String,
        provider: String,
    },
    /// G2: receipted live-session rename (`session.rename`) — `/rename`
    /// on an attached session. The daemon normalizes the title; the name
    /// moves only on the correlated NORMALIZED reply (optimism forbidden,
    /// same law as [`Self::SelectModel`]).
    Rename { session: SessionId, title: String },
    /// G3: receipted live-session effort selection
    /// (`session.select_effort`). `None` reverts to the provider default;
    /// the identity's reasoning segment moves only on the correlated reply.
    SelectEffort {
        session: SessionId,
        effort: Option<String>,
    },
    /// G3: the receipted fast-mode toggle (`session.select_fast`).
    SelectFast { session: SessionId, enabled: bool },
    /// Start an OAuth add flow (`account.oauth_start`) for the card.
    OAuthAddStart {
        provider: String,
        alias: String,
        attempt: u64,
    },
    /// Cancel the card's flow (`account.oauth_cancel` when one is bound).
    OAuthAddCancel { attempt: u64 },
    /// Create a custom OpenAI-compatible provider (`provider.configure`,
    /// W5g-4). Always a CREATE from the card: identity fields ride along
    /// and the daemon rejects a collision with an existing profile.
    ProviderConfigure {
        attempt: u64,
        name: String,
        origin: String,
        /// The served model id — seeds the inventory AND the default (an
        /// enabled create requires both, daemon law).
        model: String,
        /// G4a: true for auth-None presets — the wire carries
        /// `auth_requirement: none` instead of `api_key`.
        keyless: bool,
        expected_revision: u64,
    },
    /// G4a: re-run one provider's model discovery
    /// (`provider.models_refresh`) — pushed by `f` on `/providers` and by a
    /// committed keyless configure. A READ against the stored origin; the
    /// inventory moves only on the daemon's refreshed snapshot.
    ProviderModelsRefresh { provider: String },
    /// Open a URL in the user's browser (runtime-owned effect; the demo
    /// flashes it instead). Carried for the OAuth authorize hop — the URL
    /// always originates from the daemon's sanctioned registration.
    OpenUrl { url: String },
    /// T2: read the vaulted Deepgram key (`transcription.secret_get`,
    /// UDS-only). A READ — never outboxed; the answer routes by
    /// [`crate::talk::TalkState::secret_intent`].
    TranscriptionSecretRead,
    /// T2: vault (or clear) the Deepgram key
    /// (`transcription.secret_set`). The secret rides as [`SecretWire`] —
    /// redacted Debug, zeroize-on-drop — and NOWHERE else. Deliberately
    /// non-durable (no receipt may carry a secret; the vault file is the
    /// truth), so no command id and no outbox.
    TranscriptionSecretStore {
        secret: haider_rpc::SecretWire,
        clear: bool,
    },
    /// T2: a TUI-process STT effect (mic capture, engines, model
    /// downloads, config IO) — runtime-owned like [`Self::CopySelection`];
    /// `live_pass` hands it to the talk supervisor. Demo mode can never
    /// reach it (the reducer refuses `/talk` there).
    TalkShell(crate::talk::TalkShellCommand),
    /// Quit the app.
    Quit,
}

/// Side effects only the DEMO runtime can perform (W3c3, report R11 cut 3).
///
/// Deliberately NOT an [`AppRequest`]: the common request vocabulary must
/// carry no demo concepts, so `run_live` cannot even name these. They ride
/// their own [`AppModel::demo_requests`] queue, which only `run_demo`
/// drains — a live reset therefore can never delete demo persistence, and a
/// demo reset can never reach a profile mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemoRequest {
    /// `/reset` reseeded the demo world; the state file dies with it (sim
    /// tui.js:1918: `localStorage.removeItem("haider-tui-v1")`). Runtime-
    /// owned like `CopySelection`: only the interactive loop knows the
    /// store path.
    PurgeStore,
}

/// Per-session voice pipeline (sim `DEFAULT_VOICE`, tui.js:110 — voice
/// ships ON with Whisper STT → OpenAI TTS, non-duplex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceState {
    pub enabled: bool,
    pub stt: String,
    pub tts: String,
    pub duplex: bool,
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            enabled: true,
            stt: "whisper-large-v3".to_owned(),
            tts: "openai-tts".to_owned(),
            duplex: false,
        }
    }
}

impl VoiceState {
    /// The status-bar segment (sim tui.js:2846-2850): duplex shows the
    /// engine name; else `{stt-first-word}→{tts-first-word}`.
    #[must_use]
    pub fn bar_label(&self) -> String {
        if self.duplex {
            return "gpt-realtime".to_owned();
        }
        let first = |s: &str| s.split('-').next().unwrap_or("").to_owned();
        format!("{}→{}", first(&self.stt), first(&self.tts))
    }
}

/// A clickable region's action (hit-testing: render reports regions, the
/// runtime maps clicks back through [`AppModel::handle_hit`]).
///
/// Hits carry VALUES, not row indices (review r2 P2-2): a click resolved
/// through the previous frame's map must activate exactly what was on
/// screen — or be dropped — never a different row the model drifted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    /// The launcher row's SESSION ID at render time (review P2-9: an
    /// ordinal resolved against current state could attach a different
    /// session than the one clicked).
    ///
    /// W3c3.1 (review P1-6): this used to carry the row's display NAME,
    /// which live rows do not have — render substituted the literal
    /// `"session"` and the click handler then searched for a row actually
    /// named that, so every live row was unclickable. The session id is
    /// the coordinate the row is *made of*: it exists in both modes, it is
    /// what `open_session` takes, and it cannot collide the way a name
    /// can.
    AttachSession(SessionId),
    /// Aura / Accounts / Peers launcher rows, by identity not ordinal.
    ExtraRow(LauncherRow),
    /// The palette row's actual content at render time.
    PaletteRow(PaletteItem),
    /// A menu option, bound to the menu it was rendered for.
    MenuOption {
        menu: MenuId,
        index: usize,
    },
    /// One `/theme` picker row (model-local card): hover previews, click
    /// commits. Carries the MENU index it was rendered for.
    ThemeOption(usize),
    /// One `/effort` picker row (G3).
    EffortOption(usize),
    /// One `/model` picker row (F2a). VALUE-CARRYING (review r2 P2-2): the
    /// rect holds the pair it was rendered for, so a stale hit map can
    /// never select a different row.
    ModelPickerRow {
        provider: String,
        model: String,
    },
    /// One `/usage` account tab chip (U2). VALUE-CARRYING: the provider +
    /// the index WITHIN its group, so a stale hit map can never select a
    /// different account.
    UsageAccountTab {
        provider: String,
        index: usize,
    },
    BackChip,
    TalkChip,
    HelpHint,
    /// A SubTree row — opens the chip's own view.
    ChipRow(String),
    /// The SubTree header (collapse toggle).
    SubTreeToggle,
    /// The pinned-todos header (collapse toggle, owner item 7).
    TodosToggle,
    /// One pinned-todo row. Carries the todo's id so a stale rect can only
    /// ever light the row it was measured on. Clicking a row does nothing —
    /// the sim's rows are not buttons; the hit exists so the row can take
    /// hover chrome like every other list row.
    TodoRow(u32),
    /// `⌂ {session} — back to the main transcript` (subagent screen).
    SessionHome,
    /// The chip view's `✕ close`.
    ChipCloseBtn(String),
    /// A breadcrumb hop in the chip view (session root = empty path).
    ChipCrumb(Vec<String>),
    /// Aura stage chrome.
    AuraEngine,
    AuraMute,
    AuraExit,
    AuraTalkBtn,
    /// The sticky origin line — carries the scroll-back that puts the
    /// producing prompt's first row at the viewport top (sim jumpToSticky:
    /// stay AT the prompt, tui.js:2637-2645).
    StickyJump(u16),
    /// One `/tree` row, by VALUE (B2b-m3): the click validates the carried
    /// row against the freshly built rows and selects it (sim
    /// tui.js:3375-3377 onClick = setTreeSel) — a stale hit whose row was
    /// replaced matches nothing and is dropped whole, never activated.
    TreeRow(TreeRow),
    /// One composer text row (TUI5 item 5). Value-carrying like every
    /// hit: `start` is the ABSOLUTE byte offset (in the composer text) of
    /// the row's visible slice at render time, `content` the slice
    /// itself — a click maps its column through `content`'s graphemes.
    ///
    /// TUI5.1 fix 2: the hit also carries the SURFACE it was rendered for
    /// and the composer's text REVISION at render time — press and drag
    /// validate both against the live composer and DROP the event on any
    /// mismatch. One authority, no per-callsite length heuristics: a
    /// stale frame's hit can never move the caret into fresh text or
    /// another surface's draft.
    ///
    /// TUI6.1 fix 1: it also carries the GEOMETRY EPOCH the frame was
    /// drawn at ([`AppModel::geometry_epoch`]) — a resize between the
    /// frame and the click re-lays the band (wrap points AND row
    /// positions), which the text revision cannot see (resize mutates no
    /// text, so the TUI5 guard ACCEPTED pre-resize hits). Press and drag
    /// validate the epoch exactly like the revision: a hit from stale
    /// geometry is dropped whole, never remapped.
    ComposerText {
        start: usize,
        content: String,
        surface: DraftKey,
        revision: u64,
        epoch: u64,
    },
    /// One `/accounts` row, by its GLOBAL alias (value-carrying: a stale
    /// rect can only ever select the row it was measured on).
    AccountRow(String),
    /// One add-row button on `/accounts` (sim tui.js:3621-3628).
    AccountAdd(AccountAddKind),
    /// One SUPPORTED "found on this device" candidate row (D2), by its
    /// opaque daemon-derived id (value-carrying: a stale rect can only
    /// ever import the candidate it was measured on). Unsupported rows
    /// get NO hit at all — dim, honest, inert.
    DeviceImport(String),
    /// One `/providers` model chip: click sets the provider default.
    ProviderModel {
        provider: String,
        model: String,
    },
    /// The `/providers` row's `[accounts]` navigation chip.
    ProviderAccounts,
    /// One `/hooks` row, by its DIGEST (value-carrying: a stale rect can
    /// only ever select the hook it was measured on — a refresh that
    /// replaced the digest matches nothing and the click is dropped).
    HookRow(String),
}

/// The `/accounts` add-row buttons (sim order, tui.js:3621-3628; B6b adds
/// the two providers the sim never knew — Kimi's device-flow OAuth and the
/// Gemini API key — between the sim rows and the HF/custom tail).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountAddKind {
    OpenAiOAuth,
    AnthropicOAuth,
    OpenAiApi,
    AnthropicApi,
    KimiOAuth,
    GeminiApi,
    HuggingFace,
    OpencodeZen,
    OpencodeGo,
    /// G4a: local Ollama preset — keyless (auth-None) custom provider at
    /// the default `http://127.0.0.1:11434/v1`.
    Ollama,
    /// G4a: local LM Studio preset — keyless (auth-None) custom provider
    /// at the default `http://127.0.0.1:1234/v1`.
    LmStudio,
    Custom,
}

/// One answer on its way to the client, tagged with the SURFACE GENERATION
/// that RENDERED the card (review r2 P1-1). Answers ride the never-cancelled
/// control tag so delivery is guaranteed, but CONSUMPTION checks the
/// origin: an answer to a card the user has since replaced must never
/// reconfigure the session that took its place. The sim gets this for free
/// — its `askMenu` promise closes over the originating session/branch ids
/// and its menu ids are per-open `nid()`s (tui.js:849-878).
///
/// W3c3 (report R11 cut 1): the origin is a [`UiGeneration`], not the
/// protocol [`SessionId`]. This is an ASYNCHRONOUS RESPONSE GUARD — the
/// exact role the report forbids a session id from playing — and the
/// generation carries the old numeric id's semantics unchanged (monotonic,
/// never reused, `SCRATCH` for the no-session surface).
#[derive(Debug, Clone, PartialEq)]
pub struct OutboundAnswer {
    pub origin: UiGeneration,
    /// The branch DISPLAYED when the user answered, captured at issuance
    /// (B2b): client-side identity only — the wire answer resolves at the
    /// menu's committed opening coordinates, and the daemon derives the
    /// menu's branch from its opening envelope. A `/branch` switch between
    /// answer and drain can therefore never retarget it.
    pub branch: Option<haider_protocol::ids::BranchId>,
    pub answer: MenuAnswer,
}

impl std::ops::Deref for OutboundAnswer {
    type Target = MenuAnswer;
    fn deref(&self) -> &MenuAnswer {
        &self.answer
    }
}

/// The launcher's non-session rows (value-carrying hit payload, P2-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherRow {
    Aura,
    Accounts,
    Peers,
}

/// A composer surface's identity (TUI5 item 9): the launcher, one session
/// (by its LOCAL generation — the monotonic-identity law means a key can
/// never be reworn), or the aura. The SUBAGENT screen shares its session's
/// key (the amendment's key list is exactly launcher | session | aura), and
/// the scratch surface (screen=Session, no session) shares the launcher's —
/// documented: scratch is the launcher's envelope-driven lineage.
///
/// W3c3: keyed by [`UiGeneration`], not [`SessionId`]. A draft key is a
/// LOCAL SURFACE identity in the same family as the demo driver's arms and
/// meters — report R11 cut 1's assignment for the generation — and keeping
/// it `Copy` keeps the per-frame hit map and the stash/restore round trip
/// allocation-free. The law it must satisfy is "never reworn", which the
/// generation satisfies exactly as the old id did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DraftKey {
    Launcher,
    Session(UiGeneration),
    Aura,
}

/// Attachment bytes riding an [`AppRequest::AttachUpload`] /
/// [`crate::live::LiveCommand::ArtifactPut`] (B4b). A plain `Vec<u8>`
/// would print megabytes — or a pasted secret — through any derived
/// `Debug` (the un-redacted `SecretWire` Debug was mutation-killed TWICE
/// in W3c2; this type inherits the lesson instead of re-earning it).
#[derive(Clone, PartialEq, Eq)]
pub struct ArtifactBytes(Vec<u8>);

impl ArtifactBytes {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for ArtifactBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "ArtifactBytes(<{} bytes>)", self.0.len())
    }
}

/// Client-side mirrors of B4a's daemon acceptance caps (≤5 attachments a
/// turn, ≤5 MiB each): refusing HERE names the reason while the file is
/// still in hand, instead of durably accepting a submit the daemon must
/// bounce. The daemon stays the authority — these only pre-empt.
pub const MAX_TURN_ATTACHMENTS: usize = 5;
pub const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;

/// Pasted text at the TUI ingress (TUI6.3 fix 2, review r3 finding 2).
///
/// A paste is how API keys usually arrive, and the W3c2 `SecretWire`
/// discipline applies AT THE EDGE: the buffer zeroizes on drop and its
/// `Debug` is redacted. The protection is UNIVERSAL rather than a
/// secret-aware split — pasted text is user content either way, the cost
/// is one wrapper around the same allocation (no copy: `Zeroizing<String>`
/// takes ownership), and a split path would reopen a printable window
/// every time a new consumer picked the wrong lane. The clipboard bytes
/// crossterm itself buffered are upstream of this boundary and out of our
/// hands; OUR copy is wiped.
pub struct Pasted(zeroize::Zeroizing<String>);

impl Pasted {
    #[must_use]
    pub fn new(text: String) -> Self {
        Self(zeroize::Zeroizing::new(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The zeroizing buffer itself — a TYPE PIN for the hygiene tests:
    /// the field being `Zeroizing<String>` IS the wipe-on-drop guarantee,
    /// so a test holds this accessor and any regression to a plain
    /// `String` fails to compile.
    #[must_use]
    pub fn zeroizing_inner(&self) -> &zeroize::Zeroizing<String> {
        &self.0
    }
}

impl From<String> for Pasted {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl std::fmt::Debug for Pasted {
    /// Redacted by construction — length omitted too, the `LoginCard`
    /// precedent (a key's length is itself a hint).
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Pasted(<redacted>)")
    }
}

/// Everything the reducer consumes.
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    /// Bracketed paste arrives atomically; newlines never submit (rec 14).
    Paste(Pasted),
    /// Boxed: `EventPayload` is much larger than the other variants.
    Envelope(Box<EventPayload>),
    /// The demo script (or stream) ended.
    StreamEnded,
}

/// The `/theme` picker's overlay state (owner spec §3): the highlighted
/// row and the choice to restore on esc. Moving the highlight PREVIEWS the
/// theme instantly; ⏎ / a digit / a click commits (and the runtime
/// persists); esc reverts to `prior`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePicker {
    /// Index into [`ThemeChoice::MENU`].
    pub selection: usize,
    /// The committed choice on open — what esc restores.
    pub prior: ThemeChoice,
}

/// The `/effort` picker (G3): a composer-slot card listing the CURRENT
/// pair's declared ladder (daemon truth — the TUI holds no tables) plus a
/// leading "default" row that reverts to the provider default. ⏎ / digit
/// commits the receipted selection; esc closes; the identity's reasoning
/// segment moves only on the RESOLVED reply.
#[derive(Debug, Default)]
pub struct EffortPicker {
    /// Index into [`AppModel::effort_picker_rows`].
    pub selection: usize,
    /// In-flight `session.select_effort`: the REQUESTED value.
    pub pending: Option<Option<String>>,
    /// Honest inline error — a typed refusal from the daemon.
    pub error: Option<String>,
}

/// One `/effort` picker row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortPickerRow {
    /// `None` is the leading "provider default" row.
    pub effort: Option<String>,
    /// The provider's own declared default level, when it names one.
    pub is_provider_default: bool,
    /// The session's CURRENT selection (explicit level, or default row
    /// when nothing is pinned).
    pub is_current: bool,
}

/// The full-screen `/model` picker (F2a): one row per model × provider
/// pair across EVERY enabled provider, searchable. MODEL-LOCAL overlay —
/// it owns the keyboard while open (⏎ selects the HIGHLIGHTED row, esc
/// closes without selecting; the palette's exact-match lead jump never
/// gets near it — heeded history).
#[derive(Debug, Default)]
pub struct ModelPicker {
    /// Live substring search over model + provider (+ auth flavor).
    pub query: String,
    /// Index into the FILTERED row list.
    pub selection: usize,
    /// In-flight `session.select_model`: the REQUESTED pair. The picker
    /// renders it pulsing; the identity moves only on the resolved reply.
    pub pending: Option<(String, String)>,
    /// Honest inline error — a typed refusal or an unavailability reason.
    pub error: Option<String>,
}

/// One `/model` picker row: a model × provider pair (or an honest
/// placeholder for a provider with nothing discovered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPickerRow {
    pub provider: String,
    /// The model slug; empty for a provider placeholder row.
    pub model: String,
    /// `oauth` / `api` — what a turn on this row meters.
    pub auth: &'static str,
    pub context_window: Option<u64>,
    /// Provider availability — unavailable rows render dimmed and refuse
    /// with the reason instead of silently failing.
    pub available: bool,
    pub reason: Option<String>,
    /// The provider's own declared default model.
    pub is_default: bool,
    /// The session's CURRENT pair.
    pub is_current: bool,
    /// False only for placeholder rows (nothing to select).
    pub selectable: bool,
}

/// Identity shown in the status bar and launcher info line. Real values come
/// from config/accounts in later waves; the demo pins sim-parity defaults.
#[derive(Debug, Clone)]
pub struct IdentityLine {
    pub provider: String,
    pub model_short: String,
    pub account: String,
    pub device: String,
    pub context_window: u64,
    /// Reasoning level, when the daemon has declared one for the session
    /// (F2c). `None` renders nothing — never a guess.
    pub reasoning: Option<String>,
    /// Fast-mode marker (F2c): rides the reasoning segment when active.
    pub fast: bool,
}

impl Default for IdentityLine {
    fn default() -> Self {
        Self {
            provider: "anthropic".to_owned(),
            model_short: "fable-5".to_owned(),
            account: "none · /login".to_owned(),
            device: local_device_name().to_owned(),
            context_window: 200_000,
            reasoning: None,
            fast: false,
        }
    }
}

/// The single mutable application state (research rec 3).
#[derive(Debug)]
pub struct AppModel {
    /// WRITE LAW (TUI6.2 fix 3, review r2 finding 3): inside `AppModel`,
    /// assign this ONLY through [`Self::switch_surface`] — it owns the
    /// draft swap that keeps a surface change from leaking one surface's
    /// draft onto another (the r2 Aura→Session chip-close bug). The
    /// four named exceptions, each documented at its site: the founding
    /// donation in the launcher submit (the ring travels by design), the
    /// two identity-flip split seams (open_session's checkout,
    /// back_to_launcher) whose stash/restore halves bracket an
    /// `active_session` flip the atomic authority cannot span, and the
    /// /reset purge flow (purge-then-restore replaces the swap). The
    /// DRIVER (runtime.rs) holds no direct write either — its ChipRemove
    /// arm routes through the authority (TUI6.2c finding 6). Test
    /// fixtures set the field directly on purpose — they construct
    /// states, they do not transition.
    pub screen: Screen,
    /// The RESOLVED rendering every frame draws with. Derived state: write
    /// it through [`Self::apply_theme_choice`] (previews inside the open
    /// picker are the one blessed direct writer).
    pub theme: ThemeKey,
    /// How the theme is chosen (owner spec §3): `system` (default, follows
    /// the detected terminal appearance) or a fixed key. TUI-local display
    /// state — the runtime persists changes to the profile-dir settings
    /// file; nothing rides the wire.
    pub theme_choice: ThemeChoice,
    /// The terminal appearance detected ONCE pre-UI (OSC 11 / COLORFGBG,
    /// undetectable → dark). What `system` resolves against; re-evaluated
    /// only on the next boot.
    pub detected_system: ThemeKey,
    /// The `/theme` picker overlay (owner menu law: numbered rows, arrow
    /// highlight). MODEL-LOCAL — deliberately not a projection card so it
    /// can never ride a session checkout or block a daemon menu.
    pub theme_picker: Option<ThemePicker>,
    /// The `/effort` picker overlay (G3), composer-replacing like `/theme`.
    pub effort_picker: Option<EffortPicker>,
    /// The full-screen `/model` picker overlay (F2a). MODEL-LOCAL — never
    /// a projection card, so it can never ride a session checkout.
    pub model_picker: Option<ModelPicker>,
    /// COMMIT counter for the theme choice (ui-themes-fix): bumped by
    /// every user commit — picker ⏎/digit/click, `/theme <name>`, ⌃T —
    /// and never by boot resolution or previews. The runtime's
    /// persistence authority keys on THIS, so re-affirming the boot
    /// default still writes the settings file (the live probe's gap).
    pub theme_commits: u64,
    pub sanctum_tier: SanctumTier,
    pub projection: SessionProjection,
    pub identity: IdentityLine,
    /// The user EXPLICITLY chose a provider/model/account this run
    /// (`/model`, `/provider`, or clicking an account). Once pinned, the
    /// daemon-truth bootstrap below never overwrites their choice; until
    /// then the identity line is only a seed and daemon reality wins.
    pub identity_pinned: bool,
    /// The ACTIVE surface's composer (TUI5): text + first-class cursor +
    /// selection + input ring. Nothing in it persists (item 8).
    pub composer: crate::composer::Composer,
    /// Parked composers for the surfaces NOT on screen (TUI5 item 9):
    /// every surface — launcher, each session, aura — keeps its own draft
    /// (text AND cursor/selection/ring travel together, Claude Code's
    /// per-conversation drafts). Navigation swaps through here; nothing
    /// in it persists (item 8's DTO assertion covers it).
    pub drafts: std::collections::HashMap<DraftKey, crate::composer::Composer>,
    /// Monotonic client-side correlation id for receipt-free attachment
    /// uploads (B4b) — `artifact.put` has no command id, so this is what
    /// the link's request context carries back to complete the chip.
    pub upload_seq: u64,
    /// Session blurb (sim auto-title micro-call) — announced by the 1.5 s
    /// `· session titled` note; the HEADER shows [`Self::session_name`].
    pub session_title: Option<String>,
    /// Session slug name (sim tui.js:2014-2016) — header + window title.
    pub session_name: Option<String>,
    /// Head callsign for the live demo session (sim: claimed from the
    /// roster at `newSession`, tui.js:1631 — TUI4c makes the claim real).
    pub session_head: (String, String),
    /// Mid-turn input held for turn end (sim queue mode, §4.4): the ⧗
    /// panel's rows; consumed by the driver's `finish_turn` with no idle.
    pub msg_queue: Vec<String>,
    /// `/queue turn` — mid-turn input queues instead of steering.
    pub queue_mode: bool,
    /// ⌃G / `/tokens` context panel (sim tui.js:2946-2977) — session
    /// surfaces only; esc closes.
    pub token_panel: bool,
    /// `/tree` — selected row (sim treeSel).
    pub tree_sel: usize,
    /// `/tree` — the VIEWED branch (`None` = the root/main branch; sim
    /// treeBranchId). Drill/breadcrumb/esc walk this; it never touches the
    /// session's ACTIVE branch until a row is activated.
    pub tree_view: Option<haider_protocol::ids::BranchId>,
    /// The armed render-resolved jump (B2b-m3, research §Q3): set when a
    /// tree NODE row is activated, resolved by the next session frame
    /// against the renderer's own wrapped-row prefix sums — interior
    /// mutability because RENDER resolves it (the `scroll_max`
    /// discipline). Cleared
    /// only when it lands: a node replay has not materialized keeps the
    /// anchor armed rather than guessing another entry. Cross-screen
    /// identity is `{branch, node}` — never an entry ordinal or a cached
    /// wrapped-row offset (width/resize would invalidate those).
    pub pending_jump: std::cell::RefCell<Option<PendingJump>>,
    /// `/tools` live — the daemon's committed inventory snapshot
    /// (None while the read is in flight; the screen says "fetching").
    pub tools_inventory: Option<haider_protocol::tool::ToolInventorySnapshot>,
    /// Per-session voice pipeline (sim DEFAULT_VOICE — ships ON).
    pub voice: VoiceState,
    /// The ◉ talk hold is live (`◉ listening…` chip + status segment).
    /// Demo mode drives it directly (the canned hold); live mode keeps it
    /// in lockstep with [`Self::talk`]'s engagement so the chip chrome,
    /// the status segment and `animated()` read ONE flag in both modes.
    pub listening: bool,
    /// T2 — the LIVE talk state machine (phase, generation, ghost
    /// assembly, wave ring). Demo mode never engages it.
    pub talk: crate::talk::TalkState,
    /// The `/talk` setup card, while open. Owns the input band and the
    /// keyboard (the login-card modality) — the Deepgram key never
    /// reaches the composer.
    pub talk_setup: Option<crate::talk::TalkSetupCard>,
    /// The profile `transcription` section, loaded by the live runtime at
    /// boot and refreshed on every `ConfigStored`. Pure data here — the
    /// reducer performs no IO.
    pub talk_config: haider_stt::config::TranscriptionConfig,
    /// The TYPED config-load error, when the section exists but is
    /// corrupt (`/talk` surfaces it honestly instead of flipping the
    /// user's engine choice to a default).
    pub talk_config_error: Option<String>,
    /// The launcher's working dir for shell builtins, in DISPLAY form
    /// (`~`-abbreviated, sim `~/dev/enterprise-suite`).
    pub launcher_dir: String,
    /// The process working directory, ABSOLUTE (W3c3 M2). `session.create`
    /// carries this, never [`Self::launcher_dir`]: the daemon rejects a
    /// non-absolute cwd, and `~` is a display convention the wire has never
    /// heard of.
    pub cwd: String,
    /// The session's working dir — shown in the header; `cd` retargets it.
    pub session_dir: String,
    /// Per-open card counter: `/voice` and `/tools` mint a FRESH menu id
    /// each time, exactly as the sim's `nid()` does (review r2 P1-1 — fixed
    /// ids let a stale answer apply its consequences to a later card).
    pub card_seq: u64,
    /// The demo VFS the shell builtins run against (sim tui.js:418-426).
    pub vfs: BTreeMap<String, Vec<String>>,
    /// The launcher's `.shellout` block: last builtin (cmd, output).
    pub launcher_shellout: Option<(String, String)>,
    /// The session's subagent chip tree (§2 — demo-local).
    pub chips: Vec<ChipModel>,
    /// The ATTACHED session's branch state (B2b m1): registry, active
    /// selection, warm parked views and the fork-coordinate tracker.
    /// `projection`/`chips` always hold the ACTIVE branch's surfaces plus
    /// the session-global cursor; a branch switch swaps surfaces and
    /// transplants the cursor through ONE authority
    /// ([`Self::switch_branch`]), and the session checkout swaps this
    /// struct whole (the A→B→A law).
    pub branch_state: crate::branch::BranchState,
    /// The chip path the subagent screen is viewing (breadcrumb).
    pub view_path: Vec<String>,
    /// The SubTree header collapse toggle (`▾`/`▸ subagents`).
    pub subtree_collapsed: bool,
    /// The pinned-todos header collapse toggle (sim tui.js:2863-2888 — the
    /// header is a button and the collapsed form summarises the current
    /// item; owner item 7 promotes it from the deferred ledger).
    pub todos_collapsed: bool,
    /// An auto-resume turn is in flight (§2.7 guard).
    pub auto_resuming: bool,
    /// The aura orchestrator surface (persists across screen exits).
    pub aura: AuraModel,
    /// EVERY session, fully materialized (sim `sessions`, tui.js:497) —
    /// seeds and user-created alike; newest user session first, then the
    /// seeds. The ATTACHED session's state is checked OUT of its slot into
    /// this model's live fields (see `crate::session`).
    pub sessions: Vec<crate::session::SessionState>,
    /// Which runtime drives this model (W3c3 M2). Demo by default.
    pub mode: RuntimeMode,
    /// The masked `/login … api` card, while it is open (W3c3 M3).
    pub login: Option<LoginCard>,
    /// The checked-out session's PROTOCOL id (sim `activeId`; `None` =
    /// launcher's no-session state, exactly the sim's `setActiveId(null)`).
    pub active_session: Option<SessionId>,
    /// The most recently detached session — the empty-⏎ re-attach target.
    pub last_detached: Option<SessionId>,
    /// Local generation allocator (seeds take 1-3; `0` is the scratch
    /// sentinel). MONOTONIC for the process lifetime — never reset, not
    /// even by `/reset` (review TUI4.1 P1-2): a generation-keyed control
    /// callback must never find a replacement session wearing a dead
    /// session's generation. W3c3 renamed it from `next_session_id`: the
    /// PROTOCOL id is no longer minted by arithmetic (report R11 cut 1),
    /// only the local generation is.
    pub next_ui_generation: u64,
    /// The roster claim counter (sim `rosterRef`, tui.js:681) — shared
    /// with the driver so heads and chips draw from ONE honour roll.
    pub roster: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Demo-runtime side effects (W3c3, report R11 cut 3) — a channel
    /// SEPARATE from `requests` so `run_live` never even sees demo
    /// vocabulary. Only `run_demo` drains it.
    pub demo_requests: Vec<DemoRequest>,
    /// Selected option index while a blocking menu replaces the composer.
    pub menu_selection: usize,
    /// Selected row in the slash palette (open while composer starts with /).
    /// Ranges over the FULL match list; the render window follows.
    pub palette_selection: usize,
    /// First visible palette row — the scroll window that keeps the
    /// selection visible (sim CmdMenu internal scroll, tui.js:2710-2718).
    pub palette_scroll: usize,
    /// Esc dismissed the palette without clearing the composer (sim
    /// `menuDismissed`); any composer edit re-opens it.
    pub palette_dismissed: bool,
    /// The /help overlay (esc closes).
    pub help_open: bool,
    /// One-line transient notice shown in the status bar until the next
    /// keystroke (honest stubs: "/tree lands with the daemon").
    pub flash: Option<String>,
    /// Answers the user produced; the runtime drains these to the client
    /// (side effects never happen inside the reducer).
    pub outbox: Vec<OutboundAnswer>,
    /// Reducer-requested side effects; the runtime drains these.
    pub requests: Vec<AppRequest>,
    /// True while a demo turn is playing (submits are ignored, honestly).
    pub turn_active: bool,
    /// Wheel scroll-back offset in the session transcript (0 = follow
    /// bottom; wheel up increases, wheel down decreases). A `Cell` because
    /// RENDER is the single scroll authority (review r3 P2-2). The wheel
    /// applies reconcile-then-apply (review r5 P2-2): fold to the
    /// ≤1-frame-stale [`Self::scroll_max`], then apply the notch clamped
    /// to it — bursts bank no debt; the frame's reconcile is the backstop.
    pub scroll_back: std::cell::Cell<u16>,
    /// Max scroll-back of the LAST rendered frame — written by the
    /// renderer; wheel notches and sticky jumps clamp against it
    /// (reconcile-then-apply, review r5 P2-2). Starts at 0 (review r2
    /// P2-6).
    pub scroll_max: std::cell::Cell<u16>,
    /// The frame-geometry epoch (TUI6.1 fix 1). Bumped by every RESIZE
    /// (the reducer's only involvement — it versions the frame, it learns
    /// nothing about wrapping) and by every RENDER (`Cell`: the renderer
    /// holds `&AppModel`, the `scroll_max` discipline). Composer hits are
    /// stamped with the epoch of the frame that drew them and press/drag
    /// validate it, so consuming a layout that a resize (or a newer
    /// frame) has replaced is unrepresentable — the geometry twin of the
    /// text-revision guard.
    pub geometry_epoch: std::cell::Cell<u64>,
    /// Monotonic login ATTEMPT mint (TUI6.3 fix 1; TUI6.5 re-scope) —
    /// each card open AND each submit takes the next value (the identity
    /// is per stage ISSUANCE, not per card); never reused, so a retired
    /// or timed-out issuance's replies can never collide with a live
    /// one's.
    login_attempt_seq: u64,
    /// The sticky origin line is suppressed after a sticky jump until the
    /// next REAL wheel event (sim jumpToSticky, tui.js:2637-2657: the bar
    /// must never cover the row it just revealed). A `Cell` since B2b-m3:
    /// the render-resolved tree jump suppresses it from inside the frame
    /// (same law — the sticky must not cover the revealed row).
    pub sticky_suppressed: std::cell::Cell<bool>,
    /// The hit region under the mouse cursor (owner ask, TUI3a item 6).
    /// Value-carrying like clicks: a stale hover can never light up a
    /// different row than the one it was measured on. Render consults it
    /// for hover chrome; palette/menu hover moves the SELECTION instead
    /// (sim onMouseEnter, tui.js:2992/3073).
    pub hovered: Option<Hit>,
    /// The in-app drag selection (owner item 9): set while dragging, kept
    /// after release (the highlight survives the auto-copy), cleared by the
    /// next click or keypress. Screen-space — see [`crate::select`].
    pub selection: Option<crate::select::Selection>,
    /// A left button went down here and has not resolved yet: the potential
    /// selection anchor AND the pending click. On Up with no meaningful
    /// movement the click dispatches from THESE coordinates; a drag that
    /// selected suppresses it (owner item 9's disambiguation law).
    pub mouse_down: Option<(u16, u16)>,
    /// TUI5 item 5 — a left button went down INSIDE the composer text: the
    /// drag (if any) is a COMPOSER selection, never the transcript's
    /// screen-space drag (region disambiguation by drag START). Transient
    /// interaction state; never persisted, never arms anything.
    pub composer_drag: bool,
    pub should_quit: bool,
    /// Set by every state change; cleared when a frame is drawn (rec 6).
    pub dirty: bool,
    /// TUI4d item 14 — the ONE shared animation phase (the sim's CSS
    /// `pulse`/`railShimmer` clocks folded into a single counter). The
    /// runtime advances it every ~600 ms ONLY while [`Self::animated`]
    /// reports a live pulsing element; render derives every pulsing
    /// span's ink from it (even = full ink · odd = the sim's 0.35-opacity
    /// midpoint; `% 3` drives the rail shimmer). Pure render phase:
    /// never persisted, never touching projections or arms.
    pub anim_phase: u8,
    /// The render clock, epoch ms (S4): the LIVE chips' elapsed figures
    /// read it at draw time. Advanced by the shared anim tick (both run
    /// loops — no new timer) and by every applied envelope's
    /// `committed_at_ms` (monotone max, so a first paint before the first
    /// tick is already inside the journal's own time base). Pure display
    /// state: never persisted, and terminal chips never read it (their
    /// figure is frozen from journal timestamps).
    pub clock_ms: u64,
    /// The حيدر wordmark as a real graphics-protocol image, when the terminal
    /// speaks one — set once at startup by the runtime (see
    /// [`crate::wordmark::Wordmark::detect`]) and read by render to draw a crisp
    /// wordmark in the boot banner and session header instead of the half-block
    /// art. `None` (the default, and every non-graphics terminal) means render
    /// falls back to `crate::mark`. Behind a `RefCell` because render takes
    /// `&AppModel` while the image protocol needs `&mut` to re-encode on a size
    /// change. Never persisted; a pure presentation cache.
    pub wordmark: std::cell::RefCell<Option<crate::wordmark::Wordmark>>,
    /// `/accounts` screen state (rows, revision gate, pending select).
    pub accounts: AccountsState,
    /// The "found on this device" discovery section (D2), shared by
    /// `/accounts` and the `/providers` buttons area.
    pub device: DeviceCandidatesState,
    /// `/providers` screen state (report §5.2).
    pub providers: ProvidersState,
    /// What the CONNECTED daemon advertised in `Welcome` (features +
    /// version). Empty in demo mode — demo answers everything locally.
    ///
    /// W5e-1b: report §4.1 says "clients hide/disable only the methods whose
    /// feature is absent"; until this existed the TUI offered every button
    /// regardless and a stale daemon answered `unknown session method`
    /// (observed live: a daemon two days and five releases old).
    pub daemon_features: std::collections::BTreeSet<String>,
    pub daemon_version: Option<String>,
    /// The open OAuth add card, if any (accounts screen overlay).
    pub oauth_add: Option<OAuthAddCard>,
    /// Monotonic attempt counter for OAuth add cards.
    pub oauth_attempt_seq: u64,
    /// The open custom-provider card, if any (accounts screen overlay).
    pub custom_add: Option<CustomProviderCard>,
    /// Monotonic attempt counter for custom-provider cards.
    pub custom_attempt_seq: u64,
    /// `/hooks` screen state (H4): the `hooks.list` snapshot, cursor,
    /// confirmation card and in-flight receipt gate. APP-level like
    /// `tools_inventory` — the listing is workspace truth, not session
    /// display state.
    pub hooks: crate::hooks::HooksScreenState,
    /// The ATTACHED session's journaled hook facts + decision-chip state
    /// (H4). Checked in/out with the session exactly like `branch_state`
    /// (the A→B→A law).
    pub hook_facts: crate::hooks::HookFactsLog,
    /// `/usage` screen state (U2): the `usage.report` snapshot, provider
    /// filter, group cursor, account tabs, F2b scroll cells. APP-level —
    /// the report is account truth, not session display state.
    pub usage: UsageState,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            screen: Screen::Boot,
            token_panel: false,
            tree_sel: 0,
            tree_view: None,
            pending_jump: std::cell::RefCell::new(None),
            tools_inventory: None,
            // Dark is the registry default AND the detection fallback
            // (owner spec §3); main.rs resolves the persisted choice and
            // the detected appearance over this before the first frame.
            theme: ThemeKey::default(),
            theme_choice: ThemeChoice::default(),
            detected_system: ThemeKey::default(),
            theme_picker: None,
            effort_picker: None,
            model_picker: None,
            theme_commits: 0,
            sanctum_tier: SanctumTier::default(),
            projection: SessionProjection::new(),
            identity: IdentityLine::default(),
            identity_pinned: false,
            composer: crate::composer::Composer::new(),
            drafts: std::collections::HashMap::new(),
            upload_seq: 0,
            session_title: None,
            session_name: None,
            // The scratch surface's canonical head (the demo script's
            // voice); real sessions claim theirs from the roster.
            session_head: ("Hasan".to_owned(), "(a)".to_owned()),
            msg_queue: Vec::new(),
            queue_mode: false,
            voice: VoiceState::default(),
            listening: false,
            talk: crate::talk::TalkState::default(),
            talk_setup: None,
            talk_config: haider_stt::config::TranscriptionConfig::default(),
            talk_config_error: None,
            launcher_dir: "~/dev/enterprise-suite".to_owned(),
            cwd: "/".to_owned(),
            session_dir: "~/dev/enterprise-suite".to_owned(),
            card_seq: 0,
            vfs: vfs_seed(),
            launcher_shellout: None,
            chips: Vec::new(),
            branch_state: crate::branch::BranchState::default(),
            view_path: Vec::new(),
            subtree_collapsed: false,
            todos_collapsed: false,
            auto_resuming: false,
            aura: AuraModel::seed(),
            mode: RuntimeMode::Demo,
            login: None,
            // The first three generations the allocator can hand out, so a
            // fresh process's seeds are 1-3 exactly as before and
            // `next_ui_generation` continues at 4.
            sessions: seed_session_states(UiGeneration::FIRST.get()),
            active_session: None,
            last_detached: None,
            next_ui_generation: UiGeneration::FIRST.get() + SEED_SESSION_COUNT,
            roster: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                crate::script::ROSTER_FIRST_CLAIM,
            )),
            demo_requests: Vec::new(),
            menu_selection: 0,
            palette_selection: 0,
            palette_scroll: 0,
            palette_dismissed: false,
            help_open: false,
            flash: None,
            outbox: Vec::new(),
            requests: Vec::new(),
            turn_active: false,
            scroll_back: std::cell::Cell::new(0),
            scroll_max: std::cell::Cell::new(0),
            geometry_epoch: std::cell::Cell::new(0),
            login_attempt_seq: 0,
            sticky_suppressed: std::cell::Cell::new(false),
            hovered: None,
            selection: None,
            mouse_down: None,
            composer_drag: false,
            should_quit: false,
            dirty: true,
            anim_phase: 0,
            clock_ms: 0,
            // No graphics wordmark until the runtime queries the terminal at
            // startup; every non-graphics terminal and all tests stay None and
            // render falls back to the half-block art in `crate::mark`.
            wordmark: std::cell::RefCell::new(None),
            accounts: AccountsState::default(),
            device: DeviceCandidatesState::default(),
            providers: ProvidersState::default(),
            daemon_features: std::collections::BTreeSet::new(),
            daemon_version: None,
            oauth_add: None,
            oauth_attempt_seq: 0,
            custom_add: None,
            custom_attempt_seq: 0,
            hooks: crate::hooks::HooksScreenState::default(),
            hook_facts: crate::hooks::HookFactsLog::default(),
            usage: UsageState::default(),
        }
    }
}

impl AppModel {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The generation that outbound answers and the auto-title micro-call
    /// carry as their `origin`, and that the driver's consumption gates
    /// check (review r2 P1-1): the ATTACHED session's generation, or
    /// [`UiGeneration::SCRATCH`] for the no-session scratch surface.
    /// DERIVED from the attached row, never stored (review TUI4.1, Fable
    /// D2-1 — the old `session_epoch` field was a hand-maintained twin of
    /// `active_session` with a stale monotonicity doc). Generations never
    /// recur: `next_ui_generation` is monotonic for the process lifetime
    /// (the sim's `s-${Date.now()}` law), so a generation-keyed callback
    /// can never find a replacement wearing a dead one.
    ///
    /// W3c3 renamed this from `session_identity`: the value is no longer
    /// the session's identity, it is the SURFACE's generation (report R11
    /// cut 1 — a session id must not double as a stale-timer epoch).
    #[must_use]
    pub fn ui_generation(&self) -> UiGeneration {
        self.active_session
            .as_ref()
            .and_then(|id| self.sessions.iter().find(|entry| &entry.id == id))
            .map_or(UiGeneration::SCRATCH, |entry| entry.ui_gen)
    }

    /// The attached session's protocol id, if any — the coordinate every
    /// live RPC (`turn.submit`, `turn.cancel`, `MenuAnswer`) carries.
    #[must_use]
    pub fn active_session_id(&self) -> Option<&SessionId> {
        self.active_session.as_ref()
    }

    /// The composer surface currently on screen (TUI5 item 9). Boot maps
    /// to the launcher key: its composer is swallowed by the boot guard,
    /// and the launcher is what boot becomes.
    #[must_use]
    pub fn surface_key(&self) -> DraftKey {
        match self.screen {
            Screen::Aura => DraftKey::Aura,
            _ => self.session_draft_key(),
        }
    }

    /// The draft key of the attached session (the launcher's when nothing
    /// is attached) — the ONE place a generation becomes a surface key.
    fn session_draft_key(&self) -> DraftKey {
        match self.active_session {
            Some(_) => DraftKey::Session(self.ui_generation()),
            None => DraftKey::Launcher,
        }
    }

    // ---- Pending attachments (B4b) ----

    /// The composer wearing `surface` RIGHT NOW: the live composer when
    /// the surface is on screen, else its parked draft. `None` only for a
    /// draft `/reset` purged — an upload reply for it dies silently.
    fn composer_for_surface(
        &mut self,
        surface: DraftKey,
    ) -> Option<&mut crate::composer::Composer> {
        if self.surface_key() == surface {
            Some(&mut self.composer)
        } else {
            self.drafts.get_mut(&surface)
        }
    }

    /// Chip the active draft with one attachment and issue its
    /// receipt-free upload. Callers hold the gates (session surface, live
    /// mode, `artifact_put_v1`, the per-turn count and byte caps) — this
    /// is the one issuance seam under them all.
    pub fn begin_attachment_upload(
        &mut self,
        bytes: Vec<u8>,
        kind: crate::composer::PendingKind,
        label: String,
    ) {
        self.upload_seq += 1;
        let upload = self.upload_seq;
        self.composer
            .push_attachment(crate::composer::PendingAttachment {
                upload,
                label,
                kind,
                artifact: None,
            });
        self.requests.push(AppRequest::AttachUpload {
            upload,
            surface: self.surface_key(),
            bytes: ArtifactBytes::new(bytes),
        });
        self.dirty = true;
    }

    /// The daemon's verified content address landed: complete the chip on
    /// the ISSUING surface's draft (captured at issuance — a surface
    /// switch between upload and reply must not chip the wrong draft).
    pub fn complete_upload(
        &mut self,
        surface: DraftKey,
        upload: u64,
        artifact: haider_protocol::ids::ArtifactRef,
    ) -> bool {
        let done = self
            .composer_for_surface(surface)
            .is_some_and(|composer| composer.complete_attachment(upload, artifact));
        if done {
            self.dirty = true;
        }
        done
    }

    /// An upload failed — remove its chip and return the label for the
    /// honest notice (a dead chip must not survive to submit a ref the
    /// CAS never accepted).
    pub fn fail_upload(&mut self, surface: DraftKey, upload: u64) -> Option<String> {
        let label = self
            .composer_for_surface(surface)?
            .fail_attachment(upload)?;
        self.dirty = true;
        Some(label)
    }

    /// Disconnect sweep: every in-flight upload died with the socket
    /// (receipt-free — nothing resends it), on the live composer AND every
    /// parked draft. Returns how many chips died, for the honest notice.
    pub fn drop_uploading_attachments(&mut self) -> usize {
        let mut dropped = self.composer.drop_uploading_attachments();
        for draft in self.drafts.values_mut() {
            dropped += draft.drop_uploading_attachments();
        }
        if dropped > 0 {
            self.dirty = true;
        }
        dropped
    }

    /// `/attach <path>` (B4b + G2): chip the draft with one image or one
    /// UTF-8 text file, uploaded ahead of the submit. Every refusal is an
    /// honest notice; the read itself is shell-owned
    /// ([`AppRequest::AttachRead`]).
    fn attach_command(&mut self, remainder: &str) {
        if self.screen != Screen::Session {
            self.flash = Some("· /attach — session only".to_owned());
            self.dirty = true;
            return;
        }
        // Attachments are daemon CAS truth; the demo has no store to hold
        // bytes, and a fabricated chip would claim content no submit could
        // ever carry.
        if self.mode.fabricates_locally() {
            self.flash =
                Some("· /attach — live only; attachments ride the daemon's store".to_owned());
            self.dirty = true;
            return;
        }
        // Feature gate: without `artifact.put` there is no byte ingress —
        // the honest stale-daemon notice names the fix, nothing uploads.
        if !self.daemon_serves(haider_rpc::FEATURE_ARTIFACT_PUT_V1) {
            self.flash = Some(self.stale_daemon_note("attachments"));
            self.dirty = true;
            return;
        }
        let path = remainder.trim();
        if path.is_empty() {
            self.flash = Some("· /attach — give a file path".to_owned());
            self.dirty = true;
            return;
        }
        if self.composer.attachments().len() >= MAX_TURN_ATTACHMENTS {
            self.flash = Some("· 5 attachments a turn — ⌫ at the start removes one".to_owned());
            self.dirty = true;
            return;
        }
        self.requests.push(AppRequest::AttachRead {
            path: path.to_owned(),
        });
        self.dirty = true;
    }

    /// `/rename <name>` (G2): rename the attached session. Live rides the
    /// receipted `session.rename` — the header moves only on the daemon's
    /// NORMALIZED reply; demo renames locally (its world is local by
    /// design, like the model picker). Bare `/rename` is a usage flash —
    /// clearing is deliberately not offered here.
    fn rename_command(&mut self, remainder: &str) {
        if self.screen != Screen::Session {
            self.flash = Some("· /rename — session only".to_owned());
            self.dirty = true;
            return;
        }
        let title = remainder.trim();
        if title.is_empty() {
            self.flash = Some("· /rename — give a name".to_owned());
            self.dirty = true;
            return;
        }
        if self.mode.fabricates_locally() {
            self.session_name = Some(title.to_owned());
            self.flash = Some(format!("· renamed → {title}"));
            self.dirty = true;
            return;
        }
        // Feature gate: without `session.rename` the daemon cannot commit
        // a title — the honest stale-daemon notice names the fix.
        if !self.daemon_serves(haider_rpc::FEATURE_SESSION_RENAME_V1) {
            self.flash = Some(self.stale_daemon_note("session rename"));
            self.dirty = true;
            return;
        }
        let Some(session) = self.active_session.clone() else {
            self.flash = Some("· /rename — no attached session".to_owned());
            self.dirty = true;
            return;
        };
        self.requests.push(AppRequest::Rename {
            session,
            title: title.to_owned(),
        });
        self.dirty = true;
    }

    /// The NORMALIZED title committed (G2): render daemon truth — never an
    /// echo of the request. The active header updates when the rename hit
    /// the attached session; a background session's roster row updates in
    /// place.
    pub fn apply_renamed(&mut self, session: &SessionId, title: Option<String>) {
        if self.active_session.as_ref() == Some(session) {
            self.session_name = title.clone();
        } else if let Some(entry) = self.sessions.iter_mut().find(|entry| &entry.id == session) {
            entry.name = title.clone();
        }
        self.flash = Some(match &title {
            Some(title) => format!("· renamed → {title}"),
            None => "· session name cleared".to_owned(),
        });
        self.dirty = true;
    }

    /// A typed `session.rename` refusal (G2): the public reason reaches the
    /// session view as an error line plus a flash — never a silent IDLE
    /// (the F2e law).
    pub fn rename_failed(&mut self, session: &SessionId, code: &str, message: &str) {
        self.record_session_error(session, format!("rename failed — {code}: {message}"));
        self.flash = Some(format!("· rename failed — {code}"));
        self.dirty = true;
    }

    /// A paste over the sim's pill thresholds — B4b makes the pill REAL.
    ///
    /// DEMO keeps the sim's verbatim vocabulary (a literal pill token;
    /// the sim's world is local by design). LIVE on a session surface
    /// with `artifact_put_v1`, the content uploads as a `PastedText`
    /// artifact (UTF-8, ref-based — tool.rs's intended composer-token
    /// vocabulary) and the pill chip rides the next submit; the
    /// zeroize-and-drop theater is dead. LIVE anywhere else — launcher,
    /// aura, subagent, or an ungated daemon — the text lands LITERALLY:
    /// an honest composer full of text beats a pill claiming content no
    /// daemon holds.
    fn big_paste(&mut self, text: &str, raw_lines: usize) {
        if self.mode.fabricates_locally() {
            self.composer
                .insert_str(&format!("[Pasted {raw_lines} lines] "));
            return;
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if self.screen != Screen::Session {
            self.composer.insert_str(&normalized);
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_ARTIFACT_PUT_V1) {
            self.flash = Some(self.stale_daemon_note("paste attachments"));
            self.composer.insert_str(&normalized);
            return;
        }
        if normalized.len() > MAX_ATTACHMENT_BYTES {
            self.flash =
                Some("· paste exceeds the 5 MiB attachment limit — not inserted".to_owned());
            return;
        }
        if self.composer.attachments().len() >= MAX_TURN_ATTACHMENTS {
            self.flash = Some("· 5 attachments a turn — ⌫ at the start removes one".to_owned());
            return;
        }
        let lines = u32::try_from(raw_lines).unwrap_or(u32::MAX);
        self.begin_attachment_upload(
            normalized.into_bytes(),
            crate::composer::PendingKind::PastedText { lines },
            format!("[Pasted {raw_lines} lines]"),
        );
    }

    /// Park the live composer under the CURRENT surface's key. Callers
    /// pair this with [`Self::restore_draft`] around a surface change —
    /// exactly one stash/restore per transition (a double stash would park
    /// an already-empty composer over the real draft).
    fn stash_draft(&mut self) {
        // TUI5.1 fix 2: a held composer drag dies with the surface it
        // started on — every surface transition passes through here, so
        // this is the single cancellation authority.
        self.composer_drag = false;
        // TUI6.2c (verifier findings 1+2): the login card is
        // surface-LOCAL — it borrowed THIS surface's band. An
        // asynchronous surface switch arriving while the card is open
        // (the live `Created` reply's open_session, a background chip
        // close, an envelope flip) ABORTS the card (the secret wipes on
        // drop) and returns the borrowed band FIRST, so the stash below
        // parks the surface's REAL draft — not the login scratch over it,
        // which destroyed the parked ring. Every transition passes
        // through this stash, so the pairing is switch-safe at one seam.
        if self.login.is_some() {
            self.close_login_card();
            self.flash = Some("· /login cancelled — the surface changed".to_owned());
        }
        // T2: talk is surface-local exactly like the login card — a live
        // session (or the setup card) dies with the surface it borrowed
        // the band from. Cancel is DISCARD by contract (Esc law), and the
        // generation bump makes every late runtime event stale.
        if self.talk.engaged() {
            self.talk_cancel();
            self.flash = Some("· ◉ talk cancelled — the surface changed".to_owned());
        }
        if self.talk_setup.is_some() {
            self.talk_setup = None;
            self.flash = Some("· /talk setup closed — the surface changed".to_owned());
        }
        let key = self.surface_key();
        // TUI6.1 fix 1 closure: the frame's CURRENT wrap budget outlives
        // the swap — `mem::take` would otherwise leave the scratch
        // composer at the default 0 and `restore_draft`'s carry would
        // read that instead of the live width.
        let budget = self.composer.wrap_budget();
        let draft = std::mem::take(&mut self.composer);
        self.drafts.insert(key, draft);
        self.composer.set_wrap_budget(budget);
    }

    /// Bring the NEW surface's parked composer live (empty for a surface
    /// never visited — a fresh session starts with a fresh draft).
    ///
    /// TUI6.1 fix 1 closure (the stash-seam half): a parked draft carries
    /// the wrap budget of ITS last render — frames and possibly resizes
    /// ago. A queued ↑ racing the post-switch redraw would walk that
    /// stale width's rows (the review r1 class, one seam over). Every
    /// band spans the frame's full width, so the OUTGOING composer's
    /// budget is the current truth for every surface: carry it across
    /// the swap. This is the only restore path, so a restored draft can
    /// never wake to a stale width.
    fn restore_draft(&mut self) {
        let key = self.surface_key();
        let current_budget = self.composer.wrap_budget();
        self.composer = self.drafts.remove(&key).unwrap_or_default();
        self.composer.set_wrap_budget(current_budget);
    }

    /// Flip to the session screen WITH the item-9 draft swap when the
    /// surface key would change (review P1-2: the UserMessage envelope
    /// flip from the AURA screen crossed keys without a swap, leaking the
    /// aura draft onto the session surface and misfiling parked drafts on
    /// the next stash). Same-key flips (launcher scratch, subagent) swap
    /// nothing — an empty round-trip would be harmless but this keeps the
    /// one-stash-one-restore discipline literal.
    fn goto_session_screen(&mut self) {
        self.switch_surface(Screen::Session);
    }

    /// THE surface-switch authority (TUI6.2 fix 3, review r2 finding 3):
    /// every atomic Screen write goes through here, and when the surface
    /// KEY changes the draft swap happens as one atom — stash under the
    /// departing key, restore under the arriving one. The r2 reviewer's
    /// leak was `close_chip_state` assigning `Screen::Session` directly
    /// while the user sat in AURA: the aura draft crossed keys unswapped
    /// and could be SUBMITTED on the session surface. Direct
    /// `self.screen =` assignment is that bug waiting to recur; the only
    /// sites outside this function are enumerated on the `screen` field's
    /// doc (the founding donation and the two identity-flip split seams).
    pub(crate) fn switch_surface(&mut self, to: Screen) {
        let from = self.screen;
        let from_key = self.surface_key();
        self.screen = to;
        if self.surface_key() == from_key {
            return;
        }
        // Keys differ: rewind, then swap with the REAL key on each side.
        self.screen = from;
        self.stash_draft();
        self.screen = to;
        self.restore_draft();
    }

    /// The session's display name — the slug (sim `session.name`), never
    /// the blurb (that lives in the `· session titled` note).
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.session_name.as_deref().unwrap_or("session")
    }

    /// The terminal window title for the current screen (OSC 2).
    #[must_use]
    pub fn window_title(&self) -> String {
        match self.screen {
            Screen::Boot => "haider — starting".to_owned(),
            Screen::Launcher => "haider — launcher".to_owned(),
            Screen::Accounts => "haider — accounts".to_owned(),
            Screen::Tree => "haider — session tree".to_owned(),
            Screen::Tools => "haider — tools".to_owned(),
            Screen::Providers => "haider — providers".to_owned(),
            Screen::Hooks => "haider — hooks".to_owned(),
            Screen::Usage => "haider — usage".to_owned(),
            Screen::Session | Screen::Subagent | Screen::Aura => {
                // Strip control characters: user text must never smuggle
                // escape sequences into OSC 2 (review r1 P1).
                let title: String = self
                    .display_name()
                    .chars()
                    .filter(|c| !c.is_control())
                    .collect();
                let suffix = if self.screen == Screen::Aura {
                    " · aura"
                } else {
                    ""
                };
                format!("haider — {title} · {}{suffix}", self.identity.device)
            }
        }
    }

    /// The chip the subagent screen is viewing.
    #[must_use]
    pub fn viewed_chip(&self) -> Option<&ChipModel> {
        self.view_path
            .last()
            .and_then(|agent| find_chip(&self.chips, agent))
    }

    /// The status-bar badge with the DERIVED `◔ WAITING · N subagent(s)`
    /// overlay (§2.6): an idle session with live chips waits — display
    /// only, never a synthesized envelope. Interrupted idle (`⏸ IDLE (i)`)
    /// is respected, not overwritten.
    #[must_use]
    pub fn status_badge(&self) -> (String, crate::projection::BadgeTone) {
        let badge = self.projection.badge();
        let live = tree_live_count(&self.chips);
        if live > 0 {
            let plural = if live > 1 { "s" } else { "" };
            // The durable W6a wait ("◔ WAITING · subagent") and the
            // display-derived idle overlay both COUNT now — the daemon's
            // authoritative state stays the badge's spine, the tree adds
            // the number (research W6b checklist item 5).
            if badge == "IDLE" || badge == "◔ WAITING · subagent" {
                return (
                    format!("◔ WAITING · {live} subagent{plural}"),
                    crate::projection::BadgeTone::Restful,
                );
            }
        }
        (badge, self.projection.badge_tone())
    }

    /// TUI4d item 14 — TRUE while ANY pulsing element is on screen: the
    /// runtime's shared phase clock ticks only then (the efficiency law
    /// this port was once deferred over — ZERO wakeups otherwise; the
    /// dirty-flag economy stays intact). One arm per sim keyframes site
    /// (tui.js:3943-5563); a new animated state must register HERE or it
    /// never moves.
    ///
    /// STATE-based, not viewport-based: a pulsing element shed by a tiny
    /// frame still ticks the clock — the frame then diffs to nothing and
    /// the cost is one bounded render per phase (the CSS analogue: the
    /// sim's animations run whether or not the element is scrolled into
    /// view). Tracking visibility would couple the model to layout.
    #[must_use]
    pub fn animated(&self) -> bool {
        // The status badge's pulse set (WAITING / STARTING / PERMISSION /
        // EFFECT_UNKNOWN, tui.js:5558-5563) — the bar shows on every
        // screen, the derived ◔ WAITING included.
        if crate::projection::badge_pulses(&self.status_badge().0) {
            return true;
        }
        // The ◉ talk chip's live hold (sim `.mic.live`, tui.js:5484-5489).
        if self.listening {
            return true;
        }
        match self.screen {
            // Boot: the gold `.sub` line pulses for the whole starting
            // beat (tui.js:5104-5108).
            Screen::Boot => true,
            // Launcher: a busy row's ◉ dot pulse + rail shimmer
            // (tui.js:4386-4394).
            Screen::Launcher => self.sessions.iter().any(crate::session::SessionState::busy),
            // Accounts: a static list — only an in-flight select animates
            // (the pending row's `…` beat).
            Screen::Accounts => self.accounts.pending_select.is_some(),
            // Tree: a static main-line view — nothing animates.
            Screen::Tree => false,
            // Tools: a static read-only inventory — nothing animates.
            Screen::Tools => false,
            Screen::Providers => self.providers.pending_default.is_some(),
            // Hooks: only an in-flight trust receipt animates (the pending
            // row's `…` beat, the accounts pattern).
            Screen::Hooks => self.hooks.pending.is_some(),
            // Usage: a static snapshot — nothing animates (the fetching
            // note is a plain line, never a pulse).
            Screen::Usage => false,
            Screen::Session | Screen::Subagent => {
                // `● thinking…` (tui.js:4458-4462) · the ⚒ running tool
                // glyph (tui.js:4524-4530) · the processing todo's box
                // (tui.js:4694-4697) · chip glyph pulses (tui.js:4823-4834)
                // — plus the viewed chip's own thinking tail and tool rows
                // on the subagent screen.
                self.projection.is_thinking()
                    || streaming_tool_live(self.projection.entries())
                    || self
                        .projection
                        .todos()
                        .is_some_and(|panel| panel.pinned && panel.current().is_some())
                    || chips_animated(&self.chips)
                    // S4: ANY live chip ticks its elapsed figure on this
                    // clock (the row is on both screens' subtree) — the
                    // pulse set alone would park an idle/waiting child's
                    // counter. Terminal chips are frozen and keep the
                    // gate closed.
                    || tree_live_count(&self.chips) > 0
                    || (self.screen == Screen::Subagent
                        && self.viewed_chip().is_some_and(|chip| {
                            chip.state == ChipDisplayState::Thinking
                                || streaming_tool_live(chip.transcript.entries())
                        }))
            }
            // Aura: running roster rows (tui.js:4128-4131) + its live
            // hold-to-talk.
            Screen::Aura => {
                self.aura.state == AuraState::Listening
                    || self
                        .aura
                        .roster
                        .iter()
                        .any(|row| row.state == ChipDisplayState::Running)
            }
        }
    }

    /// The palette is open while the composer is a single-line slash query,
    /// esc has not dismissed it (sim `menuDismissed`), and no blocking menu
    /// owns the input. A newline closes it (sim getSuggestions bails on
    /// `\n`, tui.js:235).
    #[must_use]
    pub fn palette_open(&self) -> bool {
        if !self.composer.text().starts_with('/')
            || self.composer.text().contains('\n')
            || self.palette_dismissed
            || self.help_open
        {
            return false;
        }
        // A menu REPLACES the composer, palette included — the session's
        // card on the session screen, the chip's question in its view.
        if self.screen == Screen::Session
            && self
                .projection
                .open_menu()
                .is_some_and(|menu| !menu.options.is_empty())
        {
            // A SELECT menu replaces the composer; a zero-option free-text
            // ask KEEPS it — the composer is the answer line (owner
            // report: the resistor question rendered unanswerable).
            return false;
        }
        !(self.screen == Screen::Subagent
            && self
                .viewed_chip()
                .is_some_and(|chip| chip.question_menu().is_some()))
    }

    /// Current palette rows (commands, or `/theme`'s argument slot) for
    /// rendering and completion.
    #[must_use]
    /// The live candidates for the discovered argument slots (W5e-3).
    /// Providers and models come from `provider.list`'s published summaries
    /// (which serve the DISCOVERED catalog since W5e-2); accounts from
    /// `account.list`. Nothing here is a compile-time list.
    pub fn dynamic_slots(&self) -> crate::commands::DynamicSlots {
        let providers = self
            .providers
            .providers
            .iter()
            .map(|summary| {
                let health = match summary.availability {
                    haider_rpc::ProviderAvailabilityWire::Available => "available",
                    _ => summary
                        .availability_reason
                        .as_deref()
                        .unwrap_or("unavailable"),
                };
                (summary.provider.clone(), health.to_owned())
            })
            .collect();
        // `/model` offers the ACTIVE provider's models. The session's
        // provider wins; otherwise the first summary that has any.
        let active_provider = Some(self.identity.provider.clone())
            .filter(|provider| !provider.is_empty())
            .or_else(|| {
                self.providers
                    .providers
                    .iter()
                    .find(|summary| !summary.models.is_empty())
                    .map(|summary| summary.provider.clone())
            })
            .unwrap_or_default();
        let models = self
            .providers
            .providers
            .iter()
            .find(|summary| summary.provider == active_provider)
            .map(|summary| {
                summary
                    .models
                    .iter()
                    .map(|slug| {
                        let mut desc = active_provider.clone();
                        if summary.default_model.as_deref() == Some(slug.as_str()) {
                            desc.push_str(" · default");
                        }
                        (slug.clone(), desc)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let accounts = self
            .accounts
            .rows
            .iter()
            .map(|row| {
                let mut desc = format!("{} · {}", row.provider, auth_label(row.method));
                if row.selected {
                    desc.push_str(" · in use");
                }
                (row.alias.clone(), desc)
            })
            .collect();
        // `/effort` completes from the CURRENT pair's declared ladder
        // (G3, daemon truth): a leading `default` plus the levels.
        let efforts = self
            .current_pair_detail()
            .map(|detail| {
                let mut rows = vec![(
                    "default".to_owned(),
                    "revert to the provider default".to_owned(),
                )];
                rows.extend(detail.supported_efforts.iter().map(|level| {
                    let mut desc = "reasoning effort".to_owned();
                    if detail.default_effort.as_deref() == Some(level.as_str()) {
                        desc.push_str(" · provider default");
                    }
                    (level.clone(), desc)
                }));
                rows
            })
            .unwrap_or_default();
        crate::commands::DynamicSlots {
            providers,
            models,
            accounts,
            efforts,
        }
    }

    /// The CURRENT pair's daemon-projected detail row (G3): the one source
    /// of the effort ladder / default / fast gate — the TUI holds no tables.
    #[must_use]
    pub fn current_pair_detail(&self) -> Option<&haider_rpc::ModelDetailWire> {
        self.providers
            .providers
            .iter()
            .find(|summary| summary.provider == self.identity.provider)
            .and_then(|summary| {
                summary
                    .model_details
                    .iter()
                    .find(|detail| detail.name == self.identity.model_short)
            })
    }

    pub fn palette_items(&self) -> Vec<PaletteItem> {
        palette_items(
            self.composer.text().trim_start_matches('/'),
            matches!(
                self.screen,
                Screen::Session | Screen::Subagent | Screen::Aura
            ),
            &self.dynamic_slots(),
        )
    }

    /// The inline ghost completion (sim `ghostFor`, tui.js:265-276): the
    /// remainder of the highlighted palette row beyond the typed fragment,
    /// drawn dim after the cursor with a faint `⇥ tab` tag.
    #[must_use]
    pub fn ghost(&self) -> Option<String> {
        if !self.palette_open() {
            return None;
        }
        let items = self.palette_items();
        let item = items
            .get(self.palette_selection.min(items.len().saturating_sub(1)))
            .cloned()?;
        let body = self.composer.text().strip_prefix('/')?;
        match item {
            // Command rows exist only while the body is one unfinished
            // token, so the whole body is the fragment.
            PaletteItem::Cmd(spec) => {
                let rest = spec.name.strip_prefix(body)?;
                (!rest.is_empty()).then(|| rest.to_owned())
            }
            PaletteItem::Arg { cmd, value, .. } => {
                if body.ends_with(char::is_whitespace) {
                    return Some((*value).to_owned());
                }
                // Lead case (sim `sugg.lead`): the command is fully typed
                // with no space yet — ghost the space + argument.
                if body.eq_ignore_ascii_case(cmd) {
                    return Some(format!(" {value}"));
                }
                let fragment = body.split_whitespace().last().unwrap_or("");
                let rest = value.strip_prefix(fragment)?;
                (!rest.is_empty()).then(|| rest.to_owned())
            }
        }
    }

    /// Reduce one event into the model. Returns nothing; render reads state,
    /// the runtime drains [`Self::outbox`] and [`Self::requests`].
    /// `StreamEnded` is a no-op and must NOT dirty the frame (r1 P1).
    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => {
                self.dirty = true;
                self.flash = None;
                // The masked login card OWNS the keyboard while it is open
                // (W3c3 M3): a key must never reach the composer, the
                // palette, the input ring or a selection gate, because
                // every one of those would keep a copy of it.
                if self.login.is_some() {
                    self.login_key(&key);
                    return;
                }
                // The `/talk` setup card owns the keyboard while open
                // (same modality: the Deepgram key must never reach the
                // composer, the palette or the input ring).
                if self.talk_setup.is_some() {
                    self.talk_setup_key(&key);
                    return;
                }
                // T2 toggle-to-talk: while a live talk session is engaged
                // the state machine owns Esc (cancel), Enter (commit +
                // submit) and plain typing (commit into the composer and
                // keep editing — `talk_key` settles the session and
                // returns false so the char flows the NORMAL path).
                if self.talk.engaged() && self.talk_key(&key) {
                    return;
                }
                // TUI5 item 4 — the selection gates run BEFORE the
                // clear-on-keypress law, or ⌃C/Esc could never see the
                // selection they govern.
                if self.composer_owns_input() && self.selection_key(&key) {
                    return;
                }
                // A keypress clears a finished selection's highlight
                // (owner item 9's clearing law; clicks clear via Down).
                self.selection = None;
                self.handle_key(key);
            }
            AppEvent::Paste(text) => {
                self.dirty = true;
                // The zeroizing wrapper is borrowed, never unwrapped: the
                // one owned copy wipes when `text` drops at the end of
                // this arm (TUI6.3 fix 2).
                let text = text.as_str();
                // Keys are pasted more often than typed; the paste lands in
                // the masked buffer and NOWHERE else (no pill token, no
                // draft, no ring).
                if let Some(card) = self.login.as_mut() {
                    card.push_str(text);
                    return;
                }
                // The `/talk` setup card: paste lands in the FOCUSED field
                // (the key buffer or the language field) and nowhere else.
                if let Some(card) = self.talk_setup.as_mut() {
                    match card.stage {
                        crate::talk::SetupStage::DeepgramKey => card.key_push_str(text),
                        crate::talk::SetupStage::Language => {
                            for c in text.trim().chars() {
                                if c.is_ascii_alphanumeric() || c == '-' {
                                    card.language.push(c);
                                }
                            }
                        }
                        _ => {}
                    }
                    return;
                }
                // T2: pasting while a talk session is engaged COMMITS the
                // partial transcript first (the typing-commits law), then
                // the paste itself flows the normal path below.
                if self.talk.engaged() {
                    self.talk_commit_to_composer();
                }
                // While a blocking menu replaces the composer, paste has no
                // target (r2 P2).
                if self.screen == Screen::Session
                    && self
                        .projection
                        .open_menu()
                        .is_some_and(|menu| !menu.options.is_empty())
                {
                    return;
                }
                // Sim thresholds measure the RAW clipboard — UTF-16 code
                // units and raw newline count, BEFORE any normalization
                // (tui.js:2298-2317). Big pastes become a pill token; small
                // pastes keep their newlines (multi-line composer).
                let raw_lines = text.split('\n').count();
                // TUI5 item 3: paste INSERTS at the cursor (replacing an
                // active selection, item 4) — both the pill token and the
                // literal small-paste path.
                if raw_lines > 3 || text.encode_utf16().count() > 300 {
                    self.big_paste(text, raw_lines);
                } else {
                    self.composer
                        .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                }
                // Any composer edit re-opens a dismissed palette (sim
                // `setMenuDismissed(false)` on change).
                self.palette_dismissed = false;
            }
            AppEvent::Envelope(payload) => {
                self.dirty = true;
                self.handle_envelope(&payload);
            }
            AppEvent::StreamEnded => {}
        }
    }

    /// TUI5 item 4 — the two keys that act ON a COMPOSER selection,
    /// consumed before anything else sees them:
    ///
    /// - Esc with an active composer selection clears it and NOTHING
    ///   else — "Esc clears selection before any other Esc meaning
    ///   fires" (brief law; the next Esc interrupts/navigates as before).
    ///   Native inputs and Claude Code both deselect-only.
    /// - ⌃C with an active composer selection copies it (the reducer
    ///   holds the exact text → [`AppRequest::CopyText`]) and clears it.
    ///   With NO composer selection ⌃C keeps its TUI4 meaning
    ///   (navigate/quit) exactly — the gate is selection-presence,
    ///   nothing else.
    ///
    /// The gate is scoped to the COMPOSER selection only (review P2-3): a
    /// transcript drag already auto-copied on release, its highlight
    /// clears under the TUI4 any-keypress law, and time-sensitive Esc
    /// (interrupt) / ⌃C (navigate) meanings must not spend a press on a
    /// leftover highlight.
    fn selection_key(&mut self, key: &KeyEvent) -> bool {
        if !self.composer.has_selection() {
            return false;
        }
        if key.code == KeyCode::Esc {
            self.composer.clear_selection();
            return true;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(text) = self.composer.selected_text() {
                self.requests.push(AppRequest::CopyText(text.to_owned()));
            }
            self.composer.clear_selection();
            return true;
        }
        false
    }

    /// TUI5 item 5 — left button DOWN on a composer text row: place the
    /// caret at the clicked boundary and arm the composer-drag mode (the
    /// region-disambiguation law: a drag STARTING here is a composer
    /// selection). `start` and `content` are the hit's render-time values
    /// (the value-carrying law); `col` is the clicked display column
    /// within `content`; `surface` and `revision` bind the hit to the
    /// composer state it was rendered from (TUI5.1 fix 2) — any mismatch
    /// drops the press whole.
    pub fn composer_press(
        &mut self,
        start: usize,
        content: &str,
        col: usize,
        surface: DraftKey,
        revision: u64,
        epoch: u64,
    ) {
        // TUI6.1 fix 1: the epoch gate joins the surface/revision gates —
        // a hit stamped before a resize (or an older frame) is stale
        // GEOMETRY even when the text never changed.
        if surface != self.surface_key()
            || revision != self.composer.revision()
            || epoch != self.geometry_epoch.get()
        {
            return;
        }
        // Defense in depth (the revision check subsumes this, but a
        // violated invariant should still never place a caret).
        if start > self.composer.text().len() {
            return;
        }
        let byte = start + crate::composer::byte_at_col(content, col);
        self.composer.press_at(byte);
        self.composer_drag = true;
        self.dirty = true;
    }

    /// Drag with the button held after a composer press: the caret (the
    /// selection's active end) follows the pointer.
    pub fn composer_drag_to(&mut self, byte: usize) {
        if !self.composer_drag {
            return;
        }
        self.composer.drag_to(byte);
        self.dirty = true;
    }

    /// Button UP after a composer press: a selection auto-copies (same
    /// flash as the transcript drag, item 5) and KEEPS its highlight; a
    /// plain click already placed the caret on Down.
    pub fn composer_release(&mut self) {
        if !self.composer_drag {
            return;
        }
        self.composer_drag = false;
        if let Some(text) = self.composer.selected_text() {
            self.requests.push(AppRequest::CopyText(text.to_owned()));
        }
        self.dirty = true;
    }

    /// ⌃G / `/tokens` — context-by-model panel (sim tui.js:2946): session
    /// surfaces only, a toggle, esc closes.
    fn toggle_token_panel(&mut self) {
        if matches!(self.screen, Screen::Session | Screen::Subagent) {
            self.token_panel = !self.token_panel;
        } else {
            self.flash = Some("· /tokens — session only".to_owned());
        }
        self.dirty = true;
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.screen == Screen::Tools {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.screen = Screen::Session;
                self.dirty = true;
            }
            return;
        }
        if self.screen == Screen::Hooks {
            self.handle_hooks_key(key.code);
            return;
        }
        if self.screen == Screen::Tree {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.tree_sel = self.tree_sel.saturating_sub(1);
                    self.dirty = true;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let rows = tree_rows(self).len();
                    self.tree_sel = (self.tree_sel + 1).min(rows.saturating_sub(1));
                    self.dirty = true;
                }
                // esc is SESSION-SCOPED (owner law): inside the tree it
                // walks UP the drill (sim tui.js:2508-2515) and closes the
                // screen from the root — it never navigates the app.
                KeyCode::Esc => {
                    match tree_viewed(self) {
                        Some(id) => {
                            self.tree_view = self
                                .branch_state
                                .descriptor(&id)
                                .and_then(|descriptor| descriptor.source_branch_id.clone());
                            self.tree_sel = 0;
                        }
                        None => self.screen = Screen::Session,
                    }
                    self.dirty = true;
                }
                KeyCode::Enter => {
                    if let Some(row) = tree_rows(self).get(self.tree_sel).cloned() {
                        self.activate_tree_row(row);
                    }
                }
                KeyCode::Char('f') => self.tree_fork_selected(),
                _ => {}
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                // ⌃C is NAVIGATION (owner item 10): from any non-launcher
                // surface it walks back to the launcher — the ← main chip's
                // teardown, nothing more. It never interrupts: a running
                // turn and live chips keep their lifecycle laws (esc owns
                // interrupt, tui.js:2533-2539). From the launcher — and
                // from boot, which has no launcher to return to — it quits,
                // as before. TUI5 item 4: with an ACTIVE composer selection
                // ⌃C COPIES instead (the gate lives in `handle`, before
                // this arm ever sees the key).
                KeyCode::Char('c') => match self.screen {
                    Screen::Launcher | Screen::Boot => self.should_quit = true,
                    _ => self.back_to_launcher(),
                },
                // Ctrl+T cycles the theme (demo stand-in for /theme).
                KeyCode::Char('t') => self.cycle_theme(),
                // ⌃G = the token panel (sim tui.js binding).
                KeyCode::Char('g') => self.toggle_token_panel(),
                // TUI5 items 2+3 — readline editing keys, Claude Code
                // parity: ⌃A/⌃E line edges, ⌃W word-back, ⌃K kill-to-end,
                // ⌃U kill-to-start. Only while the composer actually owns
                // the input (never boot / help / a blocking menu).
                KeyCode::Char('a') if self.composer_owns_input() => {
                    self.composer.line_home(false);
                }
                KeyCode::Char('e') if self.composer_owns_input() => {
                    self.composer.line_end_key(false);
                }
                KeyCode::Char('w') if self.composer_owns_input() => {
                    self.composer.word_backspace();
                    self.note_composer_edit();
                }
                KeyCode::Char('k') if self.composer_owns_input() => {
                    self.composer.kill_to_line_end();
                    self.note_composer_edit();
                }
                KeyCode::Char('u') if self.composer_owns_input() => {
                    self.composer.kill_to_line_start();
                    self.note_composer_edit();
                }
                _ => {}
            }
            return;
        }
        // Boot renders no composer — hidden input must not accumulate or
        // start turns (review r1 P2).
        if self.screen == Screen::Boot {
            return;
        }
        if self.help_open {
            // esc/enter/q close help; everything else is swallowed.
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                self.help_open = false;
            }
            return;
        }
        // The `/model` picker owns EVERY key while it is showing (F2a —
        // heeded history: ⏎ selects the HIGHLIGHTED row, never an
        // exact-match jump; esc closes without selecting). A daemon menu
        // outranks it: local chrome never shadows a live ask.
        if self.model_picker.is_some() {
            let menu_owns = (self.screen == Screen::Session
                && self.projection.open_menu().is_some())
                || (self.screen == Screen::Subagent
                    && self
                        .viewed_chip()
                        .is_some_and(|chip| chip.question_menu().is_some()));
            if menu_owns {
                self.model_picker = None;
                self.dirty = true;
            } else {
                self.handle_model_picker_key(key.code);
                return;
            }
        }
        // The `/effort` picker owns the keys while it is showing (G3) —
        // the same input-replacement law as `/theme`; a daemon card
        // outranks it and closes it.
        if self.effort_picker.is_some() {
            let menu_owns = (self.screen == Screen::Session
                && self.projection.open_menu().is_some())
                || (self.screen == Screen::Subagent
                    && self
                        .viewed_chip()
                        .is_some_and(|chip| chip.question_menu().is_some()));
            if !menu_owns && matches!(self.screen, Screen::Session | Screen::Subagent) {
                self.handle_effort_picker_key(key.code);
                return;
            }
            self.effort_picker = None;
            self.dirty = true;
        }
        // The `/theme` picker owns the keys while it is showing. A daemon
        // card or a navigation away from its surfaces outranks it: the
        // picker then closes (reverting any preview) and the key proceeds
        // to its normal owner — local chrome never shadows a live ask.
        if self.theme_picker.is_some() {
            let menu_owns = (self.screen == Screen::Session
                && self.projection.open_menu().is_some())
                || (self.screen == Screen::Subagent
                    && self
                        .viewed_chip()
                        .is_some_and(|chip| chip.question_menu().is_some()));
            if !menu_owns
                && matches!(
                    self.screen,
                    Screen::Launcher | Screen::Session | Screen::Aura | Screen::Subagent
                )
            {
                self.handle_theme_picker_key(key.code);
                return;
            }
            if let Some(picker) = self.theme_picker.take() {
                self.theme = picker.prior.resolve(self.detected_system);
                self.dirty = true;
            }
        }
        // Subagent view (§2.10): esc ALWAYS walks back to the session (the
        // parent is not blocked); the chip's question menu replaces the
        // chip view's composer.
        if self.screen == Screen::Subagent {
            if key.code == KeyCode::Esc {
                self.switch_surface(Screen::Session);
                // TUI6.2c finding 8: the chip view's scroll offset must
                // not carry onto the session transcript (the ⌂ home row
                // already resets; esc and the crumb now match).
                self.scroll_back.set(0);
                return;
            }
            if self
                .viewed_chip()
                .is_some_and(|chip| chip.question_menu().is_some())
            {
                self.handle_chip_menu_key(key.code);
                return;
            }
        }
        // Aura (§3.1): esc exits to the session if one is attached, else
        // the launcher; exiting never resets aura state.
        if self.screen == Screen::Aura && key.code == KeyCode::Esc {
            self.exit_aura();
            return;
        }
        // `/accounts` (sim tui.js:2516-2519): the screen has no composer —
        // esc walks back, ↑/↓/enter drive the row cursor, everything else
        // is swallowed. The login card's total modality already consumed
        // the key above when a card is open.
        if self.screen == Screen::Accounts {
            self.handle_accounts_key(key.code);
            return;
        }
        if self.screen == Screen::Providers {
            self.handle_providers_key(key.code);
            return;
        }
        // `/usage` (U2): a full-screen read-only report — esc walks back,
        // ↑/↓ move the provider cursor, ←/→ tab through a provider's
        // accounts, F2b page/scroll keys reach everything; every other key
        // is swallowed (the F2a key-ownership law — no composer beneath).
        if self.screen == Screen::Usage {
            self.handle_usage_key(key.code);
            return;
        }
        // A SELECT menu replaces the composer (sim §3 law); a zero-option
        // free-text ask leaves the keys to the composer.
        if self.screen == Screen::Session
            && self
                .projection
                .open_menu()
                .is_some_and(|menu| !menu.options.is_empty())
        {
            self.handle_menu_key(key.code);
            return;
        }
        // ⇧⏎ (kitty-protocol terminals report SHIFT) / ⌥⏎ (the universal
        // path) insert a newline (sim Shift+Enter, tui.js:2792-2796). Must
        // precede the palette branch — a newline also closes the palette.
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
        {
            // TUI5 item 3: the newline INSERTS at the cursor like any edit.
            self.composer.insert_str("\n");
            self.palette_dismissed = false;
            return;
        }
        if self.palette_open() {
            match key.code {
                KeyCode::Up => {
                    // Selection wraps over the FULL match list; the window
                    // follows (sim, tui.js:2763-2772 + 2710-2718).
                    let count = self.palette_items().len();
                    if count > 0 {
                        self.palette_selection =
                            (self.palette_selection.min(count - 1) + count - 1) % count;
                        self.scroll_palette_into_view(count);
                    }
                    return;
                }
                KeyCode::Down => {
                    let count = self.palette_items().len();
                    if count > 0 {
                        self.palette_selection = (self.palette_selection + 1) % count;
                        self.scroll_palette_into_view(count);
                    }
                    return;
                }
                KeyCode::Tab => {
                    // Sim acceptSuggestion(tab): arg commands open their
                    // slot; arg-less commands complete in place; an arg row
                    // completes the full command for ⏎ to run.
                    let items = self.palette_items();
                    match items
                        .get(self.palette_selection.min(items.len().saturating_sub(1)))
                        .cloned()
                    {
                        Some(PaletteItem::Cmd(spec)) => {
                            self.composer.set_text(
                                if crate::commands::offers_arg_completions(spec.name) {
                                    format!("/{} ", spec.name)
                                } else {
                                    format!("/{}", spec.name)
                                },
                            );
                        }
                        Some(PaletteItem::Arg { cmd, value, .. }) => {
                            self.composer.set_text(format!("/{cmd} {value}"));
                        }
                        None => {}
                    }
                    self.palette_selection = 0;
                    self.palette_scroll = 0;
                    return;
                }
                KeyCode::Esc => {
                    // Sim: esc DISMISSES the palette but keeps the typed
                    // text; the next composer edit re-opens it.
                    self.palette_dismissed = true;
                    self.palette_selection = 0;
                    self.palette_scroll = 0;
                    return;
                }
                KeyCode::Enter => {
                    // Enter activates the HIGHLIGHTED row (sim
                    // acceptSuggestion): arg commands enter their slot,
                    // everything else runs.
                    let items = self.palette_items();
                    match items
                        .get(self.palette_selection.min(items.len().saturating_sub(1)))
                        .cloned()
                    {
                        Some(item) => self.activate_palette_item(item),
                        None => self.execute_slash(),
                    }
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc if self.screen == Screen::Session => {
                if self.token_panel {
                    // The panel eats the esc (sim: esc closes) — the turn
                    // keeps running; interrupt needs a second esc.
                    self.token_panel = false;
                    self.dirty = true;
                    return;
                }
                if self.turn_active {
                    // Esc mid-turn INTERRUPTS (sim, tui.js:2533-2539 +
                    // 1551-1567): the script stops, run → cancelled, badge
                    // ⏸ IDLE (i), a transcript note lands — and the session
                    // stays on screen. Only an idle esc walks back. The
                    // held queue drops with the turn (sim tui.js:1557).
                    self.turn_active = false;
                    self.listening = false;
                    self.msg_queue.clear();
                    self.requests.push(AppRequest::Interrupt {
                        branch: self.branch_state.active().cloned(),
                    });
                    // LIVE (W3c3.1 r2): the cancellation is the DAEMON's to
                    // commit. Painting `Cancelled` + the note here says the
                    // run ended before `turn.cancel` has even been sent —
                    // and if the daemon rejects it (a run that already
                    // terminalized, a stale generation) the screen is
                    // simply lying. The committed `RunState` envelope
                    // paints it, exactly as it paints every other state.
                    if self.mode.fabricates_locally() {
                        self.projection
                            .apply(&EventPayload::RunState(RunState::Cancelled));
                        self.projection
                            .push_note("· interrupted — run → cancelled · idle (i)".to_owned());
                    }
                } else {
                    // OWNER DIRECTIVE: esc is SESSION-SCOPED — it
                    // interrupts, cancels menus and a held talk (P1-3's
                    // hold-cancel law survives the navigation change),
                    // never navigates. Back is `← main` (and ⌃C).
                    self.listening = false;
                    self.flash = Some("· back — click ← main (or ⌃C)".to_owned());
                    self.dirty = true;
                }
            }
            KeyCode::Enter => self.submit_composer(),
            KeyCode::Backspace => {
                // B4b chip law: ⌫ at the very START of the draft (no
                // selection — nothing text-wise to delete there) removes
                // the NEWEST attachment chip. A chip removed mid-upload
                // stays removed: the late reply finds no chip and dies.
                if self.composer.cursor() == 0
                    && !self.composer.has_selection()
                    && self.composer.remove_newest_attachment().is_some()
                {
                    self.dirty = true;
                    return;
                }
                // TUI5 item 3: ⌫ deletes the grapheme BEFORE the cursor
                // (or the active selection); ⌥⌫ deletes the word before
                // (ESC-⌫ / kitty ALT — Claude Code binds both).
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.composer.word_backspace();
                } else {
                    self.composer.backspace();
                }
                self.note_composer_edit();
            }
            // Delete (fn⌫ / kDEL, CSI 3~): the grapheme AFTER the cursor.
            KeyCode::Delete => {
                self.composer.delete_forward();
                self.note_composer_edit();
            }
            // TUI5 item 2 — cursor movement. ⇧ extends a selection
            // (item 4); ⌥ moves by word (mac law; iTerm CSI 1;3D). The
            // palette branch above already owns ↑/↓/Tab/⏎ while open, so
            // these arms never fight it.
            KeyCode::Left => {
                let extend = key.modifiers.contains(KeyModifiers::SHIFT);
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.composer.word_left(extend);
                } else {
                    self.composer.move_left(extend);
                }
            }
            KeyCode::Right => {
                let extend = key.modifiers.contains(KeyModifiers::SHIFT);
                if key.modifiers.contains(KeyModifiers::ALT) {
                    self.composer.word_right(extend);
                } else {
                    self.composer.move_right(extend);
                }
            }
            // ↑/↓ walk the composer's rows column-sticky (item 2). At the
            // buffer's edge rows they page the input HISTORY instead
            // (item 6, Claude Code behavior) — only with no selection and
            // no ⇧ (a ⇧↑ at the top edge is a selection gesture, not a
            // recall).
            KeyCode::Up if self.composer_owns_input() => {
                let extend = key.modifiers.contains(KeyModifiers::SHIFT);
                if !self.composer.line_up(extend)
                    && !extend
                    && !self.composer.has_selection()
                    && self.composer.history_prev()
                {
                    self.note_composer_edit();
                }
            }
            KeyCode::Down if self.composer_owns_input() => {
                let extend = key.modifiers.contains(KeyModifiers::SHIFT);
                if !self.composer.line_down(extend)
                    && !extend
                    && !self.composer.has_selection()
                    && self.composer.history_next()
                {
                    self.note_composer_edit();
                }
            }
            KeyCode::Home => {
                self.composer
                    .line_home(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            KeyCode::End => {
                self.composer
                    .line_end_key(key.modifiers.contains(KeyModifiers::SHIFT));
            }
            // The digit span matches what the launcher PAINTS
            // (`launcher_rows`): three in demo (the sim's world), nine
            // live. A digit past the last row attaches nothing.
            KeyCode::Char(c @ '1'..='9')
                if self.screen == Screen::Launcher
                    && self.composer.is_empty()
                    && ((c as usize) - ('1' as usize)) < self.launcher_rows() =>
            {
                let index = (c as usize) - ('1' as usize);
                self.attach_sample(index);
            }
            // ⌥b/⌥f word movement (readline ESC-b/ESC-f — what most mac
            // terminals actually SEND for Option+arrow; Claude Code
            // honors both encodings, so we do too).
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.composer.word_left(false);
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.composer.word_right(false);
            }
            KeyCode::Char(c) => {
                // TUI5 item 3: typing INSERTS at the cursor (never
                // appends) and REPLACES an active selection (item 4).
                self.composer.insert_str(c.encode_utf8(&mut [0u8; 4]));
                self.note_composer_edit();
                // Typing decays interrupted-idle → idle (sim, tui.js:3020).
                if self.projection.interrupted() {
                    self.projection.apply(&EventPayload::IdleDecayed);
                }
            }
            _ => {}
        }
    }

    /// The composer is the live input target: no boot screen, no help
    /// overlay, no blocking menu owning the keys (session card or the
    /// viewed chip's question). Gates the TUI5 editing keys so ⌃K on a
    /// menu can never eat a hidden draft.
    #[must_use]
    fn composer_owns_input(&self) -> bool {
        if self.screen == Screen::Boot || self.help_open {
            return false;
        }
        if self.screen == Screen::Session
            && self
                .projection
                .open_menu()
                .is_some_and(|menu| !menu.options.is_empty())
        {
            // A SELECT menu replaces the composer; a zero-option free-text
            // ask KEEPS it — the composer is the answer line (owner
            // report: the resistor question rendered unanswerable).
            return false;
        }
        !(self.screen == Screen::Subagent
            && self
                .viewed_chip()
                .is_some_and(|chip| chip.question_menu().is_some()))
    }

    /// The composer-edit epilogue every text-changing key shares (the sim
    /// resets suggestion state on any change): palette selection/scroll
    /// reset + a dismissed palette re-opens.
    fn note_composer_edit(&mut self) {
        self.palette_selection = 0;
        self.palette_scroll = 0;
        self.palette_dismissed = false;
    }

    /// Sim submit() preprocessing, exact order (tui.js:1966-2041 — the
    /// aura/subagent screen steps land with their screens; the boot-queue
    /// step is unreachable here because the boot screen swallows input by
    /// earlier review law r1 P2).
    fn submit_composer(&mut self) {
        // TUI5: the take records the submitted text in this surface's
        // input ring (item 6) and clears cursor/selection state (item 8).
        // Slash submits take SILENTLY — execute_slash records the
        // canonical form (review P3-9, one entry per invocation).
        let is_slash = self.composer.text().trim().starts_with('/');
        // B4b: a REAL turn must not ride while a chip's upload is still
        // in flight — its block has no verified ref yet and a submit
        // without it would silently shed the attachment. Refuse BEFORE
        // the take (the draft survives); slash commands and menu answers
        // consume no chips and pass.
        if !is_slash
            && self.screen == Screen::Session
            && !self.mode.fabricates_locally()
            && self.projection.open_menu().is_none()
            && self.composer.has_uploading_attachment()
        {
            self.flash = Some("· attachment still uploading — a moment".to_owned());
            self.dirty = true;
            return;
        }
        let text = if is_slash {
            self.composer.take_silent()
        } else {
            self.composer.take_for_submit()
        }
        .trim()
        .to_owned();
        self.palette_selection = 0;
        self.palette_scroll = 0;
        self.palette_dismissed = false;
        if text.is_empty() {
            // Empty ⏎ on the launcher re-attaches the most recently left
            // session (a port law; the detach model keeps it honest by id).
            if self.screen == Screen::Launcher
                && let Some(id) = self.last_detached.clone()
            {
                self.open_session(&id);
            }
            return;
        }
        if text.starts_with('/') {
            self.composer.set_text(text);
            self.execute_slash();
            return;
        }
        // §4 step 3: on the aura screen non-slash text drives orchestrate
        // ONLY while the aura is idle (otherwise silently dropped).
        if self.screen == Screen::Aura {
            if self.aura.state == AuraState::Idle {
                self.aura_submit(text, false);
            }
            return;
        }
        // W6d (owner report): a zero-option Question/File menu is a
        // free-text ask — the composer text IS the answer (value rides
        // the wire's input; empty key + index 0 satisfy the option-less
        // validation arm).
        if self.screen == Screen::Session
            && let Some(menu) = self.projection.open_menu()
            && menu.options.is_empty()
        {
            let answer = MenuAnswer {
                menu: menu.id.clone(),
                option_key: None,
                option_index: 0,
                value: Some(text.clone()),
                via: AnswerVia::Tui,
            };
            self.outbox.push(OutboundAnswer {
                origin: self.ui_generation(),
                branch: self.branch_state.active().cloned(),
                answer,
            });
            self.dirty = true;
            return;
        }
        // W8b: the literal `!` escape (research Q3). LIVE only — the
        // command runs on the SESSION daemon's workspace through the
        // receipt-backed `shell.exec`, so it exists only where a session
        // exists. Exactly ONE leading `!` is stripped; `!!x` sends the
        // literal `!x`. Demo mode keeps the sim's six bare VFS commands
        // and says so instead of faking a host shell.
        if self.screen == Screen::Session
            && let Some(stripped) = text.strip_prefix('!')
        {
            if self.mode.fabricates_locally() {
                self.flash = Some(
                    "· ! — live shell escape (the demo shell is bare ls · cd · pwd)".to_owned(),
                );
            } else if stripped.trim().is_empty() {
                self.flash = Some("· ! — type a command".to_owned());
            } else {
                self.requests.push(AppRequest::ShellExec {
                    command: stripped.to_owned(),
                });
            }
            self.dirty = true;
            return;
        }
        // Shell builtins run against the VFS — local, instant, NO model
        // turn (sim tui.js:1993-2008) — never on the subagent screen, and
        // they never start a session.
        let first_word = text.split_whitespace().next().unwrap_or("");
        if self.screen != Screen::Subagent
            && SHELL_CMDS.contains(&first_word.to_ascii_lowercase().as_str())
        {
            if !self.mode.fabricates_locally() {
                // The VFS is the demo's FAKE filesystem (`vfs_seed`). In
                // live mode an intercepted `ls` would paint invented files
                // as the user's real cwd, and `cd` would retarget the dir
                // shown on real session rows — fabricated state presented
                // as truth, the exact class the P1-A sweep closes. Refuse;
                // the agent itself is how live mode runs real commands
                // (W3c3.1 r2 sibling sweep).
                self.refuse_demo_only("shell builtins");
                return;
            }
            self.run_shell_line(&text);
            return;
        }
        // §4 step 6: the subagent screen steers ITS chip (respondChip).
        // LIVE (S3): the composer rides S1's `agent.message` wire — the
        // daemon delivers (steer vs queued), journals the `AgentMessaged`
        // fact and the chip's user row, and the receipt's flash names what
        // it did. Nothing is painted locally. A daemon that does not serve
        // the method refuses honestly instead of destroying the text.
        if self.screen == Screen::Subagent {
            if !self.daemon_serves(haider_rpc::FEATURE_AGENT_MESSAGE_V1) {
                self.flash = Some(self.stale_daemon_note("messaging a subagent"));
                self.dirty = true;
                return;
            }
            if let Some(agent) = self.view_path.last().cloned() {
                self.requests.push(AppRequest::ChipSubmit { agent, text });
            }
            return;
        }
        // Mid-turn input (sim tui.js:2027-2038): queue mode holds it for
        // turn end (⧗ panel, consumed with no idle); steer delivers the
        // row now with the sim's note (display-only — the running script
        // is not altered, same as the sim).
        if self.screen == Screen::Session && self.turn_active {
            // LIVE (review P1-1): mid-turn input is a REAL delivery. The
            // demo parks it in `msg_queue` (drained by `DemoDriver::
            // finish_turn`) or paints a steer row locally — neither of
            // which exists live, so both silently destroyed the user's
            // text. The wire has carried `DeliveryMode` all along: steer
            // delivers at the next safe boundary, queue holds to turn end,
            // and the AUTHORITATIVE `UserMessage` envelope paints the row.
            // Nothing is fabricated here (R11 cut 4).
            if !self.mode.fabricates_locally() {
                self.requests.push(AppRequest::SubmitText {
                    text,
                    voice: false,
                    title: false,
                    // Captured at issuance (B2b): a later switch cannot
                    // retarget this mid-turn delivery.
                    branch: self.branch_state.active().cloned(),
                    // Captured at issuance (B4b): the uploading gate above
                    // guarantees every chip is ready; the take clears the
                    // draft's chips — they ride THIS delivery.
                    attachments: self.composer.take_ready_attachments(),
                });
                return;
            }
            if self.queue_mode {
                self.msg_queue.push(text);
            } else {
                self.projection.apply(&EventPayload::UserMessage {
                    text,
                    attachments: vec![],
                    mode: DeliveryMode::Steer,
                });
                self.projection.push_note(
                    "· steered — delivered at the next safe boundary of the current turn"
                        .to_owned(),
                );
            }
            return;
        }
        // Typing on the LAUNCHER starts a FRESH session (sim promise,
        // tui.js:2013-2016 `newSession`) — the one left behind keeps
        // running and shows as busy in its launcher row.
        //
        // LIVE (W3c3, report R11 cut 4): nothing local happens. The daemon
        // mints the session; the row, the attachment and the first turn all
        // follow its responses, so no fabricated row or session can ever
        // need reconciling with the truth that arrives.
        if self.screen == Screen::Launcher {
            if !self.mode.fabricates_locally() {
                self.requests.push(AppRequest::CreateSession { text });
                return;
            }
            self.new_session(&text);
        }
        // The blurb is NOT set here: the sim's micro-call names the session
        // inside its own 1.5 s callback. The callback SURVIVES an interrupt
        // (bare setTimeout in the sim) — only a session replacement voids it,
        // via the origin identity (review r2 P2-6).
        let title = self.session_title.is_none();
        // FOUNDING DONATION (TUI6.2 fix 3's named exception 1 of 3): the
        // submit just consumed the launcher draft, and its input RING
        // deliberately travels with the composer onto the new session
        // surface (pinned by `founding_message_recalls_in_the_new_session`)
        // — a switch_surface swap would park the ring under the launcher
        // key and wake an empty one. The destination key is freshly
        // minted (new generation), so no parked draft can be clobbered.
        self.screen = Screen::Session;
        self.turn_active = true;
        self.scroll_back.set(0);
        self.requests.push(AppRequest::SubmitText {
            text,
            voice: false,
            title,
            branch: self.branch_state.active().cloned(),
            // B4b: ready chips ride the idle-session submit and the take
            // clears them (demo drafts never hold chips — empty there).
            attachments: self.composer.take_ready_attachments(),
        });
    }

    /// The masked card's keyboard (W3c3 M3). Printable characters extend
    /// the key, ⌫ shortens it, ⏎ submits, Esc cancels — and every exit
    /// path wipes the buffer, because a card left open is a key left in
    /// memory.
    fn login_key(&mut self, key: &KeyEvent) {
        let Some(card) = self.login.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                // Drop wipes: `Zeroizing` on the way out. TUI6.2 fix 5
                // (review r2 finding 5): the close RESTORES the draft the
                // open parked — text and history ring — via the one
                // close method.
                self.close_login_card();
            }
            KeyCode::Tab | KeyCode::BackTab if card.accepts_input() => {
                card.focus = match card.focus {
                    LoginFocus::Alias => LoginFocus::Key,
                    LoginFocus::Key => LoginFocus::Alias,
                };
                self.dirty = true;
            }
            KeyCode::Enter => {
                if !card.accepts_input() || card.is_empty() {
                    return;
                }
                // A submit the daemon would bounce on grammar is refused
                // HERE, with the field to fix put under the cursor — the
                // typed key survives (nothing was staged yet).
                if !account_alias_ok(&card.alias) {
                    card.focus = LoginFocus::Alias;
                    self.dirty = true;
                    return;
                }
                let provider = card.provider.clone();
                let alias = Some(card.alias.clone());
                // TUI6.5 (review r5): every SUBMIT is a fresh stage
                // issuance with a fresh identity — minting here is what
                // permanently invalidates the previous issuance: its id
                // can never again equal a live binding or this card's
                // current attempt, so the timed-out stage's late reply is
                // dropped BY IDENTITY (no waiter bookkeeping needed — the
                // reply arrives and dies at the gates).
                self.login_attempt_seq += 1;
                card.attempt = self.login_attempt_seq;
                let attempt = card.attempt;
                let secret = card.take_secret();
                card.stage = LoginStage::Submitting;
                self.requests.push(AppRequest::LoginApi {
                    attempt,
                    provider,
                    alias,
                    secret,
                });
            }
            KeyCode::Backspace => match card.focus {
                LoginFocus::Alias => {
                    card.alias_backspace();
                    self.dirty = true;
                }
                LoginFocus::Key => card.backspace(),
            },
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                match card.focus {
                    // The focus split is a SECRECY boundary too: a key
                    // pasted while the alias is focused must not leak into
                    // a rendered field — alias input is grammar-filtered
                    // and visible, the key is masked. Each field only ever
                    // receives what the focus says it owns.
                    LoginFocus::Alias => {
                        card.alias_push(c);
                        self.dirty = true;
                    }
                    LoginFocus::Key => card.push(c),
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Same pairing as Esc (TUI6.2 fix 5).
                self.close_login_card();
            }
            _ => {}
        }
    }

    /// The login card's outcome, from the driver (W3c3 M3). Every failure
    /// returns the card to ENTRY with an empty buffer: the key is never
    /// retained across a retry, so a `busy` retry costs a retype and a
    /// `restage_required` cannot resend an expired stage.
    pub fn login_result(&mut self, outcome: Result<String, (String, String)>) {
        let Some(card) = self.login.as_mut() else {
            return;
        };
        self.dirty = true;
        match outcome {
            Ok(identity) => card.stage = LoginStage::Done(identity),
            Err((code, message)) => {
                card.stage = LoginStage::Failed(login_recovery(&code, &message))
            }
        }
    }

    /// Open the masked card for `/login <provider> api`. The stash here
    /// pairs with [`Self::close_login_card`] — the card borrows the band;
    /// the surface's draft and its history ring come back when it leaves.
    /// The draft TEXT is empty by construction at this point (the
    /// `/login…` submit consumed it), but the RING is not — pinned in the
    /// login suite.
    fn open_login_card(&mut self, provider: &str, alias: Option<String>) {
        self.stash_draft();
        self.login_attempt_seq += 1;
        // §5.3 prefill: the slash command's optional token wins (folded to
        // the grammar's case); otherwise the smallest free
        // `«provider»-api[-N]` against the current snapshot.
        let alias = alias
            .map(|token| token.trim().to_ascii_lowercase())
            .filter(|token| account_alias_ok(token))
            .unwrap_or_else(|| {
                smallest_free_alias(&format!("{provider}-api"), &self.accounts.rows)
            });
        self.login = Some(LoginCard::new(
            provider.to_owned(),
            alias,
            self.login_attempt_seq,
        ));
        self.dirty = true;
    }

    /// Close the masked card and RETURN the band it borrowed (TUI6.2
    /// fix 5 + TUI6.2c finding 5): the open's stash parked the surface's
    /// draft — text AND history ring — and every close path must restore
    /// it. The demo driver's `LoginApi` arm closes the card too, so the
    /// pairing lives in this one model-owned method, never in a caller
    /// (`restore_draft` is private; a caller-side `login = None` is the
    /// stranded-ring bug by construction).
    pub fn close_login_card(&mut self) {
        if let Some(card) = self.login.take() {
            // TUI6.3 fix 1(b): the close RETIRES the attempt. A queued,
            // not-yet-drained submit dies here — a cancelled credential
            // must never reach the wire — and the driver is told to
            // invalidate whatever already did (stage correlation, login
            // command, deadline) so late replies fall on a dead id.
            self.requests.retain(|request| {
                !matches!(request, AppRequest::LoginApi { attempt, .. }
                    if *attempt == card.attempt)
            });
            self.requests.push(AppRequest::LoginRetired {
                attempt: card.attempt,
            });
            self.restore_draft();
            self.dirty = true;
        }
    }

    /// One shell-builtin line against the VFS: a session gets a transcript
    /// `$` row; the launcher gets its `.shellout` block (sim tui.js:3302).
    fn run_shell_line(&mut self, line: &str) {
        let in_session = self.screen == Screen::Session;
        let cwd = if in_session {
            self.session_dir.clone()
        } else {
            self.launcher_dir.clone()
        };
        let (out, retarget) = run_shell(line, &cwd, &mut self.vfs);
        if let Some(dir) = retarget {
            if in_session {
                self.session_dir = dir;
            } else {
                self.launcher_dir = dir;
            }
        }
        if in_session {
            self.projection.push_shell(line.to_owned(), out);
        } else {
            self.launcher_shellout = Some((line.to_owned(), out));
        }
    }

    /// A voice submission (sim /say + push-to-talk, tui.js:1865-1875):
    /// ◉ user row + `◉ heard` note ride the reducer; the script skips its
    /// own UserMessage and tags streamed rows `♪ speaking`.
    fn submit_voice(&mut self, text: String) {
        // LIVE (W3c3.1, review D2-2): the launcher branch returned early,
        // but the SESSION branch fell through and painted a local ◉ user
        // row plus a `◉ heard` note — the authoritative `UserMessage`
        // envelope then paints the same row again. This is P1-1's exact
        // shape, left un-swept because `/say` sits behind `/voice`, which
        // live mode now refuses outright (D1-2). Both halves are closed
        // here so no path can reach the fabrication.
        if !self.mode.fabricates_locally() {
            match self.screen {
                Screen::Launcher => self.requests.push(AppRequest::CreateSession { text }),
                _ => {
                    // B4b: a voice submit consumes READY chips only when
                    // nothing is mid-upload — a split set would shed the
                    // stragglers, so an in-flight chip parks the WHOLE
                    // set for the next submit.
                    let attachments = if self.composer.has_uploading_attachment() {
                        Vec::new()
                    } else {
                        self.composer.take_ready_attachments()
                    };
                    self.requests.push(AppRequest::SubmitText {
                        text,
                        voice: true,
                        title: self.session_title.is_none(),
                        branch: self.branch_state.active().cloned(),
                        attachments,
                    });
                }
            }
            return;
        }
        if self.screen == Screen::Launcher {
            self.new_session(&text);
        }
        self.projection.push_user_voice(text.clone());
        self.projection
            .push_note(format!("◉ heard · {}", self.voice.stt));
        let title = self.session_title.is_none();
        self.goto_session_screen(); // review P1-2: draft-aware flip
        self.turn_active = true;
        self.scroll_back.set(0);
        self.requests.push(AppRequest::SubmitText {
            text,
            voice: true,
            title,
            branch: self.branch_state.active().cloned(),
            // Demo world: chips cannot exist (the reducer refuses both
            // attach paths upstream), so nothing rides.
            attachments: Vec::new(),
        });
    }

    /// The ◉ talk hold finished (driver timer): submit the canned phrase
    /// through the voice path (sim tui.js:2044-2054).
    pub fn talk_fire(&mut self) {
        // A hold nobody is holding fires nothing: Esc (and any navigation)
        // clears `listening`, so the 1.3 s timer can no longer land on the
        // Launcher and yank the user into a fresh canned session
        // (review P1-3). The timer is ALSO session-owned, so a fresh
        // session cancels it outright.
        if !self.listening {
            return;
        }
        self.listening = false;
        self.dirty = true;
        // Sim `speak` requires an attached, idle session (tui.js:2045);
        // the launcher mic is inert, so the hold can never fabricate one.
        if self.turn_active || !self.voice.enabled || self.screen != Screen::Session {
            return;
        }
        self.submit_voice(TALK_PHRASE.to_owned());
    }

    // ===================================================================
    // T2 — live toggle-to-talk (the `/talk` state machine)
    // ===================================================================

    /// The one talk entry point (TalkChip press and bare `/talk` share
    /// it): toggle-to-talk. Idle starts a session; a press while
    /// listening COMMITS AND SUBMITS (the Enter gesture); a press while
    /// starting aborts.
    pub fn talk_toggle(&mut self) {
        if self.mode.fabricates_locally() {
            // Demo keeps its canned ◉ hold on the chip; `/talk` says so.
            self.flash =
                Some("· /talk — live only (the demo chip plays the canned hold)".to_owned());
            return;
        }
        if self.screen != Screen::Session {
            self.flash = Some("· /talk — session only".to_owned());
            return;
        }
        if self.talk_setup.is_some() {
            return;
        }
        match self.talk.phase {
            crate::talk::TalkPhase::Idle => self.talk_start(),
            crate::talk::TalkPhase::Starting => {
                self.talk_cancel();
                self.flash = Some("· ◉ talk cancelled".to_owned());
            }
            crate::talk::TalkPhase::Listening => self.talk_commit_submit(),
            crate::talk::TalkPhase::Finishing => {}
        }
    }

    /// Start a session on the CONFIGURED engine. Local goes straight to
    /// the runtime (which answers `ModelMissing`/`RuntimeMissing`
    /// honestly); Deepgram first fetches the vaulted key from the daemon.
    fn talk_start(&mut self) {
        if let Some(error) = self.talk_config_error.clone() {
            self.flash = Some(format!("· /talk — transcription config is broken: {error}"));
            return;
        }
        match self.talk_config.engine {
            haider_stt::config::TranscriptionEngine::Local => {
                let generation = self
                    .talk
                    .begin(haider_stt::config::TranscriptionEngine::Local);
                self.listening = true;
                self.requests.push(AppRequest::TalkShell(
                    crate::talk::TalkShellCommand::Start {
                        generation,
                        engine: crate::talk::TalkEngineSpec::Local {
                            model_id: self.talk_config.whisper_model_id.clone(),
                        },
                    },
                ));
            }
            haider_stt::config::TranscriptionEngine::Deepgram => {
                if !self
                    .daemon_features
                    .contains(haider_rpc::FEATURE_TRANSCRIPTION_V1)
                {
                    self.flash = Some(
                        "· /talk — this daemon does not vault transcription secrets (update haiderd)"
                            .to_owned(),
                    );
                    return;
                }
                self.talk
                    .begin(haider_stt::config::TranscriptionEngine::Deepgram);
                self.listening = true;
                self.talk.secret_intent = Some(crate::talk::SecretIntent::Start);
                self.requests.push(AppRequest::TranscriptionSecretRead);
            }
        }
    }

    /// Esc law: DISCARD. Settles the machine (the generation bump makes
    /// every late runtime event stale) and tells the runtime to tear the
    /// mic/engine down. Nothing lands anywhere.
    pub fn talk_cancel(&mut self) {
        if !self.talk.engaged() {
            return;
        }
        let generation = self.talk.generation;
        self.talk.settle();
        self.listening = false;
        self.dirty = true;
        self.requests.push(AppRequest::TalkShell(
            crate::talk::TalkShellCommand::Cancel { generation },
        ));
    }

    /// Enter law: COMMIT + SUBMIT. Input stops now; the engine assembles
    /// the definitive transcript and [`Self::handle_talk`]'s `Finished`
    /// arm realizes it into the composer and submits.
    fn talk_commit_submit(&mut self) {
        if self.talk.phase != crate::talk::TalkPhase::Listening {
            return;
        }
        self.talk.phase = crate::talk::TalkPhase::Finishing;
        self.talk.intent = crate::talk::CommitIntent::Submit;
        self.dirty = true;
        self.requests.push(AppRequest::TalkShell(
            crate::talk::TalkShellCommand::Finish {
                generation: self.talk.generation,
            },
        ));
    }

    /// Typing law: COMMIT INTO THE COMPOSER AND KEEP EDITING. What the
    /// ghost showed is realized verbatim (plus one separating space); the
    /// engine's unseen tail is discarded with the session — what you saw
    /// is what you keep.
    pub(crate) fn talk_commit_to_composer(&mut self) {
        if !self.talk.engaged() {
            return;
        }
        let generation = self.talk.generation;
        self.realize_talk_ghost();
        self.talk.settle();
        self.listening = false;
        self.dirty = true;
        self.requests.push(AppRequest::TalkShell(
            crate::talk::TalkShellCommand::Cancel { generation },
        ));
    }

    /// Insert the current ghost text (capped, one trailing space) at the
    /// cursor. No-op on an empty ghost.
    fn realize_talk_ghost(&mut self) {
        let ghost = self.talk.ghost.trim().to_owned();
        if ghost.is_empty() {
            return;
        }
        let mut text = crate::talk::clamp_realized(&ghost).to_owned();
        text.push(' ');
        self.composer.insert_str(&text);
    }

    /// Keyboard while a talk session is engaged. Returns true when the
    /// key was CONSUMED; `false` lets the key flow the normal path (a
    /// plain char after its commit, and ⌃C — the navigation hatch, whose
    /// surface change cancels the session at the stash seam).
    pub fn talk_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.talk_cancel();
                self.flash = Some("· ◉ talk cancelled".to_owned());
                true
            }
            KeyCode::Enter => {
                self.talk_commit_submit();
                true
            }
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => c != 'c',
            KeyCode::Char(_) if key.modifiers.contains(KeyModifiers::ALT) => true,
            KeyCode::Char(_) => {
                self.talk_commit_to_composer();
                false
            }
            // Arrows, backspace, tab, … are inert while talking: the
            // contract stays three gestures wide.
            _ => true,
        }
    }

    /// True when the partial-transcript ghost row renders (chrome above
    /// the composer — NEVER transcript content).
    #[must_use]
    pub fn talk_ghost_visible(&self) -> bool {
        self.screen == Screen::Session
            && self.talk.engaged()
            && !self.talk.ghost.trim().is_empty()
            && self.login.is_none()
            && self.talk_setup.is_none()
    }

    /// The TalkChip's label for the current live phase (demo listening
    /// keeps the sim wording).
    #[must_use]
    pub fn talk_chip_label(&self) -> &'static str {
        if self.talk.phase == crate::talk::TalkPhase::Finishing {
            "◉ transcribing…"
        } else if self.listening {
            "◉ listening…"
        } else {
            "◉ talk"
        }
    }

    /// Route a typed engine/setup error: model/runtime gaps open the
    /// setup card (the reinstall surface); everything else is an honest
    /// flash.
    fn talk_error(&mut self, error: haider_stt::SttError) {
        match &error {
            haider_stt::SttError::ModelMissing { model_id } => {
                self.open_talk_setup_at_local(format!(
                    "whisper model `{model_id}` is not installed (the shared dir is evictable) — download to continue"
                ));
            }
            haider_stt::SttError::RuntimeMissing { hint } => {
                self.open_talk_setup_at_local(format!("whisper-cli is not installed — {hint}"));
            }
            other => {
                self.flash = Some(format!("· ◉ talk failed — {other}"));
            }
        }
    }

    /// Reduce one runtime talk event. Session-scoped events correlate by
    /// generation — anything from a settled session is dropped whole.
    pub fn handle_talk(&mut self, event: crate::talk::TalkEvent) {
        use crate::talk::{CommitIntent, TalkEvent, TalkPhase};
        match event {
            TalkEvent::Started { generation, .. } => {
                if generation == self.talk.generation && self.talk.phase == TalkPhase::Starting {
                    self.talk.phase = TalkPhase::Listening;
                    self.dirty = true;
                }
            }
            TalkEvent::Envelope { generation, level } => {
                if generation == self.talk.generation && self.talk.wave_active() {
                    self.talk.wave.push(level);
                    self.dirty = true;
                }
            }
            TalkEvent::Partial { generation, frame } => {
                if generation == self.talk.generation && self.talk.engaged() {
                    self.talk.apply_frame(&frame);
                    self.dirty = true;
                }
            }
            TalkEvent::Health { generation, health } => {
                if generation == self.talk.generation && self.talk.engaged() {
                    self.dirty = true;
                    self.flash = Some(match health {
                        haider_stt::capture::CaptureHealth::DigitalZero { hint }
                        | haider_stt::capture::CaptureHealth::Stalled { hint } => {
                            format!("· mic: {hint}")
                        }
                        haider_stt::capture::CaptureHealth::Recovered => {
                            "· mic signal recovered".to_owned()
                        }
                    });
                }
            }
            TalkEvent::CapReached { generation } => {
                if generation == self.talk.generation && self.talk.phase == TalkPhase::Listening {
                    // The 900 s capture cap: finish into the composer —
                    // the user presses ⏎ themselves.
                    self.talk.phase = TalkPhase::Finishing;
                    self.talk.intent = CommitIntent::Insert;
                    self.flash = Some("· ◉ capture cap reached — transcribing".to_owned());
                    self.dirty = true;
                    self.requests.push(AppRequest::TalkShell(
                        crate::talk::TalkShellCommand::Finish { generation },
                    ));
                }
            }
            TalkEvent::Finished { generation, result } => {
                if generation != self.talk.generation || self.talk.phase != TalkPhase::Finishing {
                    return;
                }
                let intent = self.talk.intent;
                let ghost = self.talk.ghost.trim().to_owned();
                self.talk.settle();
                self.listening = false;
                self.dirty = true;
                match result {
                    Ok(result) => {
                        let text = result.text.trim().to_owned();
                        if text.is_empty() {
                            self.flash = Some("· ◉ heard nothing".to_owned());
                        } else {
                            self.composer.insert_str(crate::talk::clamp_realized(&text));
                            match intent {
                                CommitIntent::Submit => self.submit_composer(),
                                CommitIntent::Insert => {
                                    self.flash = Some("· ◉ transcribed — ⏎ to send".to_owned());
                                }
                            }
                        }
                    }
                    Err(error) => {
                        // Best-effort honesty: the words the ghost showed
                        // were watched landing — keep them; say what broke.
                        if !ghost.is_empty() {
                            let mut text = crate::talk::clamp_realized(&ghost).to_owned();
                            text.push(' ');
                            self.composer.insert_str(&text);
                        }
                        self.talk_error(error);
                    }
                }
            }
            TalkEvent::StartFailed { generation, error } => {
                if generation != self.talk.generation {
                    return;
                }
                self.talk.settle();
                self.listening = false;
                self.dirty = true;
                self.talk_error(error);
            }
            TalkEvent::SetupSnapshot { snapshot } => {
                if let Some(card) = self.talk_setup.as_mut() {
                    card.apply_snapshot(snapshot);
                    self.dirty = true;
                }
            }
            TalkEvent::KeyAccepted { secret, models } => {
                self.talk_setup_key_accepted(secret, models);
            }
            TalkEvent::KeyRejected { error } => self.talk_setup_key_rejected(&error),
            TalkEvent::DownloadProgress { model_id, percent } => {
                if let Some(card) = self.talk_setup.as_mut() {
                    for row in &mut card.whisper {
                        if row.id == model_id {
                            row.state = crate::talk::WhisperRowState::Downloading { percent };
                        }
                    }
                    self.dirty = true;
                }
            }
            TalkEvent::DownloadFinished { model_id, error } => {
                if let Some(card) = self.talk_setup.as_mut() {
                    for row in &mut card.whisper {
                        if row.id == model_id {
                            row.state = match &error {
                                None => crate::talk::WhisperRowState::Installed,
                                Some(message) => {
                                    crate::talk::WhisperRowState::Failed(message.clone())
                                }
                            };
                        }
                    }
                    self.dirty = true;
                }
            }
            TalkEvent::RuntimeInstalled { outcome, hint } => {
                if let Some(card) = self.talk_setup.as_mut() {
                    card.runtime = match (outcome, hint) {
                        (Ok(Some(path)), _) => crate::talk::RuntimeRowState::Found(path),
                        (Ok(None), Some(hint)) => crate::talk::RuntimeRowState::Missing(hint),
                        (Ok(None), None) => crate::talk::RuntimeRowState::Failed(
                            "installed, but no whisper executable was found".to_owned(),
                        ),
                        (Err(message), _) => crate::talk::RuntimeRowState::Failed(message),
                    };
                    self.dirty = true;
                }
            }
            TalkEvent::ConfigStored { config, error } => {
                self.dirty = true;
                match error {
                    None => {
                        self.talk_config = config;
                        self.talk_config_error = None;
                        if self.talk_setup.as_ref().is_some_and(|card| card.saving) {
                            self.talk_setup = None;
                            let label = match self.talk_config.engine {
                                haider_stt::config::TranscriptionEngine::Local => "local whisper",
                                haider_stt::config::TranscriptionEngine::Deepgram => "deepgram",
                            };
                            self.flash =
                                Some(format!("· ◉ talk ready — {label} · press ◉ or /talk"));
                        }
                    }
                    Some(message) => {
                        if let Some(card) = self.talk_setup.as_mut() {
                            card.saving = false;
                            card.error = Some(format!("could not save the config — {message}"));
                        } else {
                            self.flash =
                                Some(format!("· /talk — could not save the config: {message}"));
                        }
                    }
                }
            }
        }
    }

    /// The daemon's `transcription.secret_get` answer, routed by the
    /// intent recorded at issuance.
    pub fn talk_secret_answer(&mut self, secret: Option<haider_rpc::SecretWire>) {
        let Some(intent) = self.talk.secret_intent.take() else {
            return;
        };
        self.dirty = true;
        match intent {
            crate::talk::SecretIntent::Start => {
                if self.talk.phase != crate::talk::TalkPhase::Starting {
                    return;
                }
                match secret {
                    Some(secret) => {
                        let generation = self.talk.generation;
                        self.requests.push(AppRequest::TalkShell(
                            crate::talk::TalkShellCommand::Start {
                                generation,
                                engine: crate::talk::TalkEngineSpec::Deepgram {
                                    secret,
                                    model: self.talk_config.deepgram_model.clone(),
                                    language: self.talk_config.language.clone(),
                                },
                            },
                        ));
                    }
                    None => {
                        self.talk.settle();
                        self.listening = false;
                        self.open_talk_setup();
                        if let Some(card) = self.talk_setup.as_mut() {
                            card.stage = crate::talk::SetupStage::DeepgramKey;
                            card.error = Some(
                                "no Deepgram key vaulted yet — paste one to continue".to_owned(),
                            );
                        }
                    }
                }
            }
            crate::talk::SecretIntent::SetupPresence => {
                if let Some(card) = self.talk_setup.as_mut() {
                    card.key_present = secret.is_some();
                    if card.stage == crate::talk::SetupStage::DeepgramKey
                        && card.key_present
                        && card.key_stage == crate::talk::KeyStage::Entry
                        && card.key_is_empty()
                    {
                        card.key_stage = crate::talk::KeyStage::Reuse;
                    }
                }
                // The secret itself drops (and zeroizes) here — presence
                // was the only question.
            }
            crate::talk::SecretIntent::SetupProbe => match (self.talk_setup.as_mut(), secret) {
                (Some(_card), Some(secret)) => {
                    self.requests.push(AppRequest::TalkShell(
                        crate::talk::TalkShellCommand::ProbeKey { secret },
                    ));
                }
                (Some(card), None) => {
                    card.key_present = false;
                    card.key_reused = false;
                    card.key_stage = crate::talk::KeyStage::Entry;
                    card.error = Some("the vaulted key is gone — paste one to continue".to_owned());
                }
                _ => {}
            },
        }
    }

    /// The daemon's `transcription.secret_set` answer.
    pub fn talk_secret_stored(&mut self, present: bool) {
        self.dirty = true;
        let Some(card) = self.talk_setup.as_mut() else {
            return;
        };
        if card.key_stage != crate::talk::KeyStage::Storing {
            return;
        }
        if present {
            card.key_present = true;
            card.key_stage = crate::talk::KeyStage::Entry;
            card.error = None;
            card.stage = crate::talk::SetupStage::DeepgramModels;
            card.selection = 0;
        } else {
            card.key_stage = crate::talk::KeyStage::Entry;
            card.error = Some("the daemon reported no key after the store — try again".to_owned());
        }
    }

    /// A transcription-secret RPC failed (typed daemon refusal or
    /// transport) — surfaced where the flow lives.
    pub fn talk_secret_failed(&mut self, op: crate::live::TranscriptionOp, message: String) {
        self.dirty = true;
        match op {
            crate::live::TranscriptionOp::Get => match self.talk.secret_intent.take() {
                Some(crate::talk::SecretIntent::Start) => {
                    self.talk.settle();
                    self.listening = false;
                    self.flash = Some(format!("· ◉ talk failed — {message}"));
                }
                Some(
                    crate::talk::SecretIntent::SetupPresence
                    | crate::talk::SecretIntent::SetupProbe,
                ) => {
                    if let Some(card) = self.talk_setup.as_mut() {
                        if card.key_stage == crate::talk::KeyStage::Validating {
                            card.key_stage = crate::talk::KeyStage::Entry;
                        }
                        card.error = Some(message);
                    }
                }
                None => {
                    self.flash = Some(format!("· transcription vault — {message}"));
                }
            },
            crate::live::TranscriptionOp::Set => {
                if let Some(card) = self.talk_setup.as_mut() {
                    card.key_stage = crate::talk::KeyStage::Entry;
                    card.error = Some(format!("could not vault the key — {message}"));
                } else {
                    self.flash = Some(format!("· transcription vault — {message}"));
                }
            }
        }
    }

    /// A probed key validated (model list riding along): vault it (typed
    /// path) or proceed straight to the model picker (reuse path). A
    /// closed card drops the secret unvaulted (it zeroizes).
    fn talk_setup_key_accepted(
        &mut self,
        secret: haider_rpc::SecretWire,
        models: Vec<crate::talk::DeepgramModelRow>,
    ) {
        let store = {
            let Some(card) = self.talk_setup.as_mut() else {
                return;
            };
            if card.key_stage != crate::talk::KeyStage::Validating {
                return;
            }
            self.dirty = true;
            card.models = models;
            card.error = None;
            if card.key_reused {
                card.key_stage = crate::talk::KeyStage::Entry;
                card.stage = crate::talk::SetupStage::DeepgramModels;
                card.selection = 0;
                false
            } else {
                card.key_stage = crate::talk::KeyStage::Storing;
                true
            }
        };
        if store {
            self.requests.push(AppRequest::TranscriptionSecretStore {
                secret,
                clear: false,
            });
        }
        // else: `secret` drops (zeroizes) — the reuse path never held a
        // second copy to begin with.
    }

    /// A probed key was refused (or the endpoint failed).
    fn talk_setup_key_rejected(&mut self, error: &haider_stt::SttError) {
        let Some(card) = self.talk_setup.as_mut() else {
            return;
        };
        if card.key_stage != crate::talk::KeyStage::Validating {
            return;
        }
        self.dirty = true;
        card.key_stage = crate::talk::KeyStage::Entry;
        card.key_reused = false;
        card.error = Some(match error {
            haider_stt::SttError::Unauthorized(_) => {
                "Deepgram refused the key (401) — check it and try again".to_owned()
            }
            other => format!("could not validate the key — {other}"),
        });
    }

    /// Open the `/talk` setup card (engine picker first). Cancels any
    /// engaged session; loads the world snapshot and the vaulted-key
    /// presence.
    pub fn open_talk_setup(&mut self) {
        if self.mode.fabricates_locally() {
            self.flash = Some("· /talk setup — live only".to_owned());
            return;
        }
        if self.screen != Screen::Session {
            self.flash = Some("· /talk — session only".to_owned());
            return;
        }
        if self.talk.engaged() {
            self.talk_cancel();
        }
        let vault_supported = self
            .daemon_features
            .contains(haider_rpc::FEATURE_TRANSCRIPTION_V1);
        let mut card = crate::talk::TalkSetupCard::new(self.talk_config.clone(), vault_supported);
        if let Some(error) = &self.talk_config_error {
            card.error = Some(error.clone());
        }
        self.talk_setup = Some(card);
        self.dirty = true;
        self.requests.push(AppRequest::TalkShell(
            crate::talk::TalkShellCommand::LoadSetup,
        ));
        if vault_supported {
            self.talk.secret_intent = Some(crate::talk::SecretIntent::SetupPresence);
            self.requests.push(AppRequest::TranscriptionSecretRead);
        }
    }

    /// Open setup directly on the whisper stage with an honest error (the
    /// `ModelMissing`/`RuntimeMissing` reinstall surface).
    fn open_talk_setup_at_local(&mut self, error: String) {
        self.open_talk_setup();
        if let Some(card) = self.talk_setup.as_mut() {
            card.stage = crate::talk::SetupStage::Local;
            card.selection = 0;
            card.error = Some(error);
        } else {
            // Setup could not open (screen changed under the error) — the
            // message still lands.
            self.flash = Some(format!("· ◉ talk — {error}"));
        }
    }

    /// Keyboard while the setup card owns the band.
    pub fn talk_setup_key(&mut self, key: &KeyEvent) {
        use crate::talk::{KeyStage, SetupStage};
        enum Action {
            None,
            Activate,
            Close,
        }
        let action = {
            let Some(card) = self.talk_setup.as_mut() else {
                return;
            };
            match key.code {
                KeyCode::Esc => Action::Close,
                KeyCode::Enter => Action::Activate,
                KeyCode::Up => {
                    card.selection = card.selection.saturating_sub(1);
                    Action::None
                }
                KeyCode::Down => {
                    card.selection = (card.selection + 1).min(card.row_count().saturating_sub(1));
                    Action::None
                }
                KeyCode::Backspace => {
                    match card.stage {
                        SetupStage::DeepgramKey => card.key_backspace(),
                        SetupStage::Language => {
                            card.language.pop();
                        }
                        _ => {}
                    }
                    Action::None
                }
                KeyCode::Char(c) => match card.stage {
                    SetupStage::DeepgramKey => {
                        if card.key_stage == KeyStage::Reuse {
                            if c == 'r' {
                                // Retype: abandon the vaulted key's reuse.
                                card.key_stage = KeyStage::Entry;
                                card.key_reused = false;
                            }
                        } else {
                            card.key_push(c);
                        }
                        Action::None
                    }
                    SetupStage::Language => {
                        if c.is_ascii_alphanumeric() || c == '-' {
                            card.language.push(c);
                        }
                        Action::None
                    }
                    _ => {
                        // Digit shortcuts activate rows directly (menu
                        // parity).
                        if let Some(digit) = c.to_digit(10) {
                            let index = digit.saturating_sub(1) as usize;
                            if index < card.row_count() {
                                card.selection = index;
                                Action::Activate
                            } else {
                                Action::None
                            }
                        } else {
                            Action::None
                        }
                    }
                },
                _ => Action::None,
            }
        };
        match action {
            Action::None => {}
            Action::Close => {
                self.talk_setup = None;
                self.flash = Some("· /talk setup closed".to_owned());
            }
            Action::Activate => self.talk_setup_activate(),
        }
    }

    /// ⏎ (or a digit) on the setup card's highlighted row.
    fn talk_setup_activate(&mut self) {
        use crate::talk::{
            KeyStage, RuntimeRowState, SetupStage, TalkShellCommand, WhisperRowState,
        };
        enum Action {
            None,
            Request(AppRequest),
            ProbeVaulted,
        }
        let action = {
            let Some(card) = self.talk_setup.as_mut() else {
                return;
            };
            match card.stage {
                SetupStage::Engine => {
                    if card.selection == 0 {
                        card.stage = SetupStage::Local;
                        card.selection = 0;
                        card.error = None;
                        Action::None
                    } else if card.vault_supported {
                        card.stage = SetupStage::DeepgramKey;
                        card.selection = 0;
                        card.error = None;
                        card.key_stage = if card.key_present && card.key_is_empty() {
                            KeyStage::Reuse
                        } else {
                            KeyStage::Entry
                        };
                        Action::None
                    } else {
                        card.error = Some(
                            "this daemon does not vault transcription secrets — update haiderd (feature transcription_v1)"
                                .to_owned(),
                        );
                        Action::None
                    }
                }
                SetupStage::Local => {
                    if !card.loaded {
                        Action::None
                    } else if card.selection < card.whisper.len() {
                        let row = &mut card.whisper[card.selection];
                        match &row.state {
                            WhisperRowState::Installed => {
                                let mut config = card.config.clone();
                                config.engine = haider_stt::config::TranscriptionEngine::Local;
                                config.whisper_model_id = Some(row.id.to_owned());
                                card.config = config.clone();
                                card.saving = true;
                                card.error = None;
                                Action::Request(AppRequest::TalkShell(
                                    TalkShellCommand::StoreConfig { config },
                                ))
                            }
                            WhisperRowState::Absent | WhisperRowState::Failed(_) => {
                                row.state = WhisperRowState::Downloading { percent: None };
                                let model_id = row.id.to_owned();
                                Action::Request(AppRequest::TalkShell(
                                    TalkShellCommand::InstallModel { model_id },
                                ))
                            }
                            WhisperRowState::Downloading { .. } => Action::None,
                        }
                    } else {
                        // The runtime row.
                        match &card.runtime {
                            RuntimeRowState::Missing(_) | RuntimeRowState::Failed(_) => {
                                card.runtime = RuntimeRowState::Installing;
                                Action::Request(AppRequest::TalkShell(
                                    TalkShellCommand::InstallRuntime,
                                ))
                            }
                            RuntimeRowState::Found(_)
                            | RuntimeRowState::Installing
                            | RuntimeRowState::Unknown => Action::None,
                        }
                    }
                }
                SetupStage::DeepgramKey => match card.key_stage {
                    KeyStage::Reuse => {
                        card.key_reused = true;
                        card.key_stage = KeyStage::Validating;
                        card.error = None;
                        Action::ProbeVaulted
                    }
                    KeyStage::Entry => {
                        if card.key_is_empty() {
                            card.error = Some("paste your Deepgram API key first".to_owned());
                            Action::None
                        } else {
                            card.key_reused = false;
                            card.key_stage = KeyStage::Validating;
                            card.error = None;
                            let secret = card.take_key();
                            Action::Request(AppRequest::TalkShell(TalkShellCommand::ProbeKey {
                                secret,
                            }))
                        }
                    }
                    KeyStage::Validating | KeyStage::Storing => Action::None,
                },
                SetupStage::DeepgramModels => {
                    if card.models.is_empty() {
                        card.error = Some(
                            "no streaming models came back — go back and probe the key again"
                                .to_owned(),
                        );
                        Action::None
                    } else {
                        let index = card.selection.min(card.models.len() - 1);
                        card.config.deepgram_model = Some(card.models[index].name.clone());
                        card.stage = SetupStage::Language;
                        card.error = None;
                        Action::None
                    }
                }
                SetupStage::Language => {
                    let language = card.language.trim().to_owned();
                    let ok = !language.is_empty()
                        && language.len() <= 24
                        && language
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-');
                    if ok {
                        let mut config = card.config.clone();
                        config.engine = haider_stt::config::TranscriptionEngine::Deepgram;
                        config.language = language;
                        card.config = config.clone();
                        card.saving = true;
                        card.error = None;
                        Action::Request(AppRequest::TalkShell(TalkShellCommand::StoreConfig {
                            config,
                        }))
                    } else {
                        card.error = Some(
                            "language must be 1-24 characters of letters, digits or `-` (e.g. en, en-US)"
                                .to_owned(),
                        );
                        Action::None
                    }
                }
            }
        };
        match action {
            Action::None => {}
            Action::Request(request) => {
                self.dirty = true;
                self.requests.push(request);
            }
            Action::ProbeVaulted => {
                self.dirty = true;
                self.talk.secret_intent = Some(crate::talk::SecretIntent::SetupProbe);
                self.requests.push(AppRequest::TranscriptionSecretRead);
            }
        }
    }

    /// An aura orchestrate turn: user row + driver request (§3.4).
    fn aura_submit(&mut self, text: String, voice: bool) {
        if voice {
            self.aura.transcript.push_user_voice(text.clone());
        } else {
            self.aura.transcript.apply(&EventPayload::UserMessage {
                text: text.clone(),
                attachments: vec![],
                mode: DeliveryMode::Steer,
            });
        }
        // The orb leaves idle NOW, not when the first async beat lands:
        // the `idle` submit gate is what stops two rapid submits from
        // interleaving (review P1-2; the driver additionally cancels the
        // previous run, as the sim's `++auraRunRef` does).
        self.aura.state = AuraState::Thinking;
        self.aura.runs += 1;
        self.requests.push(AppRequest::AuraSubmit { text, voice });
    }

    /// The aura talk hold finished (driver timer, tui.js:2128-2132).
    pub fn aura_talk_fire(&mut self) {
        // Only a hold still in `listening` fires (navigation away cancels
        // the arm; a run started meanwhile owns the orb).
        if self.aura.state != AuraState::Listening {
            return;
        }
        self.dirty = true;
        self.aura_submit(crate::script::AURA_TALK_PHRASE.to_owned(), true);
    }

    /// Enter the aura stage (the `/aura` command and the launcher's Aura
    /// row share this): the departing surface's draft parks, the aura's
    /// own comes live (TUI5 item 9 — Aura has its own composer instance).
    fn enter_aura(&mut self) {
        if self.screen == Screen::Aura {
            return;
        }
        if !self.mode.fabricates_locally() {
            // THE ONE DOOR into the aura stage (`/aura` and the launcher's
            // ◉ Aura row both come through here). Everything behind it —
            // `AuraSubmit`, `AuraTalk`, `ResetAura` — is demo-driver
            // vocabulary the live driver discards, so the stage would take
            // a hold and sit in `Listening` forever (W3c3.1 r2, P1-A).
            self.refuse_demo_only("Aura Mode");
            return;
        }
        self.switch_surface(Screen::Aura);
    }

    /// Esc from the aura stage: back to the session if one is attached,
    /// else the launcher — aura state persists either way.
    fn exit_aura(&mut self) {
        // TUI4c: attachment is the map's word now — a checked-out session
        // (or a content-bearing scratch) takes esc back to the session;
        // an aura entered from the menu returns to the menu. TUI5 item 9
        // (the draft swap) rides the switch authority.
        let target = if self.active_session.is_some()
            || !self.projection.entries().is_empty()
            || self.session_name.is_some()
        {
            Screen::Session
        } else {
            Screen::Launcher
        };
        self.switch_surface(target);
    }

    /// THE ONE DOOR into `/accounts` (`/accounts`, `/account`, and the
    /// launcher's ⚿ row all come through here). Clears the stale action
    /// message (sim `startLogin`/screen-entry behavior) and asks the driver
    /// for fresh rows — demo answers from the seed, live from
    /// `account.list`.
    fn enter_accounts(&mut self) {
        if self.screen == Screen::Accounts {
            return;
        }
        self.accounts.message = None;
        // P1 MASK LAW (the U2 owner addendum): every open starts masked —
        // a reveal never survives into a later visit, whichever way the
        // last one ended (esc, ⌃C, a screen switch).
        self.accounts.revealed = false;
        self.switch_surface(Screen::Accounts);
        self.requests.push(AppRequest::AccountsRefresh);
        // D2: the device-discovery read rides SCREEN ENTRY only (no
        // polling), and only when the live daemon serves it — demo and
        // ungated daemons keep the section honestly absent.
        if self.device_discovery_available() {
            self.requests.push(AppRequest::DeviceCandidatesRefresh);
        }
        self.dirty = true;
    }

    /// Esc from `/accounts` (sim tui.js:2516-2519): with a login card open
    /// the card's own total-modality already consumed the key; otherwise
    /// back to the session if one is attached, else the launcher. Closing
    /// RESTORES the mask (P1) — a reveal is per-visit.
    fn exit_accounts(&mut self) {
        self.accounts.revealed = false;
        let target = if self.active_session.is_some()
            || !self.projection.entries().is_empty()
            || self.session_name.is_some()
        {
            Screen::Session
        } else {
            Screen::Launcher
        };
        self.switch_surface(target);
        self.dirty = true;
    }

    /// Click/Enter on an accounts row (sim `useAccount`, tui.js:2160-2168).
    ///
    /// OPTIMISM FORBIDDEN (report §5.1): this sets `pending_select` and
    /// pushes the request — the rows themselves DO NOT move here. The dot
    /// moves in [`Self::apply_account_selected`] / a newer snapshot.
    pub fn select_account(&mut self, alias: &str) {
        if self.accounts.pending_select.is_some() {
            return;
        }
        let Some(row) = self.accounts.rows.iter().find(|row| row.alias == alias) else {
            self.accounts.message =
                Some(format!("· no account \"{alias}\" — /accounts to see them"));
            self.dirty = true;
            return;
        };
        if row.selected {
            // Sim parity: re-clicking the active row re-emits the message
            // without a daemon round-trip (useAccount has no early return).
            self.accounts.message = Some(format!(
                "✓ {} → {} · {} · active",
                row.provider,
                row.alias,
                auth_label(row.method)
            ));
            self.dirty = true;
            return;
        }
        // W5 extension (additive status vocabulary): an unusable row is
        // refused locally with an honest reason instead of a doomed RPC.
        match row.status {
            haider_protocol::credential::CredentialStatus::Expired
            | haider_protocol::credential::CredentialStatus::Revoked => {
                self.accounts.message = Some(format!(
                    "· {alias} is not usable — /login to re-authenticate"
                ));
                self.dirty = true;
                return;
            }
            haider_protocol::credential::CredentialStatus::Ok
            | haider_protocol::credential::CredentialStatus::Limited { .. } => {}
        }
        self.accounts.pending_select = Some(alias.to_owned());
        self.accounts.message = None;
        self.requests.push(AppRequest::AccountSetActive {
            alias: alias.to_owned(),
        });
        self.dirty = true;
    }

    /// A correlated `account.set_active` result: move the dot within the
    /// descriptor's provider, stamp the revision, emit the sim's message.
    /// Late/foreign results are gated by `pending_select` + revision.
    pub fn apply_account_selected(
        &mut self,
        descriptor: &haider_protocol::credential::CredentialDescriptor,
        revision: u64,
    ) {
        if self
            .accounts
            .pending_select
            .as_deref()
            .is_some_and(|pending| pending == descriptor.alias.as_str())
        {
            self.accounts.pending_select = None;
        }
        if let Some(current) = self.accounts.revision
            && revision < current
        {
            return;
        }
        for row in &mut self.accounts.rows {
            if row.provider == descriptor.provider {
                row.selected = row.alias == descriptor.alias.as_str();
            }
        }
        self.accounts.revision = Some(revision);
        self.accounts.message = Some(format!(
            "✓ {} → {} · {} · active",
            descriptor.provider,
            descriptor.alias,
            auth_label(descriptor.auth_method)
        ));
        // Choosing an account IS choosing the session identity (W5f-2):
        // the committed pick rides into the composer line and pins, so the
        // next `session.create` carries it and no later snapshot undoes it.
        self.adopt_identity(&descriptor.provider, descriptor.alias.as_str(), true);
        self.dirty = true;
    }

    /// Point the composer identity at `provider`/`alias`, taking the model
    /// from the provider's own declaration (its default, else its first
    /// discovered slug — NEVER an invented one; with nothing discovered the
    /// current model stands until `/model` can offer real candidates).
    fn adopt_identity(&mut self, provider: &str, alias: &str, pin: bool) {
        if self.identity.provider != provider {
            self.identity.provider = provider.to_owned();
        }
        if self.identity.account != alias {
            self.identity.account = alias.to_owned();
        }
        if let Some(model) = self
            .providers
            .providers
            .iter()
            .find(|summary| summary.provider == provider)
            .and_then(|summary| {
                summary
                    .default_model
                    .clone()
                    .or_else(|| summary.models.first().cloned())
            })
            && self.identity.model_short != model
        {
            self.identity.model_short = model;
        }
        self.refresh_context_window();
        self.identity_pinned |= pin;
        self.dirty = true;
    }

    /// Re-derives the identity's context window from the discovered catalog
    /// (W5g-1: real limits, never guessed). A provider-declared window
    /// always wins; with none declared the current figure stands — seed
    /// defaults remain honest fallbacks, not fabrications. Idempotent, so
    /// catalog arrivals may call it even for a PINNED identity: the pin
    /// protects the user's provider/model choice, not a stale number.
    pub fn refresh_context_window(&mut self) {
        if let Some(window) = self
            .providers
            .declared_window(&self.identity.provider, &self.identity.model_short)
            && self.identity.context_window != window
        {
            self.identity.context_window = window;
            self.dirty = true;
        }
    }

    /// The auth flavor of the CURRENT identity pair — `oauth` or `api` —
    /// so the user knows what is getting metered (F2c). Derivation, in
    /// truth order: the provider key's own encoding (`*-oauth`), the
    /// selected account's method for that provider, then a provider
    /// registry row that declares exactly one method. Ambiguity renders
    /// nothing — never a guess.
    #[must_use]
    pub fn identity_auth_label(&self) -> Option<&'static str> {
        use haider_protocol::credential::AuthMethod;
        let provider = &self.identity.provider;
        if provider.ends_with("-oauth") {
            return Some("oauth");
        }
        if let Some(row) = self
            .accounts
            .rows
            .iter()
            .find(|row| row.provider == *provider && row.selected)
        {
            return Some(match row.method {
                AuthMethod::OAuth => "oauth",
                AuthMethod::ApiKey => "api",
            });
        }
        match self
            .providers
            .providers
            .iter()
            .find(|summary| summary.provider == *provider)
            .map(|summary| summary.auth_methods.as_slice())
        {
            Some([AuthMethod::OAuth]) => Some("oauth"),
            Some([AuthMethod::ApiKey]) => Some("api"),
            _ => None,
        }
    }

    /// The composer-top-rule identity (F2c): `model · oauth|api ·
    /// reasoning [· fast]` — NO alias, right-aligned on the band's top
    /// border by the renderer.
    ///
    /// WIDTH-DEGRADATION LAW: segments drop WHOLE, never truncated
    /// mid-word — the reasoning segment (its fast marker riding it)
    /// drops first, then the auth label, then the whole line; the model
    /// name is all-or-nothing.
    #[must_use]
    pub fn composer_identity(&self, budget: usize) -> Option<String> {
        let model = self.identity.model_short.as_str();
        let auth = self.identity_auth_label();
        // G3 (LE7): the tuning segment exists when EITHER knob is set —
        // explicit effort, the fast marker riding it, or fast alone.
        let reasoning = match (&self.identity.reasoning, self.identity.fast) {
            (Some(level), true) => Some(format!("{level} · fast")),
            (Some(level), false) => Some(level.clone()),
            (None, true) => Some("fast".to_owned()),
            (None, false) => None,
        };
        let mut candidates: Vec<String> = Vec::new();
        if let (Some(auth), Some(reasoning)) = (auth, reasoning.as_ref()) {
            candidates.push(format!("{model} · {auth} · {reasoning}"));
        }
        if let Some(auth) = auth {
            candidates.push(format!("{model} · {auth}"));
        }
        candidates.push(model.to_owned());
        candidates
            .into_iter()
            .find(|candidate| candidate.chars().count() <= budget && !candidate.is_empty())
    }

    /// Daemon-truth identity bootstrap (W5f-2): until the user pins a
    /// choice, the composer identity follows the ACTIVE account — so the
    /// first session lands on a provider that can actually serve a turn
    /// instead of the demo seed pair. Called by the LIVE driver whenever an
    /// account or provider snapshot applies; demo never calls it.
    pub fn bootstrap_identity_from_daemon(&mut self) {
        if self.identity_pinned {
            return;
        }
        let Some((provider, alias)) = self
            .accounts
            .rows
            .iter()
            .find(|row| row.selected)
            .map(|row| (row.provider.clone(), row.alias.clone()))
        else {
            return;
        };
        // Adopt only once the provider's MODEL truth is here: a
        // half-adopted identity (right provider, demo-seed model) would
        // send a foreign slug to the subscription API and 400. The next
        // snapshot completes the picture; nothing is lost by waiting.
        let model_known = self
            .providers
            .providers
            .iter()
            .find(|summary| summary.provider == provider)
            .is_some_and(|summary| summary.default_model.is_some() || !summary.models.is_empty());
        if !model_known {
            return;
        }
        self.adopt_identity(&provider, &alias, false);
    }

    /// A failed `account.set_active`: clear the pending gate and surface the
    /// public reason. The rows never moved, so there is nothing to undo.
    pub fn account_select_failed(&mut self, alias: &str, message: &str) {
        if self
            .accounts
            .pending_select
            .as_deref()
            .is_some_and(|pending| pending == alias)
        {
            self.accounts.pending_select = None;
        }
        self.accounts.message = Some(format!("· {alias}: {message}"));
        self.dirty = true;
    }

    /// One-key import of a discovered device credential (D2). The TUI
    /// sends ONLY the opaque candidate id — the daemon re-discovers and
    /// reads the local store itself, so no credential bytes cross the
    /// wire and nothing is installed locally: the pending pulse is the
    /// only visible change until the receipt lands and the chained
    /// `account.list` refresh materializes the account (daemon truth
    /// only, the §5.1 discipline).
    ///
    /// An UNSUPPORTED candidate is inert here by construction: its row
    /// carries no hit, no number, and no cursor slot, and this method
    /// re-checks the flag so even a stale coordinate cannot dispatch it.
    pub fn import_device_candidate(&mut self, candidate_id: &str) {
        let Some(candidate) = self
            .device
            .candidates
            .iter()
            .find(|candidate| candidate.candidate == candidate_id)
        else {
            return;
        };
        if !candidate.import_supported {
            return;
        }
        if self.device.pending_import.is_some() {
            self.device.message =
                Some("· one import at a time — waiting for the daemon".to_owned());
            self.dirty = true;
            return;
        }
        self.device.pending_import = Some(candidate.candidate.clone());
        self.device.message = None;
        self.requests.push(AppRequest::DeviceImport {
            candidate: candidate.candidate.clone(),
        });
        self.dirty = true;
    }

    /// THE ONE DOOR into `/providers` (report §5.2).
    fn enter_providers(&mut self) {
        if self.screen == Screen::Providers {
            return;
        }
        self.providers.message = None;
        // P1 MASK LAW: same one-door reset as `/accounts` — the shared
        // device section's labels always open masked here too.
        self.providers.revealed = false;
        self.switch_surface(Screen::Providers);
        self.requests.push(AppRequest::ProvidersRefresh);
        // D2: the shared buttons area shows the same "found on this
        // device" section here — same entry-only refresh, same gate.
        if self.device_discovery_available() {
            self.requests.push(AppRequest::DeviceCandidatesRefresh);
        }
        self.dirty = true;
    }

    /// Esc from `/providers`: same routing as `/accounts`. Closing
    /// RESTORES the mask (P1) — a reveal is per-visit.
    fn exit_providers(&mut self) {
        self.providers.revealed = false;
        let target = if self.active_session.is_some()
            || !self.projection.entries().is_empty()
            || self.session_name.is_some()
        {
            Screen::Session
        } else {
            Screen::Launcher
        };
        self.switch_surface(target);
        self.dirty = true;
    }

    /// THE ONE DOOR into `/usage` (U2). Works everywhere like `/accounts`;
    /// `filter` is `/usage <provider>`'s first token (empty clears). The
    /// live path is feature-gated BEFORE anything opens (the B2b lesson);
    /// demo opens an honest empty state — usage is daemon truth and the
    /// demo fabricates no meter. Re-running `/usage` while the screen is
    /// up re-filters and (live) re-reads.
    fn enter_usage(&mut self, filter: Option<&str>) {
        self.dirty = true;
        let filter = filter
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_ascii_lowercase);
        if !self.mode.fabricates_locally()
            && !self.daemon_serves(haider_rpc::FEATURE_USAGE_REPORT_V1)
        {
            self.flash = Some(self.stale_daemon_note("the usage report"));
            return;
        }
        let refilter = self.usage.filter != filter;
        self.usage.filter = filter;
        if refilter {
            // A new filter re-anchors navigation; the frame reconciles the
            // scroll against the new true range (render authority).
            self.usage.cursor = 0;
            self.usage.scroll.set(0);
        }
        // MASK LAW (owner addendum): every open starts masked — a reveal
        // never survives into a later visit, whichever way the last one
        // ended (esc, ⌃C, a screen switch).
        self.usage.revealed = false;
        if self.screen != Screen::Usage {
            self.switch_surface(Screen::Usage);
        }
        if !self.mode.fabricates_locally() {
            self.usage.fetching = true;
            self.requests.push(AppRequest::UsageRefresh);
        }
    }

    /// Esc from `/usage`: same routing as `/accounts`. Closing the screen
    /// RESTORES the mask (owner addendum) — a reveal is per-visit.
    fn exit_usage(&mut self) {
        self.usage.revealed = false;
        let target = if self.active_session.is_some()
            || !self.projection.entries().is_empty()
            || self.session_name.is_some()
        {
            Screen::Session
        } else {
            Screen::Launcher
        };
        self.switch_surface(target);
        self.dirty = true;
    }

    /// Keys on `/usage` (U2). KEY-OWNERSHIP: esc closes (never a ⏎
    /// action — the screen is read-only), ↑/↓ move the provider-group
    /// cursor (F2b follow), ←/→ (and tab/shift-tab) cycle the cursor
    /// group's accounts wrapping, PageUp/PageDown/Home/End scroll against
    /// the frame-written max, `r` toggles the identity reveal (owner
    /// addendum — per-visit), `f` re-reads (live). Everything else is
    /// swallowed.
    fn handle_usage_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.exit_usage(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.usage.cursor = self.usage.cursor.saturating_sub(1);
                self.usage.follow_cursor.set(true);
                self.dirty = true;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let groups = self.usage.groups().len();
                if groups > 0 {
                    self.usage.cursor = (self.usage.cursor + 1).min(groups - 1);
                }
                self.usage.follow_cursor.set(true);
                self.dirty = true;
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                let groups = self.usage.groups();
                let Some(group) = groups.get(self.usage.cursor.min(groups.len().saturating_sub(1)))
                else {
                    return;
                };
                let len = group.accounts.len();
                if len < 2 {
                    return;
                }
                let current = self.usage.selected_tab(group);
                let next = if matches!(code, KeyCode::Right | KeyCode::Tab) {
                    (current + 1) % len
                } else {
                    (current + len - 1) % len
                };
                self.usage.tabs.insert(group.provider.clone(), next);
                self.usage.follow_cursor.set(true);
                self.dirty = true;
            }
            KeyCode::Char('r') => {
                // Owner addendum: `r` toggles the identity REVEAL for this
                // visit only — the screen always opens masked and closing
                // restores the mask.
                self.usage.revealed = !self.usage.revealed;
                self.dirty = true;
            }
            // A manual re-read (live only — the demo has nothing to fetch
            // and the honest empty state already says so).
            KeyCode::Char('f') if !self.mode.fabricates_locally() => {
                self.usage.fetching = true;
                self.requests.push(AppRequest::UsageRefresh);
                self.dirty = true;
            }
            // F2b: page keys move the viewport and clamp at the true
            // frame-written range.
            KeyCode::PageUp => {
                self.usage
                    .scroll
                    .set(self.usage.scroll.get().saturating_sub(8));
                self.dirty = true;
            }
            KeyCode::PageDown => {
                let max = self.usage.scroll_max.get();
                self.usage
                    .scroll
                    .set(self.usage.scroll.get().saturating_add(8).min(max));
                self.dirty = true;
            }
            KeyCode::Home => {
                self.usage.scroll.set(0);
                self.dirty = true;
            }
            KeyCode::End => {
                self.usage.scroll.set(self.usage.scroll_max.get());
                self.dirty = true;
            }
            _ => {}
        }
    }

    /// Click on a model chip: request the default change under the CAS.
    /// The `*` marker moves ONLY on the correlated reply (§5.1's law
    /// applied to the management screen).
    pub fn set_default_model(&mut self, provider: &str, model: &str) {
        if self.providers.pending_default.is_some() {
            return;
        }
        let Some(summary) = self
            .providers
            .providers
            .iter()
            .find(|summary| summary.provider == provider)
        else {
            return;
        };
        if summary.default_model.as_deref() == Some(model) {
            self.providers.message = Some(format!("· {model} is already {provider}'s default"));
            self.dirty = true;
            return;
        }
        if !summary.models.iter().any(|known| known == model) {
            self.providers.message =
                Some(format!("· {provider} has no model \"{model}\" configured"));
            self.dirty = true;
            return;
        }
        let Some(expected_revision) = self.providers.revision else {
            self.providers.message =
                Some("· provider snapshot not loaded yet — try again".to_owned());
            self.dirty = true;
            return;
        };
        self.providers.pending_default = Some((provider.to_owned(), model.to_owned()));
        self.providers.message = None;
        self.requests.push(AppRequest::SetDefaultModel {
            provider: provider.to_owned(),
            model: model.to_owned(),
            expected_revision,
        });
        self.dirty = true;
    }

    /// A committed default-model change (correlated + revision-gated).
    pub fn apply_default_model_set(
        &mut self,
        summary: haider_rpc::ProviderSummaryWire,
        revision: u64,
    ) {
        let provider = summary.provider.clone();
        let model = summary.default_model.clone().unwrap_or_default();
        if self.providers.apply_default_set(summary, revision) {
            self.providers.message = Some(format!("✓ {provider} default → {model}"));
        }
        self.dirty = true;
    }

    /// A failed default-model change: release the gate, honest reason. A
    /// `revision_conflict` also refreshes the snapshot (the CAS told us we
    /// are stale).
    pub fn default_model_failed(&mut self, provider: &str, message: &str, refresh: bool) {
        if self
            .providers
            .pending_default
            .as_ref()
            .is_some_and(|(pending, _)| pending == provider)
        {
            self.providers.pending_default = None;
        }
        self.providers.message = Some(format!("· {provider}: {message}"));
        if refresh {
            self.requests.push(AppRequest::ProvidersRefresh);
        }
        self.dirty = true;
    }

    /// Keys on the `/providers` screen.
    fn handle_providers_key(&mut self, code: KeyCode) {
        if self.custom_add.is_some() {
            self.handle_custom_card_key(code);
            return;
        }
        match code {
            KeyCode::Esc if self.providers.pending_remove.is_some() => {
                self.providers.pending_remove = None;
                self.providers.message = None;
                self.dirty = true;
            }
            KeyCode::Esc => self.exit_providers(),
            KeyCode::Char('x') => {
                if let Some(provider) = self
                    .providers
                    .providers
                    .get(self.providers.cursor)
                    .map(|summary| summary.provider.clone())
                {
                    // The DAEMON refuses builtins/referenced providers with a
                    // typed reason — the client arms without pre-judging.
                    self.providers.message = Some(format!(
                        "remove provider `{provider}`? enter confirms · esc cancels"
                    ));
                    self.providers.pending_remove = Some(provider);
                    self.dirty = true;
                }
            }
            KeyCode::Enter if self.providers.pending_remove.is_some() => {
                let provider = self.providers.pending_remove.take().unwrap_or_default();
                if self.mode.fabricates_locally() {
                    self.providers.providers.retain(|s| s.provider != provider);
                    self.providers.cursor = self
                        .providers
                        .cursor
                        .min(self.providers.providers.len().saturating_sub(1));
                    self.providers.message = Some(format!("removed `{provider}` (demo)"));
                } else if self.daemon_serves(haider_rpc::FEATURE_PROVIDER_REMOVE_V1) {
                    self.providers.message = Some(format!("removing `{provider}`…"));
                    self.requests.push(AppRequest::ProviderRemove { provider });
                } else {
                    self.providers.message = Some(self.stale_daemon_note("provider removal"));
                }
                self.dirty = true;
            }
            KeyCode::Char('e') => {
                if let Some(summary) = self.providers.providers.get(self.providers.cursor).cloned()
                {
                    self.open_custom_edit(&summary);
                }
            }
            KeyCode::Char('h') => self.open_huggingface_preset(),
            KeyCode::Char('z') => self.open_opencode_zen_preset(),
            KeyCode::Char('g') => self.open_opencode_go_preset(),
            KeyCode::Char('o') => self.open_ollama_preset(),
            KeyCode::Char('l') => self.open_lmstudio_preset(),
            // G4a: `f` re-runs model discovery for the selected provider —
            // the affordance behind the local presets' "start the server,
            // then refresh" hint. Live-only vocabulary; the daemon answers
            // with the refreshed inventory or a typed reason.
            KeyCode::Char('f') if !self.mode.fabricates_locally() => {
                if let Some(provider) = self
                    .providers
                    .providers
                    .get(self.providers.cursor)
                    .map(|summary| summary.provider.clone())
                {
                    self.providers.message = Some(format!("refreshing {provider} models…"));
                    self.requests
                        .push(AppRequest::ProviderModelsRefresh { provider });
                    self.dirty = true;
                }
            }
            KeyCode::Char('r') => {
                // P1 (the U2 owner addendum): `r` toggles the identity
                // REVEAL for this visit only — the device section's
                // account labels are the only identity this screen shows.
                self.providers.revealed = !self.providers.revealed;
                self.dirty = true;
            }
            // D2: the shared "found on this device" section is numbered
            // here too — the same one-key import as `/accounts` (the
            // provider cursor keeps ↑/↓; digits belong to the section).
            KeyCode::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if let Some(id) = self.device.supported_id(index) {
                    self.import_device_candidate(&id);
                }
            }
            KeyCode::Up => {
                self.providers.cursor = self.providers.cursor.saturating_sub(1);
                self.providers.follow_cursor.set(true);
                self.dirty = true;
            }
            KeyCode::Down => {
                if !self.providers.providers.is_empty() {
                    self.providers.cursor =
                        (self.providers.cursor + 1).min(self.providers.providers.len() - 1);
                }
                self.providers.follow_cursor.set(true);
                self.dirty = true;
            }
            // F2b: the page scrolls — long rosters reach every row and the
            // bottom-pinned add buttons stay a PageDown/End away. The
            // frame reconciles against the true max (render authority).
            KeyCode::PageUp => {
                self.providers
                    .scroll
                    .set(self.providers.scroll.get().saturating_sub(8));
                self.dirty = true;
            }
            KeyCode::PageDown => {
                let max = self.providers.scroll_max.get();
                self.providers
                    .scroll
                    .set(self.providers.scroll.get().saturating_add(8).min(max));
                self.dirty = true;
            }
            KeyCode::Home => {
                self.providers.scroll.set(0);
                self.dirty = true;
            }
            KeyCode::End => {
                self.providers.scroll.set(self.providers.scroll_max.get());
                self.dirty = true;
            }
            _ => {}
        }
    }

    /// Opens the OAuth add card and starts the flow. Alias derivation per
    /// §5.3: `<provider>` then the smallest free numeric suffix against the
    /// CURRENT rows (the daemon re-checks uniqueness at commit).
    fn open_oauth_add(&mut self, kind: AccountAddKind) {
        if self.oauth_add.is_some() || self.custom_add.is_some() {
            return;
        }
        let (provider, title) = match kind {
            AccountAddKind::OpenAiOAuth => ("openai-oauth", "OpenAI — ChatGPT"),
            AccountAddKind::AnthropicOAuth => ("anthropic-oauth", "Anthropic — Claude"),
            // B6b: the daemon's device flow rides the SAME oauth_start/
            // status/add wire — only the provider id differs; the card is
            // flow-agnostic by design.
            AccountAddKind::KimiOAuth => ("kimi-oauth", "Kimi — Moonshot"),
            AccountAddKind::OpenAiApi
            | AccountAddKind::AnthropicApi
            | AccountAddKind::GeminiApi
            | AccountAddKind::HuggingFace
            | AccountAddKind::OpencodeZen
            | AccountAddKind::OpencodeGo
            | AccountAddKind::Ollama
            | AccountAddKind::LmStudio
            | AccountAddKind::Custom => return,
        };
        let alias = smallest_free_alias(provider, &self.accounts.rows);
        self.oauth_attempt_seq += 1;
        let attempt = self.oauth_attempt_seq;
        self.accounts.message = None;
        self.oauth_add = Some(OAuthAddCard {
            provider: provider.to_owned(),
            title,
            alias: alias.clone(),
            attempt,
            phase: OAuthAddPhase::Starting,
        });
        self.requests.push(AppRequest::OAuthAddStart {
            provider: provider.to_owned(),
            alias,
            attempt,
        });
        self.dirty = true;
    }

    /// Closes the card and cancels its flow (esc / `[2]` — sim cancelAuth).
    fn cancel_oauth_add(&mut self) {
        if let Some(card) = self.oauth_add.take() {
            self.requests.push(AppRequest::OAuthAddCancel {
                attempt: card.attempt,
            });
            self.accounts.message = Some("· authorize cancelled".to_owned());
            self.dirty = true;
        }
    }

    /// Driver-applied phase change, attempt-gated: a retired card's late
    /// reply touches nothing (the login-card law). A no-op phase (the
    /// device flow's 1.5 s status poll re-reporting WaitingDevice) neither
    /// rewrites nor re-dirties.
    pub fn oauth_add_phase(&mut self, attempt: u64, phase: OAuthAddPhase) {
        if let Some(card) = self.oauth_add.as_mut()
            && card.attempt == attempt
            && card.phase != phase
        {
            card.phase = phase;
            self.dirty = true;
        }
    }

    /// Terminal public failure for the card (attempt-gated).
    pub fn oauth_add_failed(&mut self, attempt: u64, message: &str) {
        if let Some(card) = self.oauth_add.as_mut()
            && card.attempt == attempt
        {
            card.phase = OAuthAddPhase::Failed {
                message: message.to_owned(),
            };
            self.dirty = true;
        }
    }

    /// The durable add committed: close the card, note the identity, and
    /// refresh rows from the daemon (single authority — no local insert).
    pub fn oauth_add_completed(
        &mut self,
        attempt: u64,
        descriptor: &haider_protocol::credential::CredentialDescriptor,
    ) {
        if self
            .oauth_add
            .as_ref()
            .is_some_and(|card| card.attempt == attempt)
        {
            self.oauth_add = None;
            // P1 MASK LAW: the receipt is transient chrome with no key
            // loop of its own, so the identity rides it MASKED-ALWAYS
            // (one authority — `mask_identity`); the durable, revealable
            // surface is the account row itself.
            self.accounts.message = Some(format!(
                "✓ {} → {} · oauth · {}",
                descriptor.provider,
                descriptor.alias,
                crate::format::mask_identity(&descriptor.identity)
            ));
            self.requests.push(AppRequest::AccountsRefresh);
            self.dirty = true;
        }
    }

    /// Card keys (total over the accounts screen while open, sim
    /// tui.js:2495): `[1]` re-opens the browser link (or SIMULATES the
    /// authorize in demo, sim parity); `[2]`/esc cancels.
    fn handle_oauth_card_key(&mut self, code: KeyCode) {
        let Some(card) = self.oauth_add.as_ref() else {
            return;
        };
        // A FAILED card is the §5.3 collision-recovery surface: the alias
        // becomes editable in place and ⏎ retries the whole flow under it
        // with a FRESH attempt (the daemon rejected before consuming the
        // ready reference, so nothing durable rode the dead one). Digits
        // are legal alias characters here, so the `[1]`/`[2]` key map
        // yields to typing: ⏎ retry · esc close.
        if matches!(card.phase, OAuthAddPhase::Failed { .. }) {
            match code {
                KeyCode::Esc => self.cancel_oauth_add(),
                KeyCode::Enter => self.retry_oauth_add(),
                KeyCode::Backspace => {
                    if let Some(card) = self.oauth_add.as_mut() {
                        card.alias.pop();
                        self.dirty = true;
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(c) = alias_char(c)
                        && let Some(card) = self.oauth_add.as_mut()
                    {
                        card.alias.push(c);
                        self.dirty = true;
                    }
                }
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Esc | KeyCode::Char('2') => self.cancel_oauth_add(),
            KeyCode::Char('1') => {
                // Only the waiting phases answer `[1]` — Failed never
                // reaches here (the guard above owns it), and the other
                // phases advertise no keys. WaitingDevice re-opens the
                // VERIFICATION url (the code-entry page), same affordance.
                if let OAuthAddPhase::WaitingBrowser { url, .. }
                | OAuthAddPhase::WaitingDevice { url, .. } = &card.phase
                {
                    if self.mode.fabricates_locally() {
                        // Sim confirmAuth: the simulated authorize lands the
                        // account locally and selects it for its provider.
                        let (provider, identity) = match card.provider.as_str() {
                            "openai-oauth" => ("openai", "you@work.com · ChatGPT"),
                            // B6b: the openai/anthropic arms condense into
                            // the SIM's seeded groups; kimi has no sim seed
                            // group, so it lands under the daemon-truth
                            // provider id.
                            "kimi-oauth" => ("kimi-oauth", "you@kimi.com · Kimi Code"),
                            _ => ("anthropic", "you@me.com · Claude Max"),
                        };
                        let alias = card.alias.clone();
                        let attempt = card.attempt;
                        for row in &mut self.accounts.rows {
                            if row.provider == provider {
                                row.selected = false;
                            }
                        }
                        self.accounts.rows.push(AccountRow {
                            alias: alias.clone(),
                            provider: provider.to_owned(),
                            method: haider_protocol::credential::AuthMethod::OAuth,
                            identity: identity.to_owned(),
                            status: haider_protocol::credential::CredentialStatus::Ok,
                            selected: true,
                            base_url: None,
                        });
                        self.oauth_add = None;
                        self.accounts.message =
                            Some(format!("✓ {provider} → {alias} · oauth · active"));
                        let _ = attempt;
                        self.dirty = true;
                    } else {
                        self.requests.push(AppRequest::OpenUrl { url: url.clone() });
                    }
                }
            }
            _ => {}
        }
    }

    /// ⏎ on a FAILED OAuth card: restart the flow under the (possibly
    /// edited) alias. The attempt is re-minted — the failed issuance's id
    /// is dead forever, so its late replies die at the identity gates.
    fn retry_oauth_add(&mut self) {
        let Some(card) = self.oauth_add.as_mut() else {
            return;
        };
        if !matches!(card.phase, OAuthAddPhase::Failed { .. }) || !account_alias_ok(&card.alias) {
            return;
        }
        self.oauth_attempt_seq += 1;
        card.attempt = self.oauth_attempt_seq;
        card.phase = OAuthAddPhase::Starting;
        let provider = card.provider.clone();
        let alias = card.alias.clone();
        let attempt = card.attempt;
        self.accounts.message = None;
        self.requests.push(AppRequest::OAuthAddStart {
            provider,
            alias,
            attempt,
        });
        self.dirty = true;
    }

    /// Opens the `+ Custom (OpenAI-compatible)` card. The name prefills
    /// with the smallest free `custom[-N]` against the provider registry;
    /// the origin with the sim's demo URL (a real vLLM default).
    fn open_custom_add(&mut self) {
        if self.custom_add.is_some() || self.oauth_add.is_some() {
            return;
        }
        let taken: Vec<AccountRow> = self
            .providers
            .providers
            .iter()
            .map(|summary| AccountRow {
                alias: summary.provider.clone(),
                provider: summary.provider.clone(),
                method: haider_protocol::credential::AuthMethod::ApiKey,
                identity: String::new(),
                status: haider_protocol::credential::CredentialStatus::Ok,
                selected: false,
                base_url: None,
            })
            .collect();
        self.custom_attempt_seq += 1;
        self.accounts.message = None;
        self.custom_add = Some(CustomProviderCard {
            name: smallest_free_alias("custom", &taken),
            origin: "http://127.0.0.1:8000/v1".to_owned(),
            model: String::new(),
            focus: CustomField::Name,
            phase: CustomPhase::Editing { error: None },
            attempt: self.custom_attempt_seq,
            edit: false,
            keyless: false,
        });
        self.dirty = true;
    }

    /// W10b: the edit card — the SAME custom card prefilled from the
    /// summary with identity locked; ⏎ re-configures mutable fields under
    /// the current revision (the daemon refuses identity drift with a
    /// typed reason — builtins included; the client never pre-judges).
    fn open_custom_edit(&mut self, summary: &haider_rpc::ProviderSummaryWire) {
        if self.custom_add.is_some() || self.oauth_add.is_some() {
            return;
        }
        if !self.mode.fabricates_locally()
            && !self.daemon_serves(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1)
        {
            self.providers.message = Some(self.stale_daemon_note("provider editing"));
            self.dirty = true;
            return;
        }
        self.custom_attempt_seq += 1;
        self.custom_add = Some(CustomProviderCard {
            name: summary.provider.clone(),
            origin: summary.endpoint.clone().unwrap_or_default(),
            model: summary
                .default_model
                .clone()
                .or_else(|| summary.models.first().cloned())
                .unwrap_or_default(),
            focus: CustomField::Model,
            phase: CustomPhase::Editing { error: None },
            attempt: self.custom_attempt_seq,
            edit: true,
            keyless: summary.auth_methods.is_empty(),
        });
        self.dirty = true;
    }

    /// W10b: the HuggingFace preset — the custom card prefilled with the
    /// HF router (openai-compatible); the user supplies the served model,
    /// then the normal login flow adds the token.
    fn open_huggingface_preset(&mut self) {
        self.open_custom_preset("huggingface", "https://router.huggingface.co/v1");
    }

    /// U1: the OpenCode Zen preset — the custom card prefilled with the Zen
    /// gateway (openai-compatible, Bearer OPENCODE_API_KEY via the normal
    /// login flow; `GET {origin}/models` serves the inventory
    /// unauthenticated). Zen has no usage endpoint today (`/zen/v1/usage`,
    /// `/balance`, `/credits` all 404, verified 2026-08-05) — local
    /// accounting applies; anomalyco/opencode#10448 tracks a future balance
    /// API.
    fn open_opencode_zen_preset(&mut self) {
        self.open_custom_preset("opencode-zen", "https://opencode.ai/zen/v1");
    }

    /// U1: the OpenCode Go preset — Zen's budget lane, same contract with
    /// its own model roster at `https://opencode.ai/zen/go/v1`.
    fn open_opencode_go_preset(&mut self) {
        self.open_custom_preset("opencode-go", "https://opencode.ai/zen/go/v1");
    }

    /// G4a: the Ollama preset — a KEYLESS (auth-None) custom provider at
    /// ollama's default compat origin. The user supplies the served model
    /// (`ollama pull` names it); commit skips the key card and discovers.
    fn open_ollama_preset(&mut self) {
        self.open_keyless_preset("ollama", "http://127.0.0.1:11434/v1");
    }

    /// G4a: the LM Studio preset — the same keyless contract at LM Studio's
    /// default local server origin.
    fn open_lmstudio_preset(&mut self) {
        self.open_keyless_preset("lmstudio", "http://127.0.0.1:1234/v1");
    }

    /// One-click custom-provider preset: the custom card prefilled with a
    /// known openai-compatible origin; the user supplies the served model,
    /// then the normal login flow adds the key.
    fn open_custom_preset(&mut self, stem: &str, origin: &str) {
        self.open_preset_card(stem, origin, false);
    }

    /// G4a keyless variant: same card, `auth_requirement: none` on commit,
    /// no key card afterwards.
    fn open_keyless_preset(&mut self, stem: &str, origin: &str) {
        self.open_preset_card(stem, origin, true);
    }

    fn open_preset_card(&mut self, stem: &str, origin: &str, keyless: bool) {
        if self.custom_add.is_some() || self.oauth_add.is_some() {
            return;
        }
        if !self.mode.fabricates_locally()
            && !self.daemon_serves(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1)
        {
            self.providers.message = Some(self.stale_daemon_note("custom providers"));
            self.dirty = true;
            return;
        }
        let taken: Vec<AccountRow> = self
            .providers
            .providers
            .iter()
            .map(|summary| AccountRow {
                alias: summary.provider.clone(),
                provider: summary.provider.clone(),
                method: haider_protocol::credential::AuthMethod::ApiKey,
                identity: String::new(),
                status: haider_protocol::credential::CredentialStatus::Ok,
                selected: false,
                base_url: None,
            })
            .collect();
        self.custom_attempt_seq += 1;
        self.custom_add = Some(CustomProviderCard {
            name: smallest_free_alias(stem, &taken),
            origin: origin.to_owned(),
            model: String::new(),
            focus: CustomField::Model,
            phase: CustomPhase::Editing { error: None },
            attempt: self.custom_attempt_seq,
            edit: false,
            keyless,
        });
        self.dirty = true;
    }

    fn cancel_custom_add(&mut self) {
        if self.custom_add.take().is_some() {
            self.dirty = true;
        }
    }

    /// ⏎ on the live card: `provider.configure` under the CURRENT provider
    /// revision (CAS — a stale snapshot is a typed conflict, never a
    /// silent overwrite).
    fn submit_custom_add(&mut self) {
        let expected_revision = self.providers.revision.unwrap_or(0);
        let Some(card) = self.custom_add.as_mut() else {
            return;
        };
        if !matches!(card.phase, CustomPhase::Editing { .. }) {
            return;
        }
        if !account_alias_ok(&card.name) {
            card.focus = CustomField::Name;
            self.dirty = true;
            return;
        }
        if card.origin.trim().is_empty() {
            card.focus = CustomField::Origin;
            self.dirty = true;
            return;
        }
        // An ENABLED create requires a model inventory and a default
        // (daemon law) — the card refuses to submit what would bounce.
        if card.model.trim().is_empty() {
            card.focus = CustomField::Model;
            self.dirty = true;
            return;
        }
        card.phase = CustomPhase::Submitting;
        let attempt = card.attempt;
        let name = card.name.clone();
        let origin = card.origin.trim().to_owned();
        let model = card.model.trim().to_owned();
        let keyless = card.keyless;
        self.requests.push(AppRequest::ProviderConfigure {
            attempt,
            name,
            origin,
            model,
            keyless,
            expected_revision,
        });
        self.dirty = true;
    }

    /// A failed `provider.configure`: back to editing with the public
    /// reason above the still-filled fields.
    pub fn custom_add_failed(&mut self, attempt: u64, message: &str) {
        if let Some(card) = self.custom_add.as_mut()
            && card.attempt == attempt
        {
            card.phase = CustomPhase::Editing {
                error: Some(message.to_owned()),
            };
            self.dirty = true;
        }
    }

    /// A committed `provider.configure`: close the card and chain straight
    /// into the masked key card — the provider needs a credential before
    /// it can serve anything (report §4.4: custom = base URL + key).
    /// G4a: a KEYLESS card skips the key card entirely — there is no
    /// credential to add — and goes straight to model discovery.
    pub fn custom_add_committed(&mut self, attempt: u64) {
        let Some(card) = self.custom_add.take_if(|card| card.attempt == attempt) else {
            return;
        };
        if card.keyless {
            self.providers.message = Some(format!(
                "✓ provider {} created · keyless — discovering models…",
                card.name
            ));
            self.requests.push(AppRequest::ProviderModelsRefresh {
                provider: card.name,
            });
            self.dirty = true;
            return;
        }
        self.accounts.message = Some(format!(
            "✓ provider {} created · OpenAI-compatible — now add its key",
            card.name
        ));
        self.open_login_card(&card.name, None);
        self.dirty = true;
    }

    /// Keys on the custom-provider card. DEMO is the sim's verbatim key
    /// map (`[1]` fabricate · `[2]`/esc cancel); LIVE edits fields (tab ·
    /// ⏎ create · esc cancel — digits are name characters here).
    fn handle_custom_card_key(&mut self, code: KeyCode) {
        if self.mode.fabricates_locally() {
            match code {
                KeyCode::Esc | KeyCode::Char('2') => self.cancel_custom_add(),
                KeyCode::Char('1') => self.confirm_custom_add_demo(),
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Esc => self.cancel_custom_add(),
            KeyCode::Enter => self.submit_custom_add(),
            KeyCode::Tab => {
                if let Some(card) = self.custom_add.as_mut()
                    && matches!(card.phase, CustomPhase::Editing { .. })
                {
                    card.focus = if card.edit {
                        // Identity is locked in edit mode — only the model
                        // line takes focus.
                        CustomField::Model
                    } else {
                        match card.focus {
                            CustomField::Name => CustomField::Origin,
                            CustomField::Origin => CustomField::Model,
                            CustomField::Model => CustomField::Name,
                        }
                    };
                    self.dirty = true;
                }
            }
            KeyCode::BackTab => {
                if let Some(card) = self.custom_add.as_mut()
                    && matches!(card.phase, CustomPhase::Editing { .. })
                {
                    card.focus = match card.focus {
                        CustomField::Name => CustomField::Model,
                        CustomField::Origin => CustomField::Name,
                        CustomField::Model => CustomField::Origin,
                    };
                    self.dirty = true;
                }
            }
            KeyCode::Backspace => {
                if let Some(card) = self.custom_add.as_mut()
                    && matches!(card.phase, CustomPhase::Editing { .. })
                {
                    match card.focus {
                        CustomField::Name => {
                            card.name.pop();
                        }
                        CustomField::Origin => {
                            card.origin.pop();
                        }
                        CustomField::Model => {
                            card.model.pop();
                        }
                    }
                    self.dirty = true;
                }
            }
            KeyCode::Char(c) => {
                if let Some(card) = self.custom_add.as_mut()
                    && matches!(card.phase, CustomPhase::Editing { .. })
                {
                    match card.focus {
                        // The name is a provider id — alias grammar.
                        CustomField::Name => {
                            if let Some(c) = alias_char(c) {
                                card.name.push(c);
                                self.dirty = true;
                            }
                        }
                        // The origin is a URL, the model a free-form
                        // server id (`llama3.1:8b`) — any printable.
                        CustomField::Origin => {
                            if !c.is_control() {
                                card.origin.push(c);
                                self.dirty = true;
                            }
                        }
                        CustomField::Model => {
                            if !c.is_control() {
                                card.model.push(c);
                                self.dirty = true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Sim confirmAuth's custom arm, verbatim (tui.js:2183-2207): the demo
    /// fabricates `custom-N` · `local-N` on the fixed demo URL, selects
    /// it, and closes the card with the sim's ✓ message.
    fn confirm_custom_add_demo(&mut self) {
        if self.custom_add.take().is_none() {
            return;
        }
        let count = self
            .accounts
            .rows
            .iter()
            .filter(|row| row.provider.starts_with("custom-"))
            .map(|row| row.provider.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            + 1;
        let provider = format!("custom-{count}");
        let alias = format!("local-{count}");
        let base_url = "http://127.0.0.1:8000/v1";
        // Sim hex(k): (0xa000 + k*7).toString(16).slice(-4).
        let hex = format!("{:x}", 0xa000 + count * 7);
        let identity = format!("{base_url} · sk-…{}", &hex[hex.len().saturating_sub(4)..]);
        for row in &mut self.accounts.rows {
            if row.provider == provider {
                row.selected = false;
            }
        }
        self.accounts.rows.push(AccountRow {
            alias: alias.clone(),
            provider: provider.clone(),
            method: haider_protocol::credential::AuthMethod::ApiKey,
            identity,
            status: haider_protocol::credential::CredentialStatus::Ok,
            selected: true,
            base_url: Some(base_url.to_owned()),
        });
        self.accounts.message = Some(format!(
            "✓ added {provider} · {alias} · api key · OpenAI-compatible — now active"
        ));
        self.dirty = true;
    }

    /// Keys on the `/accounts` screen (no composer; the login card, when
    /// open, is total-modal and never reaches here).
    fn handle_accounts_key(&mut self, code: KeyCode) {
        if self.oauth_add.is_some() {
            self.handle_oauth_card_key(code);
            return;
        }
        if self.custom_add.is_some() {
            self.handle_custom_card_key(code);
            return;
        }
        match code {
            KeyCode::Esc if self.accounts.pending_remove.is_some() => {
                self.accounts.pending_remove = None;
                self.accounts.message = None;
                self.dirty = true;
            }
            KeyCode::Esc => self.exit_accounts(),
            KeyCode::Char('x') => {
                if let Some(alias) = self
                    .accounts
                    .rows
                    .get(self.accounts.cursor)
                    .map(|row| row.alias.clone())
                {
                    self.accounts.message = Some(format!(
                        "remove account `{alias}`? enter confirms · esc cancels"
                    ));
                    self.accounts.pending_remove = Some(alias);
                    self.dirty = true;
                }
            }
            KeyCode::Enter if self.accounts.pending_remove.is_some() => {
                let alias = self.accounts.pending_remove.take().unwrap_or_default();
                if self.mode.fabricates_locally() {
                    self.accounts.rows.retain(|row| row.alias != alias);
                    self.accounts.cursor = self
                        .accounts
                        .cursor
                        .min(self.accounts.rows.len().saturating_sub(1));
                    self.accounts.message = Some(format!("removed `{alias}` (demo)"));
                } else {
                    self.accounts.message = Some(format!("removing `{alias}`…"));
                    self.requests.push(AppRequest::AccountRemove { alias });
                }
                self.dirty = true;
            }
            KeyCode::Up => {
                self.accounts.cursor = self.accounts.cursor.saturating_sub(1);
                self.dirty = true;
            }
            KeyCode::Down => {
                // D2: the flattened selectable rows extend into the
                // "found on this device" section's SUPPORTED candidates
                // (unsupported rows are inert and get no cursor slot).
                let total = self.accounts.rows.len() + self.device.supported_len();
                if total > 0 {
                    self.accounts.cursor = (self.accounts.cursor + 1).min(total - 1);
                }
                self.dirty = true;
            }
            // D2 one-key import (the owner menu law, the hooks digits
            // precedent): `[n]` names the nth SUPPORTED candidate. The
            // cursor follows so the pending pulse lands under the
            // highlight the user just addressed.
            KeyCode::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if let Some(id) = self.device.supported_id(index) {
                    self.accounts.cursor = self.accounts.rows.len() + index;
                    self.import_device_candidate(&id);
                    self.dirty = true;
                }
            }
            KeyCode::Char('r') => {
                // P1 (the U2 owner addendum): `r` toggles the identity
                // REVEAL for this visit only — the screen always opens
                // masked and closing restores the mask. The login/OAuth
                // cards' total modality already consumed the key above
                // when a card is open, so a typed alias/key `r` can never
                // land here.
                self.accounts.revealed = !self.accounts.revealed;
                self.dirty = true;
            }
            KeyCode::Enter => {
                if let Some(alias) = self
                    .accounts
                    .rows
                    .get(self.accounts.cursor)
                    .map(|row| row.alias.clone())
                {
                    self.select_account(&alias);
                } else if let Some(id) = self
                    .device
                    .supported_id(
                        self.accounts
                            .cursor
                            .saturating_sub(self.accounts.rows.len()),
                    )
                    .filter(|_| self.accounts.cursor >= self.accounts.rows.len())
                {
                    // ⏎ on a highlighted candidate row imports it — the
                    // same dispatch as its digit.
                    self.import_device_candidate(&id);
                }
            }
            _ => {}
        }
    }

    /// THE ONE DOOR into `/hooks` (H4). Session-scoped like `/tools`; the
    /// live path is feature-gated BEFORE anything opens (the B2b lesson —
    /// an ungated daemon fabricates nothing, the honest stale-daemon note
    /// names the fix), and the demo path opens a sim-honest EMPTY state
    /// that refuses trust actions.
    fn enter_hooks(&mut self) {
        self.dirty = true;
        if self.screen != Screen::Session {
            self.flash = Some("· /hooks — session only".to_owned());
            return;
        }
        if self.mode.fabricates_locally() {
            self.hooks.open_demo();
            // Same-key screen write (the Tools/Tree precedent): the hooks
            // screen shares the session's surface key, so no draft moves.
            self.screen = Screen::Hooks;
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_HOOKS_V1) {
            self.flash = Some(self.stale_daemon_note("hooks"));
            return;
        }
        self.hooks.open_live();
        self.screen = Screen::Hooks;
        // The cwd is CAPTURED AT ISSUANCE (the B2b capture law): the
        // listing is for the workspace this process runs in — the same
        // absolute directory `session.create` carried.
        self.requests.push(AppRequest::HooksRefresh {
            cwd: self.cwd.clone(),
        });
    }

    /// Keys on the `/hooks` screen. The confirmation card is total while
    /// open: esc cancels the CARD (session-scoped esc law — it never
    /// navigates), ⏎ dispatches; without a card the rows follow the owner
    /// menu law (arrow highlight, digits pick, ⏎ opens the card) and esc
    /// walks back to the session.
    fn handle_hooks_key(&mut self, code: KeyCode) {
        if self.hooks.confirm.is_some() {
            match code {
                KeyCode::Esc => {
                    self.hooks.confirm = None;
                    self.dirty = true;
                }
                KeyCode::Enter => self.dispatch_hook_trust(),
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Esc => {
                self.screen = Screen::Session;
                self.dirty = true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.hooks.cursor = self.hooks.cursor.saturating_sub(1);
                self.dirty = true;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let rows = self.hooks.rows.as_ref().map_or(0, Vec::len);
                self.hooks.cursor = (self.hooks.cursor + 1).min(rows.saturating_sub(1));
                self.dirty = true;
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                let rows = self.hooks.rows.as_ref().map_or(0, Vec::len);
                if index < rows {
                    self.hooks.cursor = index;
                    self.open_hook_confirm();
                    self.dirty = true;
                }
            }
            KeyCode::Enter => {
                self.open_hook_confirm();
                self.dirty = true;
            }
            _ => {}
        }
    }

    /// Open the trust/revoke confirmation card for the highlighted row.
    /// The card CAPTURES the digest (value-carrying law); a trusted row
    /// offers revoke, an untrusted or revoked-by-edit row offers trust.
    fn open_hook_confirm(&mut self) {
        if self.hooks.pending.is_some() {
            self.hooks.message =
                Some("· one trust action at a time — waiting for the daemon".to_owned());
            return;
        }
        let Some(row) = self
            .hooks
            .rows
            .as_ref()
            .and_then(|rows| rows.get(self.hooks.cursor))
        else {
            return;
        };
        self.hooks.confirm = Some(crate::hooks::TrustConfirm {
            digest: row.digest.clone(),
            name: row.name.clone(),
            grant: !row.trusted,
        });
    }

    /// ⏎ on the confirmation card: dispatch the receipted command. NOTHING
    /// is installed locally — the daemon's receipt retires the gate and a
    /// fresh `hooks.list` moves the rows (the branch discipline). Demo mode
    /// refuses honestly: trust is daemon-owned and the demo has no daemon.
    fn dispatch_hook_trust(&mut self) {
        self.dirty = true;
        let Some(confirm) = self.hooks.confirm.take() else {
            return;
        };
        if self.mode.fabricates_locally() {
            self.hooks.message =
                Some("· hook trust is live-only — the demo installs nothing".to_owned());
            return;
        }
        if self.hooks.pending.is_some() {
            self.hooks.message =
                Some("· one trust action at a time — waiting for the daemon".to_owned());
            return;
        }
        self.hooks.pending = Some(confirm.digest.clone());
        self.hooks.message = None;
        self.requests.push(AppRequest::HooksTrust {
            digest: confirm.digest,
            trusted: confirm.grant,
        });
    }

    /// Chip close lifecycle flags (§2.5) — the DRIVER owns the 5 s removal
    /// timer and the resume check; returns whether the chip WAS live
    /// (closing the last live child discharges the wait).
    pub fn close_chip_state(&mut self, agent: &str) -> Option<bool> {
        let was_live = close_chip_core(&mut self.chips, &mut self.projection, agent)?;
        // Sim closeChip (tui.js:1176-1178): the screen ALWAYS returns to the
        // session, but the remembered view path only clears when the CLOSED
        // chip is the one being viewed (`viewChipId === chipId ? null : v`).
        // TUI6.2 fix 3 (review r2 finding 3): through the switch authority
        // — this ran while the user sat in AURA (a background chip close),
        // and the direct assignment leaked the aura draft onto the
        // session surface, submittable there.
        self.switch_surface(Screen::Session);
        if self.view_path.last().is_some_and(|last| last == agent) {
            self.view_path.clear();
        }
        self.dirty = true;
        Some(was_live)
    }

    /// Keys while the viewed chip's question menu replaces its composer
    /// (§2.10): digits/arrows/enter answer; the parent is never blocked.
    fn handle_chip_menu_key(&mut self, code: KeyCode) {
        let Some(menu) = self
            .viewed_chip()
            .and_then(ChipModel::question_menu)
            .cloned()
        else {
            return;
        };
        let option_count = menu.options.len();
        match code {
            KeyCode::Up if option_count > 0 => {
                self.menu_selection =
                    (self.menu_selection.min(option_count - 1) + option_count - 1) % option_count;
            }
            KeyCode::Down if option_count > 0 => {
                self.menu_selection = (self.menu_selection + 1) % option_count;
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if index < option_count {
                    self.menu_selection = index;
                    self.answer_chip_menu(&menu);
                }
            }
            KeyCode::Enter => self.answer_chip_menu(&menu),
            _ => {}
        }
    }

    fn answer_chip_menu(&mut self, menu: &Menu) {
        let Some(option) = menu.options.get(self.menu_selection) else {
            return;
        };
        self.outbox.push(OutboundAnswer {
            origin: self.ui_generation(),
            branch: self.branch_state.active().cloned(),
            answer: MenuAnswer {
                menu: menu.id.clone(),
                option_key: Some(option.key.clone()),
                option_index: u32::try_from(self.menu_selection).unwrap_or(u32::MAX),
                value: None,
                via: AnswerVia::Tui,
            },
        });
        self.menu_selection = 0;
    }

    /// Keep the palette selection inside the visible window (sim CmdMenu
    /// scroll keep-visible, tui.js:2710-2718).
    fn scroll_palette_into_view(&mut self, count: usize) {
        self.palette_scroll = self
            .palette_scroll
            .min(count.saturating_sub(PALETTE_MAX_ROWS));
        if self.palette_selection < self.palette_scroll {
            self.palette_scroll = self.palette_selection;
        } else if self.palette_selection >= self.palette_scroll + PALETTE_MAX_ROWS {
            self.palette_scroll = self.palette_selection + 1 - PALETTE_MAX_ROWS;
        }
    }

    /// Activate one palette row — ⏎ and mouse click share this law (the
    /// click carries the VALUE, so a stale map can never run a different
    /// row). Sim acceptSuggestion (tui.js:2720-2753): a command with
    /// argument slots ENTERS its slot instead of executing; arg-less
    /// commands and argument rows execute.
    fn activate_palette_item(&mut self, item: PaletteItem) {
        match item {
            PaletteItem::Cmd(spec) if has_arg_slots(spec.name) => {
                self.composer.set_text(format!("/{} ", spec.name));
                self.palette_selection = 0;
                self.palette_scroll = 0;
                self.palette_dismissed = false;
            }
            PaletteItem::Cmd(spec) => {
                let args: String = self
                    .composer
                    .text()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(" ");
                self.composer.set_text(if args.is_empty() {
                    format!("/{}", spec.name)
                } else {
                    format!("/{} {args}", spec.name)
                });
                self.execute_slash();
            }
            PaletteItem::Arg { cmd, value, .. } => {
                self.composer.set_text(format!("/{cmd} {value}"));
                self.execute_slash();
            }
        }
    }

    fn execute_slash(&mut self) {
        let raw = self
            .composer
            .text()
            .trim_start_matches('/')
            .trim()
            .to_owned();
        // TUI5 item 6: slash executions are recallable like any submit —
        // palette-activated commands never pass `take_for_submit`, so the
        // ring is fed here with the CANONICAL form. The consecutive-dupe
        // dedupe absorbs the double record on the plain-⏎ path.
        if !raw.is_empty() {
            self.composer.record_submitted(&format!("/{raw}"));
        }
        self.composer.clear();
        self.palette_selection = 0;
        self.palette_scroll = 0;
        self.palette_dismissed = false;
        let mut words = raw.split_whitespace();
        let name = words.next().unwrap_or("").to_ascii_lowercase();
        let remainder = words.collect::<Vec<_>>().join(" ");
        let arg = remainder
            .split_whitespace()
            .next()
            .map(str::to_ascii_lowercase);
        match name.as_str() {
            "help" => self.help_open = true,
            // THE REDUCER MAY NOT OPEN A CARD IT CANNOT CLOSE (W3c3.1,
            // review D1-2). `/voice` and `/tools` mint a LOCAL `MenuOpened`
            // with no committed opening envelope, so live mode has no
            // `request_seq`/`worker_generation` to answer at:
            // `LiveDriver::answer_command` finds no coordinates, drops the
            // answer, and the card stays open FOREVER — blocking every
            // later card, including the daemon's own `request_input`. The
            // whole voice surface is local (engine names, a `/say` that
            // plays a canned turn), so live mode says so, exactly as
            // `/reset` does, rather than promising an RPC it never sends.
            "tools" if !self.mode.fabricates_locally() => {
                // W8b: the LIVE inventory is a daemon READ (research
                // §W8b-4) — no locally minted card, no fabricated
                // registration. The screen renders the committed
                // snapshot when the reply lands.
                if self.screen == Screen::Session {
                    self.tools_inventory = None;
                    self.screen = Screen::Tools;
                    self.requests.push(AppRequest::ToolsRefresh);
                } else {
                    self.flash = Some("· /tools — session only".to_owned());
                }
            }
            "voice" | "say" if !self.mode.fabricates_locally() => {
                self.flash = Some(format!(
                    "· /{name} — demo only; the live voice/tool surface lands after v0.0.12"
                ));
            }
            // T2: live dictation. Bare `/talk` toggles; `setup` opens the
            // engine/model/key card; `wave` flips the glyph style (plain
            // ASCII for fonts without partial blocks). Demo mode says so
            // honestly — the canned ◉ hold stays the demo chip's.
            "talk" => match arg.as_deref() {
                Some("setup") => self.open_talk_setup(),
                Some("wave") => {
                    self.talk.wave_plain = !self.talk.wave_plain;
                    self.flash = Some(format!(
                        "· ◉ wave style — {}",
                        if self.talk.wave_plain {
                            "plain glyphs"
                        } else {
                            "partial blocks"
                        }
                    ));
                }
                Some(other) => {
                    self.flash = Some(format!(
                        "· /talk {other} — try /talk, /talk setup, /talk wave"
                    ));
                }
                None => self.talk_toggle(),
            },
            "theme" => match arg.as_deref() {
                Some(name) => match ThemeChoice::parse(name) {
                    Some(choice) => {
                        self.commit_theme_choice(choice);
                        self.flash = Some(self.theme_flash());
                    }
                    None => {
                        self.flash = Some(format!(
                            "· unknown theme “{name}” — system · light · dark · desert · oasis"
                        ));
                    }
                },
                // Owner spec §3: bare /theme opens the numbered
                // arrow-highlight picker (supersedes the sim-era cycle
                // divergence — the picker IS the listing now).
                None => self.open_theme_picker(),
            },
            "clear" | "back" => {
                // Sim tui.js:1950-1958: /clear DETACHES (activeId = null)
                // and nothing more — the session keeps running and shows
                // as busy in its row. The /clear fresh-start promise
                // (review r1 P2) is kept by `new_session`: the next typed
                // message starts a brand-new session, never this one.
                self.back_to_launcher();
            }
            // W3c3 M3 (report R10 + §6.3's `/login` argument slots): the
            // ONE account command this release makes executable.
            "login" => {
                let mut words = remainder.split_whitespace();
                let provider = words.next().unwrap_or("");
                let method = words.next().unwrap_or("");
                let alias = words.next().map(str::to_owned);
                match (provider, method) {
                    ("", _) => {
                        self.flash = Some(
                            "· /login <provider> <oauth|api> — e.g. /login anthropic api"
                                .to_owned(),
                        );
                    }
                    (provider, "api") => self.open_login_card(provider, alias),
                    // B6b/B2b-m3: every `/login <provider> oauth` mirrors
                    // its account-add button EXACTLY by routing through the
                    // same hit arm — jump to /accounts first (the card
                    // renders and owns keys there), then the arm's feature
                    // gate and card open run unchanged (mirror by
                    // construction, never a second dispatch). The daemon
                    // owns every flow: loopback PKCE for openai/anthropic,
                    // the device-code grant for kimi.
                    ("openai", "oauth") => {
                        self.enter_accounts();
                        self.handle_hit(Hit::AccountAdd(AccountAddKind::OpenAiOAuth));
                    }
                    ("anthropic", "oauth") => {
                        self.enter_accounts();
                        self.handle_hit(Hit::AccountAdd(AccountAddKind::AnthropicOAuth));
                    }
                    ("kimi", "oauth") => {
                        self.enter_accounts();
                        self.handle_hit(Hit::AccountAdd(AccountAddKind::KimiOAuth));
                    }
                    (provider, "oauth") => {
                        self.flash = Some(format!(
                            "· /login {provider} oauth — no OAuth flow for this provider; try /login {provider} api"
                        ));
                    }
                    (provider, _) => {
                        self.flash =
                            Some(format!("· /login {provider} <oauth|api> — pick a method"));
                    }
                }
            }
            // `/reset` reseeds the DEMO world. In live mode the sessions
            // are the daemon's, so reseeding would replace real rows with
            // three fabricated ones and strand every attachment the driver
            // holds — the live stream would then be discarded as
            // `WrongSession` in silence (review P1-2). `RuntimeMode`'s
            // charter always named this as one of the three source-
            // dependent decisions; this is that branch.
            "reset" if !self.mode.fabricates_locally() => {
                self.flash = Some(
                    "· /reset — demo only; live sessions live in the daemon's store".to_owned(),
                );
            }
            "reset" => {
                // TUI5 item 9: park the departing surface first; session
                // drafts die with the reseed (the identity law — a
                // reseeded roster must not wear old drafts) and the
                // aura's dies with its reseed below. The LAUNCHER draft
                // SURVIVES — documented choice: the launcher is not an
                // identity-keyed surface, and the owner's monotonic
                // rules govern session ids only.
                self.stash_draft();
                self.fresh_session();
                // IDENTITY NEVER RECURS (W3c3.1, review P1-5). The
                // replacement seeds DRAW from the monotonic allocator —
                // they used to be minted with a hardcoded 1-3, so every
                // `/reset` reissued three dead generations and a
                // generation-keyed callback (the auto-title micro-call,
                // the answer outbox's `origin`, a `DraftKey::Session`)
                // could land on a replacement wearing its predecessor's
                // identity. Their SESSION IDS stay `demo-session-1..3`:
                // the reseeded world is the same world, and the demo
                // store's upcaster maps a legacy `id: 2` onto exactly that
                // string (report R11 cut 1 — a session id is not a
                // generation).
                self.sessions = seed_session_states(self.next_ui_generation);
                self.next_ui_generation += SEED_SESSION_COUNT;
                self.active_session = None;
                self.last_detached = None;
                // The allocator itself is deliberately NOT rewound (review
                // TUI4.1 P1-2): the control-tagged auto-title callback is
                // keyed by generation and survives /reset by design (sim:
                // a bare setTimeout). The sim's `s-${Date.now()}` ids
                // never recur — monotonicity ports that law, killing the
                // class.
                self.roster.store(
                    crate::script::ROSTER_FIRST_CLAIM,
                    std::sync::atomic::Ordering::SeqCst,
                );
                self.aura = AuraModel::seed();
                self.requests.push(AppRequest::ResetAura);
                // Sim tui.js:1918: the state file dies with the reset; the
                // seeds re-save on the next change exactly as the sim's
                // save effect refills localStorage after removeItem.
                self.demo_requests.push(DemoRequest::PurgeStore);
                // PURGE FLOW (TUI6.2 fix 3's named exception 4 of 4):
                // /reset wipes the world. The identity was already cleared
                // above, so this is a same-key screen write — and the swap
                // semantics are deliberately REPLACED by an explicit
                // purge-then-restore: every non-launcher parked draft dies
                // (the live scratch included, by overwrite), the surviving
                // launcher draft comes back. switch_surface's no-swap fast
                // path would strand that parked draft instead.
                self.screen = Screen::Launcher;
                self.drafts.retain(|key, _| *key == DraftKey::Launcher);
                self.restore_draft();
                self.flash = Some("· demo reset".to_owned());
            }
            "quit" | "exit" => self.requests.push(AppRequest::Quit),
            "aura" => self.enter_aura(),
            "tokens" => self.toggle_token_panel(),
            "tree" => {
                // B2b-m3: the tree screen (sim tui.js:3366-3430) — opens
                // at the ROOT branch, not necessarily the active one (sim
                // tui.js:1735-1741).
                if self.screen == Screen::Session {
                    self.tree_sel = 0;
                    self.tree_view = None;
                    self.screen = Screen::Tree;
                    self.dirty = true;
                } else {
                    self.flash = Some("· /tree — session only".to_owned());
                }
            }
            "branch" => self.branch_command(&remainder),
            "attach" => self.attach_command(&remainder),
            "rename" => self.rename_command(&remainder),
            "compact" => {
                // Manual compaction (sim tui.js:1791-1806). Adapted gate:
                // the sim's single-threaded state writes tolerate /compact
                // mid-turn; the envelope demo refuses honestly instead of
                // clobbering a live turn's run state.
                if !self.mode.fabricates_locally() {
                    // W7b: live /compact routes to the daemon's
                    // receipt-backed idle-only `session.compact`. The
                    // daemon's journal events drive every visible state
                    // change — `turn_active` is NEVER fabricated here
                    // (W3c3.1 r2, P1-A: that wedge parked a session
                    // forever).
                    if self.screen != Screen::Session {
                        self.flash = Some("· /compact — session only".to_owned());
                    } else if self.turn_active {
                        self.flash = Some("· /compact — wait for the turn to end".to_owned());
                    } else {
                        self.requests.push(AppRequest::Compact {
                            // Captured at issuance (B2b): compaction stays
                            // on the branch the user asked from.
                            branch: self.branch_state.active().cloned(),
                        });
                    }
                } else if self.screen != Screen::Session {
                    self.flash = Some("· /compact — session only".to_owned());
                } else if self.turn_active {
                    self.flash = Some("· /compact — wait for the turn to end".to_owned());
                } else {
                    self.turn_active = true;
                    self.requests.push(AppRequest::Compact {
                        branch: self.branch_state.active().cloned(),
                    });
                }
            }
            "queue" => {
                // Mid-turn input mode (sim tui.js:1810-1817).
                if self.screen != Screen::Session {
                    self.flash = Some("· /queue — session only".to_owned());
                } else {
                    match arg.as_deref() {
                        Some("steer") => {
                            self.queue_mode = false;
                            self.projection.push_note(
                                "· mid-turn input → STEER — delivered at the next safe boundary"
                                    .to_owned(),
                            );
                        }
                        Some("turn" | "queue") => {
                            self.queue_mode = true;
                            self.projection.push_note(
                                "· mid-turn input → QUEUE — held until the turn ends, then consumed without idling"
                                    .to_owned(),
                            );
                        }
                        _ => {
                            let mode = if self.queue_mode {
                                "queue (after turn)"
                            } else {
                                "steer (safe boundary)"
                            };
                            self.projection.push_note(format!(
                                "· mid-turn input mode is {mode} — /queue steer|turn"
                            ));
                        }
                    }
                }
            }
            "say" => {
                // Voice turn via simulated STT (sim tui.js:1865-1875).
                if self.screen != Screen::Session {
                    self.flash = Some("· /say — session only".to_owned());
                } else if !self.voice.enabled {
                    self.projection
                        .push_note("· enable voice first with /voice".to_owned());
                } else if self.turn_active {
                    // Sim-honest: the note promises a queue that never
                    // happens — ported as-is (tui.js:1868).
                    self.projection
                        .push_note("· busy — voice turn queues once idle".to_owned());
                } else if remainder.is_empty() {
                    self.projection
                        .push_note("· /say <words> — what should I hear?".to_owned());
                } else {
                    self.submit_voice(remainder);
                }
            }
            "voice" => {
                if self.screen == Screen::Session {
                    self.card_seq += 1;
                    let card = voice_card(&self.voice, self.card_seq);
                    self.projection.apply(&EventPayload::MenuOpened(card));
                } else {
                    self.flash = Some("· /voice — session only".to_owned());
                }
            }
            "tools" => {
                if self.screen == Screen::Session {
                    self.card_seq += 1;
                    self.projection
                        .apply(&EventPayload::MenuOpened(tools_card(self.card_seq)));
                } else {
                    self.flash = Some("· /tools — session only".to_owned());
                }
            }
            // `/sessions for all` is what the launcher's own header
            // promises, and with more rows than the launcher paints it is
            // the ONLY way to see — and OPEN — the rest (review P1-6: the
            // cold list the driver already tracks had no surface at all).
            //
            // DEMO keeps the honest stub: the sim implements `/sessions` as
            // a full screen with selection (tui.js:1753-1755, :3485-3492),
            // which this port has not built, and the demo world has three
            // sessions the launcher already paints. Inventing a text
            // listing there would be a divergence from the sim for no gain
            // (W3c3.1 r2, P3-H).
            "sessions" if !self.mode.fabricates_locally() => {
                if remainder.is_empty() {
                    self.list_sessions();
                } else {
                    self.open_listed_session(&remainder);
                }
            }
            "sessions" => {
                // The demo stub must stay a KNOWN command: without this arm
                // it fell to the typo catch-all, which called a command
                // `/help` itself lists "unknown" (W3c3.1 r2 completion —
                // the first cut of P3-H removed the listing arm without
                // re-homing the stub).
                self.flash = Some(
                    "· /sessions — demo stub; the sim's sessions screen is unbuilt \
                     (live mode lists and opens)"
                        .to_owned(),
                );
            }
            "accounts" => self.enter_accounts(),
            "providers" => self.enter_providers(),
            "hooks" => self.enter_hooks(),
            // U2: `/usage [provider]` — the cross-provider usage report;
            // the optional first token is a provider prefix filter.
            "usage" => self.enter_usage(arg.as_deref()),
            // W5e-3: choose from the DISCOVERED catalog. Both are
            // feature-gated BEFORE shipping this time (the W5e-1b lesson).
            "model" => {
                // F2a: `/model [query]` opens the FULL-SCREEN picker —
                // one row per model × provider pair across every enabled
                // provider, query pre-filled. An empty registry keeps the
                // honest flash (stale daemon named when undiscoverable).
                let requested = remainder.trim().to_owned();
                if self.providers.providers.is_empty() {
                    self.flash = Some(
                        if self.daemon_serves(haider_rpc::FEATURE_PROVIDER_MODELS_V1) {
                            "· no models discovered yet — /providers then refresh".to_owned()
                        } else {
                            self.stale_daemon_note("model discovery")
                        },
                    );
                    self.requests.push(AppRequest::ProvidersRefresh);
                } else {
                    self.open_model_picker(requested);
                }
            }
            "provider" => {
                let requested = remainder.trim().to_owned();
                let slots = self.dynamic_slots();
                if slots.providers.is_empty() {
                    self.flash = Some(self.stale_daemon_note("provider listing"));
                } else if requested.is_empty() {
                    let names: Vec<&str> = slots
                        .providers
                        .iter()
                        .map(|(name, _)| name.as_str())
                        .collect();
                    self.flash = Some(format!("· providers — {}", names.join(" · ")));
                } else if let Some((name, health)) = slots
                    .providers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(&requested))
                {
                    self.identity.provider = name.clone();
                    self.refresh_context_window();
                    self.identity_pinned = true;
                    self.flash = Some(format!("· provider → {name} · {health}"));
                } else {
                    self.flash = Some(format!("· no provider \"{requested}\" in the registry"));
                }
            }
            // G3: `/effort [level|default]` — with an argument commits the
            // receipted selection; bare opens the ladder picker.
            "effort" => {
                let requested = remainder.trim().to_owned();
                self.effort_command(if requested.is_empty() {
                    None
                } else {
                    Some(requested)
                });
            }
            // G3: `/fast` toggles fast mode on the current pair.
            "fast" => self.fast_command(),
            "account" => {
                // Sim tui.js:1770-1780: no alias → note listing them; a
                // known alias selects (same daemon-gated path as a click);
                // an unknown alias says so.
                let alias = remainder.trim().to_owned();
                if alias.is_empty() {
                    if self.accounts.rows.is_empty() {
                        self.enter_accounts();
                    } else {
                        let names: Vec<&str> = self
                            .accounts
                            .rows
                            .iter()
                            .map(|row| row.alias.as_str())
                            .collect();
                        self.flash = Some(format!("· accounts — {}", names.join(" · ")));
                    }
                } else {
                    self.enter_accounts();
                    self.select_account(&alias);
                }
            }
            "" => {}
            other => {
                // Known stubs name their wave; typos say so (review r1 P2).
                let wave = match other {
                    "fork" => Some("the daemon wave (W3)"),
                    "peers" => Some("the mesh wave (post-v0.1)"),
                    "update" => Some("the gates wave (W4)"),
                    _ => None,
                };
                self.flash = Some(match wave {
                    Some(wave) => format!("· /{other} — UI ready; lands with {wave}"),
                    None => format!("· unknown command /{other} — /help lists commands"),
                });
            }
        }
    }

    fn handle_menu_key(&mut self, code: KeyCode) {
        let Some(menu) = self.projection.open_menu() else {
            return;
        };
        let option_count = menu.options.len();
        // OWNER DIRECTIVE (supersedes the sim's swallow law): esc on a
        // blocking card INTERRUPTS the run — the daemon's cancellation
        // closes the menu and the committed note lands in the transcript.
        // Non-blocking command cards (/voice, /tools) still just dismiss.
        if code == KeyCode::Esc {
            if !menu.blocking {
                let id = menu.id.clone();
                self.projection.apply(&EventPayload::MenuClosed {
                    menu: id,
                    reason: MenuCloseReason::Dismissed,
                });
                return;
            }
            self.turn_active = false;
            self.listening = false;
            self.msg_queue.clear();
            self.requests.push(AppRequest::Interrupt {
                branch: self.branch_state.active().cloned(),
            });
            if self.mode.fabricates_locally() {
                let id = menu.id.clone();
                self.projection.apply(&EventPayload::MenuClosed {
                    menu: id,
                    reason: MenuCloseReason::Dismissed,
                });
                self.projection
                    .apply(&EventPayload::RunState(RunState::Cancelled));
                self.projection
                    .push_note("· interrupted — menu cancelled · idle (i)".to_owned());
            }
            self.dirty = true;
            return;
        }
        match code {
            // Selection wraps around (sim, tui.js:2441-2449).
            KeyCode::Up if option_count > 0 => {
                self.menu_selection =
                    (self.menu_selection.min(option_count - 1) + option_count - 1) % option_count;
            }
            KeyCode::Down if option_count > 0 => {
                self.menu_selection = (self.menu_selection + 1) % option_count;
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if index < option_count {
                    self.menu_selection = index;
                    self.submit_menu_answer();
                }
            }
            KeyCode::Enter => self.submit_menu_answer(),
            _ => {}
        }
    }

    fn submit_menu_answer(&mut self) {
        let Some(menu) = self.projection.open_menu() else {
            return;
        };
        let Some(option) = menu.options.get(self.menu_selection) else {
            return;
        };
        // The `/branch` picker is REDUCER-LOCAL (B2b m2): its consequence
        // is a display switch, so the reducer closes its own card and
        // nothing rides the outbox — the D1-2 closable-card law holds in
        // both modes.
        if menu.id.as_str().starts_with(BRANCH_CARD_PREFIX) {
            let menu_id = menu.id.clone();
            let key = option.key.clone();
            self.projection.apply(&EventPayload::MenuClosed {
                menu: menu_id,
                reason: MenuCloseReason::Dismissed,
            });
            self.menu_selection = 0;
            let target = (key != "main").then(|| haider_protocol::ids::BranchId::new(key));
            match self.switch_branch(target.as_ref()) {
                Some(display) => self.flash = Some(format!("· branch → {display}")),
                None => {
                    self.flash = Some(format!("· already on {}", self.active_branch_name()));
                }
            }
            self.dirty = true;
            return;
        }
        let answer = MenuAnswer {
            menu: menu.id.clone(),
            option_key: Some(option.key.clone()),
            option_index: u32::try_from(self.menu_selection).unwrap_or(u32::MAX),
            value: None,
            via: AnswerVia::Tui,
        };
        self.outbox.push(OutboundAnswer {
            origin: self.ui_generation(),
            branch: self.branch_state.active().cloned(),
            answer,
        });
        self.menu_selection = 0;
    }

    /// Route one RAW envelope to whichever session owns it (W3c3, report
    /// R11 cut 2) — the single live entry point for the event stream.
    ///
    /// The attached session reduces through the model's checked-out live
    /// fields; every other session reduces in its own slot. A gap STOPS
    /// reduction with the cursor unmoved and emits
    /// [`AppRequest::Reattach`] BEFORE any later state can mutate — the
    /// store is the lag buffer (R9), so the client never papers over a
    /// hole. An envelope for a session this model does not know is
    /// rejected, not invented.
    pub fn route_raw(&mut self, envelope: &RawEnvelope) -> crate::projection::RawOutcome {
        use crate::projection::RawOutcome;
        let outcome = if self.active_session.as_ref() == Some(&envelope.session_id) {
            self.absorb_raw_active(envelope)
        } else if let Some(entry) = self
            .sessions
            .iter_mut()
            .find(|entry| entry.id == envelope.session_id)
        {
            entry.absorb_raw(envelope)
        } else {
            RawOutcome::WrongSession
        };
        match outcome {
            RawOutcome::Applied => {
                self.dirty = true;
                // S4: applied journal truth advances the render clock —
                // the first paint after a spawn reads a clock already
                // inside the journal's own time base, tick or no tick.
                self.clock_ms = self.clock_ms.max(envelope.committed_at_ms);
            }
            RawOutcome::Gap { after_seq } => self.requests.push(AppRequest::Reattach {
                session: envelope.session_id.clone(),
                after_seq,
            }),
            RawOutcome::Duplicate | RawOutcome::WrongSession => {}
        }
        outcome
    }

    /// The ATTACHED session's half of [`Self::route_raw`]: the cursor lives
    /// on the checked-out projection, so it travels with checkout/checkin
    /// and no second cursor authority exists.
    ///
    /// B2b: the branch command-state hooks (fork-coordinate tracker, branch
    /// registry) run for `Apply` AND `Skip` — they are command/topology
    /// coordinates, not display state, so a `render.ui == false` envelope
    /// records them while the display-never-mutates law keeps holding for
    /// every display surface (both halves are pinned). Content then routes
    /// type-first (aggregates session-global), then branch, then agent.
    fn absorb_raw_active(&mut self, envelope: &RawEnvelope) -> crate::projection::RawOutcome {
        use crate::projection::{Admission, RawOutcome};
        match self.projection.admit(envelope) {
            Admission::Duplicate => RawOutcome::Duplicate,
            Admission::Gap { after_seq } => RawOutcome::Gap { after_seq },
            admission @ (Admission::Skip | Admission::Apply) => {
                let note = self.branch_state.note_admitted(envelope);
                // H4: hook-engine facts are `render.ui == false` journal
                // truth — recorded for Apply AND Skip like the branch
                // registry (they surface only on `/hooks` and the decision
                // chip; every transcript surface keeps the display gate).
                self.hook_facts.note_envelope(envelope);
                if let crate::branch::AdmittedNote::BranchInstalled(id) = &note {
                    // The daemon's journal fact is the ONLY materializer;
                    // if OUR fork's receipt already armed activation, the
                    // install is the moment it takes effect.
                    let id = id.clone();
                    if self.branch_state.take_pending_activation(&id) {
                        let name = self.switch_branch(Some(&id));
                        if let Some(name) = name {
                            self.flash = Some(format!("· forked → {name}"));
                        }
                        // The tree's `f` completing returns to the session
                        // (sim forkAtNode → setScreen("session")); any
                        // other surface is never hijacked.
                        if self.screen == Screen::Tree {
                            self.screen = Screen::Session;
                        }
                    }
                }
                if matches!(note, crate::branch::AdmittedNote::Content)
                    && admission == Admission::Apply
                {
                    match serde_json::from_value::<EventPayload>(envelope.payload.clone()) {
                        Ok(payload) => self.route_admitted(
                            &payload,
                            envelope.branch_id.as_ref(),
                            envelope.agent_id.as_ref(),
                            envelope.committed_at_ms,
                        ),
                        // S3: the additive agent-event union rides raw
                        // envelopes OUTSIDE `EventPayload` — try it before
                        // counting the payload unknown (both twins).
                        Err(_) => {
                            if !crate::session::route_agent_event(
                                &mut self.branch_state,
                                &mut self.projection,
                                &self.chips,
                                envelope,
                            ) {
                                self.projection.count_unknown_payload();
                            }
                        }
                    }
                }
                RawOutcome::Applied
            }
        }
    }

    /// Route one admitted content payload for the ATTACHED session:
    /// aggregate session-scope types FIRST (risk 6 — even off a
    /// branch-stamped envelope they land session-global), then branch
    /// (outer), then agent. `SessionState::route_admitted` is the
    /// background twin.
    fn route_admitted(
        &mut self,
        payload: &EventPayload,
        branch: Option<&haider_protocol::ids::BranchId>,
        agent: Option<&haider_protocol::ids::AgentId>,
        at_ms: u64,
    ) {
        use crate::branch::BranchScope;
        match self.branch_state.scope_of(payload, branch) {
            BranchScope::Aggregate => {
                self.branch_state.apply_aggregate_to_parked(payload);
                // Type-first: straight to the session reducer — an
                // aggregate never lands in a chip transcript.
                self.handle_envelope(payload);
            }
            BranchScope::Active => self.absorb_scoped(payload, agent, at_ms),
            BranchScope::ParkedMain => {
                self.note_parked_turn(payload);
                if let Some(view) = self.branch_state.parked_main_mut() {
                    crate::branch::absorb_into_view(view, payload, agent, at_ms);
                }
            }
            BranchScope::ParkedNamed(id) => {
                self.note_parked_turn(payload);
                if let Some(view) = self.branch_state.view_mut(&id) {
                    crate::branch::absorb_into_view(view, payload, agent, at_ms);
                }
            }
            BranchScope::Orphan => self.branch_state.count_orphan(),
        }
    }

    /// Session-wide run bookkeeping for INACTIVE-branch content (run
    /// execution stays session-wide, research Q2): the busy state flips
    /// here, the DISPLAY stays in the owning branch's warm view — no
    /// screen flip, no menu reset, nothing painted.
    fn note_parked_turn(&mut self, payload: &EventPayload) {
        if let EventPayload::UserMessage { .. } = payload {
            self.turn_active = true;
        }
        if let EventPayload::RunState(state) = payload
            && state.is_terminal()
        {
            self.turn_active = false;
            self.auto_resuming = false;
        }
    }

    /// Switch the ATTACHED session's displayed branch — the ONE atomic
    /// switch authority (risk 8): transcript, tokens/footprint, menus,
    /// todos and chips swap as a unit, the session cursor transplants onto
    /// the incoming projection, and the view chrome (scroll, menu
    /// selection, subagent path) resets exactly as a session attach does.
    /// Returns the displayed name on an actual switch (`"main"` for the
    /// main branch), `None` when the target was unknown or already active.
    pub fn switch_branch(
        &mut self,
        target: Option<&haider_protocol::ids::BranchId>,
    ) -> Option<String> {
        match self
            .branch_state
            .switch(target, &mut self.projection, &mut self.chips)
        {
            crate::branch::SwitchOutcome::Switched => {
                self.menu_selection = 0;
                self.view_path.clear();
                self.scroll_back.set(0);
                self.scroll_max.set(0);
                self.sticky_suppressed.set(false);
                // A switch invalidates any armed jump anchor — the tree's
                // node activation re-arms AFTER its own switch, so only a
                // LATER switch can kill it (never the one that set it up).
                *self.pending_jump.borrow_mut() = None;
                self.dirty = true;
                Some(self.branch_state.active_name().unwrap_or("main").to_owned())
            }
            crate::branch::SwitchOutcome::AlreadyActive
            | crate::branch::SwitchOutcome::UnknownBranch => None,
        }
    }

    /// The status bar's branch segment: the active branch's name, `"main"`
    /// on the main branch.
    #[must_use]
    pub fn active_branch_name(&self) -> &str {
        self.branch_state.active_name().unwrap_or("main")
    }

    /// The projection displaying `target`'s transcript, wherever it is
    /// checked out (B2b-m3): the live fields for the ACTIVE branch, the
    /// parked main slot, or a named branch's warm view. Read-only — the
    /// tree screen reads every branch without disturbing any.
    #[must_use]
    pub fn branch_projection(
        &self,
        target: Option<&haider_protocol::ids::BranchId>,
    ) -> Option<&SessionProjection> {
        if self.branch_state.active() == target {
            return Some(&self.projection);
        }
        match target {
            None => self.branch_state.parked_main().map(|view| &view.projection),
            Some(id) => self.branch_state.view(id).map(|view| &view.projection),
        }
    }

    /// The ATTACHED session's `busy()` twin (sim `sessionBusy` — the same
    /// derivation `SessionState::busy` uses for background rows): live
    /// subagents on ANY branch, a mid-turn engine, or an unsettled run.
    #[must_use]
    pub fn session_busy(&self) -> bool {
        tree_live_count(&self.chips) + self.branch_state.parked_live() > 0
            || self.turn_active
            || !self.projection.settled()
    }

    /// `/branch` (B2b m2): bare opens the numbered picker, `new [name]`
    /// forks at the active branch's last committed node, anything else
    /// switches to the named branch. Owner menu/esc laws bind: the picker
    /// is a numbered arrow-highlight card, esc is session-scoped, and
    /// every refusal is an honest notice.
    fn branch_command(&mut self, remainder: &str) {
        if self.screen != Screen::Session {
            self.flash = Some("· /branch — session only".to_owned());
            self.dirty = true;
            return;
        }
        // Feature gate (brief item 5): without the advertised branch
        // feature nothing is fabricated — the honest stale-daemon notice
        // names the fix. Demo mode answers everything locally and passes.
        if !self.daemon_serves(haider_rpc::FEATURE_BRANCH_CREATE_V1) {
            self.flash = Some(self.stale_daemon_note("branches"));
            self.dirty = true;
            return;
        }
        let request = remainder.trim();
        match request.split_whitespace().next() {
            None => self.open_branch_picker(),
            Some("new") => {
                let name = request.strip_prefix("new").unwrap_or("").trim();
                self.branch_new((!name.is_empty()).then(|| name.to_owned()));
            }
            Some(_) => self.branch_switch_by_name(request),
        }
    }

    /// The `/branch` picker — a numbered non-blocking card (arrow
    /// highlight, digits/⏎ activate, esc dismisses — session-scoped esc
    /// law). Its answer is REDUCER-LOCAL: switching a displayed branch is
    /// display state, so the card closes itself and nothing rides the
    /// outbox (the D1-2 law holds — this reducer can close every card it
    /// opens).
    fn open_branch_picker(&mut self) {
        if self.projection.open_menu().is_some() {
            self.flash = Some("· /branch — answer the open card first".to_owned());
            self.dirty = true;
            return;
        }
        self.card_seq += 1;
        let card = branch_card(&self.branch_state, self.card_seq);
        self.menu_selection = 0;
        self.projection.apply(&EventPayload::MenuOpened(card));
        self.dirty = true;
    }

    /// `/branch new [name]` — fork issuance at the active branch's last
    /// committed node (the tracker's EXACT captured coordinates).
    fn branch_new(&mut self, name: Option<String>) {
        // Branches are daemon truth; the demo has no daemon to keep them —
        // refused BEFORE the tracker read so the demo's answer names the
        // real gap, not a missing coordinate (`issue_fork` re-checks for
        // its other caller).
        if self.mode.fabricates_locally() {
            self.flash = Some("· /branch new — live only; branches are daemon-owned".to_owned());
            self.dirty = true;
            return;
        }
        let Some((fork_node_id, fork_seq)) = self.branch_state.fork_point().cloned() else {
            self.flash = Some("· /branch new — nothing committed to fork from yet".to_owned());
            self.dirty = true;
            return;
        };
        let source_branch = self.branch_state.active().cloned();
        self.issue_fork("/branch new", source_branch, fork_node_id, fork_seq, name);
    }

    /// Fork issuance at EXACT coordinates — the ONE gate/dispatch shared by
    /// `/branch new` (tracker coordinates) and the tree's `f` (the selected
    /// row's `{node_id, node_seq}`). `what` names the asking surface in
    /// each honest refusal. Nothing local is fabricated: the daemon's
    /// `BranchCreated` fact is the only materializer.
    fn issue_fork(
        &mut self,
        what: &str,
        source_branch: Option<haider_protocol::ids::BranchId>,
        fork_node_id: haider_protocol::ids::NodeId,
        fork_seq: u64,
        name: Option<String>,
    ) {
        self.dirty = true;
        // Branches are daemon truth; the demo has no daemon to keep them.
        if self.mode.fabricates_locally() {
            self.flash = Some(format!("· {what} — live only; branches are daemon-owned"));
            return;
        }
        // Feature gate (brief item 5): without the advertised branch
        // feature nothing is fabricated — the honest stale-daemon notice
        // names the fix. (`/branch` gates its whole grammar upstream; the
        // tree's `f` reaches here directly, so the gate must live at the
        // dispatch too.)
        if !self.daemon_serves(haider_rpc::FEATURE_BRANCH_CREATE_V1) {
            self.flash = Some(self.stale_daemon_note("branches"));
            return;
        }
        let Some(session) = self.active_session.clone() else {
            self.flash = Some(format!("· {what} — no live session attached"));
            return;
        };
        // The busy() gate (brief item 5): forking a live turn would split
        // ownership of its open menus/tools/children.
        if self.session_busy() {
            self.flash = Some(format!("· {what} — wait for the turn to end"));
            return;
        }
        self.requests.push(AppRequest::BranchCreate {
            session,
            source_branch,
            fork_node_id,
            fork_seq,
            name,
        });
        // Honest: no local branch appears — the daemon's BranchCreated
        // fact is what installs and activates it.
        self.flash = Some("· forking — the branch lands when the daemon commits it".to_owned());
    }

    /// Activate one `/tree` row (⏎, or a click validated against the
    /// FRESH rows — sim tui.js:2581-2591): a FORK marker drills into its
    /// branch; a BRANCH header switches the session to it (the same atomic
    /// [`Self::switch_branch`] swap every switch takes) and returns at its
    /// normal tail; a NODE row switches and ARMS the render-resolved jump
    /// onto that node's transcript entry.
    fn activate_tree_row(&mut self, row: TreeRow) {
        match row {
            TreeRow::Fork { branch, .. } => {
                if self.branch_state.contains(&branch) {
                    self.tree_view = Some(branch);
                    self.tree_sel = 0;
                    self.dirty = true;
                }
            }
            TreeRow::Branch { branch, .. } => {
                self.switch_branch(branch.as_ref());
                self.screen = Screen::Session;
                self.dirty = true;
            }
            TreeRow::Node { branch, coords, .. } => {
                self.switch_branch(branch.as_ref());
                // Arm AFTER the switch (which clears any stale anchor):
                // the next session frame resolves node → entry → wrapped
                // row with ITS OWN width/prefix sums (research §Q3 — the
                // sim's Enter never scrolled; production lands the
                // promised jump). A coordinate-free demo row returns at
                // the tail: without a durable node identity there is
                // nothing honest to anchor.
                if let Some((node, _)) = coords {
                    *self.pending_jump.borrow_mut() = Some(PendingJump { branch, node });
                }
                self.screen = Screen::Session;
                self.dirty = true;
            }
        }
    }

    /// `f` on the selected `/tree` row (sim tui.js:2593-2596): fork at the
    /// row's EXACT committed coordinates — the VIEWED branch is the source,
    /// the row's `{node_id, node_seq}` the fork point. Non-node rows do
    /// nothing (sim parity); a node row without durable coordinates
    /// refuses honestly instead of guessing one.
    fn tree_fork_selected(&mut self) {
        let Some(row) = tree_rows(self).get(self.tree_sel).cloned() else {
            return;
        };
        let TreeRow::Node { branch, coords, .. } = row else {
            return;
        };
        // The demo answer names the REAL gap (branches are daemon-owned)
        // before the missing-coordinate one — demo rows never carry
        // coordinates, so the order decides which truth the user hears.
        if self.mode.fabricates_locally() {
            self.flash = Some("· fork — live only; branches are daemon-owned".to_owned());
            self.dirty = true;
            return;
        }
        let Some((node, seq)) = coords else {
            self.flash = Some("· fork — this row carries no committed node coordinates".to_owned());
            self.dirty = true;
            return;
        };
        self.issue_fork("fork", branch, node, seq, None);
    }

    /// `/branch <name>` — direct switch (reading another branch is always
    /// allowed, running turn or not).
    fn branch_switch_by_name(&mut self, name: &str) {
        let target = if name == "main" {
            None
        } else if let Some(descriptor) = self.branch_state.find_by_name(name) {
            Some(descriptor.branch_id.clone())
        } else {
            let names: Vec<&str> = std::iter::once("main")
                .chain(self.branch_state.descriptors().map(|d| d.name.as_str()))
                .collect();
            self.flash = Some(format!(
                "· no branch named “{name}” — {}",
                names.join(" · ")
            ));
            self.dirty = true;
            return;
        };
        match self.switch_branch(target.as_ref()) {
            Some(display) => self.flash = Some(format!("· branch → {display}")),
            None => self.flash = Some(format!("· already on {}", self.active_branch_name())),
        }
        self.dirty = true;
    }

    /// The attached path's scope router — the same [`crate::session::classify`]
    /// decision the background path takes, applied to the live fields.
    fn absorb_scoped(
        &mut self,
        payload: &EventPayload,
        agent: Option<&haider_protocol::ids::AgentId>,
        at_ms: u64,
    ) {
        use crate::session::Destination;
        match crate::session::classify(&mut self.projection, &self.chips, payload, agent) {
            Destination::Agent => {
                crate::session::apply_agent_payload(&mut self.chips, payload, at_ms);
            }
            Destination::Chip(target) => {
                if let Some(chip) = find_chip_mut(&mut self.chips, &target) {
                    crate::session::chip_apply(chip, payload, at_ms);
                }
            }
            Destination::Session => self.handle_envelope(payload),
        }
    }

    fn handle_envelope(&mut self, payload: &EventPayload) {
        // Screen auto-transitions (sim: boot → launcher when startup
        // completes; the first user message attaches the session view).
        if matches!(payload, EventPayload::HarnessStatus(HarnessStatus::Ready))
            && self.screen == Screen::Boot
        {
            self.switch_surface(Screen::Launcher);
        }
        if let EventPayload::UserMessage { .. } = payload {
            self.goto_session_screen();
            self.turn_active = true;
            // NB: no titling here. The sim names a session ONLY inside the
            // 1.5 s micro-call callback (tui.js:1219-1227); titling on the
            // user-row envelope pre-empted that callback, so its note never
            // landed (review P2-12).
        }
        if let EventPayload::RunState(state) = payload
            && state.is_terminal()
        {
            self.turn_active = false;
            self.auto_resuming = false;
            // The `♪ speaking` tag ends where the TURN ends. A trailing
            // `Voice(false)` beat could not: a branch parked on a menu
            // never reaches its own tail, so later ordinary rows kept
            // rendering as spoken (review P2-10).
            self.projection.set_voice_live(false);
        }
        if matches!(payload, EventPayload::MenuOpened(_)) {
            self.menu_selection = 0;
        }
        self.projection.apply(payload);
        // Chip questions are Subagent-scoped menus living in the CHIP's
        // projection — an answer closes the matching chip card too.
        if matches!(payload, EventPayload::MenuAnswered(_)) {
            fn route(chips: &mut [ChipModel], payload: &EventPayload) {
                for chip in chips {
                    chip.transcript.apply(payload);
                    route(&mut chip.children, payload);
                }
            }
            route(&mut self.chips, payload);
        }
        // Command-card consequences (sim /voice + /tools, tui.js:1824-1906)
        // apply AFTER the answer closed the card.
        if let EventPayload::MenuAnswered(answer) = payload {
            let index = usize::try_from(answer.option_index).unwrap_or(usize::MAX);
            let id = answer.menu.as_str();
            if id.starts_with(VOICE_CARD_PREFIX) {
                self.voice_card_answered(index);
            } else if id.starts_with(TOOLS_CARD_PREFIX) {
                self.tools_card_answered(index);
            }
        }
    }

    /// `/voice` card consequences (sim tui.js:1824-1864).
    fn voice_card_answered(&mut self, index: usize) {
        match index {
            0..=2 => {
                let (stt, tts, duplex) = match index {
                    0 => ("whisper-large-v3", "openai-tts", false),
                    1 => ("deepgram-nova-3", "elevenlabs", false),
                    _ => ("gpt-realtime", "gpt-realtime", true),
                };
                self.voice = VoiceState {
                    enabled: true,
                    stt: stt.to_owned(),
                    tts: tts.to_owned(),
                    duplex,
                };
                let pipeline = if duplex {
                    "gpt-realtime native duplex".to_owned()
                } else {
                    format!("{stt} → {tts}")
                };
                self.projection.push_note(format!(
                    "· voice enabled · {pipeline} · hold-to-talk under the input, or /say <words>"
                ));
            }
            3 => {
                if self.voice.enabled {
                    self.voice.enabled = false;
                    self.projection.push_note("· voice disabled".to_owned());
                } else {
                    self.projection.push_note("· voice stays off".to_owned());
                }
            }
            _ => {}
        }
    }

    /// `/tools` card consequences (sim tui.js:1876-1906).
    fn tools_card_answered(&mut self, index: usize) {
        const MODES: [&str; 3] = [
            "fire-and-forget — the turn continues the instant it dispatches",
            "await — the turn parks in TOOL_RUNNING until the result returns",
            "deferred — returns a ticket, the session waits in WAITING(dependency) for the callback",
        ];
        match index {
            0..=2 => self.projection.push_note(format!(
                "· custom tool registered · dispatch = {}",
                MODES[index]
            )),
            3 => self.projection.push_note("· tools card closed".to_owned()),
            _ => {}
        }
    }

    /// Start-fresh semantics (review r1 P2): a new session begins from an
    /// empty projection; the previous demo transcript does not leak in —
    /// including its scroll ceiling and any pending timers
    /// (ResetAllSessions cancels the Session and Chip ARMS — Aura
    /// deliberately survives, see `ArmOwner` — so a stale idle-decay or
    /// script beat from the OLD session drops at consumption).
    fn fresh_session(&mut self) {
        // Answers and micro-calls born under the old surface are now
        // stale: any that never left the outbox are dropped outright
        // (review r2 P1-1); in-flight ones fail the driver's
        // [`Self::session_identity`] gate — the reset surface has no
        // session, so their by-id origin can never match.
        self.outbox.clear();
        self.projection = SessionProjection::new();
        self.branch_state = crate::branch::BranchState::default();
        self.hook_facts = crate::hooks::HookFactsLog::default();
        self.session_title = None;
        self.session_name = None;
        self.turn_active = false;
        self.msg_queue.clear();
        self.queue_mode = false;
        self.voice = VoiceState::default();
        self.listening = false;
        self.session_dir = self.launcher_dir.clone();
        self.chips.clear();
        self.view_path.clear();
        self.subtree_collapsed = false;
        self.todos_collapsed = false;
        self.auto_resuming = false;
        self.scroll_back.set(0);
        self.scroll_max.set(0);
        self.sticky_suppressed.set(false);
        *self.pending_jump.borrow_mut() = None;
        self.tree_view = None;
        self.requests.push(AppRequest::ResetAllSessions);
    }

    /// Refuse a DEMO-ONLY surface in live mode, honestly and in one voice
    /// (W3c3.1 r2, P1-A).
    ///
    /// The rule this enforces is [`RuntimeMode::fabricates_locally`]'s:
    /// live mode must not mint local state the daemon will never resolve.
    /// The first pass of this fix swept only the three commands the review
    /// named, and `/compact` kept the class alive — it set `turn_active`
    /// and handed `AppRequest::Compact` to a driver that discarded it, so
    /// the session sat mid-turn forever with `/compact` itself answering
    /// "wait for the turn to end".
    ///
    /// Refusing HERE, not in the driver, is the whole point: nothing local
    /// is fabricated, so nothing has to be undone.
    fn refuse_demo_only(&mut self, what: &str) {
        self.flash = Some(format!(
            "· {what} — demo only; no daemon behavior stands behind it yet"
        ));
        self.dirty = true;
    }

    /// `/sessions` — EVERY session the model knows, not just the rows the
    /// launcher has room to paint (review P1-6).
    ///
    /// The listing is a read of state already held: `session.list` has
    /// already populated the row for every session the daemon has, hot or
    /// cold. Each line names the row's digit when it has one, its id, its
    /// status and its head — the id because that is the coordinate, and
    /// the digit because that is how the user reaches it.
    fn list_sessions(&mut self) {
        let rows = self.launcher_rows();
        let lines: Vec<String> = if self.sessions.is_empty() {
            vec!["no sessions yet — type to start one".to_owned()]
        } else {
            self.sessions
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    // The number is the ROW's coordinate either way: a
                    // digit for the rows the launcher paints, and the
                    // `/sessions <n>` argument for the rest.
                    let reach = if index < rows {
                        format!("{:>2}", index + 1)
                    } else {
                        format!("/{}", index + 1)
                    };
                    let status = if entry.busy() {
                        "running"
                    } else if entry.errored() {
                        "errored"
                    } else {
                        "idle"
                    };
                    let name = entry.name.as_deref().unwrap_or("—");
                    format!(
                        "{reach}  {}  {name}  {status}  {} turns",
                        entry.id.as_str(),
                        entry.turns()
                    )
                })
                .collect()
        };
        let out = lines.join("\n");
        if self.screen == Screen::Session {
            self.projection.push_shell("sessions".to_owned(), out);
        } else {
            self.launcher_shellout = Some(("sessions".to_owned(), out));
        }
        self.dirty = true;
    }

    /// `/sessions <n|id>` — open ANY listed session, including the ones
    /// past the launcher's painted rows (W3c3.1 r2, P2-D).
    ///
    /// Without this, R11's "cold sessions … listable and READABLE" held
    /// only for the first nine: rows past the digit span had no hit target
    /// and no key, so raising the bound from three to nine moved the
    /// defect rather than closing it. The read itself is the attach's own
    /// replay, so opening is all that was missing.
    fn open_listed_session(&mut self, arg: &str) {
        let by_ordinal = arg
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .and_then(|n| self.sessions.get(n - 1))
            .map(|entry| entry.id.clone());
        let target = by_ordinal.or_else(|| {
            self.sessions
                .iter()
                .find(|entry| entry.id.as_str() == arg)
                .map(|entry| entry.id.clone())
        });
        match target {
            Some(id) => self.open_session(&id),
            None => {
                self.flash = Some(format!(
                    "· /sessions {arg} — no such row; /sessions lists them by number and id"
                ));
                self.dirty = true;
            }
        }
    }

    /// How many session rows the launcher shows — and therefore how many
    /// are clickable and digit-bindable.
    ///
    /// DEMO keeps the sim's three (`tui.js:3246 slice(0, 3)`): the sim is
    /// read-only law for demo behavior and its world only ever has three.
    /// LIVE shows the full digit span, because the daemon's list is
    /// whatever the user has actually got — with three rows, session four
    /// onward was listed by `session.list`, held cold by the driver, and
    /// reachable by nothing at all (review P1-6). `/sessions` lists the
    /// remainder for the rare profile with more than nine.
    #[must_use]
    pub const fn launcher_rows(&self) -> usize {
        match self.mode {
            RuntimeMode::Demo => SEED_SESSION_COUNT as usize,
            RuntimeMode::Live => LIVE_LAUNCHER_ROWS,
        }
    }

    /// Attach the session a launcher row was rendered FOR (the clicked
    /// row's identity, P2-9). A row the model no longer holds — the frame's
    /// hit map may be one frame stale — activates nothing.
    fn attach_session_id(&mut self, id: &SessionId) {
        if self.sessions.iter().any(|entry| &entry.id == id) {
            self.open_session(id);
        }
    }

    /// Attach the launcher's nth row (digit binding). TUI4c: switching is
    /// FREE — the sim's `openSession` never blocks on a running turn
    /// (tui.js:1606: "attaching never cancels a turn"); the old
    /// one-turn-at-a-time flash guarded a single shared projection that no
    /// longer exists.
    fn attach_sample(&mut self, index: usize) {
        if let Some(id) = self.sessions.get(index).map(|entry| entry.id.clone()) {
            self.open_session(&id);
        }
    }

    /// Sim `openSession` (tui.js:1606-1615): sweep closed chips whose 5 s
    /// removal never fired, attach, and NOTHING else — no turn starts
    /// (owner item 1), and the one left behind keeps running.
    pub fn open_session(&mut self, id: &SessionId) {
        if self.active_session.as_ref() == Some(id) {
            self.switch_surface(Screen::Session);
            return;
        }
        // TUI5 item 9: park the departing surface's draft BEFORE identity
        // flips (checkin() itself is draft-free — exactly one stash and
        // one restore per transition).
        self.stash_draft();
        self.checkin();
        let Some(index) = self.sessions.iter().position(|entry| &entry.id == id) else {
            // Unknown id: the checkin left us on the no-session surface —
            // bring ITS (the launcher's) draft live, stranding nothing.
            self.restore_draft();
            return;
        };
        // Move the slot out so its fields can swap with `self`'s without
        // aliasing; the slot keeps a neutral placeholder meanwhile.
        let ui_gen = self.sessions[index].ui_gen;
        let mut slot = std::mem::replace(
            &mut self.sessions[index],
            crate::session::SessionState::neutral(id.clone(), ui_gen),
        );
        crate::session::sweep_closed_chips(&mut slot.chips);
        self.projection = std::mem::replace(&mut slot.projection, SessionProjection::new());
        self.chips = std::mem::take(&mut slot.chips);
        // B2b: the branch registry/active/parked views travel as ONE unit
        // with the session — the A→B→A checkout law.
        self.branch_state = std::mem::take(&mut slot.branch_state);
        // H4: the journaled hook facts + decision-chip state travel the
        // same way.
        self.hook_facts = std::mem::take(&mut slot.hook_facts);
        self.msg_queue = std::mem::take(&mut slot.msg_queue);
        self.queue_mode = slot.queue_mode;
        self.turn_active = slot.turn_active;
        self.auto_resuming = slot.auto_resuming;
        self.subtree_collapsed = slot.subtree_collapsed;
        self.todos_collapsed = slot.todos_collapsed;
        self.session_title = slot.title.take();
        self.session_name = slot.name.take();
        self.session_head = std::mem::take(&mut slot.head);
        self.session_dir = std::mem::take(&mut slot.dir);
        self.sessions[index] = slot;
        self.active_session = Some(id.clone());
        self.menu_selection = 0;
        self.view_path.clear();
        // IDENTITY-FLIP SPLIT SEAM (TUI6.2 fix 3's named exception 2 of
        // 3): the departing surface's draft was stashed at open_session's
        // entry, BEFORE `active_session` flipped — switch_surface cannot
        // span the flip because both of its keys derive from it. The
        // restore below completes the pair under the NEW key.
        self.screen = Screen::Session;
        self.scroll_back.set(0);
        self.scroll_max.set(0);
        self.sticky_suppressed.set(false);
        // Screen chrome, not session state: a jump armed for the OLD
        // session and its tree drill state die with the surface.
        *self.pending_jump.borrow_mut() = None;
        self.tree_view = None;
        // TUI5 item 9: the attached session's own draft comes live —
        // text, cursor, selection and input ring exactly as it left.
        self.restore_draft();
    }

    /// Detach: write the live fields back into the session's slot (sim
    /// `setActiveId(null)` — the state lives on and its scripts keep
    /// running). The surface then returns to the neutral no-session state
    /// item 12 requires of the launcher.
    pub fn checkin(&mut self) {
        let Some(active) = self.active_session.take() else {
            return;
        };
        if let Some(index) = self.sessions.iter().position(|entry| entry.id == active) {
            // (identity is the protocol id; the row's generation stays put)
            let slot = &mut self.sessions[index];
            slot.projection = std::mem::replace(&mut self.projection, SessionProjection::new());
            slot.chips = std::mem::take(&mut self.chips);
            slot.branch_state = std::mem::take(&mut self.branch_state);
            slot.hook_facts = std::mem::take(&mut self.hook_facts);
            slot.msg_queue = std::mem::take(&mut self.msg_queue);
            slot.queue_mode = std::mem::take(&mut self.queue_mode);
            slot.turn_active = std::mem::take(&mut self.turn_active);
            slot.auto_resuming = std::mem::take(&mut self.auto_resuming);
            slot.subtree_collapsed = std::mem::take(&mut self.subtree_collapsed);
            slot.todos_collapsed = std::mem::take(&mut self.todos_collapsed);
            slot.title = self.session_title.take();
            slot.name = self.session_name.take();
            slot.head = std::mem::replace(
                &mut self.session_head,
                ("Hasan".to_owned(), "(a)".to_owned()),
            );
            slot.dir = std::mem::replace(&mut self.session_dir, self.launcher_dir.clone());
        }
        self.last_detached = Some(active);
        self.msg_queue.clear();
        self.queue_mode = false;
        self.view_path.clear();
        self.menu_selection = 0;
        self.scroll_back.set(0);
        self.scroll_max.set(0);
        self.sticky_suppressed.set(false);
        *self.pending_jump.borrow_mut() = None;
        self.tree_view = None;
    }

    /// Sim `newSession` (tui.js:1617-1650): a fresh id, a head claimed
    /// from the roster (the seeds hold 0-2, so the first user session
    /// claims Hasan), the launcher dir, newest-first in the list. The
    /// session left behind is checked in, never cancelled.
    fn new_session(&mut self, text: &str) {
        self.checkin();
        let ui_gen = UiGeneration::new(self.next_ui_generation);
        self.next_ui_generation += 1;
        // The DEMO mints its own protocol id from the generation (report
        // R11 cut 1). `run_live` never reaches here: a live session exists
        // only once `session.create` answers with the daemon's id.
        let id = crate::identity::demo_session_id(ui_gen);
        let ros = self
            .roster
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let head = crate::script::roster_at(ros);
        let mut entry = crate::session::SessionState::neutral(id.clone(), ui_gen);
        entry.name = Some(slug_name(text));
        entry.head = (head.callsign, head.hon.to_owned());
        entry.head_ros = Some(ros);
        entry.dir = self.launcher_dir.clone();
        entry.model_short = self.identity.model_short.clone();
        entry.device = self.identity.device.clone();
        entry.ago = "now".to_owned();
        self.sessions.insert(0, entry);
        self.open_session(&id);
        // Review P3-8: the founding message recalls IN the new session
        // (Claude Code recalls in-conversation); the launcher's own ring
        // kept its copy via take_for_submit before the surface swap.
        self.composer.record_submitted(text);
    }

    /// Walk back to the launcher — the ONE teardown every back path shares
    /// (the ← main chip, idle esc, ⌃C navigation per owner item 10): the
    /// live talk hold is cancelled (P1-3), the subagent view path and any
    /// overlay reset. NAVIGATION ONLY — the projection, chips, queue and a
    /// running turn are untouched, so the session resumes exactly where it
    /// was left.
    pub fn back_to_launcher(&mut self) {
        // TUI5 item 9: park the departing surface's draft (session, aura,
        // or the scratch surface — which shares the launcher key, so its
        // stash/restore is an exact round-trip).
        self.stash_draft();
        // TUI4c: leaving DETACHES (sim `setActiveId(null)`, tui.js:1956) —
        // the session's state checks into its slot, its scripts keep
        // running, and the launcher's surface derives from NO session
        // (item 12: a background turn never reaches the main menu's badge).
        if self.active_session.is_some() {
            self.checkin();
        } else if !self.projection.entries().is_empty() || self.turn_active {
            // A content-bearing SCRATCH (envelope-driven flows with no
            // session id — the headless harness, the plain oracle): there
            // is no slot to keep it in, so /clear's fresh-start promise
            // applies literally — reset and stop its scripts. Real UI
            // flows always mint a session id first (`new_session`).
            self.fresh_session();
        }
        // IDENTITY-FLIP SPLIT SEAM (TUI6.2 fix 3's named exception 3 of
        // 3): checkin()/fresh_session() stash the departing draft and
        // clear `active_session` above — switch_surface cannot span the
        // flip. The restore below completes the pair under the launcher
        // key.
        self.screen = Screen::Launcher;
        self.listening = false;
        self.view_path.clear();
        self.help_open = false;
        // TUI5 item 9: the launcher's own draft comes back.
        self.restore_draft();
    }

    /// Learn (or re-learn) a LIVE session row (W3c3 M2). Idempotent: a
    /// session the model already holds keeps its row, its transcript and
    /// its generation, so a `session.list` after every reconnect neither
    /// duplicates rows nor resets the drafts/arms keyed by generation.
    ///
    /// A NEW row is minted with the next local generation and NO content:
    /// the launcher shows the daemon's session, and its transcript arrives
    /// by attach, never by guess.
    pub fn upsert_live_session(&mut self, id: &SessionId) -> UiGeneration {
        if let Some(existing) = self.sessions.iter().find(|row| &row.id == id) {
            return existing.ui_gen;
        }
        let ui_gen = UiGeneration::new(self.next_ui_generation);
        self.next_ui_generation += 1;
        let mut entry = crate::session::SessionState::neutral(id.clone(), ui_gen);
        entry.dir = self.launcher_dir.clone();
        entry.model_short = self.identity.model_short.clone();
        entry.device = self.identity.device.clone();
        entry.ago = "now".to_owned();
        // Newest first, exactly like `new_session` — the launcher's order
        // is a display law, not a source law.
        self.sessions.insert(0, entry);
        self.dirty = true;
        ui_gen
    }

    /// Hydrate one roster row's counts from a `session.list` summary
    /// (launcher fix 2 — the additive daemon fields). Tolerant by law:
    ///
    /// * BOTH fields absent (an older daemon) → store nothing — the row
    ///   keeps whatever it already shows (its projection-derived display,
    ///   or an earlier value-carrying summary), never a fabricated count;
    /// * a summary at or behind one already stored never replaces it;
    /// * freshness against live/checked-in values is judged at READ time
    ///   ([`crate::session::SessionState::turns`] / `row_tokens`), so a
    ///   checkin AFTER this call still beats a stale summary.
    pub fn note_summary_counts(&mut self, summary: &haider_rpc::SessionSummary) {
        // G2: the wire title names the row FIRST — the counts gate below
        // must not starve it. Absence hydrates nothing (an older daemon
        // omits the field; a live rename reply is the clearing authority).
        if summary.title.is_some() {
            if self.active_session.as_ref() == Some(&summary.session_id) {
                if self.session_name != summary.title {
                    self.session_name = summary.title.clone();
                    self.dirty = true;
                }
            } else if let Some(entry) = self
                .sessions
                .iter_mut()
                .find(|row| row.id == summary.session_id)
                && entry.name != summary.title
            {
                entry.name = summary.title.clone();
                self.dirty = true;
            }
        }
        if summary.turn_count.is_none() && summary.footprint_tokens.is_none() {
            return;
        }
        let Some(entry) = self
            .sessions
            .iter_mut()
            .find(|row| row.id == summary.session_id)
        else {
            return;
        };
        if entry
            .summary_counts
            .as_ref()
            .is_some_and(|held| held.head_seq >= summary.head_seq)
        {
            return;
        }
        entry.summary_counts = Some(crate::session::SummaryCounts {
            head_seq: summary.head_seq,
            turns: summary.turn_count,
            footprint_tokens: summary.footprint_tokens,
            footprint_truth: summary.footprint_truth,
        });
        self.dirty = true;
    }

    /// Seed a session's cursor with the sequence its attach asked FROM, so
    /// the strict gap law covers the FIRST delivered envelope too (W3c3.1,
    /// review P1-1).
    ///
    /// Without this, continuity was only checked once `last_seq` was set:
    /// a fresh attach at cursor 0 answered with seq 2 applied seq 2 as its
    /// first event and painted a hole as history — and a hole with no
    /// later sequence behind it can never be discovered. Seeding is
    /// MONOTONE (it never rewinds a cursor): the attach's `after_seq` is
    /// read from this same authority, so the only case that moves is the
    /// one that was never set.
    pub fn seed_cursor(&mut self, session: &SessionId, after_seq: u64) {
        let projection = if self.active_session.as_ref() == Some(session) {
            Some(&mut self.projection)
        } else {
            self.sessions
                .iter_mut()
                .find(|entry| &entry.id == session)
                .map(|entry| &mut entry.projection)
        };
        debug_assert!(
            projection.is_some(),
            "seed_cursor for a session with no row: every `ensure_attached` \
             caller upserts the row first, and a silent no-op here hands the \
             strict gap law back its blind spot"
        );
        if let Some(projection) = projection
            && projection
                .last_applied()
                .is_none_or(|last| last < after_seq)
        {
            projection.set_last_applied(after_seq);
        }
    }

    /// A NON-attached session's slot (background event routing).
    pub fn session_entry_mut(
        &mut self,
        id: &SessionId,
    ) -> Option<&mut crate::session::SessionState> {
        if self.active_session.as_ref() == Some(id) {
            return None;
        }
        self.sessions.iter_mut().find(|entry| &entry.id == id)
    }

    /// A non-attached session's slot BY GENERATION — the demo driver's
    /// lookup, whose arms are generation-keyed (report R11 cut 1).
    pub fn session_entry_by_generation(
        &mut self,
        generation: UiGeneration,
    ) -> Option<&mut crate::session::SessionState> {
        if self.ui_generation() == generation && self.active_session.is_some() {
            return None;
        }
        self.sessions
            .iter_mut()
            .find(|entry| entry.ui_gen == generation)
    }

    /// A left-click resolved through the frame's hit map. The map may be
    /// one frame stale (review r2 P2-2): hits carry values and every
    /// context-sensitive hit re-checks its context — activate exactly what
    /// was clicked, or drop the click.
    pub fn handle_hit(&mut self, hit: Hit) {
        self.dirty = true;
        self.flash = None;
        // A visible overlay owns the screen; hits from the covered frame
        // must not act through it.
        if self.help_open {
            return;
        }
        // TUI6.2b: the login card is MODAL against hits exactly as it is
        // against keys (login_key owns the keyboard) — the frame beneath
        // it still lists clickable rows, and a click that fell through
        // ran open_session mid-login: its stash overwrote the
        // login-parked draft (ring destroyed), the screen flipped under
        // the open card, and the card's Esc-restore later clobbered the
        // session draft. The card has no hit targets of its own, so the
        // gate is total.
        if self.login.is_some() {
            return;
        }
        match hit {
            // Every hit below re-checks its OWNING SURFACE: the map may be
            // one frame stale, so a rect from a screen we have since left
            // must never act (review P1-5 — the law documented above was
            // only honored by the palette/menu hits).
            Hit::AttachSession(id) if self.screen == Screen::Launcher => {
                self.attach_session_id(&id);
            }
            Hit::ExtraRow(which) if self.screen == Screen::Launcher => match which {
                LauncherRow::Aura => self.enter_aura(),
                LauncherRow::Accounts => self.enter_accounts(),
                LauncherRow::Peers => {
                    self.flash = Some(
                        "· /peers — UI ready; lands with the mesh wave (post-v0.1)".to_owned(),
                    );
                }
            },
            // `/accounts` rows: click = make active for its provider (sim
            // tui.js:3604 onClick useAccount). Value-carrying alias, and
            // NEVER an optimistic flip — select_account only requests.
            Hit::AccountRow(alias) if self.screen == Screen::Accounts => {
                self.select_account(&alias);
            }
            // D2: click = the row's digit. Only SUPPORTED candidate rows
            // ever rendered a hit; the dispatch re-checks the flag anyway
            // (a stale rect can only import what it was measured on).
            Hit::DeviceImport(candidate)
                if matches!(self.screen, Screen::Accounts | Screen::Providers) =>
            {
                self.import_device_candidate(&candidate);
            }
            Hit::AccountAdd(kind)
                if matches!(self.screen, Screen::Accounts | Screen::Providers) =>
            {
                // From /providers the add flow lives on /accounts (the
                // cards and their keyboard ownership are that screen's) —
                // jump first, then open (owner ask: providers offers the
                // same add options).
                if self.screen == Screen::Providers {
                    self.enter_accounts();
                }
                self.accounts.message = None;
                match kind {
                    // API-key adds ride the existing masked LoginCard flow
                    // (TUI6 total modality; the alias field prefills from
                    // the provider).
                    AccountAddKind::OpenAiApi => self.open_login_card("openai", None),
                    AccountAddKind::AnthropicApi => self.open_login_card("anthropic", None),
                    // B6b: the Gemini adapter (B6a) shipped with NO feature
                    // bit, so provider-listing truth gates the button — a
                    // daemon that does not list the provider would bounce
                    // the eventual account.login_api obscurely.
                    AccountAddKind::GeminiApi => {
                        if self.daemon_lists_provider("gemini") {
                            self.open_login_card("gemini", None);
                        } else {
                            self.accounts.message = Some(self.stale_daemon_note("Gemini accounts"));
                            self.dirty = true;
                        }
                    }
                    // OAuth adds run the REAL loopback flow (W5e-1): the
                    // card drives account.oauth_start/status + account.add
                    // live, and the sim's simulated authorize in demo.
                    AccountAddKind::OpenAiOAuth | AccountAddKind::AnthropicOAuth => {
                        // Feature-gated (report §4.1): never offer a method
                        // the connected daemon cannot serve.
                        if self.daemon_serves(haider_rpc::FEATURE_ACCOUNT_OAUTH_PKCE_V1) {
                            self.open_oauth_add(kind);
                        } else {
                            self.accounts.message = Some(self.stale_daemon_note("OAuth sign-in"));
                            self.dirty = true;
                        }
                    }
                    // B6b: Kimi rides the DEVICE flow — its own feature bit
                    // (shipped beside the kimi-oauth builtin, v0.0.52), the
                    // same §4.1 gate as the PKCE pair above.
                    AccountAddKind::KimiOAuth => {
                        if self.daemon_serves(haider_rpc::FEATURE_ACCOUNT_OAUTH_DEVICE_V1) {
                            self.open_oauth_add(kind);
                        } else {
                            self.accounts.message =
                                Some(self.stale_daemon_note("Kimi OAuth sign-in"));
                            self.dirty = true;
                        }
                    }
                    // The custom card is the provider.configure front door
                    // (W5g-4): demo shows the sim's fabrication card, live
                    // shows the editable name/origin fields.
                    AccountAddKind::Custom => {
                        if self.mode.fabricates_locally()
                            || self.daemon_serves(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1)
                        {
                            self.open_custom_add();
                        } else {
                            self.accounts.message =
                                Some(self.stale_daemon_note("custom providers"));
                            self.dirty = true;
                        }
                    }
                    AccountAddKind::HuggingFace => {
                        self.open_huggingface_preset();
                    }
                    AccountAddKind::OpencodeZen => {
                        self.open_opencode_zen_preset();
                    }
                    AccountAddKind::OpencodeGo => {
                        self.open_opencode_go_preset();
                    }
                    AccountAddKind::Ollama => {
                        self.open_ollama_preset();
                    }
                    AccountAddKind::LmStudio => {
                        self.open_lmstudio_preset();
                    }
                }
            }
            Hit::ProviderModel { provider, model } if self.screen == Screen::Providers => {
                self.set_default_model(&provider, &model);
            }
            Hit::ProviderAccounts if self.screen == Screen::Providers => {
                self.enter_accounts();
            }
            // U2: a click on an account tab chip selects exactly the
            // account the rect was rendered for (value-carrying) and moves
            // the cursor to its group.
            Hit::UsageAccountTab { provider, index } if self.screen == Screen::Usage => {
                let groups = self.usage.groups();
                let Some(position) = groups.iter().position(|group| group.provider == provider)
                else {
                    return;
                };
                if index < groups[position].accounts.len() {
                    self.usage.cursor = position;
                    self.usage.tabs.insert(provider, index);
                    self.dirty = true;
                }
            }
            // Dismissed/replaced palettes drop the click.
            Hit::PaletteRow(item) if self.palette_open() => self.activate_palette_item(item),
            // A `/theme` picker row: click commits (the hover already
            // previewed it). A stale hit after the picker closed drops.
            Hit::ThemeOption(index) if self.theme_picker.is_some() => {
                self.commit_theme_row(index);
            }
            // G3: an `/effort` row click commits exactly that row.
            Hit::EffortOption(index) if self.effort_picker.is_some() => {
                self.commit_effort_row(index);
            }
            // F2a: a picker row click selects exactly the pair the rect
            // was rendered for (value-carrying — a stale map can never
            // select a different row).
            Hit::ModelPickerRow { provider, model } if self.model_picker.is_some() => {
                if let Some(row) = self
                    .model_picker_rows()
                    .into_iter()
                    .find(|row| row.provider == provider && row.model == model)
                {
                    self.select_model_row(&row);
                }
            }
            Hit::MenuOption { menu, index } => {
                // Only the SAME menu the row was rendered for may answer —
                // and on the subagent screen that menu is the CHIP's card,
                // which the session projection knows nothing about (review
                // P2-7: chip-question clicks were silently dead).
                if self.screen == Screen::Subagent {
                    let card = self
                        .viewed_chip()
                        .and_then(ChipModel::question_menu)
                        .filter(|m| m.id == menu && index < m.options.len())
                        .cloned();
                    if let Some(card) = card {
                        self.menu_selection = index;
                        self.answer_chip_menu(&card);
                    }
                } else if self.screen == Screen::Session
                    && self
                        .projection
                        .open_menu()
                        .is_some_and(|m| m.id == menu && index < m.options.len())
                {
                    // The card is only answerable while its own surface is
                    // showing: Back leaves the projection (and its card)
                    // intact, so without this a queued click on the old
                    // option rect would answer an invisible card and start
                    // its parked continuation (review r2 P1-2).
                    self.menu_selection = index;
                    self.submit_menu_answer();
                }
            }
            Hit::BackChip if self.screen == Screen::Session => {
                self.back_to_launcher();
            }
            // ◉ talk (sim `speak`, tui.js:2044-2049): the mic RENDERS on the
            // launcher, but pressing it there does nothing — `speak` returns
            // unless a session is attached and idle (review r2 P2-3). The
            // screen gate is also the owning-surface guard the other hits
            // already carry (review r2 P2-4).
            Hit::TalkChip if self.screen == Screen::Session => {
                if !self.mode.fabricates_locally() {
                    // T2: the live chip drives the real toggle-to-talk
                    // machine (start · commit+submit) — the demo-only
                    // refusal died with this wave.
                    self.talk_toggle();
                } else if !self.voice.enabled {
                    self.flash = Some("· enable voice first with /voice".to_owned());
                } else if !self.turn_active && !self.listening {
                    self.listening = true;
                    self.requests.push(AppRequest::Talk);
                }
            }
            Hit::HelpHint if self.screen == Screen::Launcher => self.help_open = true,
            // The SubTree panel exists only on the session/subagent screens,
            // and its rows only while it is expanded.
            Hit::ChipRow(agent)
                if matches!(self.screen, Screen::Session | Screen::Subagent)
                    && !self.subtree_collapsed =>
            {
                if let Some(path) = path_to_chip(&self.chips, &agent) {
                    self.view_path = path;
                    self.switch_surface(Screen::Subagent);
                    self.menu_selection = 0;
                    self.scroll_back.set(0);
                }
            }
            Hit::SubTreeToggle
                if matches!(self.screen, Screen::Session | Screen::Subagent)
                    && !self.chips.is_empty() =>
            {
                self.subtree_collapsed = !self.subtree_collapsed;
            }
            Hit::TodosToggle if self.screen == Screen::Session => {
                self.todos_collapsed = !self.todos_collapsed;
            }
            // Hover-only affordance (see the variant's doc comment).
            Hit::TodoRow(_) => {}
            // The ⌂ home row and the ✕ close button belong to the subagent
            // screen; ✕ closes only the chip actually being VIEWED.
            Hit::SessionHome if self.screen == Screen::Subagent => {
                self.switch_surface(Screen::Session);
                self.scroll_back.set(0);
            }
            Hit::ChipCloseBtn(agent)
                if self.screen == Screen::Subagent
                    && self.view_path.last() == Some(&agent)
                    && find_chip(&self.chips, &agent).is_some_and(|chip| !chip.closed) =>
            {
                if self.mode.fabricates_locally() {
                    self.requests.push(AppRequest::ChipClose { agent });
                } else {
                    // A live chip closes when its `AgentChipState` says so.
                    self.refuse_demo_only("closing a subagent");
                }
            }
            Hit::ChipCrumb(path) if self.screen == Screen::Subagent => {
                if path.is_empty() {
                    self.switch_surface(Screen::Session);
                    self.scroll_back.set(0); // TUI6.2c finding 8
                } else if path
                    .last()
                    .is_some_and(|agent| find_chip(&self.chips, agent).is_some())
                {
                    self.view_path = path;
                    self.switch_surface(Screen::Subagent);
                }
            }
            Hit::AuraEngine if self.screen == Screen::Aura => {
                self.aura.realtime = !self.aura.realtime;
                let label = self.aura.engine_label();
                self.aura
                    .transcript
                    .push_note(format!("· engine hot-swapped → {label} · dialogue kept"));
            }
            Hit::AuraMute if self.screen == Screen::Aura => {
                self.aura.muted = !self.aura.muted;
                self.aura.transcript.push_note(
                    if self.aura.muted {
                        "· audio output muted — orchestrating silently, activity still shown"
                    } else {
                        "· audio output on"
                    }
                    .to_owned(),
                );
            }
            Hit::AuraExit if self.screen == Screen::Aura => self.exit_aura(),
            Hit::AuraTalkBtn
                if self.screen == Screen::Aura && self.aura.state == AuraState::Idle =>
            {
                self.aura.state = AuraState::Listening;
                self.requests.push(AppRequest::AuraTalk);
            }
            Hit::TreeRow(row) if self.screen == Screen::Tree => {
                // Value-carrying + existence check (law: a stale hit on a
                // replaced row must not activate): the click selects the
                // row ONLY where the freshly built rows still contain the
                // exact value the frame rendered. Activation stays on ⏎/f,
                // the sim's single-click semantics (tui.js:3375-3377).
                if let Some(index) = tree_rows(self)
                    .iter()
                    .position(|candidate| candidate == &row)
                {
                    self.tree_sel = index;
                    self.dirty = true;
                }
            }
            Hit::HookRow(digest) if self.screen == Screen::Hooks => {
                // Value-carrying + existence check (the TreeRow law): the
                // click selects — and opens the confirmation card for —
                // ONLY a row still wearing the exact digest the frame
                // rendered; a refresh that replaced it drops the click.
                if let Some(index) = self
                    .hooks
                    .rows
                    .as_ref()
                    .and_then(|rows| rows.iter().position(|row| row.digest == digest))
                {
                    self.hooks.cursor = index;
                    self.open_hook_confirm();
                    self.dirty = true;
                }
            }
            Hit::StickyJump(scroll_back)
                if matches!(self.screen, Screen::Session | Screen::Subagent) =>
            {
                // Stay AT the producing prompt, and suppress the sticky
                // until the next REAL wheel (sim jumpToSticky: "the bar is
                // suppressed … so it never covers the row it just
                // revealed", tui.js:2637-2657). Surface-guarded like every
                // other hit arm (Fable review D3-12).
                self.scroll_back.set(scroll_back.min(self.scroll_max.get()));
                self.sticky_suppressed.set(true);
            }
            // A hit whose owning surface is gone: dropped, never acted on.
            _ => {}
        }
    }

    /// Wheel scroll in the session transcript (text selection is IN-APP —
    /// drag-select + auto-copy, owner item 9; the old "left to native
    /// ⇧-drag" row is retired). Reconcile-then-apply (review r5 P2-2): the
    /// offset first
    /// folds to the last frame's truth (`scroll_max` is at most one frame
    /// stale), THEN the notch applies clamped to it — queued bursts can
    /// never bank unbounded debt, and a reversal mid-burst always moves
    /// the view. The frame's own reconcile stays as the backstop (sim
    /// reads live DOM geometry, tui.js:2648).
    pub fn handle_wheel(&mut self, up: bool) {
        // The login gate joins the help gate (TUI6.2c finding 7 —
        // consistency: nothing scrolls beneath a modal).
        if self.help_open || self.login.is_some() {
            return;
        }
        // F2b: the providers roster scrolls under the wheel too.
        if self.screen == Screen::Providers {
            let max = self.providers.scroll_max.get();
            let current = self.providers.scroll.get().min(max);
            let next = if up {
                current.saturating_sub(3)
            } else {
                current.saturating_add(3).min(max)
            };
            self.providers.scroll.set(next);
            self.dirty = true;
            return;
        }
        // U2: the usage report rides the same F2b wheel discipline.
        if self.screen == Screen::Usage {
            let max = self.usage.scroll_max.get();
            let current = self.usage.scroll.get().min(max);
            let next = if up {
                current.saturating_sub(3)
            } else {
                current.saturating_add(3).min(max)
            };
            self.usage.scroll.set(next);
            self.dirty = true;
            return;
        }
        if !matches!(self.screen, Screen::Session | Screen::Subagent) {
            return;
        }
        self.dirty = true;
        // A real scroll lifts the post-jump sticky suppression (sim
        // onTranscriptScroll → computeSticky).
        self.sticky_suppressed.set(false);
        let max = self.scroll_max.get();
        let current = self.scroll_back.get().min(max);
        let next = if up {
            current.saturating_add(3).min(max)
        } else {
            current.saturating_sub(3)
        };
        self.scroll_back.set(next);
    }

    /// Terminal resize: force a redraw. The frame itself reconciles the
    /// scroll offset against the new true range (review r3 P2-2 — render
    /// is the single scroll authority, so no resize-ordering bug exists).
    ///
    /// TUI6.1 fix 1: it also advances the GEOMETRY EPOCH, killing every
    /// composer hit stamped by pre-resize frames — a queued click can win
    /// the event race against the redraw, and resize bumps no text
    /// revision, so the TUI5 guard alone accepted stale-layout clicks
    /// (review r1 finding 1). The wrap-budget half of the same fix lives
    /// at the dispatch seam (`runtime::dispatch_input`), which reflows
    /// the composer's budget from the new width BEFORE any queued key can
    /// navigate the old geometry — the reducer itself stays
    /// wrap-ignorant.
    pub fn handle_resize(&mut self) {
        self.geometry_epoch
            .set(self.geometry_epoch.get().wrapping_add(1));
        // A resize moves every target under a STATIONARY pointer: the old
        // hovered Hit would repaint its highlight at the target's NEW row
        // while the mouse sits somewhere else, and nothing corrects it
        // until the mouse moves (owner report, W5g-6). Hover re-arms on
        // the next real motion.
        self.hovered = None;
        self.dirty = true;
    }

    /// Mouse motion over the frame (owner ask, TUI3a item 6). Motion
    /// events FLOOD — the model only dirties when the hovered target
    /// actually changes. Palette rows and menu options move the SELECTION
    /// on hover (sim onMouseEnter, tui.js:2992/3073); everything else is
    /// hover chrome the renderer paints from [`Self::hovered`].
    pub fn handle_hover(&mut self, hit: Option<Hit>) {
        // TUI6.2c (verifier finding 4): modals own hover exactly as they
        // own hits and keys — with the help overlay or the login card up,
        // hover must not move palette/menu selections beneath the modal.
        if self.help_open || self.login.is_some() {
            return;
        }
        if self.hovered == hit {
            return;
        }
        self.hovered = hit;
        self.dirty = true;
        match self.hovered.clone() {
            Some(Hit::PaletteRow(item)) => {
                if self.palette_open()
                    && let Some(position) = self.palette_items().iter().position(|i| *i == item)
                {
                    self.palette_selection = position;
                    let count = self.palette_items().len();
                    self.scroll_palette_into_view(count);
                }
            }
            Some(Hit::MenuOption { menu, index }) => {
                // Hover moves the selection on BOTH card surfaces (sim
                // `onMouseEnter` on `.imo`, tui.js:3093 — review P2-7).
                let valid = if self.screen == Screen::Subagent {
                    self.viewed_chip()
                        .and_then(ChipModel::question_menu)
                        .is_some_and(|m| m.id == menu && index < m.options.len())
                } else {
                    // Same surface gate as the click (review r2 P1-2).
                    self.screen == Screen::Session
                        && self
                            .projection
                            .open_menu()
                            .is_some_and(|m| m.id == menu && index < m.options.len())
                };
                if valid {
                    self.menu_selection = index;
                }
            }
            // Hover moves the `/theme` picker's highlight — and with it
            // the live PREVIEW (the picker's whole point).
            Some(Hit::ThemeOption(index)) if self.theme_picker.is_some() => {
                self.preview_theme_row(index);
            }
            // G3: hover moves the effort highlight (no preview side effect —
            // effort commits only on the RESOLVED reply).
            Some(Hit::EffortOption(index)) if self.effort_picker.is_some() => {
                if index < self.effort_picker_rows().len()
                    && let Some(picker) = self.effort_picker.as_mut()
                {
                    picker.selection = index;
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    /// ⌃T: cycle the FIXED themes (a quick toggle beside the `/theme`
    /// picker). Each step is a committed choice — the runtime persists it.
    fn cycle_theme(&mut self) {
        let keys = ThemeKey::ALL;
        let index = keys.iter().position(|k| *k == self.theme).unwrap_or(0);
        self.commit_theme_choice(ThemeChoice::Fixed(keys[(index + 1) % keys.len()]));
        self.flash = Some(format!("· theme → {}", self.theme.theme().label));
    }

    /// Apply a theme choice WITHOUT committing it: resolve against the
    /// boot-time detection and re-ground the frame. Boot resolution uses
    /// this directly; user actions go through [`Self::commit_theme_choice`].
    pub fn apply_theme_choice(&mut self, choice: ThemeChoice) {
        self.theme_choice = choice;
        self.theme = choice.resolve(self.detected_system);
        self.dirty = true;
    }

    /// A USER commit of the theme choice (picker ⏎/digit/click, `/theme
    /// <name>`, ⌃T): apply and bump the commit counter the runtime's
    /// persistence authority watches. Re-committing the current choice
    /// still counts — the settings file must land even when the pick
    /// matches the boot default (ui-themes-fix, live probe).
    pub fn commit_theme_choice(&mut self, choice: ThemeChoice) {
        self.apply_theme_choice(choice);
        self.theme_commits = self.theme_commits.wrapping_add(1);
    }

    /// The flash for a committed choice — `system` names what it resolved
    /// to right now so the choice is never opaque.
    fn theme_flash(&self) -> String {
        match self.theme_choice {
            ThemeChoice::System => format!(
                "· theme → system · follows the terminal (now {})",
                self.theme.theme().label
            ),
            ThemeChoice::Fixed(key) => format!("· theme → {}", key.theme().label),
        }
    }

    /// Bare `/theme` (owner menu law): the numbered arrow-highlight
    /// picker, opening on EVERY composer surface — launcher, session,
    /// aura, subagent — through this ONE authority (ui-themes-fix: the
    /// launcher is the owner's primary surface). A daemon card outranks
    /// it — local chrome never sits on a live ask.
    /// F2a: open the full-screen `/model` picker with `query` pre-filled.
    /// Pushes a registry refresh so the roster is as fresh as the daemon
    /// serves; rows render from the snapshot in hand meanwhile.
    pub fn open_model_picker(&mut self, query: String) {
        self.model_picker = Some(ModelPicker {
            query,
            ..ModelPicker::default()
        });
        self.requests.push(AppRequest::ProvidersRefresh);
        self.dirty = true;
    }

    /// Every `/model` picker row: one per model × provider pair across
    /// ALL enabled providers (daemon truth — `provider.list` order); an
    /// enabled provider with nothing discovered contributes one honest
    /// placeholder row carrying its reason.
    #[must_use]
    pub fn model_picker_rows(&self) -> Vec<ModelPickerRow> {
        use haider_protocol::credential::AuthMethod;
        let mut rows = Vec::new();
        for summary in &self.providers.providers {
            if !summary.enabled {
                continue;
            }
            let available = matches!(
                summary.availability,
                haider_rpc::ProviderAvailabilityWire::Available
            );
            let reason = summary
                .availability_reason
                .clone()
                .or_else(|| (!available).then(|| "provider unavailable".to_owned()));
            // Auth flavor: the provider key's own encoding, the selected
            // account's method, then a single declared method — the same
            // truth order as the composer identity (F2c).
            let auth = if summary.provider.ends_with("-oauth") {
                "oauth"
            } else if let Some(row) = self
                .accounts
                .rows
                .iter()
                .find(|row| row.provider == summary.provider && row.selected)
            {
                match row.method {
                    AuthMethod::OAuth => "oauth",
                    AuthMethod::ApiKey => "api",
                }
            } else {
                match summary.auth_methods.as_slice() {
                    [AuthMethod::OAuth] => "oauth",
                    _ => "api",
                }
            };
            if summary.models.is_empty() {
                rows.push(ModelPickerRow {
                    provider: summary.provider.clone(),
                    model: String::new(),
                    auth,
                    context_window: None,
                    available,
                    reason: Some(reason.unwrap_or_else(|| "no discovered models".to_owned())),
                    is_default: false,
                    is_current: false,
                    selectable: false,
                });
                continue;
            }
            for model in &summary.models {
                rows.push(ModelPickerRow {
                    provider: summary.provider.clone(),
                    model: model.clone(),
                    auth,
                    context_window: self.providers.declared_window(&summary.provider, model),
                    available,
                    reason: reason.clone(),
                    is_default: summary.default_model.as_deref() == Some(model),
                    is_current: self.identity.provider == summary.provider
                        && self.identity.model_short == *model,
                    selectable: true,
                });
            }
        }
        rows
    }

    /// The picker's LIVE search: case-insensitive; every whitespace-
    /// separated token must substring-match the row's model + provider
    /// (+ auth flavor) haystack.
    #[must_use]
    pub fn model_picker_filtered(&self, query: &str) -> Vec<ModelPickerRow> {
        let needle = query.to_ascii_lowercase();
        let tokens: Vec<&str> = needle.split_whitespace().collect();
        self.model_picker_rows()
            .into_iter()
            .filter(|row| {
                let haystack =
                    format!("{} {} {}", row.model, row.provider, row.auth).to_ascii_lowercase();
                tokens.iter().all(|token| haystack.contains(token))
            })
            .collect()
    }

    /// KEY-OWNERSHIP LAW (F2a, heeded history): while the picker is open
    /// it owns every key — ⏎ selects the HIGHLIGHTED row (never an
    /// exact-match jump), esc closes WITHOUT selecting, characters edit
    /// the search, ↑/↓ move the highlight (wrapping).
    fn handle_model_picker_key(&mut self, code: KeyCode) {
        self.dirty = true;
        match code {
            KeyCode::Esc => {
                // Closes WITHOUT selecting — nothing else moves.
                self.model_picker = None;
            }
            KeyCode::Up | KeyCode::Down => {
                let Some(query) = self.model_picker.as_ref().map(|p| p.query.clone()) else {
                    return;
                };
                let len = self.model_picker_filtered(&query).len();
                if let Some(picker) = self.model_picker.as_mut()
                    && len > 0
                {
                    picker.selection = if code == KeyCode::Up {
                        (picker.selection + len - 1) % len
                    } else {
                        (picker.selection + 1) % len
                    };
                }
            }
            KeyCode::Enter => {
                // ⏎ selects the HIGHLIGHTED row — never an exact-match
                // jump (heeded history). No row under the highlight (empty
                // filter) selects nothing; the picker stays open.
                let Some((query, selection, pending)) = self
                    .model_picker
                    .as_ref()
                    .map(|p| (p.query.clone(), p.selection, p.pending.is_some()))
                else {
                    return;
                };
                if pending {
                    return;
                }
                let rows = self.model_picker_filtered(&query);
                if let Some(row) = rows.get(selection).cloned() {
                    self.select_model_row(&row);
                }
            }
            KeyCode::Backspace => {
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.query.pop();
                    picker.selection = 0;
                    picker.error = None;
                }
            }
            KeyCode::Char(c) => {
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.query.push(c);
                    picker.selection = 0;
                    picker.error = None;
                }
            }
            _ => {}
        }
    }

    /// Selecting one picker row. Unavailable / placeholder rows show
    /// their reason (never a silent failure). A live attached session
    /// issues the receipted `session.select_model` and waits for the
    /// RESOLVED pair; the launcher sets the default pair new sessions
    /// use; demo fabricates locally.
    pub fn select_model_row(&mut self, row: &ModelPickerRow) {
        self.dirty = true;
        if !row.selectable || !row.available {
            let reason = row
                .reason
                .clone()
                .unwrap_or_else(|| "provider unavailable".to_owned());
            if let Some(picker) = self.model_picker.as_mut() {
                picker.error = Some(format!("{} — {reason}", row.provider));
            }
            return;
        }
        // Demo fabricates locally; the launcher (no attached session)
        // sets the default pair used by the next CreateSession.
        let live_session = (!self.mode.fabricates_locally() && self.screen == Screen::Session)
            .then(|| self.active_session.clone())
            .flatten();
        let Some(session) = live_session else {
            self.identity.provider = row.provider.clone();
            self.identity.model_short = row.model.clone();
            self.identity_pinned = true;
            self.refresh_context_window();
            self.model_picker = None;
            self.flash = Some(format!("· model → {} · {}", row.model, row.provider));
            return;
        };
        if !self.daemon_serves(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1) {
            let note = self
                .stale_daemon_note("cross-provider model selection")
                .trim_start_matches("· ")
                .to_owned();
            if let Some(picker) = self.model_picker.as_mut() {
                picker.error = Some(note);
            }
            return;
        }
        if let Some(picker) = self.model_picker.as_mut() {
            picker.pending = Some((row.provider.clone(), row.model.clone()));
            picker.error = None;
        }
        self.requests.push(AppRequest::SelectModel {
            session,
            model: row.model.clone(),
            provider: row.provider.clone(),
        });
    }

    /// The RESOLVED pair committed (F2a/R2): render daemon truth — never
    /// an echo of the request. Closes the picker when it is still open.
    pub fn apply_model_selected(&mut self, provider: &str, model: &str) {
        self.identity.provider = provider.to_owned();
        self.identity.model_short = model.to_owned();
        self.identity_pinned = true;
        self.refresh_context_window();
        self.model_picker = None;
        self.flash = Some(format!("· model → {model} · {provider}"));
        self.dirty = true;
    }

    /// F2e: route a client-observed failure to `session`'s OWN view —
    /// the attached projection when it is live on screen, the parked
    /// slot's otherwise — so the error line is there when the user looks.
    pub fn record_session_error(&mut self, session: &SessionId, text: String) {
        if self.active_session.as_ref() == Some(session) {
            self.projection.record_local_error(text);
        } else if let Some(entry) = self.sessions.iter_mut().find(|entry| &entry.id == session) {
            entry.projection.record_local_error(text);
        }
        self.dirty = true;
    }

    /// A typed `session.select_model` refusal. With the picker open the
    /// public reason lands INLINE (the row stays selectable for a retry);
    /// otherwise it reaches the session view as an error line — never a
    /// silent IDLE (F2e).
    pub fn model_select_failed(&mut self, provider: &str, model: &str, code: &str, message: &str) {
        let reason = format!("{model} · {provider} — {code}: {message}");
        if let Some(picker) = self.model_picker.as_mut() {
            picker.pending = None;
            picker.error = Some(reason);
        } else {
            self.projection
                .record_local_error(format!("model selection failed — {reason}"));
            self.flash = Some(format!("· model selection failed — {code}"));
        }
        self.dirty = true;
    }

    /// `/effort` (G3). With an argument the level is validated against the
    /// CURRENT pair's daemon-declared ladder and committed as a receipted
    /// selection (`default` reverts); bare opens the picker. An empty
    /// ladder refuses honestly — the TUI holds no tables to guess from.
    pub fn effort_command(&mut self, requested: Option<String>) {
        self.dirty = true;
        if self.screen != Screen::Session && self.screen != Screen::Subagent {
            self.flash = Some("· /effort — session only".to_owned());
            return;
        }
        let ladder: Vec<String> = self
            .current_pair_detail()
            .map(|detail| detail.supported_efforts.clone())
            .unwrap_or_default();
        if ladder.is_empty() {
            self.flash = Some(format!(
                "· /effort — {} · {} declares no effort ladder",
                self.identity.model_short, self.identity.provider
            ));
            return;
        }
        let Some(requested) = requested else {
            self.effort_picker = Some(EffortPicker::default());
            return;
        };
        let effort = if requested.eq_ignore_ascii_case("default") {
            None
        } else if ladder.iter().any(|level| *level == requested) {
            Some(requested)
        } else {
            self.flash = Some(format!(
                "· effort \"{requested}\" is not in this pair's ladder — {}",
                ladder.join(" · ")
            ));
            return;
        };
        self.request_effort(effort);
    }

    /// `/fast` (G3): toggles fast mode. Enabling refuses on a pair whose
    /// daemon-projected detail declares no `fast` speed (decision 6: client
    /// refusal AND daemon refusal — no silent no-op); disabling always goes
    /// through so recovery is never gated.
    pub fn fast_command(&mut self) {
        self.dirty = true;
        if self.screen != Screen::Session && self.screen != Screen::Subagent {
            self.flash = Some("· /fast — session only".to_owned());
            return;
        }
        let enabled = !self.identity.fast;
        if enabled && !self.pair_supports_fast() {
            self.flash = Some(format!(
                "· /fast — {} · {} does not support fast mode",
                self.identity.model_short, self.identity.provider
            ));
            return;
        }
        let live_session = (!self.mode.fabricates_locally())
            .then(|| self.active_session.clone())
            .flatten();
        let Some(session) = live_session else {
            self.apply_fast_selected(enabled);
            return;
        };
        if !self.daemon_serves(haider_rpc::FEATURE_SESSION_FAST_SELECT_V1) {
            self.flash = Some(self.stale_daemon_note("fast-mode selection"));
            return;
        }
        self.requests
            .push(AppRequest::SelectFast { session, enabled });
    }

    /// Whether the CURRENT pair's daemon detail declares the `fast` speed.
    #[must_use]
    pub fn pair_supports_fast(&self) -> bool {
        self.current_pair_detail()
            .is_some_and(|detail| detail.supported_speeds.iter().any(|speed| speed == "fast"))
    }

    /// The `/effort` picker rows: `default` first, then the CURRENT pair's
    /// declared ladder with provider-default and current markers.
    #[must_use]
    pub fn effort_picker_rows(&self) -> Vec<EffortPickerRow> {
        let detail = self.current_pair_detail();
        let ladder: Vec<String> = detail
            .map(|detail| detail.supported_efforts.clone())
            .unwrap_or_default();
        let provider_default = detail.and_then(|detail| detail.default_effort.clone());
        let current = self.identity.reasoning.clone();
        let mut rows = vec![EffortPickerRow {
            effort: None,
            is_provider_default: false,
            is_current: current.is_none(),
        }];
        rows.extend(ladder.into_iter().map(|level| EffortPickerRow {
            is_provider_default: provider_default.as_deref() == Some(level.as_str()),
            is_current: current.as_deref() == Some(level.as_str()),
            effort: Some(level),
        }));
        rows
    }

    /// KEY-OWNERSHIP (G3, the `/theme` law): while the effort picker shows
    /// it owns every key — ⏎ commits the highlighted row, digits commit
    /// directly, esc closes without selecting.
    fn handle_effort_picker_key(&mut self, code: KeyCode) {
        self.dirty = true;
        let count = self.effort_picker_rows().len();
        match code {
            KeyCode::Esc => {
                self.effort_picker = None;
            }
            KeyCode::Up | KeyCode::Down => {
                if let Some(picker) = self.effort_picker.as_mut()
                    && count > 0
                {
                    picker.selection = if code == KeyCode::Up {
                        (picker.selection + count - 1) % count
                    } else {
                        (picker.selection + 1) % count
                    };
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if index < count {
                    self.commit_effort_row(index);
                }
            }
            KeyCode::Enter => {
                let Some(selection) = self.effort_picker.as_ref().map(|picker| picker.selection)
                else {
                    return;
                };
                if self
                    .effort_picker
                    .as_ref()
                    .is_some_and(|picker| picker.pending.is_some())
                {
                    return;
                }
                self.commit_effort_row(selection);
            }
            _ => {}
        }
    }

    /// Commits one picker row as the receipted selection.
    pub fn commit_effort_row(&mut self, index: usize) {
        let Some(row) = self.effort_picker_rows().into_iter().nth(index) else {
            return;
        };
        self.request_effort(row.effort);
    }

    /// Issues the receipted effort selection (live) or fabricates locally
    /// (demo / launcher default identity).
    fn request_effort(&mut self, effort: Option<String>) {
        self.dirty = true;
        let live_session = (!self.mode.fabricates_locally())
            .then(|| self.active_session.clone())
            .flatten();
        let Some(session) = live_session else {
            self.apply_effort_selected(effort.as_deref());
            return;
        };
        if !self.daemon_serves(haider_rpc::FEATURE_SESSION_EFFORT_SELECT_V1) {
            let note = self.stale_daemon_note("effort selection");
            if let Some(picker) = self.effort_picker.as_mut() {
                picker.error = Some(note.trim_start_matches("· ").to_owned());
            } else {
                self.flash = Some(note);
            }
            return;
        }
        if let Some(picker) = self.effort_picker.as_mut() {
            picker.pending = Some(effort.clone());
            picker.error = None;
        }
        self.requests
            .push(AppRequest::SelectEffort { session, effort });
    }

    /// The RESOLVED effort committed (G3/R2): render daemon truth. On
    /// anthropic pairs the flash notes the prompt-cache re-warm (decision
    /// 5 — changing effort invalidates the prompt cache).
    pub fn apply_effort_selected(&mut self, effort: Option<&str>) {
        self.identity.reasoning = effort.map(str::to_owned);
        self.effort_picker = None;
        let label = effort.unwrap_or("default");
        self.flash = Some(if self.identity.provider.starts_with("anthropic") {
            format!("· effort → {label} · cache re-warm")
        } else {
            format!("· effort → {label}")
        });
        self.dirty = true;
    }

    /// The committed fast toggle (G3/R2): render daemon truth.
    pub fn apply_fast_selected(&mut self, enabled: bool) {
        self.identity.fast = enabled;
        let state = if enabled { "on" } else { "off" };
        self.flash = Some(if self.identity.provider.starts_with("anthropic") {
            format!("· fast → {state} · cache re-warm")
        } else {
            format!("· fast → {state}")
        });
        self.dirty = true;
    }

    /// A typed `session.select_effort` refusal: inline when the picker is
    /// open, an error line + flash otherwise — never a silent no-op.
    pub fn effort_select_failed(&mut self, code: &str, message: &str) {
        let reason = format!("{code}: {message}");
        if let Some(picker) = self.effort_picker.as_mut() {
            picker.pending = None;
            picker.error = Some(reason);
        } else {
            self.projection
                .record_local_error(format!("effort selection failed — {reason}"));
            self.flash = Some(format!("· effort selection failed — {code}"));
        }
        self.dirty = true;
    }

    /// A typed `session.select_fast` refusal.
    pub fn fast_select_failed(&mut self, code: &str, message: &str) {
        self.projection
            .record_local_error(format!("fast-mode selection failed — {code}: {message}"));
        self.flash = Some(format!("· fast-mode selection failed — {code}"));
        self.dirty = true;
    }

    fn open_theme_picker(&mut self) {
        self.dirty = true;
        if !matches!(
            self.screen,
            Screen::Launcher | Screen::Session | Screen::Aura | Screen::Subagent
        ) {
            self.flash = Some(
                "· /theme — pick by name here: /theme system · light · dark · desert · oasis"
                    .to_owned(),
            );
            return;
        }
        if self.screen == Screen::Session && self.projection.open_menu().is_some() {
            self.flash = Some("· /theme — answer the open card first".to_owned());
            return;
        }
        if self.screen == Screen::Subagent
            && self
                .viewed_chip()
                .is_some_and(|chip| chip.question_menu().is_some())
        {
            self.flash = Some("· /theme — answer the open card first".to_owned());
            return;
        }
        let selection = ThemeChoice::MENU
            .iter()
            .position(|choice| *choice == self.theme_choice)
            .unwrap_or(0);
        self.theme_picker = Some(ThemePicker {
            selection,
            prior: self.theme_choice,
        });
    }

    /// Picker keys: ↑↓ move AND PREVIEW instantly (owner: "applies
    /// instantly"), digits/⏎ commit, esc reverts to the choice on open —
    /// the session-scoped esc law: the innermost surface answers first.
    fn handle_theme_picker_key(&mut self, code: KeyCode) {
        let Some(picker) = self.theme_picker else {
            return;
        };
        let count = ThemeChoice::MENU.len();
        self.dirty = true;
        match code {
            KeyCode::Esc => {
                self.theme = picker.prior.resolve(self.detected_system);
                self.theme_picker = None;
            }
            KeyCode::Up => self.preview_theme_row((picker.selection + count - 1) % count),
            KeyCode::Down => self.preview_theme_row((picker.selection + 1) % count),
            KeyCode::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if index < count {
                    self.commit_theme_row(index);
                }
            }
            KeyCode::Enter => self.commit_theme_row(picker.selection),
            _ => {}
        }
    }

    /// Move the highlight and preview: the resolved theme flips with the
    /// row; the committed choice (and persistence) wait for a commit.
    fn preview_theme_row(&mut self, index: usize) {
        if index < ThemeChoice::MENU.len()
            && let Some(picker) = &mut self.theme_picker
        {
            picker.selection = index;
            self.theme = ThemeChoice::MENU[index].resolve(self.detected_system);
            self.dirty = true;
        }
    }

    fn commit_theme_row(&mut self, index: usize) {
        if index >= ThemeChoice::MENU.len() {
            return;
        }
        self.theme_picker = None;
        self.commit_theme_choice(ThemeChoice::MENU[index]);
        self.flash = Some(self.theme_flash());
    }
}

/// Command-card id prefixes — each open mints `{prefix}{seq}` so a stale
/// answer can never drive a later card's consequences (review r2 P1-1).
/// How many sessions the demo world seeds (sim tui.js:497-579). The
/// generation allocator advances by exactly this on every reseed.
pub const SEED_SESSION_COUNT: u64 = 3;

/// How many launcher rows LIVE mode shows. Owner ask (2026-07-31): the
/// nine-row digit span buried the launcher under old sessions — FOUR
/// recents keep it scannable, `/sessions` lists the rest. Digits `1`-`4`
/// still reach every painted row.
pub const LIVE_LAUNCHER_ROWS: usize = 4;

pub const VOICE_CARD_PREFIX: &str = "voice-card-";
pub const TOOLS_CARD_PREFIX: &str = "tools-card-";
/// The `/branch` picker's id prefix (B2b m2). Cards with this prefix are
/// REDUCER-LOCAL: their answer switches the displayed branch and closes
/// the card without touching the outbox.
pub const BRANCH_CARD_PREFIX: &str = "branch-card-";

/// The `/branch` picker (B2b m2): main plus every named branch, numbered,
/// the ACTIVE one marked ● and the rest ○ (the sim's branch vocabulary,
/// tui.js:3366-3427). Non-blocking Choice card; esc dismisses.
#[must_use]
pub fn branch_card(state: &crate::branch::BranchState, seq: u64) -> Menu {
    let marker = |active: bool| if active { '●' } else { '○' };
    let mut options = vec![card_option(
        "main",
        format!("{} main", marker(state.active().is_none())),
    )];
    for descriptor in state.descriptors() {
        options.push(card_option(
            descriptor.branch_id.as_str(),
            format!(
                "{} {}",
                marker(state.active() == Some(&descriptor.branch_id)),
                descriptor.name
            ),
        ));
    }
    Menu {
        id: MenuId::new(format!("{BRANCH_CARD_PREFIX}{seq}")),
        kind: MenuKind::Choice,
        title: "branch — switch the displayed branch".to_owned(),
        body: vec![
            "switch   every branch stays warm — switching is instant, nothing rewinds".to_owned(),
            "fork     /branch new [name] forks at the last committed node (idle only)".to_owned(),
        ],
        options,
        blocking: false,
        scope: MenuScope::Session,
        origin: "branch".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }
}

fn card_option(key: &str, label: String) -> MenuOption {
    MenuOption {
        key: key.to_owned(),
        label,
        detail: None,
        decision: None,
    }
}

/// The `/voice` menu card (sim tui.js:1824-1864, verbatim body/options).
/// Non-blocking Choice card; `origin: "voice"` selects the ◉ glyph.
#[must_use]
pub fn voice_card(voice: &VoiceState, seq: u64) -> Menu {
    let last = if voice.enabled {
        "disable voice"
    } else {
        "keep voice off"
    };
    Menu {
        id: MenuId::new(format!("{VOICE_CARD_PREFIX}{seq}")),
        kind: MenuKind::Choice,
        title: "voice — enable duplex speech for this session".to_owned(),
        body: vec![
            "input    STT provider transcribes mic → a normal user turn".to_owned(),
            "output   TTS provider speaks each assistant turn".to_owned(),
            "duplex   gpt-realtime handles both natively (barge-in, no round-trip)".to_owned(),
            "privacy  audio streams to the chosen provider only — never to the mesh".to_owned(),
        ],
        options: vec![
            card_option("whisper", "enable — Whisper STT · OpenAI TTS".to_owned()),
            card_option(
                "deepgram",
                "enable — Deepgram STT · ElevenLabs TTS".to_owned(),
            ),
            card_option(
                "realtime",
                "enable — gpt-realtime (native duplex STT+TTS)".to_owned(),
            ),
            card_option("off", last.to_owned()),
        ],
        blocking: false,
        scope: MenuScope::Session,
        origin: "voice".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }
}

/// The `/tools` menu card (sim tui.js:1876-1906, verbatim body/options).
/// Non-blocking Choice card; `origin: "tools"` selects the ⚒ glyph.
#[must_use]
pub fn tools_card(seq: u64) -> Menu {
    Menu {
        id: MenuId::new(format!("{TOOLS_CARD_PREFIX}{seq}")),
        kind: MenuKind::Choice,
        title: "tools — core surface + custom tools".to_owned(),
        body: vec![
            "core     fs_read fs_patch process_exec agent_spawn request_input … (13, always on)"
                .to_owned(),
            "custom   notify_slack (fire-and-forget) · preview_deploy (await) · preview_smoke (deferred)"
                .to_owned(),
            "dispatch each custom tool declares a mode: how the turn treats its result".to_owned(),
            "register adding a tool is itself a menu-answerable action — a remote agent can provision another"
                .to_owned(),
        ],
        options: vec![
            card_option(
                "fire",
                "register a custom tool — fire-and-forget (dispatch, never block)".to_owned(),
            ),
            card_option(
                "await",
                "register a custom tool — await (block the turn for the result)".to_owned(),
            ),
            card_option(
                "deferred",
                "register a custom tool — deferred (returns a ticket, calls back later)".to_owned(),
            ),
            card_option("close", "close".to_owned()),
        ],
        blocking: false,
        scope: MenuScope::Session,
        origin: "tools".to_owned(),
        ttl_ms: None,
        timeout_option: None,
    }
}
