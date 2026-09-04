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
use haider_protocol::error::ErrorAction;
use haider_protocol::ids::{MenuId, SessionId};
use haider_protocol::menu::{
    AnswerVia, Menu, MenuAnswer, MenuCloseReason, MenuKind, MenuOption, MenuScope,
};
use haider_protocol::state::{HarnessStatus, RunState};
use haider_protocol::{DeliveryMode, EventPayload};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{BTreeMap, BTreeSet, HashSet};

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
const BACKTRACK_ESC_WINDOW: std::time::Duration = std::time::Duration::from_millis(750);

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
    /// `/resume` — every session on this machine in one browsable list,
    /// rendered from roster truth alone (unseen dot + needs-you chip), so a
    /// parked or unread session is impossible to miss (owner 2026-08-21).
    Sessions,
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
    /// The fleet view (slice 1, mockup `FleetStage`) — SESSION-BORN, never
    /// a menu destination: ⌥F / the collapsed subagents summary row opens
    /// it for the current session. Backed by `session.fleet` in live mode
    /// ([`crate::fleet`]); demo synthesizes from the local chip tree.
    Fleet,
    /// The Convergence Graph status view (CG-M1) — SESSION-SCOPED, opened by
    /// `/graph`. Backed by `graph.status` in live mode; demo has no graph
    /// truth and `/graph` refuses honestly there.
    Graph,
    /// D3 — the Loom registry browser (`/loom`): agent types (capability-
    /// scoped specialists) and pipe workflows, from the once-per-connection
    /// loom.list snapshot, plus the feature-gated typed authoring RPC flow.
    Loom,
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
    pub account_identity: Option<haider_protocol::credential::AccountIdentity>,
    pub created_at_ms: Option<u64>,
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
            account_identity: descriptor.account_identity.clone(),
            created_at_ms: descriptor.created_at_ms,
            status: descriptor.status.clone(),
            selected: descriptor.active,
            base_url: descriptor.base_url.clone(),
        }
    }
}

/// Secret-free source metadata paired with an account descriptor by alias.
/// This is a TUI projection rather than the wire type, matching `AccountRow`:
/// rendering never depends on transport-only fields or credential material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSourceRow {
    pub source_id: String,
    pub account_alias: Option<haider_protocol::ids::CredentialAlias>,
    pub kind: String,
    pub label: String,
    pub path: Option<String>,
    pub credential_store: String,
    pub refresh_owner: String,
    pub health: String,
    pub last_seen_at_ms: Option<u64>,
    pub last_refreshed_at_ms: Option<u64>,
    pub access_expires_at_ms: Option<u64>,
    pub plan: Option<String>,
    pub masked_identity: Option<String>,
}

/// Probe fixtures use only these whole-alias shapes: `probefix`,
/// `probefix-api[-N]`, or `probe<PID>-api`. Keeping the match anchored avoids
/// hiding legitimate aliases that merely contain the word `probe`.
#[must_use]
pub fn is_probe_account_alias(alias: &str) -> bool {
    if matches!(alias, "probefix" | "probefix-api") {
        return true;
    }
    if let Some(index) = alias.strip_prefix("probefix-api-") {
        return canonical_positive_decimal(index) && index != "1";
    }
    alias
        .strip_prefix("probe")
        .and_then(|suffix| suffix.strip_suffix("-api"))
        .is_some_and(canonical_positive_decimal)
}

fn canonical_positive_decimal(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(|first| matches!(first, b'1'..=b'9'))
        && value.as_bytes()[1..].iter().all(u8::is_ascii_digit)
}

/// The `/accounts` screen state. OPTIMISTIC SELECTION IS FORBIDDEN (report
/// §5.1): the dot moves only when a correlated daemon result or a
/// newer-revision snapshot applies — never on click.
#[derive(Debug, Default)]
pub struct AccountsState {
    pub rows: Vec<AccountRow>,
    /// Secret-free credential-source metadata from the same `account.list`
    /// snapshot. Entries join to rows by stable alias; alias-less/unmatched
    /// enrolled roots remain visible as sources without linked accounts.
    pub sources: Vec<AccountSourceRow>,
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
    /// P1 MASK LAW (the U2 owner addendum extended): row identities render
    /// MASKED unless this is set.
    /// `r` toggles it for the CURRENT visit only — the one door in
    /// ([`AppModel::enter_accounts`]) and the esc exit both reset to
    /// masked, so the screen never OPENS revealed (the U2 ⌃C lesson: the
    /// enter-door reset covers exits that bypass `exit_accounts`).
    pub revealed: bool,
    /// One explicit local-login copy awaiting y/n. Candidate ids are opaque;
    /// no credential bytes ever enter TUI state.
    pub adoption_candidate: Option<haider_rpc::DeviceCredentialCandidateWire>,
    /// Notice identity already shown on this TUI session.
    pub adoption_noticed: HashSet<String>,
}

/// Round 4 — a plan proposal's identity for scroll-reset purposes: menu id
/// plus a CONTENT hash of the body. Byte-length alone let a same-id,
/// same-length body swap keep the old offset.
#[must_use]
pub fn plan_menu_key(menu: &haider_protocol::menu::Menu) -> (MenuId, u64) {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    menu.body.hash(&mut hasher);
    (menu.id.clone(), hasher.finish())
}

impl AppModel {
    /// D1 — resolve a Loom agent type by id (chip coloring, graph rows).
    #[must_use]
    pub fn loom_type(&self, id: &str) -> Option<&haider_protocol::loom::LoomAgentType> {
        self.loom_types.iter().find(|record| record.id == id)
    }

    /// D1/fleet — split a daemon-stamped `@type · rest` task label against
    /// the loom snapshot. ONE trust gate for every surface that paints the
    /// specialist accent (subtree chips, fleet rows): the `@type · ` prefix
    /// is daemon truth only on a Loom-aware daemon (C3 strips cosplay
    /// there); an old daemon passes task text through verbatim, so nothing
    /// it sends earns the paint. `None` also when the type is not in the
    /// snapshot — the caller falls back to its plain styling.
    #[must_use]
    pub fn loom_task_type<'a>(
        &'a self,
        task: &'a str,
    ) -> Option<(&'a haider_protocol::loom::LoomAgentType, &'a str)> {
        self.daemon_serves(haider_rpc::FEATURE_LOOM_V1)
            .then_some(task)
            .and_then(|task| task.strip_prefix('@'))
            .and_then(|rest| rest.split_once(" · "))
            .and_then(|(type_id, remainder)| {
                self.loom_type(type_id).map(|record| (record, remainder))
            })
    }

    /// The MAIN-session built-ins published by this connection's daemon.
    /// Unknown origins and child-only entries carry no row authority here.
    /// Feature absence leaves the list empty; the TUI never substitutes its
    /// linked protocol crate's local catalog.
    #[must_use]
    pub fn builtin_workflow_templates(&self) -> Vec<haider_protocol::graph::GraphTemplateSpec> {
        if !self.daemon_serves(haider_rpc::FEATURE_WORKFLOW_CATALOG_V1) {
            return Vec::new();
        }
        self.workflow_catalog
            .iter()
            .filter_map(|entry| match entry {
                haider_rpc::WorkflowCatalogEntryV1::BuiltIn {
                    main_session_eligible: true,
                    template,
                    ..
                } => Some(template.clone()),
                _ => None,
            })
            .collect()
    }

    /// Total /workflows rows: `none` + built-ins + registered while the
    /// catalog feature is present; zero is typed catalog unavailability.
    #[must_use]
    pub fn workflow_row_count(&self) -> usize {
        if !self.daemon_serves(haider_rpc::FEATURE_WORKFLOW_CATALOG_V1) {
            return 0;
        }
        1 + self.builtin_workflow_templates().len() + self.loom_workflows.len()
    }

    /// W-flow — resolve one /workflows selection index into its row.
    #[must_use]
    pub fn workflow_row(&self, index: usize) -> Option<WorkflowRow> {
        if !self.daemon_serves(haider_rpc::FEATURE_WORKFLOW_CATALOG_V1) {
            return None;
        }
        if index == 0 {
            return Some(WorkflowRow::None);
        }
        let builtins = self.builtin_workflow_templates();
        if let Some(template) = builtins.get(index - 1) {
            return Some(WorkflowRow::BuiltIn(template.clone()));
        }
        let registered = index - 1 - builtins.len();
        (registered < self.loom_workflows.len()).then_some(WorkflowRow::Registered(registered))
    }

    /// Row index for one catalog workflow id, excluding the synthetic `none`.
    #[must_use]
    pub fn workflow_row_index(&self, workflow_id: &str) -> Option<usize> {
        if !self.daemon_serves(haider_rpc::FEATURE_WORKFLOW_CATALOG_V1) {
            return None;
        }
        let builtins = self.builtin_workflow_templates();
        if let Some(index) = builtins
            .iter()
            .position(|template| template.name == workflow_id)
        {
            return Some(index + 1);
        }
        self.loom_workflows
            .iter()
            .position(|workflow| workflow.id == workflow_id)
            .map(|index| index + 1 + builtins.len())
    }

    /// Open the selected workflow's next rejected node. Repeated Enter cycles
    /// stable runtime order, so every concurrent reject remains reachable.
    /// Evidence-less rejects still open with their typed reason and journal
    /// cursor; the inspector never guesses across a workflow id.
    fn open_selected_workflow_rejection_evidence(&mut self) -> bool {
        if self.loom_pane != LoomPane::Workflows || !self.loom_detail {
            return false;
        }
        let selected_workflow_id = match self.workflow_row(self.loom_selection) {
            Some(WorkflowRow::BuiltIn(template)) => Some(template.name),
            Some(WorkflowRow::Registered(index)) => self
                .loom_workflows
                .get(index)
                .map(|workflow| workflow.id.clone()),
            Some(WorkflowRow::None) | None => None,
        };
        if self.workflow_graph.workflow_id() != selected_workflow_id.as_deref() {
            return false;
        }
        let rejected = self
            .workflow_graph
            .nodes()
            .filter_map(|node| {
                self.workflow_graph
                    .rejection(&node.node_id)
                    .map(|rejection| (node.node_id.clone(), rejection.clone()))
            })
            .collect::<Vec<_>>();
        if rejected.is_empty() {
            return false;
        }
        let next = self
            .workflow_evidence_inspection
            .as_ref()
            .and_then(|open| {
                rejected
                    .iter()
                    .position(|(node_id, _)| node_id == &open.node_id)
            })
            .map_or(0, |index| (index + 1) % rejected.len());
        let (node_id, rejection) = &rejected[next];
        let inspection = WorkflowEvidenceInspection {
            node_id: node_id.clone(),
            code: rejection.code_label().to_owned(),
            message: rejection.message.clone(),
            cursor: rejection.cursor,
            reference: rejection
                .evidence
                .as_ref()
                .map(|reference| reference.as_str().to_owned()),
        };
        self.workflow_evidence_inspection = Some(inspection);
        true
    }

    /// W-flow inline identity — total /loom (Types) rows: `none` +
    /// registered. Never zero; the types pane lost its whole-pane empty
    /// state exactly as the workflows pane did.
    #[must_use]
    pub fn type_row_count(&self) -> usize {
        1 + self.loom_types.len()
    }

    /// W-flow inline identity — resolve one /loom selection index.
    #[must_use]
    pub fn type_row(&self, index: usize) -> Option<TypeRow> {
        if index == 0 {
            return Some(TypeRow::None);
        }
        (index - 1 < self.loom_types.len()).then_some(TypeRow::Registered(index - 1))
    }

    /// W-flow inline identity — the loom record behind a BOUND agent-type
    /// id, for the session-accent surfaces (header callsign, composer
    /// identity rule, roster rows). ONE fallback law: no binding, no
    /// snapshot entry, or an un-Loom daemon → `None`, and the caller keeps
    /// today's default styling exactly (a stale accent is never painted —
    /// the snapshot dies with the socket, so the gate re-judges every
    /// frame).
    #[must_use]
    pub fn bound_loom_type(
        &self,
        agent_type: Option<&str>,
    ) -> Option<&haider_protocol::loom::LoomAgentType> {
        let id = agent_type?;
        self.daemon_serves(haider_rpc::FEATURE_LOOM_V1)
            .then(|| self.loom_type(id))
            .flatten()
    }

    /// D2 — the Loom workflow behind a pinned template, if the registry
    /// still holds the PINNED revision. Review round 2: the join requires
    /// the instance digest — annotating graph A with re-registered B's
    /// tasks/specialists would be UI fiction, so drifted registries render
    /// no Loom annotations at all.
    #[must_use]
    pub fn loom_workflow_meta(
        &self,
        template: &str,
        digest: &str,
    ) -> Option<&haider_protocol::loom::LoomWorkflow> {
        self.loom_workflows.iter().find(|record| {
            record.id == template
                && haider_protocol::graph::graph_template_digest(&record.template) == digest
        })
    }

    /// Whether the connected daemon serves a method family. Demo mode
    /// answers everything locally, so it is always capable there.
    #[must_use]
    pub fn daemon_serves(&self, feature: &str) -> bool {
        self.mode.fabricates_locally() || self.daemon_features.contains(feature)
    }

    /// Direct user commands require the same complete semantic surface as
    /// the typed client: admission, provenance/context, and cancellation.
    #[must_use]
    fn daemon_serves_user_commands(&self) -> bool {
        self.mode.fabricates_locally()
            || haider_client::required_user_command_features().is_subset(&self.daemon_features)
    }

    /// Whether the connected daemon can report local-login adoption offers.
    /// Discovery is metadata-only; confirmation owns the separate import.
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
        self.rows = rows
            .into_iter()
            .filter(|row| !is_probe_account_alias(&row.alias))
            .collect();
        if revision.is_some() {
            self.revision = revision;
        }
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
        true
    }

    /// Installs the source half of an accepted `account.list` snapshot.
    /// Kept separate from `apply_snapshot` so legacy/demo callers retain
    /// their established account-row fixture seam.
    pub fn apply_sources(&mut self, sources: Vec<AccountSourceRow>) {
        self.sources = sources;
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
    /// Generic discovery-backed cards choose bearer-key versus no auth.
    Auth,
    /// Generic discovery-backed cards choose OpenAI-compatible versus
    /// Anthropic Messages transport.
    ApiFamily,
    /// The generic card's API key. Its value is never exposed through the
    /// field-value helpers or renderer; only its capped mask length is used.
    Key,
    /// G4b: the vertex card's second coordinate (location); unused by
    /// every other card kind.
    Extra,
}

/// Which provider surface the card configures (G4b). `Generic` is the
/// pre-G4b custom/preset card byte-for-byte; the enterprise kinds relabel
/// the fields and reshape the submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomCardKind {
    /// OpenAI-compatible custom/preset (name + origin + model).
    Generic,
    /// Azure OpenAI v1: name + resource endpoint + DEPLOYMENT name; submit
    /// derives `{endpoint}/openai/v1` and chains the key card.
    Azure,
    /// Bedrock mantle: the `origin` field holds the REGION; submit builds
    /// the mantle URL and echoes the seeded model inventory.
    Bedrock,
    /// Claude on Vertex: `origin` holds the PROJECT ID, `extra` the
    /// LOCATION (default `global`).
    Vertex,
}

/// Where the `+ Add custom server` card is in its flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomPhase {
    /// Typing name/origin (also the retype state after a failure — the
    /// error line renders above the still-editable fields).
    Editing { error: Option<String> },
    /// `provider.configure` is in flight.
    Submitting,
}

/// The `+ Add custom server` card (sim tui.js:3629-3682).
///
/// The DEMO card is the sim's verbatim MenuBox — info lines and a fixed
/// `[1] add http://127.0.0.1:8000/v1 (demo)`. The EDITABLE name/origin
/// fields are the live extension (report §4.4: "custom provider rows are
/// created/edited through provider.configure" — the sim only fabricates).
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
    /// Caret offset in the focused field, counted in Unicode scalar values
    /// rather than bytes. Every edit converts this to a byte boundary only
    /// at the final `String` operation, so UTF-8 is never split.
    pub cursor: usize,
    pub phase: CustomPhase,
    /// Attempt identity (the card discipline): every driver reply must
    /// correlate to it or die silently.
    pub attempt: u64,
    /// W10b: editing an EXISTING provider — the provider NAME is the locked
    /// identity; origin and model are mutable. Create cards keep all of their
    /// prefilled fields editable.
    pub edit: bool,
    /// G4a: an auth-None (keyless) preset — the configure carries
    /// `auth_requirement: none` and commit SKIPS the key card, going
    /// straight to model discovery.
    pub keyless: bool,
    /// API family selected for a discovery-backed generic provider. Presets
    /// and enterprise cards retain their fixed family.
    pub family: haider_rpc::ProviderApiFamilyWire,
    /// `true` only for the user-authored custom-server card. Unlike legacy
    /// presets, this shape leaves the inventory empty so the daemon probes
    /// the server and publishes its live `/v1/models` list.
    pub discover_models: bool,
    /// Raw API key while the custom card is being edited. This is the same
    /// zeroize-on-drop boundary as [`LoginCard`]: Debug is redacted, render
    /// receives only [`Self::masked_key_len`], and submit takes/wipes it.
    secret: zeroize::Zeroizing<String>,
    /// G4b: which provider surface this card configures.
    pub kind: CustomCardKind,
    /// G4b: the vertex LOCATION field; empty for every other kind.
    pub extra: String,
}

impl CustomProviderCard {
    fn has_field(&self, field: CustomField) -> bool {
        if self.kind == CustomCardKind::Generic && self.discover_models {
            return matches!(
                field,
                CustomField::Name
                    | CustomField::Origin
                    | CustomField::Auth
                    | CustomField::ApiFamily
            ) || (!self.keyless && field == CustomField::Key);
        }
        matches!(
            (self.kind, field),
            (
                CustomCardKind::Generic | CustomCardKind::Azure,
                CustomField::Name | CustomField::Origin | CustomField::Model
            ) | (CustomCardKind::Bedrock, CustomField::Origin)
                | (
                    CustomCardKind::Vertex,
                    CustomField::Origin | CustomField::Extra
                )
        )
    }

    /// The single field-lock authority used by keyboard, paste, mouse, and
    /// rendering. `provider` is the update identity, so edit-mode Name never
    /// reaches a mutation operation; create-mode prefills remain ordinary
    /// editable values.
    pub(crate) fn can_edit_field(&self, field: CustomField) -> bool {
        matches!(self.phase, CustomPhase::Editing { .. })
            && self.has_field(field)
            && !(self.edit && field == CustomField::Name)
    }

    fn field_value(&self, field: CustomField) -> Option<&str> {
        match field {
            CustomField::Name => Some(&self.name),
            CustomField::Origin => Some(&self.origin),
            CustomField::Model => Some(&self.model),
            CustomField::Extra => Some(&self.extra),
            CustomField::Auth | CustomField::ApiFamily | CustomField::Key => None,
        }
    }

    fn field_value_mut(&mut self, field: CustomField) -> Option<&mut String> {
        match field {
            CustomField::Name => Some(&mut self.name),
            CustomField::Origin => Some(&mut self.origin),
            CustomField::Model => Some(&mut self.model),
            CustomField::Extra => Some(&mut self.extra),
            CustomField::Auth | CustomField::ApiFamily | CustomField::Key => None,
        }
    }

    fn focus_at(&mut self, field: CustomField, character: usize) -> bool {
        if !self.can_edit_field(field) {
            return false;
        }
        self.focus = field;
        self.cursor = self
            .field_value(field)
            .map_or(0, |value| character.min(value.chars().count()));
        true
    }

    fn focus_end(&mut self, field: CustomField) {
        self.focus = field;
        self.cursor = self
            .field_value(field)
            .map_or(0, |value| value.chars().count());
    }

    fn insert_char(&mut self, character: char) -> bool {
        if !self.can_edit_field(self.focus) {
            return false;
        }
        let Some(value) = self.field_value(self.focus) else {
            return false;
        };
        let cursor = self.cursor.min(value.chars().count());
        let byte = byte_index_at_character(value, cursor);
        let Some(value) = self.field_value_mut(self.focus) else {
            return false;
        };
        value.insert(byte, character);
        self.cursor = cursor + 1;
        true
    }

    fn backspace(&mut self) -> bool {
        if !self.can_edit_field(self.focus) || self.cursor == 0 {
            return false;
        }
        let Some(value) = self.field_value(self.focus) else {
            return false;
        };
        let cursor = self.cursor.min(value.chars().count());
        if cursor == 0 {
            return false;
        }
        let start = byte_index_at_character(value, cursor - 1);
        let end = byte_index_at_character(value, cursor);
        let Some(value) = self.field_value_mut(self.focus) else {
            return false;
        };
        value.replace_range(start..end, "");
        self.cursor = cursor - 1;
        true
    }

    fn delete_forward(&mut self) -> bool {
        if !self.can_edit_field(self.focus) {
            return false;
        }
        let Some(value) = self.field_value(self.focus) else {
            return false;
        };
        let characters = value.chars().count();
        let cursor = self.cursor.min(characters);
        if cursor == characters {
            return false;
        }
        let start = byte_index_at_character(value, cursor);
        let end = byte_index_at_character(value, cursor + 1);
        let Some(value) = self.field_value_mut(self.focus) else {
            return false;
        };
        value.replace_range(start..end, "");
        self.cursor = cursor;
        true
    }

    fn move_left(&mut self) -> bool {
        if !self.can_edit_field(self.focus) || self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    fn move_right(&mut self) -> bool {
        if !self.can_edit_field(self.focus) {
            return false;
        }
        let Some(value) = self.field_value(self.focus) else {
            return false;
        };
        let end = value.chars().count();
        if self.cursor >= end {
            return false;
        }
        self.cursor += 1;
        true
    }

    fn cycle_choice(&mut self) -> bool {
        if !self.can_edit_field(self.focus) {
            return false;
        }
        match self.focus {
            CustomField::Auth => {
                self.keyless = !self.keyless;
                if self.keyless {
                    zeroize::Zeroize::zeroize(&mut *self.secret);
                }
            }
            CustomField::ApiFamily => {
                self.family = if matches!(
                    self.family,
                    haider_rpc::ProviderApiFamilyWire::AnthropicMessages
                ) {
                    haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions
                } else {
                    haider_rpc::ProviderApiFamilyWire::AnthropicMessages
                };
            }
            _ => return false,
        }
        true
    }

    fn move_focus(&mut self, backwards: bool) {
        const GENERIC_DISCOVERY_KEYED: &[CustomField] = &[
            CustomField::Name,
            CustomField::Origin,
            CustomField::Auth,
            CustomField::ApiFamily,
            CustomField::Key,
        ];
        const GENERIC_DISCOVERY_KEYLESS: &[CustomField] = &[
            CustomField::Name,
            CustomField::Origin,
            CustomField::Auth,
            CustomField::ApiFamily,
        ];
        const GENERIC_CREATE: &[CustomField] =
            &[CustomField::Name, CustomField::Origin, CustomField::Model];
        const GENERIC_EDIT: &[CustomField] = &[CustomField::Origin, CustomField::Model];
        const BEDROCK: &[CustomField] = &[CustomField::Origin];
        const VERTEX: &[CustomField] = &[CustomField::Origin, CustomField::Extra];

        let fields = match (self.kind, self.discover_models, self.edit, self.keyless) {
            (CustomCardKind::Generic, true, _, false) => GENERIC_DISCOVERY_KEYED,
            (CustomCardKind::Generic, true, _, true) => GENERIC_DISCOVERY_KEYLESS,
            (CustomCardKind::Generic | CustomCardKind::Azure, false, true, _) => GENERIC_EDIT,
            (CustomCardKind::Generic | CustomCardKind::Azure, false, false, _) => GENERIC_CREATE,
            (CustomCardKind::Bedrock, _, _, _) => BEDROCK,
            (CustomCardKind::Vertex, _, _, _) => VERTEX,
            (CustomCardKind::Azure, true, _, _) => GENERIC_CREATE,
        };
        let current = fields
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0);
        let next = if backwards {
            current.checked_sub(1).unwrap_or(fields.len() - 1)
        } else {
            (current + 1) % fields.len()
        };
        self.focus_end(fields[next]);
    }

    fn push_key(&mut self, character: char) -> bool {
        if self.focus != CustomField::Key
            || !self.can_edit_field(CustomField::Key)
            || character.is_control()
        {
            return false;
        }
        self.secret.push(character);
        true
    }

    fn key_backspace(&mut self) -> bool {
        self.focus == CustomField::Key
            && self.can_edit_field(CustomField::Key)
            && self.secret.pop().is_some()
    }

    #[must_use]
    pub fn masked_key_len(&self) -> usize {
        self.secret.chars().count()
    }

    fn take_key(&mut self) -> haider_rpc::SecretWire {
        let value = std::mem::take(&mut *self.secret);
        haider_rpc::SecretWire::new(value)
    }
}

impl std::fmt::Debug for CustomProviderCard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CustomProviderCard")
            .field("name", &self.name)
            .field("origin", &self.origin)
            .field("model", &self.model)
            .field("focus", &self.focus)
            .field("phase", &self.phase)
            .field("attempt", &self.attempt)
            .field("edit", &self.edit)
            .field("keyless", &self.keyless)
            .field("family", &self.family)
            .field("discover_models", &self.discover_models)
            .field("secret", &"<redacted>")
            .field("kind", &self.kind)
            .field("extra", &self.extra)
            .finish()
    }
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

/// The composer queue panel (954): daemon-held mid-turn messages, listed
/// render-complete and maintained by `QueueChanged` deltas. The revision
/// rides every mutation; a `RevisionConflict` refusal names the current
/// revision and this state re-reads — it never guesses.
#[derive(Debug, Default)]
pub struct QueuePanelState {
    pub rows: Vec<haider_protocol::queue::QueueRow>,
    pub revision: Option<u64>,
    pub fetching: bool,
    pub error: Option<String>,
    /// A mode toggle in flight: remove committed, resubmit pending. The
    /// text is held HERE so a crash between the two legs cannot lose it
    /// silently — the error path renders it.
    pub pending_toggle: Option<(
        haider_protocol::ids::EventId,
        String,
        haider_protocol::DeliveryMode,
    )>,
}

impl QueuePanelState {
    /// Install a committed list snapshot. The ONLY full-state writer.
    pub fn apply_list(&mut self, rows: Vec<haider_protocol::queue::QueueRow>, revision: u64) {
        self.rows = rows;
        self.revision = Some(revision);
        self.fetching = false;
        self.error = None;
    }

    /// Apply one revision-bearing delta. Enqueued carries the COMPLETE row
    /// (the render-complete law), so the panel maintains itself without
    /// re-reads; an Unknown change means a newer daemon said something we
    /// cannot interpret — the honest response is a fresh list, requested
    /// by the caller when this returns true.
    pub fn apply_delta(&mut self, delta: &haider_protocol::queue::QueueDelta) -> bool {
        use haider_protocol::queue::QueueChange;
        self.revision = Some(delta.revision);
        match &delta.change {
            QueueChange::Enqueued { row } => {
                self.rows.retain(|held| held.id != row.id);
                self.rows.push(row.clone());
                false
            }
            QueueChange::Removed { id }
            | QueueChange::PromotedSteer { id }
            | QueueChange::Consumed { id } => {
                self.rows.retain(|held| held.id != *id);
                false
            }
            _ => true,
        }
    }

    /// A mutation was refused stale: record the daemon's CURRENT revision
    /// and drop any in-flight toggle (its leg-one premise is gone).
    pub fn conflicted(&mut self, current_revision: u64) {
        self.revision = Some(current_revision);
        self.pending_toggle = None;
    }

    /// A queue read/mutation failed: typed message, held rows stay.
    pub fn failed(&mut self, message: &str) {
        self.fetching = false;
        self.error = Some(message.to_owned());
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

/// `/usage` viewing scope (954 UI wave). `Accounts` is the full
/// per-provider detail (the U2 layout, unchanged); `Global` is the
/// cross-account aggregate: every meter at a glance plus summed local
/// journal counters. Summing is legitimate ONLY for same-unit local
/// facts — meters are never summed (different windows, different plans),
/// and partially-priced cost sums say so instead of understating
/// silently. `Models` is a range-selectable provider/model fold over the
/// same usage-history ledger RPC; no dead tab ships before its data source.
/// `Calendar` is a pure projection of the provider meter reset instants in
/// the held `usage.report`: it reads no client clock and invents no cadence.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum UsageScope {
    #[default]
    Accounts,
    Global,
    /// The usage-history heatmap (954 headline's UI): daily totals from
    /// the device-local ledger via `usage.history_range`.
    History,
    /// Per-model/provider rows folded over a selectable UTC ledger range.
    Models,
    /// Month grid of exact provider-published five-hour and weekly resets.
    Calendar,
}

impl UsageScope {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Accounts => "accounts",
            Self::Global => "global",
            Self::History => "history",
            Self::Models => "models",
            Self::Calendar => "calendar",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "accounts" => Some(Self::Accounts),
            "global" => Some(Self::Global),
            "history" => Some(Self::History),
            "models" => Some(Self::Models),
            "calendar" => Some(Self::Calendar),
            _ => None,
        }
    }

    /// The `s` key cycles scopes; the ring grows as ledger scopes land.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Accounts => Self::Calendar,
            Self::Calendar => Self::Global,
            Self::Global => Self::History,
            Self::History => Self::Models,
            Self::Models => Self::Accounts,
        }
    }
}

/// Ledger range folded by the Models scope. All-time is the complete
/// returned attribution lifetime (the ledger is newer than the protocol's
/// bounded history window).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum UsageModelRange {
    #[default]
    Today,
    SevenDays,
    ThirtyDays,
    AllTime,
}

impl UsageModelRange {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Today => Self::SevenDays,
            Self::SevenDays => Self::ThirtyDays,
            Self::ThirtyDays => Self::AllTime,
            Self::AllTime => Self::Today,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Today => "today (UTC)",
            Self::SevenDays => "7d",
            Self::ThirtyDays => "30d",
            Self::AllTime => "all-time",
        }
    }

    #[must_use]
    pub const fn days(self) -> Option<usize> {
        match self {
            Self::Today => Some(1),
            Self::SevenDays => Some(7),
            Self::ThirtyDays => Some(30),
            Self::AllTime => None,
        }
    }
}

/// The `/usage` screen state (U2). The report is U1's `usage.report`
/// snapshot CONSUMED whole — meter windows, typed unavailability, local
/// counters; nothing here re-derives or fabricates a reading.
#[derive(Debug, Default)]
pub struct UsageState {
    /// The active viewing scope. Bare `/usage` resets to `Accounts`; a
    /// direct scope argument selects its named destination.
    pub scope: UsageScope,
    /// The last committed `usage.report` snapshot. `None` until the first
    /// reply lands (live) — the demo never fabricates one.
    pub report: Option<haider_protocol::usage::UsageReportV1>,
    /// A report read is in flight (screen entry / `f`).
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
    /// The last committed `usage.history_range` window (954): dated cells
    /// where `total: None` is NO LOCAL SAMPLE for that date and a present
    /// all-zero total is a measured zero day — the ledger's absence law,
    /// carried into the model verbatim.
    pub history: Option<Vec<haider_protocol::usage::UsageHistoryRangeDayV1>>,
    /// A history read is in flight (History/Models scope entry / `f`).
    pub history_fetching: bool,
    /// The last history read's typed failure — rendered in the History
    /// scope, never flattened into an empty heatmap (the consumer-boundary
    /// law: a read failure is not absence).
    pub history_error: Option<String>,
    /// Today's committed `usage.history_day` (954 Models scope): the full
    /// day — key dictionary, sampled slots, roles — consumed verbatim.
    pub today: Option<haider_protocol::usage::UsageHistoryDayV1>,
    /// `None` day answer for today: the daemon answered "no file yet",
    /// which is a fact distinct from never-asked and from failed.
    pub today_absent: bool,
    pub today_fetching: bool,
    /// Typed failure for the day read — never flattened into absence.
    pub today_error: Option<String>,
    /// Models range; `r` cycles today → 7d → 30d → all-time.
    pub model_range: UsageModelRange,
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

    /// Install a committed `usage.history_range` window (954). The ONLY
    /// writer of `history`; clears the in-flight mark and any stale error.
    pub fn apply_history(&mut self, days: Vec<haider_protocol::usage::UsageHistoryRangeDayV1>) {
        self.history = Some(days);
        self.history_fetching = false;
        self.history_error = None;
    }

    /// A history read failed: typed message, held window stays visible —
    /// older truth under an error line, never an empty heatmap.
    pub fn history_failed(&mut self, message: &str) {
        self.history_fetching = false;
        self.history_error = Some(message.to_owned());
    }

    /// Install today's committed day answer (954 Models). `day: None` is
    /// the daemon's honest "no local file for today yet" — recorded as a
    /// FACT (`today_absent`), never conflated with a failed read.
    pub fn apply_today(&mut self, day: Option<haider_protocol::usage::UsageHistoryDayV1>) {
        self.today_absent = day.is_none();
        self.today = day;
        self.today_fetching = false;
        self.today_error = None;
    }

    /// The day read failed: typed message; held state stays.
    pub fn today_failed(&mut self, message: &str) {
        self.today_fetching = false;
        self.today_error = Some(message.to_owned());
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
    /// Daemon-stamped provider ceiling for this child.
    pub lockdown: bool,
    pub device: String,
    pub state: ChipDisplayState,
    pub tokens: u64,
    /// The child's own session id, from the manifest `coordinates`
    /// (`child_session_id` — the key W6d attaches the chip view by). The
    /// S4 row's token join reads it against the roster's session-summary
    /// truth; `None` (older daemon, demo seeds) never joins — a figure is
    /// never guessed off another row.
    pub child_session: Option<String>,
    /// Exact daemon-advertised parent handoff path. Display code may name
    /// its final directory component but never re-derive it.
    pub handoff_dir: Option<String>,
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
    /// Daemon-derived direct metrics, replaced only by a strictly newer
    /// child-session head. `None` preserves the legacy elapsed/token row.
    pub metrics: Option<haider_protocol::agent::AgentMetricsSnapshot>,
    pub question: Option<ChipQuestion>,
    /// Latest workflow-run rollup published by the delegation mirror
    /// (`agent_graph_rollup_v1`). `None` = the child runs no pinned
    /// workflow (or an older daemon) — the row keeps plain activity.
    pub graph: Option<haider_protocol::agent::AgentGraphRollupV1>,
    pub closed: bool,
    pub removing: bool,
    pub children: Vec<ChipModel>,
    pub transcript: SessionProjection,
    pub(crate) transcript_layout: std::cell::RefCell<crate::render::TranscriptLayoutCache>,
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
                                text: text.clone().into(),
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
            lockdown: false,
            device: seed.device,
            state: seed.state,
            tokens: seed.tokens,
            // Seeded chips carry no time base or child session: the mock's
            // pre-seeded history has no honest spawn instant, so the row
            // simply shows no elapsed. The demo driver's LIVE ChipAdd arm
            // stamps `spawned_at_ms` at creation instead.
            child_session: None,
            handoff_dir: None,
            spawned_at_ms: None,
            last_event_at_ms: None,
            metrics: None,
            question: None,
            graph: None,
            closed: false,
            removing: false,
            children: Vec::new(),
            transcript,
            transcript_layout: std::cell::RefCell::new(Default::default()),
        }
    }

    /// A chip built from a live `AgentSpawned` manifest (W3c3, report R11
    /// cut 2). The manifest is the ONLY source: `callsign` is display-only
    /// identity (§5.1 — never an address), `model_profile` is the model
    /// line. Placement is local-only. The chip starts IDLE because
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
            haider_protocol::agent::Placement::Device { .. } => {
                "not supported — local-only".to_owned()
            }
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
            lockdown: manifest
                .coordinates
                .as_ref()
                .and_then(|coordinates| coordinates.get("lockdown"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
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
            handoff_dir: manifest
                .coordinates
                .as_ref()
                .and_then(|coordinates| coordinates.get("handoff_dir"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            spawned_at_ms: None,
            last_event_at_ms: None,
            metrics: None,
            question: None,
            graph: None,
            closed: false,
            removing: false,
            children: Vec::new(),
            transcript: SessionProjection::new(),
            transcript_layout: std::cell::RefCell::new(Default::default()),
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

    /// Replace-by-source-head freshness. Equal delivery is an idempotent
    /// replay; a stale snapshot can never rewind live/settled state or totals.
    pub fn note_metrics(&mut self, metrics: haider_protocol::agent::AgentMetricsSnapshot) {
        if self
            .metrics
            .as_ref()
            .is_some_and(|held| held.head_seq >= metrics.head_seq)
        {
            return;
        }
        self.metrics = Some(metrics);
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
                haider_protocol::item::TurnItem::AgentMessage { text } => {
                    let (mut preview, truncated) = text.to_owned_prefix(52);
                    if truncated {
                        preview.push('…');
                    }
                    preview
                }
                _ => String::new(),
            },
            crate::projection::TranscriptEntry::User { text, .. } => text.clone(),
            crate::projection::TranscriptEntry::Peer { sender, text, .. } => {
                format!("peer {sender}: {text}")
            }
            crate::projection::TranscriptEntry::Note { text } => text.clone(),
            crate::projection::TranscriptEntry::Refusal {
                provider,
                tool,
                reason,
            } => format!("lockdown {provider} refused {tool}: {reason}"),
            crate::projection::TranscriptEntry::Error { text, .. } => format!("✗ {text}"),
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

// ---------------------------------------------------------------------------
// 970 monitorui — the monitor display vocabulary. ONE composition each for
// the source line, the state chip, and the fired note, so the overlay, the
// transcript and the tests can never word a monitor three different ways.
// ---------------------------------------------------------------------------

/// Bound applied to any command/summary excerpt on a monitor row.
const MONITOR_EXCERPT_CHARS: usize = 48;

fn monitor_excerpt(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let trimmed = flat.trim();
    if trimmed.chars().count() <= MONITOR_EXCERPT_CHARS {
        trimmed.to_owned()
    } else {
        let mut cut: String = trimmed.chars().take(MONITOR_EXCERPT_CHARS).collect();
        cut.push('…');
        cut
    }
}

/// A human interval: `60s`, `5m`, `2h`. Monitor intervals are coarse by
/// nature, so sub-second precision is deliberately dropped. The unit only
/// steps up PAST two of the next one, so the owner's `timer 60s` stays
/// `60s` rather than rounding into a `1m` nobody typed.
fn monitor_interval(interval_ms: u64) -> String {
    let seconds = interval_ms / 1000;
    if seconds < 120 {
        format!("{seconds}s")
    } else if seconds < 7200 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3600)
    }
}

/// What one monitor WATCHES, in one line: `timer 60s`,
/// `poll gh run 123 · until conclusion`, `cli codex exec …`, `file src/x.rs`.
/// The daemon's own `source_summary` wins when it sent one — this is the
/// client's honest fallback for the typed source it already holds.
#[must_use]
pub fn monitor_source_summary(monitor: &haider_rpc::MonitorRegistrationWire) -> String {
    if !monitor.source_summary.trim().is_empty() {
        return monitor_excerpt(&monitor.source_summary);
    }
    use haider_rpc::MonitorSourceWire as S;
    match &monitor.source {
        S::Sms => "sms".to_owned(),
        S::Process { command, .. } => format!("process {}", monitor_excerpt(command)),
        S::File { path } => format!("file {}", monitor_excerpt(path)),
        S::Poll {
            command,
            interval_ms,
            until,
            ..
        } => {
            let until = match until {
                haider_rpc::MonitorPollUntilWire::ExitCode { code } => {
                    format!("until exit {code}")
                }
                haider_rpc::MonitorPollUntilWire::StdoutMatches { pattern, .. } => {
                    format!("until /{}/", monitor_excerpt(pattern))
                }
                haider_rpc::MonitorPollUntilWire::StdoutChanged => "until changed".to_owned(),
                _ => "until ?".to_owned(),
            };
            format!(
                "poll {} · {} · {until}",
                monitor_excerpt(command),
                monitor_interval(*interval_ms)
            )
        }
        S::Timer { interval_ms } => format!("timer {}", monitor_interval(*interval_ms)),
        S::Cli { preset, argv, .. } => {
            let preset = match preset {
                haider_rpc::MonitorCliPresetWire::Codex => "codex",
                haider_rpc::MonitorCliPresetWire::ClaudeCode => "claude-code",
                haider_rpc::MonitorCliPresetWire::Opencode => "opencode",
                haider_rpc::MonitorCliPresetWire::Antigravity => "antigravity",
                haider_rpc::MonitorCliPresetWire::GhCi => "gh-ci",
                haider_rpc::MonitorCliPresetWire::Custom => "custom",
                _ => "cli",
            };
            if argv.is_empty() {
                format!("cli {preset}")
            } else {
                format!("cli {preset} {}", monitor_excerpt(&argv.join(" ")))
            }
        }
        _ => "unknown source".to_owned(),
    }
}

/// The row's state chip word. `firing` is a CLIENT overlay on daemon truth
/// (see [`AppModel::monitor_row_state`]) and reads the same as the rest.
#[must_use]
pub const fn monitor_state_chip(state: haider_rpc::MonitorStateWire) -> &'static str {
    match state {
        haider_rpc::MonitorStateWire::Armed => "armed",
        haider_rpc::MonitorStateWire::Paused => "paused",
        haider_rpc::MonitorStateWire::Firing => "firing",
        haider_rpc::MonitorStateWire::Exited => "exited",
    }
}

/// The ambient transcript note one monitor delivery becomes (owner item 3):
/// `◉ monitor timer-60s fired → …`. Never a modal — a fire is news.
#[must_use]
pub fn monitor_fired_note(report: &haider_rpc::MonitorDeliveryReportWire) -> String {
    let summary = report
        .events
        .last()
        .map(|event| monitor_event_summary(&event.payload))
        .filter(|summary| !summary.is_empty());
    let coalesced = if report.coalesced_count > 1 {
        format!(" · {} events", report.coalesced_count)
    } else {
        String::new()
    };
    match summary {
        Some(summary) => format!(
            "◉ monitor {} fired{coalesced} → {summary}",
            report.monitor_id
        ),
        None => format!("◉ monitor {} fired{coalesced}", report.monitor_id),
    }
}

/// One event payload's one-line summary. The payload is opaque data from a
/// watched source, so it is excerpted and flattened, never interpreted.
fn monitor_event_summary(payload: &haider_rpc::MonitorEventPayloadWire) -> String {
    use haider_rpc::MonitorEventPayloadWire as P;
    let text = match payload {
        // The SMS body is the message itself — the ADDRESS is not echoed
        // into the transcript.
        P::Sms { body, .. } => body.clone(),
        P::Process {
            line, exit_code, ..
        }
        | P::Cli {
            line, exit_code, ..
        } => match exit_code {
            Some(code) if line.trim().is_empty() => format!("exit {code}"),
            Some(code) => format!("{line} · exit {code}"),
            None => line.clone(),
        },
        P::File { payload } | P::Poll { payload } => payload.clone(),
        P::Timer { tick, .. } => format!("tick {tick}"),
        _ => String::new(),
    };
    monitor_excerpt(&text)
}

/// A structured refusal in the user's words. Never a raw debug dump.
fn monitor_rejection_note(rejection: &haider_rpc::MonitorControlRejectionWire) -> String {
    use haider_rpc::MonitorControlRejectionWire as R;
    match rejection {
        R::NotFound { monitor_id } => format!("· no monitor {monitor_id}"),
        R::CapabilityDenied { .. } => "· monitor control needs a control attachment".to_owned(),
        R::ControlAttachmentRequired => "· monitor control needs a control attachment".to_owned(),
        R::SessionNotFound => "· that session is gone".to_owned(),
        R::InvalidRequest { detail, .. } => format!("· monitor refused — {detail}"),
        R::ServiceStopped => "· the monitor service is stopped".to_owned(),
        R::StoreUnavailable { detail, .. } => format!("· monitor store unavailable — {detail}"),
        R::CommandConflict => "· another monitor command is in flight".to_owned(),
        _ => "· monitor control refused".to_owned(),
    }
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
        haider_protocol::agent::AgentMessageDelivery::DeliveredSubturn => "subturn",
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
        // HAIDER_TEST_DEVICE_NAME: the deterministic-device seam for test
        // runners — CI hostnames run 60+ chars and shed fixed-width row
        // segments, breaking every chip pin host-dependently (the round-5/6
        // whack-a-mole). ci-test.sh exports it; production never sets it.
        std::env::var("HAIDER_TEST_DEVICE_NAME").unwrap_or_else(|_| {
            haider_platform::local_device_name().unwrap_or_else(|| "this-mac".into())
        })
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
            TranscriptEntry::Peer { sender, text, .. } => {
                let mut text = text.replace(['\n', '\r'], " ");
                if text.chars().count() > 48 {
                    text = text.chars().take(48).collect::<String>() + "…";
                }
                format!("  ├─ ⇠ {sender}: {text}")
            }
            TranscriptEntry::Item(block) => {
                let haider_protocol::item::TurnItem::ContextCompaction {
                    tokens_before,
                    tokens_after,
                    tokens_estimated,
                    ..
                } = &block.item
                else {
                    continue;
                };
                let detail = match (tokens_before, tokens_after) {
                    (Some(before), Some(after)) => format!(
                        "⊟ compacted {}{} → {}{}",
                        if *tokens_estimated { "~" } else { "" },
                        crate::format::fmt_tok(*before),
                        if *tokens_estimated { "~" } else { "" },
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
                    text: "Aura online. I orchestrate local sessions — I don't write code myself. Say or type what to spin up.".into(),
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
                device: "local".to_owned(),
                state: ChipDisplayState::Done,
                activity: "webhook tests green".to_owned(),
            }],
            log: vec![
                "spawned billing-service locally".to_owned(),
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
/// Azure v1 base derivation (G4b): the resource endpoint gains
/// `/openai/v1` unless the user already pasted it. Format-only — the
/// daemon's origin predicate and validator hold the authority.
fn azure_v1_base(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    if trimmed.ends_with("/openai/v1") {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/openai/v1")
    }
}

/// Bedrock mantle URL template (G4b) — format-only; the daemon's shape
/// validator refuses anything the region field smuggles in.
fn bedrock_mantle_url(region: &str) -> String {
    format!("https://bedrock-mantle.{region}.api.aws/anthropic")
}

/// The region of a stored mantle endpoint, for card prefill.
fn bedrock_region_of(endpoint: &str) -> Option<&str> {
    endpoint
        .trim_end_matches('/')
        .strip_prefix("https://bedrock-mantle.")?
        .strip_suffix(".api.aws/anthropic")
}

/// Vertex models URL template (G4b) — format-only, daemon-validated.
fn vertex_models_url(project: &str, location: &str) -> String {
    if location == "global" {
        format!(
            "https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/publishers/anthropic/models"
        )
    } else {
        format!(
            "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/anthropic/models"
        )
    }
}

/// The (project, location) of a stored vertex endpoint, for card prefill.
fn vertex_coordinates_of(endpoint: &str) -> Option<(String, String)> {
    let rest = endpoint.trim_end_matches('/').strip_prefix("https://")?;
    let (_, path) = rest.split_once('/')?;
    let (project, rest) = path
        .strip_prefix("v1/projects/")?
        .split_once("/locations/")?;
    let location = rest.strip_suffix("/publishers/anthropic/models")?;
    Some((project.to_owned(), location.to_owned()))
}

fn alias_char(c: char) -> Option<char> {
    let c = c.to_ascii_lowercase();
    (c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')).then_some(c)
}

fn byte_index_at_character(value: &str, character: usize) -> usize {
    value
        .char_indices()
        .nth(character)
        .map_or(value.len(), |(byte, _)| byte)
}

fn insert_custom_card_character(card: &mut CustomProviderCard, character: char) -> bool {
    match card.focus {
        CustomField::Name => alias_char(character).is_some_and(|c| card.insert_char(c)),
        CustomField::Origin | CustomField::Model | CustomField::Extra => {
            !character.is_control() && card.insert_char(character)
        }
        CustomField::Key => card.push_key(character),
        CustomField::Auth | CustomField::ApiFamily => false,
    }
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
/// | subagent destroy | `ChipClose` | `AgentCancel` → `agent.cancel`; feature-gated honest flash |
/// | shell builtins (`ls` · `cd` …) | the demo VFS | honest flash |
/// | `/sessions` | honest daemon-truth refusal | full listing + search + open |
///
/// The last row is the one INVERSION: demo refuses and live acts, because
/// the roster is daemon truth that demo cannot fabricate. Every row above it
/// is the same shape — demo may invent local state, live may not.
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

/// Opaque terminal input owned only long enough to reach `ssh.shell_input`.
/// Debug is redacted and the allocation is wiped on drop because PTY input
/// can include passwords typed at a remote prompt.
#[derive(Clone, PartialEq, Eq)]
pub struct SshTerminalInput(zeroize::Zeroizing<Vec<u8>>);

impl SshTerminalInput {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(zeroize::Zeroizing::new(bytes))
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl std::fmt::Debug for SshTerminalInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SshTerminalInput(<redacted>)")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SshTerminalPane {
    pub profile: String,
    pub shell_id: Option<String>,
    output: zeroize::Zeroizing<Vec<u8>>,
    pub size: haider_rpc::SshPtySizeWire,
}

impl std::fmt::Debug for SshTerminalPane {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SshTerminalPane")
            .field("profile", &self.profile)
            .field("shell_id", &self.shell_id)
            .field("output", &"<remote terminal bytes>")
            .field("size", &self.size)
            .finish()
    }
}

impl SshTerminalPane {
    const OUTPUT_CAPACITY: usize = 1024 * 1024;

    fn opening(profile: String, size: haider_rpc::SshPtySizeWire) -> Self {
        Self {
            profile,
            shell_id: None,
            output: zeroize::Zeroizing::new(Vec::new()),
            size,
        }
    }

    pub(crate) fn push_output(&mut self, bytes: &[u8]) {
        let keep = bytes.len().min(Self::OUTPUT_CAPACITY);
        let needed = self.output.len().saturating_add(keep);
        if needed > Self::OUTPUT_CAPACITY {
            use zeroize::Zeroize as _;
            let remove = needed - Self::OUTPUT_CAPACITY;
            self.output[..remove].zeroize();
            self.output.drain(..remove);
        }
        self.output.extend_from_slice(&bytes[bytes.len() - keep..]);
    }

    #[must_use]
    pub(crate) fn display_text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshFormAuthKind {
    Keep,
    KeyFile,
    Agent,
    Password,
    KeyMaterial,
}

impl SshFormAuthKind {
    fn label(self) -> &'static str {
        match self {
            Self::Keep => "keep existing",
            Self::KeyFile => "key file (recommended)",
            Self::Agent => "ssh-agent (recommended)",
            Self::Password => "password in FileVault",
            Self::KeyMaterial => "pasted key in FileVault",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshPendingAuth {
    Keep,
    KeyFile { path: String },
    Agent,
    Password,
    KeyMaterial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshProfileMutation {
    pub original: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshPendingAuth,
    pub default_cwd: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SshProfileForm {
    pub original: Option<String>,
    pub name: String,
    pub description: String,
    pub host: String,
    pub user: String,
    pub port: String,
    pub auth: SshFormAuthKind,
    pub credential: String,
    secret: zeroize::Zeroizing<String>,
    pub cwd: String,
    pub focus: usize,
    pub error: Option<String>,
}

impl std::fmt::Debug for SshProfileForm {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SshProfileForm")
            .field("original", &self.original)
            .field("name", &self.name)
            .field("description", &self.description)
            .field("host", &self.host)
            .field("user", &self.user)
            .field("port", &self.port)
            .field("auth", &self.auth)
            .field("credential", &self.credential)
            .field("secret", &"<redacted>")
            .field("cwd", &self.cwd)
            .field("focus", &self.focus)
            .field("error", &self.error)
            .finish()
    }
}

impl SshProfileForm {
    const FIELD_COUNT: usize = 8;

    fn add() -> Self {
        Self {
            original: None,
            name: String::new(),
            description: String::new(),
            host: String::new(),
            user: String::new(),
            port: "22".into(),
            auth: SshFormAuthKind::KeyFile,
            credential: String::new(),
            secret: zeroize::Zeroizing::new(String::new()),
            cwd: String::new(),
            focus: 0,
            error: None,
        }
    }

    fn edit(profile: &haider_rpc::SshProfileWire) -> Self {
        Self {
            original: Some(profile.name.clone()),
            name: profile.name.clone(),
            description: profile.description.clone().unwrap_or_default(),
            host: profile.host.clone(),
            user: profile.user.clone(),
            port: profile.port.to_string(),
            auth: SshFormAuthKind::Keep,
            credential: String::new(),
            secret: zeroize::Zeroizing::new(String::new()),
            cwd: profile.default_cwd.clone().unwrap_or_default(),
            focus: 1,
            error: None,
        }
    }

    pub(crate) fn credential_display(&self) -> String {
        match self.auth {
            SshFormAuthKind::Password | SshFormAuthKind::KeyMaterial => {
                "•".repeat(self.secret.chars().count())
            }
            SshFormAuthKind::Agent | SshFormAuthKind::Keep => "—".into(),
            SshFormAuthKind::KeyFile => self.credential.clone(),
        }
    }

    pub(crate) fn auth_label(&self) -> &'static str {
        self.auth.label()
    }

    fn cycle_auth(&mut self, forward: bool) {
        let order: &[SshFormAuthKind] = if self.original.is_some() {
            &[
                SshFormAuthKind::Keep,
                SshFormAuthKind::KeyFile,
                SshFormAuthKind::Agent,
                SshFormAuthKind::Password,
                SshFormAuthKind::KeyMaterial,
            ]
        } else {
            &[
                SshFormAuthKind::KeyFile,
                SshFormAuthKind::Agent,
                SshFormAuthKind::Password,
                SshFormAuthKind::KeyMaterial,
            ]
        };
        let current = order
            .iter()
            .position(|kind| *kind == self.auth)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % order.len()
        } else {
            current.checked_sub(1).unwrap_or(order.len() - 1)
        };
        self.auth = order[next];
        self.credential.clear();
        self.secret.clear();
    }

    fn take_request(
        &mut self,
    ) -> Result<(SshProfileMutation, Option<haider_rpc::SecretWire>), String> {
        let port = self
            .port
            .parse::<u16>()
            .map_err(|_| "port must be 1..=65535".to_owned())?;
        if port == 0 {
            return Err("port must be 1..=65535".into());
        }
        if self.name.is_empty() || self.host.is_empty() || self.user.is_empty() {
            return Err("name, host, and user are required".into());
        }
        let (auth, secret) = match self.auth {
            SshFormAuthKind::Keep => (SshPendingAuth::Keep, None),
            SshFormAuthKind::Agent => (SshPendingAuth::Agent, None),
            SshFormAuthKind::KeyFile if !self.credential.is_empty() => (
                SshPendingAuth::KeyFile {
                    path: self.credential.clone(),
                },
                None,
            ),
            SshFormAuthKind::Password | SshFormAuthKind::KeyMaterial if !self.secret.is_empty() => {
                let secret =
                    std::mem::replace(&mut self.secret, zeroize::Zeroizing::new(String::new()));
                let auth = if self.auth == SshFormAuthKind::Password {
                    SshPendingAuth::Password
                } else {
                    SshPendingAuth::KeyMaterial
                };
                (auth, Some(haider_rpc::SecretWire::new(secret.as_str())))
            }
            SshFormAuthKind::KeyFile => return Err("key file path is required".into()),
            _ => return Err("authentication secret is required".into()),
        };
        Ok((
            SshProfileMutation {
                original: self.original.clone(),
                name: self.name.clone(),
                description: nonempty_owned(&self.description),
                host: self.host.clone(),
                port,
                user: self.user.clone(),
                auth,
                default_cwd: nonempty_owned(&self.cwd),
            },
            secret,
        ))
    }
}

fn nonempty_owned(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn ssh_form_push(form: &mut SshProfileForm, character: char) {
    match form.focus {
        0 if form.original.is_none() => form.name.push(character),
        1 => form.description.push(character),
        2 => form.host.push(character),
        3 => form.user.push(character),
        4 if character.is_ascii_digit() => form.port.push(character),
        6 => match form.auth {
            SshFormAuthKind::KeyFile => form.credential.push(character),
            SshFormAuthKind::Password | SshFormAuthKind::KeyMaterial => form.secret.push(character),
            _ => {}
        },
        7 => form.cwd.push(character),
        _ => {}
    }
}

fn ssh_form_backspace(form: &mut SshProfileForm) {
    match form.focus {
        0 if form.original.is_none() => {
            form.name.pop();
        }
        1 => {
            form.description.pop();
        }
        2 => {
            form.host.pop();
        }
        3 => {
            form.user.pop();
        }
        4 => {
            form.port.pop();
        }
        6 => match form.auth {
            SshFormAuthKind::KeyFile => {
                form.credential.pop();
            }
            SshFormAuthKind::Password | SshFormAuthKind::KeyMaterial => {
                form.secret.pop();
            }
            _ => {}
        },
        7 => {
            form.cwd.pop();
        }
        _ => {}
    }
}

fn ssh_terminal_key_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    if key.modifiers.contains(KeyModifiers::ALT) {
        bytes.push(0x1b);
    }
    match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let lower = character.to_ascii_lowercase();
            if lower.is_ascii_lowercase() {
                bytes.push((lower as u8) & 0x1f);
            } else {
                return None;
            }
        }
        KeyCode::Char(character) => {
            let mut encoded = [0_u8; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        }
        KeyCode::Enter => bytes.push(b'\r'),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::BackTab => bytes.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => bytes.push(0x7f),
        KeyCode::Esc => bytes.push(0x1b),
        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
        KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
        KeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
        KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
        _ => return None,
    }
    Some(bytes)
}

/// Side effects the reducer requests from the runtime (the reducer itself
/// never performs IO).
#[derive(Debug, Clone, PartialEq)]
pub enum AppRequest {
    /// `/update` without a pending release: ask the process shell to run an
    /// immediate release check. Unlike the quiet startup check, this user-
    /// initiated check bypasses the persisted rate limit and reports its
    /// outcome through an [`AppEvent`].
    CheckForUpdate,
    /// `/update` with a pending release: ask the process shell to run the
    /// existing atomic update transaction. The reducer performs no release,
    /// filesystem, daemon, terminal, or process IO.
    RunUpdate,
    /// W8b: live `!` shell escape — one exact command for the SESSION
    /// daemon's workspace (receipt-backed `shell.exec`; zero provider
    /// requests). The branch is captured at issuance so a later switch
    /// cannot retarget the durable command record. Never demo vocabulary.
    ShellExec {
        command: String,
        branch: Option<haider_protocol::ids::BranchId>,
    },
    /// W10b: durable account removal (receipt-backed `account.remove`).
    AccountRemove {
        alias: String,
    },
    /// Explicitly confirmed copy of a discovered first-party CLI login.
    AccountImportDevice {
        candidate: String,
        source: String,
    },
    /// W10b: durable custom-provider removal (`provider.remove`) — the
    /// daemon refuses builtins and account-referenced providers with
    /// typed reasons; the client never pre-judges.
    ProviderRemove {
        provider: String,
    },
    /// Toggle the selected provider's daemon-enforced trust ceiling.
    ProviderSetTrust {
        provider: String,
        trust: haider_rpc::ProviderTrustWire,
        expected_revision: u64,
    },
    /// Read the envelope/quota for the active provider explainer.
    LockdownStatus {
        provider: Option<String>,
    },
    /// W8b: `/tools` live — read the daemon's canonical tool inventory.
    ToolsRefresh,
    /// `/peer` live — read the profile's advertised peer registry.
    PeerList,
    /// `/peer <name> <message>` live — send one user-authored message.
    PeerSend {
        to: String,
        message: String,
    },
    SshList,
    SshSetScope {
        scope: haider_rpc::SshScopeWire,
    },
    SshTest {
        profile: String,
    },
    SshRemove {
        profile: String,
    },
    SshShellOpen {
        profile: String,
        size: haider_rpc::SshPtySizeWire,
    },
    SshShellInput {
        id: String,
        input: SshTerminalInput,
    },
    SshShellResize {
        id: String,
        size: haider_rpc::SshPtySizeWire,
    },
    SshShellEof {
        id: String,
    },
    SshProfileSave {
        mutation: SshProfileMutation,
        secret: Option<haider_rpc::SecretWire>,
    },
    ShellList,
    ShellClose {
        id: String,
    },
    MonitorList,
    /// `monitor.remove` — stop one monitor for good (970 owner item 2).
    /// Durable: it carries a command id through the outbox, so a socket
    /// loss retries the stop rather than silently dropping it.
    MonitorRemove {
        monitor_id: String,
    },
    /// `monitor.mutate` / pause.
    MonitorPause {
        monitor_id: String,
    },
    /// `monitor.mutate` / resume.
    MonitorResume {
        monitor_id: String,
    },
    /// `monitor.mutate` / trigger — fire this monitor once, now.
    MonitorTrigger {
        monitor_id: String,
    },
    /// `/hooks` live (H4): read the daemon's hook discovery for `cwd` —
    /// workspace + profile truth. The cwd is CAPTURED AT ISSUANCE (the B2b
    /// capture law): a later screen or session switch cannot retarget the
    /// listing.
    HooksRefresh {
        cwd: String,
    },
    /// A trust (`trusted == true`) or revoke (`false`) for one digest —
    /// receipted daemon commands (H3's R2 pattern). The receipt installs
    /// NOTHING locally: the driver chains a fresh `hooks.list` and daemon
    /// truth moves the rows (the branch discipline).
    HooksTrust {
        digest: String,
        trusted: bool,
    },
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
    AttachRead {
        path: String,
    },
    /// ⌃V / ⌘V / ⌃⇧V: read the OS CLIPBOARD (970 owner bug 2).
    ///
    /// Bracketed paste carries text and only text — no terminal delivers
    /// image bytes through it — so a clipboard picture has to be fetched
    /// from the OS on the keystroke. The read is SHELL-owned for exactly
    /// the reason [`Self::AttachRead`] is: the reducer performs no IO. The
    /// outcome re-enters through [`crate::runtime::clipboard_paste_effects`]
    /// — a chip + upload, an image notice, or nothing at all when the
    /// clipboard holds text (which the terminal's own paste already owns).
    ClipboardRead,
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
    /// Esc-Esc `f` / `/fork <n>` — fork the session at ONE previously
    /// committed user prompt (`session.fork` with a prompt selector).
    ///
    /// This is a SESSION fork, not [`Self::BranchCreate`]'s named ref: the
    /// daemon mints a whole new session, copies history up to the boundary
    /// before `seq`, and returns that prompt as an editable, unsent draft.
    /// The source session is untouched — no mutation is issued against it —
    /// and the driver opens the CHILD, leaving the original on the roster.
    /// Coordinates are captured at issuance (the B2b capture law): a later
    /// branch switch cannot retarget the cut.
    ForkFromPrompt {
        session: haider_protocol::ids::SessionId,
        source_branch: Option<haider_protocol::ids::BranchId>,
        /// Durable journal sequence of the selected `UserMessage` envelope.
        seq: u64,
    },
    /// Durable checkpoint reads/mutations. Branch identity is captured at
    /// issuance; the live driver supplies session/generation coordinates.
    Checkpoints {
        branch: Option<haider_protocol::ids::BranchId>,
        rollback: Option<String>,
    },
    CheckpointUndo {
        branch: Option<haider_protocol::ids::BranchId>,
        target: String,
    },
    CheckpointRedo {
        branch: Option<haider_protocol::ids::BranchId>,
        target: String,
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
    ChipSubmit {
        agent: String,
        text: String,
    },
    /// Cancel one direct child through the daemon's durable agent-control
    /// surface. The reducer captures the opaque agent id; the live driver
    /// supplies parent-session and generation coordinates.
    AgentCancel {
        agent: String,
    },
    /// Close a chip (✕ / the docs-recovery close arm): lifecycle flags are
    /// the reducer's; the driver owns the 5 s removal + resume timers.
    ChipClose {
        agent: String,
    },
    /// Run an aura orchestrate turn (§3.4).
    AuraSubmit {
        text: String,
        voice: bool,
    },
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
    LoginRetired {
        attempt: u64,
    },
    /// LIVE launcher submit (W3c3, report R11 cut 4): ask the daemon to
    /// create a session for `text`. Deliberately NOT accompanied by a row,
    /// a session id, a screen flip or a turn: in live mode there is no
    /// local truth to fabricate, so the launcher shows nothing new until
    /// `session.create` answers. The demo path (`new_session`) is the
    /// opposite by design — its world IS local.
    CreateSession {
        text: String,
    },
    /// The strict gap law fired (W3c3, report R11 cut 2): reduction STOPPED
    /// for `session` with its cursor still at `after_seq`, and NOTHING later
    /// may be applied until the driver reattaches from there. The demo
    /// driver never produces gaps and ignores this; `LiveDriver` reattaches.
    Reattach {
        session: haider_protocol::ids::SessionId,
        after_seq: u64,
    },
    /// Fetch/refresh the `/accounts` rows (`account.list`). Pushed on
    /// screen entry and after a successful add commit; the demo driver
    /// answers from the seed list.
    AccountsRefresh,
    /// Trigger metadata-only device credential discovery. Pushed on
    /// accounts/provider entry and once per live connection.
    DeviceCandidatesRefresh,
    /// `account.set_active` for the clicked/entered row. The model already
    /// holds `pending_select` — the dot moves only when the driver's reply
    /// applies (optimism forbidden, report §5.1).
    AccountSetActive {
        alias: String,
        confirm_new_epoch: bool,
    },
    /// Fetch/refresh the `/providers` summaries (`provider.list`).
    ProvidersRefresh,
    /// Fetch/refresh the `/usage` snapshot (U1's `usage.report`). A READ —
    /// pushed on screen entry and by `r`, live-only vocabulary: the demo
    /// opens an honest empty state and never fabricates a meter.
    UsageRefresh,
    /// 954: the bounded heatmap read (`usage.history_range`).
    UsageHistoryRefresh,
    /// 954 Models scope: today's full day read (`usage.history_day`).
    UsageTodayRefresh,
    /// 954 queue panel: list the daemon-held messages.
    QueueList,
    /// 954 queue panel: promote a held message to steer (fenced).
    QueuePromoteSteer {
        id: haider_protocol::ids::EventId,
        revision: u64,
    },
    /// 954 queue panel mode toggle, leg one: fenced remove; on the OK
    /// reply the held text resubmits under the next mode (leg two).
    QueueToggleRemove {
        id: haider_protocol::ids::EventId,
        revision: u64,
    },
    /// Fetch/refresh the fleet snapshot (`session.fleet`) for the ACTIVE
    /// session. A READ — never outboxed; pushed at fleet-view open, then
    /// chased by the driver on the existing event cadence while the screen
    /// stays open (single-flight — no new polling loop). Live-only
    /// vocabulary: the demo synthesizes from its own chips at open.
    FleetRefresh,
    /// CG-M1: read the daemon's `graph.status` reduction for the active
    /// session (single-flight in the driver). Emitted on session open and
    /// on `/graph`; ongoing freshness rides the event-cadence chase.
    GraphRefresh,
    /// v963 L3: establish the typed activation baseline used by the
    /// reconnectable `workflow.graph.watch` cursor.
    WorkflowGraphRefresh,
    /// v963 L3: continue the activation stream from the checked-out
    /// projection's retained cursor, falling back to a baseline only when
    /// that session has no runtime state yet.
    WorkflowGraphResume,
    /// M2c: one-shot `graph.inspect` telemetry read (template rollups, tool-
    /// selection stats, evidence provenance) for the `/graph` screen.
    GraphInspectRefresh,
    /// Owner 2026-08-16 (manual retry): re-run a terminal-failed session's
    /// last user turn — receipt-backed `run.retry`, no new user message.
    RunRetry {
        session: SessionId,
    },
    /// Recovery leg for `/retry` when the latest turn reported a vanished
    /// workspace: select the TUI process's current cwd first, then retry the
    /// failed run when one exists.
    WorkspaceSet {
        session: SessionId,
        path: String,
        retry_after: bool,
    },
    /// Grant card: open the macOS System Settings pane for a parked computer
    /// OS-permission (`computer.permission_open_settings`).
    OpenPermissionSettings {
        session: SessionId,
        request_id: String,
        permission: haider_protocol::permission::SystemPermission,
    },
    /// Owner 2026-08-16 (fleet member detail): read the DETAIL member's own
    /// child-graph status for its workflow section.
    FleetMemberGraph {
        session: SessionId,
    },
    /// CG-M1: receipt-backed pin of a graph template for the active session
    /// (`/graph pin`, `p` on the /workflows pane). The driver mints the
    /// command id + worker generation; nothing is installed until the
    /// daemon's fact arrives. `template` is the `graph.pin` name (built-in
    /// catalog first, then the Loom registry — store resolution order);
    /// `None` keeps the legacy ship-loop fallback for old callers.
    GraphPin {
        template: Option<String>,
    },
    /// CG-M1: receipt-backed abandonment of the active graph (`/graph
    /// abandon`, `p` on the /workflows `none` row). Carries a public reason
    /// string.
    GraphAbandon {
        why: String,
    },
    /// W-flow: re-read the Loom registry (`loom.list`). Pushed on every
    /// loom-pane entry so a registration landed by authoring confirmation is
    /// visible on return — the once-per-connection Listed fetch stays the
    /// hydration path; this is the freshness path. Receipt-free read; the
    /// reply still rides the connection-epoch fence.
    LoomRefresh,
    /// Start a feature-negotiated Loom authoring session from prose.
    LoomAuthorDraft {
        generation: u64,
        session: SessionId,
        kind: haider_protocol::loom::LoomAuthorKind,
        prose: String,
    },
    /// Revalidate the exact edited typed document without mutating registry
    /// state. Location-bearing errors return inline to the editor.
    LoomAuthorRevise {
        generation: u64,
        authoring_id: String,
        expected_revision: u64,
        kind: haider_protocol::loom::LoomAuthorKind,
        text: String,
    },
    /// Confirm and register one immutable revision. The daemon returns the
    /// execution digest; the TUI never hashes locally.
    LoomAuthorConfirm {
        generation: u64,
        authoring_id: String,
        expected_revision: u64,
        kind: haider_protocol::loom::LoomAuthorKind,
        text: String,
        expected_rev: u32,
        expected_digest: Option<String>,
    },
    /// Pure L4 validation of the editor text. Unlike author revise this does
    /// not advance the ephemeral authoring revision or mutate the registry.
    LoomValidate {
        generation: u64,
        kind: haider_protocol::loom::LoomAuthorKind,
        text: String,
    },
    /// Archive the selected user-owned registry row under its current CAS.
    LoomArchive {
        kind: haider_protocol::loom::LoomRegistryEntryKind,
        id: String,
        expected_rev: u32,
        expected_digest: String,
    },
    /// Cancel the durable required-CLI install created by this confirmation.
    LoomInstallCancel {
        generation: u64,
        job_id: String,
    },
    /// `account.set_default_model` under the expected-revision CAS. The
    /// default marker moves only on the correlated reply.
    SetDefaultModel {
        provider: String,
        model: String,
        expected_revision: u64,
    },
    /// F2a: receipted live-session model selection (`session.select_model`)
    /// — the picker's ⏎ on an exact OAuth row or API-provider-stage row.
    /// The provider always rides along; the identity pair moves only on the
    /// correlated RESOLVED reply.
    SelectModel {
        session: SessionId,
        model: String,
        provider: String,
        confirm_new_epoch: bool,
    },
    /// G2: receipted live-session rename (`session.rename`) — `/rename`
    /// on an attached session. The daemon normalizes the title; the name
    /// moves only on the correlated NORMALIZED reply (optimism forbidden,
    /// same law as [`Self::SelectModel`]).
    Rename {
        session: SessionId,
        title: String,
    },
    /// Durable shared attention acknowledgement. The daemon owns the seen
    /// timestamp; this surface merely says that the user viewed a session.
    Seen {
        session: SessionId,
    },
    /// G3: receipted live-session effort selection
    /// (`session.select_effort`). `None` reverts to the provider default;
    /// the identity's reasoning segment moves only on the correlated reply.
    SelectEffort {
        session: SessionId,
        effort: Option<String>,
        confirm_new_epoch: bool,
    },
    /// G3: the receipted fast-mode toggle (`session.select_fast`).
    SelectFast {
        session: SessionId,
        enabled: bool,
        confirm_new_epoch: bool,
    },
    /// W-flow inline identity: receipted agent-type binding for the ACTIVE
    /// session (`session.select_agent_type`, `p` on the /loom Types pane).
    /// `None` reverts to a plain session; a present id is registry-
    /// validated by the daemon (a miss is a typed refusal that binds
    /// nothing). Identity moves only on the `agent_type_selected` FACT —
    /// the response is receipt + flash, never an install.
    SelectAgentType {
        agent_type: Option<String>,
    },
    /// Start an OAuth add flow (`account.oauth_start`) for the card.
    OAuthAddStart {
        provider: String,
        alias: String,
        attempt: u64,
    },
    /// Cancel the card's flow (`account.oauth_cancel` when one is bound).
    OAuthAddCancel {
        attempt: u64,
    },
    /// Retire a custom-provider card whose staged credential/configure may
    /// still be in flight. The stage is connection-scoped and not durable;
    /// a late reply for this attempt must not configure anything.
    CustomProviderRetired {
        attempt: u64,
    },
    /// Create or edit a custom OpenAI-compatible provider
    /// (`provider.configure`, W5g-4/W10b). The provider name is the stable
    /// identity; origin and model are mutable under the daemon revision CAS.
    ProviderConfigure {
        attempt: u64,
        name: String,
        origin: String,
        /// The served model id — seeds the inventory AND the default (an
        /// enabled create requires both, daemon law). EMPTY when `models`
        /// carries an explicit inventory echo (G4b bedrock/vertex).
        model: String,
        /// G4a: true for auth-None presets — the wire carries
        /// `auth_requirement: none` instead of `api_key`.
        keyless: bool,
        /// Present only for the discovery-backed generic card. The raw key
        /// crosses no durable command: the driver stages it first, then
        /// sends only the opaque vault reference to configure/login.
        secret: Option<haider_rpc::SecretWire>,
        /// G4b: the profile API family — chat-completions for
        /// customs/azure, anthropic-messages for the enterprise builtins.
        family: haider_rpc::ProviderApiFamilyWire,
        /// G4b: explicit inventory echo for seeded-list providers; EMPTY
        /// derives `[model]` (the pre-G4b shape).
        models: Vec<String>,
        /// G4b: the echoed default; `None` derives `Some(model)`.
        default_model: Option<String>,
        expected_revision: u64,
    },
    /// G4a: re-run one provider's model discovery
    /// (`provider.models_refresh`) — pushed by `f` on `/providers` and by a
    /// committed keyless configure. A READ against the stored origin; the
    /// inventory moves only on the daemon's refreshed snapshot.
    ProviderModelsRefresh {
        provider: String,
    },
    /// Open a URL in the user's browser (runtime-owned effect; the demo
    /// flashes it instead). Carried for the OAuth authorize hop — the URL
    /// always originates from the daemon's sanctioned registration.
    OpenUrl {
        url: String,
    },
    /// Reveal an image-created transcript payload in the OS file explorer.
    RevealPath {
        path: String,
    },
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

/// Daemon-summarized attention state for one roster row. Applied live event
/// timestamps may advance recency before the first list summary arrives, but
/// seen/input truth remains daemon-authored so every connected surface agrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAttention {
    pub seen_at_ms: Option<u64>,
    pub last_activity_ms: Option<u64>,
    pub waiting_why: Option<haider_rpc::WaitingWhyWire>,
    /// v0.0.937 unified card: the typed reason this session needs a human,
    /// whatever the kind (permission, recovery, update, secret, …).
    pub needs_input: Option<haider_rpc::NeedsInputWire>,
}

/// Cache-rate truth from one committed roster head. Both values are optional:
/// in particular, an absent re-read rate means there was no cacheable
/// preceding prefix, never 0% cache health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCacheRates {
    pub head_seq: u64,
    pub lifetime_basis_points: Option<u32>,
    pub reread_basis_points: Option<u32>,
}

/// One rendered row of the `/resume` browser — roster truth only, no
/// journal replay (the 936 attention fields are exactly what this needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBrowserRow {
    pub id: SessionId,
    pub title: String,
    pub dir: String,
    pub model_short: String,
    pub agent_type: Option<String>,
    pub ago: String,
    pub busy: bool,
    pub unseen: bool,
    pub needs_input: Option<haider_rpc::NeedsInputWire>,
    pub last_activity_ms: Option<u64>,
    pub created_at_ms: Option<u64>,
}

impl SessionAttention {
    #[must_use]
    pub fn unseen(&self) -> bool {
        self.last_activity_ms
            .is_some_and(|activity| self.seen_at_ms.is_none_or(|seen| activity > seen))
    }
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Human-readable age for durable roster activity. Future timestamps clamp
/// to `now` so clock skew never produces a negative-looking age.
#[must_use]
pub fn format_session_age_at(now_ms: u64, activity_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(activity_ms) / 1_000;
    match seconds {
        0..=4 => "now".to_owned(),
        5..=59 => format!("{seconds}s ago"),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
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
    /// M2c: the always-visible graph strip above the composer — a click opens
    /// the `/graph` screen (status + telemetry: rollups, tool-selection,
    /// evidence provenance), the owner's "click the workflow → stats" gesture.
    GraphStrip,
    /// A durable image-created transcript row. The absolute payload path is
    /// carried by value so a stale frame can never reveal a different file.
    RevealPath(String),
    /// Owner 2026-08-16: the manual-retry ambient row — click retries the
    /// failed turn.
    RetryRun,
    /// Computer OS-permission grant card: open the exact System Settings pane
    /// for the parked permission (routes to `computer.permission_open_settings`).
    PermissionOpenSettings,
    /// Computer OS-permission grant card: recheck now — answers the paired
    /// `computer-os-permission` menu's retry option.
    PermissionRetry,
    /// Aura / Accounts / Peers launcher rows, by identity not ordinal.
    ExtraRow(LauncherRow),
    /// The Loom/Workflows tab's create row — seeds the composer with the
    /// authoring prompt for whichever pane is showing.
    LoomNew,
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
    /// rect holds the row identity and whether it was an API aggregate, so a
    /// stale first-stage hit can never select a provider-stage pair.
    ModelPickerRow {
        provider: String,
        model: String,
        api_group: bool,
    },
    /// Persistent provider-lockdown status segment.
    LockdownStatus,
    /// One `/usage` account tab chip (U2). VALUE-CARRYING: the provider +
    /// the index WITHIN its group, so a stale hit map can never select a
    /// different account.
    UsageAccountTab {
        provider: String,
        index: usize,
    },
    /// One `/usage` scope-strip label. Value-carrying so a stale hit map
    /// can only select the scope that occupied the clicked cell.
    UsageScope(UsageScope),
    BackChip,
    TalkChip,
    HelpHint,
    /// Clickable shell count on the band's task line (970 owner item 1 —
    /// it moved off the bottom status strip onto the `▾ subagents` row).
    ShellStatus,
    /// Clickable monitor count on the band's task line (970 owner item 1).
    MonitorStatus,
    /// Close affordance for the shell-registry overlay.
    ShellClose(String),
    /// One monitor row in the `/monitors` overlay — selects it and reveals
    /// its actions. Carries the monitor id, never an ordinal, so a stale
    /// hit map can never act on a different monitor (the value-carrying
    /// hit law).
    MonitorRow(String),
    /// Row action: stop (remove) the monitor. Arm-then-confirm.
    MonitorStop(String),
    /// Row action: pause an armed monitor, resume a paused one.
    MonitorPause(String),
    /// Row action: fire the monitor now.
    MonitorTrigger(String),
    /// Row action: hand the edit to the AGENT by prefilling the composer
    /// with `/monitor edit <id>: `.
    MonitorEdit(String),
    /// Row action: copy the monitor id.
    MonitorCopyId(String),
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
    /// The collapsed subagents-panel summary row (`⣿ N subagents · … · ⌥F
    /// fleet`) — opens the fleet view for the current session.
    FleetSummary,
    /// One fleet row/cell, VALUE-CARRYING (the stale-hit-map law): the
    /// agent id it was rendered for, so a refreshed snapshot can never
    /// drill a different agent than the one clicked.
    FleetNode(String),
    /// The fleet member-detail frame's transcript door. VALUE-CARRYING:
    /// the agent id the row was rendered for.
    FleetTranscript(String),
    /// The fleet member-detail frame's `✕ destroy this subagent`. Clicking
    /// it only ARMS the kill — the second, confirming press is what acts.
    FleetKill(String),
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
    StickyJump(u64),
    /// 954 owner item: the bottom jump band — click returns the transcript
    /// to follow (scroll-back 0); the unseen counter clears through the
    /// watermark the next FOLLOWING frame stamps.
    JumpToBottom,
    /// 954 queue panel: deliver this held message at the next safe
    /// boundary (queue.promote_steer, fenced).
    QueueRowSteer(haider_protocol::ids::EventId),
    /// 954 queue panel: cycle this held message's delivery mode
    /// (turn end ⇄ next tool call) — fenced remove + verbatim resubmit.
    QueueRowToggle(haider_protocol::ids::EventId),
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
    /// One editable custom-provider field. The attempt binds a one-frame-old
    /// hit to the exact card that rendered it; mouse dispatch derives the
    /// character offset from the paired rect's value-column origin.
    CustomProviderField {
        attempt: u64,
        field: CustomField,
    },
    /// One `/accounts` row, by its GLOBAL alias (value-carrying: a stale
    /// rect can only ever select the row it was measured on).
    AccountRow(String),
    /// One add-row button on `/accounts` (sim tui.js:3621-3628).
    AccountAdd(AccountAddKind),
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
    /// One decision-hook firing linked to the exact permission menu it
    /// inspected. A stale hit is dropped unless the retained committed menu
    /// snapshot and firing coordinate still agree.
    HookFiring(MenuId),
}

/// The Google Antigravity provider id (970). Haider supervises Google's
/// official `antigravity-acp` agent and speaks ACP to it: **Google owns the
/// OAuth**, the profile and the refresh, and no Google token ever enters
/// Haider's vault. Deliberately distinct from the API-key `gemini` provider,
/// which is a separate account with a separate credential.
pub const GOOGLE_ANTIGRAVITY_PROVIDER: &str = "google-antigravity";

/// The terms warning, VERBATIM (owner decision 2026-09-03). The provider
/// ships ENABLED BY DEFAULT with no policy gate; this text is the disclosure
/// that replaces the gate — shown once before the first login and then as a
/// standing badge on `/accounts` for as long as a Google account exists.
///
/// Renderers WRAP it to the terminal width and never reword it
/// (`docs/testing/v0.0.970/googleoauth.md` §1.6 is the authority).
pub const GOOGLE_ANTIGRAVITY_TERMS_WARNING: &str = "Google's published terms restrict third-party access to Gemini subscriptions/Antigravity; Google ships this ACP agent for editors and reportedly does not enforce the clause — use at your own risk.";

/// Journal subject for the acknowledgement of
/// [`GOOGLE_ANTIGRAVITY_TERMS_WARNING`] (see [`crate::terms_journal`]).
pub const GOOGLE_ANTIGRAVITY_TERMS_SUBJECT: &str = "google-antigravity-terms";

/// Credential-source KIND for an account reached through Google's agent.
/// Google's agent owns the token in its own `$GEMINI_HOME` profile, so this
/// names a real credential source that simply is not Haider's — the accounts
/// screen renders it through the one source renderer, badged
/// `google-antigravity (ACP)`.
pub const GOOGLE_ANTIGRAVITY_SOURCE_KIND: &str = "google_antigravity";

/// What Google's agent COSTS, measured first-hand on 2026-09-04 and pinned in
/// `docs/testing/v0.0.970/_antigravity-pins.md`. Every figure here was read
/// off the artefact — a renderer must never estimate one, and a platform we
/// have not measured gets a measurement, never an extrapolation.
pub const GOOGLE_ANTIGRAVITY_COST_LINES: &[&str] = &[
    "first run downloads ~316 MB (macOS arm64; ~682 MB on Linux x86_64)",
    "~885 MiB on disk once installed (~2.0 GB on Linux)",
    "~225 MiB resident while running · about 15 s to cold start",
];

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
    /// SuperGrok/X Premium subscription sign-in via xAI's RFC-8628 device
    /// grant and dedicated CLI chat proxy.
    GrokOAuth,
    /// 970 — Gemini subscription access through Google's own official
    /// `antigravity-acp` agent. AGENT-OWNED OAuth: Google's binary performs
    /// the sign-in and keeps the token, so this is never an API-key route
    /// (that is [`Self::GeminiApi`], a different account entirely).
    GoogleAntigravity,
    GeminiApi,
    /// Haider Code hosted-model plans through the fixed first-party API.
    HaiderCodeApi,
    HuggingFace,
    OpencodeZen,
    OpencodeGo,
    /// G4a: local Ollama preset — keyless (auth-None) custom provider at
    /// the default `http://127.0.0.1:11434/v1`.
    Ollama,
    /// G4a: local LM Studio preset — keyless (auth-None) custom provider
    /// at the default `http://127.0.0.1:1234/v1`.
    LmStudio,
    /// G4b: Azure OpenAI v1 — the custom card in Azure shape (resource
    /// endpoint + api-key header + deployment-name model).
    AzureOpenAi,
    /// G4b: Bedrock mantle — the builtin `bedrock` profile's region card,
    /// then its bearer key.
    Bedrock,
    /// G4b: Claude on Vertex — the builtin `vertex` profile's
    /// project/location card, then an access token (or the gcloud device
    /// import).
    Vertex,
    Custom,
    /// Named DeepSeek builtin — masked API-key entry at the fixed vendor
    /// endpoint, followed by authenticated model discovery.
    DeepSeekApi,
    /// Named xAI builtin — masked API-key entry at the fixed API origin,
    /// followed by authenticated model discovery.
    XaiApi,
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

/// Which half of the Loom registry the browser shows. The sim splits the
/// two surfaces (`/loom` = Agent Types, `/workflows` = pipe workflows);
/// one Screen::Loom carries both as panes so every `match Screen` stays put.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoomPane {
    #[default]
    Types,
    Workflows,
}

/// Editable Loom authoring state. The composer owns `text`; this record owns
/// validation/confirmation facts returned by the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoomAuthoringState {
    /// Monotonic local editor identity. It fences replies that outlive a
    /// close/reopen cycle and is never sent to the daemon.
    pub generation: u64,
    pub kind: haider_protocol::loom::LoomAuthorKind,
    pub authoring_id: Option<String>,
    pub revision: Option<u64>,
    pub errors: Vec<haider_protocol::loom::LoomAuthorValidationError>,
    pub confirmed: Option<haider_protocol::loom::LoomAuthorConfirmed>,
    pub pending: bool,
    pub validated: bool,
    /// Digest preview returned only by `loom.validate`; absence means it has
    /// not been computed or the document is invalid, never an empty digest.
    pub preview_digest: Option<String>,
    /// Exact durable install progress for the confirmation's optional job.
    /// Absence means no status response has installed, not succeeded.
    pub install_job: Option<haider_protocol::typed_agent::TypedAgentInstallJob>,
}

/// One /workflows row (W-flow). The row space is FIXED-HEAD: the synthetic
/// `none` row first (not a registry record — which is exactly what makes it
/// undeletable), then the built-in MAIN-session catalog templates, then the
/// registered Loom workflows. Selection indices live over this space, never
/// over `loom_workflows` alone.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowRow {
    /// "No session workflow" — every session's default. Selecting it means
    /// abandon the active graph, never a registry mutation.
    None,
    /// A built-in catalog template (immutable daemon truth, pinnable by
    /// name).
    BuiltIn(haider_protocol::graph::GraphTemplateSpec),
    /// Index into [`AppModel::loom_workflows`].
    Registered(usize),
}

/// The reject-evidence coordinate currently opened from a live workflow
/// detail. L2 deliberately exposes an opaque artifact reference, not artifact
/// bytes, so inspection preserves and displays that coordinate verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowEvidenceInspection {
    pub node_id: String,
    pub code: String,
    pub message: String,
    pub cursor: u64,
    pub reference: Option<String>,
}

/// Result of admitting one `workflow.graph.watch` page. The driver uses this
/// to drain bounded pages or replace a discontinuous baseline; the renderer
/// continues showing the last good projection throughout repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowGraphPageOutcome {
    Ignored,
    Applied { has_more: bool },
    Rebaseline,
}

/// One /loom (Types) row — the workflows pane's fixed-head law mirrored
/// (W-flow inline identity): the synthetic `∅ none` default first (not a
/// registry record, which is exactly what makes it undeletable), then the
/// registered agent types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeRow {
    /// "Plain session" — every session's default. Selecting it means the
    /// receipted clear (`agent_type: None`), never a registry mutation.
    None,
    /// Index into [`AppModel::loom_types`].
    Registered(usize),
}

/// The launcher's non-session rows (value-carrying hit payload, P2-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherRow {
    Aura,
    Accounts,
    Peers,
    /// The all-sessions browser (`/resume`).
    Sessions,
    /// The Workflows registry browser (`/workflows`) — typed pipe templates.
    Workflows,
    /// The Loom agent-type browser (`/loom`) — capability-scoped specialists.
    Loom,
}

/// A composer surface's identity (TUI5 item 9): the launcher, one session
/// (by its LOCAL generation — the monotonic-identity law means a key can
/// never be reworn), the aura, or the dedicated Loom editor. The SUBAGENT
/// screen shares its session's key, and the scratch surface (screen=Session,
/// no session) shares the launcher's — documented: scratch is the launcher's
/// envelope-driven lineage.
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
    /// Loom owns an editor distinct from every chat/session composer.
    Loom,
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

/// Why the composer refused (or could not obtain) an IMAGE — the ONE
/// vocabulary shared by BOTH image entry points, `/attach <path>` and the
/// ⌃V clipboard paste (970 owner bug 2), so the two can never word the same
/// refusal differently.
///
/// It is a typed value rather than a string because the refusal is a fact
/// about the SESSION (which model, which failure), and the tests assert the
/// fact — not a sentence someone may reword later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageNotice {
    /// The session's current pair DECLARES it does not accept images.
    NoVision { model: String },
    /// The clipboard held nothing an attachment could be made from.
    ClipboardEmpty,
    /// The clipboard could not be read at all (no clipboard server, a
    /// locked Windows clipboard, an image we could not decode).
    ClipboardUnreadable { note: String },
}

impl ImageNotice {
    /// The rendered sentence. `NoVision` names the model AND both ways out,
    /// because a bare refusal leaves the user guessing.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::NoVision { model } => {
                format!("{model} does not accept images — pick a vision model or /attach as text")
            }
            Self::ClipboardEmpty => "nothing on the clipboard to paste".to_owned(),
            Self::ClipboardUnreadable { note } => format!("clipboard — {note}"),
        }
    }
}
pub const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;

/// Render a release version with exactly one conventional `v` prefix.
/// Discovery may hand the UI either a semver (`0.0.933`) or a GitHub tag
/// (`v0.0.933`); visible copy must not depend on that transport detail.
#[must_use]
pub fn update_version_label(version: &str) -> String {
    let version = version.trim();
    if version.starts_with('v') {
        version.to_owned()
    } else {
        format!("v{version}")
    }
}

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
    /// Volatile mirror input replaces the active surface's draft. This is
    /// reducer-local text state: no prompt-history or journal side effect.
    SurfaceInputReplace {
        text: String,
    },
    /// Boxed: `EventPayload` is much larger than the other variants.
    Envelope(Box<EventPayload>),
    /// Release discovery found a version newer than the running binary.
    /// Shell-owned discovery feeds this fact back as data; the reducer only
    /// stores it and announces it once.
    UpdateAvailable {
        version: String,
    },
    /// A user-initiated release check found no newer version. Startup checks
    /// deliberately do not inject this event, so their equal/older outcome
    /// remains silent.
    UpdateCurrent {
        version: String,
    },
    /// A user-visible update check or transaction failed. Quiet startup
    /// network failures are filtered by the shell and never reach this arm.
    UpdateFailed {
        message: String,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptBacktrack {
    pub selection: usize,
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

/// The full-screen `/model` picker (F2a): OAuth subscriptions remain exact
/// model × provider pairs, while API inventory is one row per model slug and
/// expands to an API-provider stage when more than one provider serves it.
/// MODEL-LOCAL overlay — it owns the keyboard while open (⏎ acts on the
/// HIGHLIGHTED row; esc backs out one stage before closing; the palette's
/// exact-match lead jump never gets near it — heeded history).
#[derive(Debug, Default)]
pub struct ModelPicker {
    /// Live substring search over model + every represented provider (+ auth
    /// flavor). The provider stage owns a fresh query and restores this one
    /// when esc returns to the model list.
    pub query: String,
    /// Index into the FILTERED row list.
    pub selection: usize,
    /// The list viewport's top-row index. Interior-mutable because the
    /// viewport height is a RENDER-time fact: render follows the selection
    /// with minimal scroll (only when it would leave the window), exactly
    /// like the `/` command palette — the selection moves inside a stable
    /// window instead of the list scrolling under a pinned highlight.
    pub scroll: std::cell::Cell<usize>,
    /// In-flight `session.select_model`: the REQUESTED pair. The picker
    /// renders it pulsing; the identity moves only on the resolved reply.
    pub pending: Option<(String, String)>,
    /// Honest inline error — a typed refusal or an unavailability reason.
    pub error: Option<String>,
    /// Present only while choosing which API provider serves a collapsed
    /// model slug. Parent navigation is restored exactly on esc.
    pub provider_stage: Option<ModelProviderStage>,
}

/// Parent-list state retained while the `/model` picker is choosing an API
/// provider. The provider-stage query/selection/scroll live in `ModelPicker`
/// itself so the existing viewport-follow rule is shared by both stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProviderStage {
    pub model: String,
    parent_query: String,
    parent_selection: usize,
    parent_scroll: usize,
}

/// The `/` palette's viewport-follow rule, shared by the `/model` picker: the
/// selection moves inside a STABLE window, and the window's top advances only
/// when the selection would leave it (never a list scrolling under a pinned
/// highlight). `top` is the remembered scroll; the result is the new top,
/// clamped so the last page fills the window.
#[must_use]
pub fn follow_viewport(top: usize, selection: usize, len: usize, window: usize) -> usize {
    if window == 0 || len <= window {
        return 0;
    }
    let max_start = len - window;
    let mut top = top.min(max_start);
    if selection < top {
        top = selection;
    } else if selection >= top + window {
        top = selection + 1 - window;
    }
    top.min(max_start)
}

/// One visible `/model` picker row: an exact OAuth/API provider pair, an API
/// model-slug aggregate at the first stage, or an honest placeholder for a
/// provider with nothing discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPickerRow {
    /// The exact provider for pair rows and the first provider (in registry
    /// order) for a collapsed API row.
    pub provider: String,
    /// Every provider represented by this row. Exact pair rows contain one;
    /// collapsed API rows retain the complete set for provider-name search
    /// and honest aggregate status.
    pub providers: Vec<String>,
    /// Number of represented providers that are both selectable and
    /// currently available.
    pub available_providers: usize,
    /// Number of represented providers currently in lockdown.
    pub lockdown_providers: usize,
    /// Number of represented providers that declare this slug as their
    /// default. Exact rows contain zero or one.
    pub default_providers: usize,
    /// The exact live provider when any represented pair is current. This
    /// keeps the pair visible even when the provider column is aggregated.
    pub current_provider: Option<String>,
    /// True when usable providers disagree about the declared context
    /// window (including known versus unknown). The aggregate must not
    /// imply that one provider's limit applies to every provider.
    pub context_window_varies: bool,
    pub lockdown: bool,
    /// The model slug; empty for a provider placeholder row.
    pub model: String,
    /// `oauth` / `api` — what a turn on this row meters.
    pub auth: &'static str,
    pub context_window: Option<u64>,
    /// Age in milliseconds of the provider inventory used for this row.
    pub inventory_age_ms: Option<u64>,
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

/// Exact cache-sensitive change awaiting a deliberate repeat. The first
/// request is daemon-preflight only; repeating the same selection sends the
/// explicit new-epoch confirmation, while a different selection replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingCacheChange {
    Model {
        session: SessionId,
        provider: String,
        model: String,
    },
    Effort {
        session: SessionId,
        effort: Option<String>,
    },
    Fast {
        session: SessionId,
        enabled: bool,
    },
    Account {
        alias: String,
    },
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
    /// Bound Loom agent-type id (W-flow inline identity), from the durable
    /// `agent_type_selected` fact. `None` = plain session. The accent
    /// surfaces paint from the loom snapshot's color for this id.
    pub agent_type: Option<String>,
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
            agent_type: None,
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
    /// Compact previous-prompt chooser above the composer. The prompt
    /// bytes live in `prompt_history`; this is transient selection chrome.
    pub backtrack: Option<PromptBacktrack>,
    /// Last Esc in the rapid backtrack gesture. An Esc outside the window
    /// closes an open chooser; rapid Esc presses walk to older prompts.
    last_backtrack_escape: Option<std::time::Instant>,
    /// COMMIT counter for the theme choice (ui-themes-fix): bumped by
    /// every user commit — picker ⏎/digit/click, `/theme <name>`, ⌃T —
    /// and never by boot resolution or previews. The runtime's
    /// persistence authority keys on THIS, so re-affirming the boot
    /// default still writes the settings file (the live probe's gap).
    pub theme_commits: u64,
    pub sanctum_tier: SanctumTier,
    pub projection: SessionProjection,
    pub(crate) transcript_layout: std::cell::RefCell<crate::render::TranscriptLayoutCache>,
    /// Durable-journal prompt recall for the attached session, newest first.
    /// Identical redo prompts are distinct entries by design. Each entry
    /// carries the committed sequence a fork cut needs, or `None` when it
    /// never had one — see [`crate::session::PromptEntry`].
    pub prompt_history: std::collections::VecDeque<crate::session::PromptEntry>,
    /// Session-wide latest-snapshot usage fold. Kept outside branch/chip
    /// projections so every billed lane survives view switches.
    pub cache_usage: crate::cache_usage::SessionUsageFold,
    /// Daemon-issued cache impact warning. Repeating exactly this selection
    /// is the explicit confirmation that opens a new epoch.
    pub pending_cache_change: Option<PendingCacheChange>,
    pub identity: IdentityLine,
    /// The user EXPLICITLY chose a provider/model/account this run
    /// (`/model`, `/provider`, or clicking an account). Once pinned, the
    /// daemon-truth bootstrap below never overwrites their choice; until
    /// then the identity line is only a seed and daemon reality wins.
    pub identity_pinned: bool,
    /// The ACTIVE surface's composer (TUI5): text + first-class cursor +
    /// selection + input ring. Nothing in it persists (item 8).
    pub composer: crate::composer::Composer,
    /// Metadata-only refs from a foreign input mirror. They are keyed to the
    /// active session at render time and remain display-only: remote CAS refs
    /// never become local pending attachments or submit input.
    mirrored_input_attachments: Option<(
        SessionId,
        Vec<haider_protocol::hook::HookAttachmentMetadata>,
    )>,
    /// Parked composers for the surfaces NOT on screen (TUI5 item 9):
    /// every surface — launcher, each session, aura, Loom — keeps its own draft
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
    /// 954: the live composer queue panel (daemon-held messages).
    pub queue_panel: QueuePanelState,
    /// `/queue turn` — mid-turn input queues instead of steering.
    pub queue_mode: bool,
    /// `/queue subturn` — hold for the next tool boundary and re-prompt.
    pub subturn_mode: bool,
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
    /// Last `peer.list` snapshot, used by `/peer`'s inline send affordance.
    pub peer_agents: Vec<haider_protocol::peer::PeerDescriptor>,
    /// True only for a user-issued bare `/peer`. Boot/reconnect also reads
    /// `peer.list` to subscribe this connection, but must not paint a list.
    pub peer_list_requested: bool,
    /// Public, secret-free SSH target rows from the daemon.
    pub ssh_profiles: Vec<haider_rpc::SshProfileWire>,
    /// Unified local/SSH terminal rows used by `/shells` and the status strip.
    pub shells: Vec<haider_rpc::ShellWire>,
    /// The distinct existing monitor primitive's active count. This is never
    /// derived from or stored in the shell registry.
    pub monitor_count: usize,
    /// Rows from the existing monitor primitive; kept separate from shells.
    pub monitors: Vec<haider_rpc::MonitorRegistrationWire>,
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
    /// Canonical workspace of the attached session, learned from
    /// `SessionSummary` or the create response.
    pub session_workspace_cwd: Option<String>,
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
    /// The fleet view's state (snapshot, drill stack, selection) — see
    /// [`crate::fleet`]. Session-scoped display state; the snapshot is
    /// daemon truth in live mode and never fabricated there.
    pub fleet: crate::fleet::FleetView,
    /// Convergence Graph M1: the daemon's `graph.status` reduction for the
    /// active session, or `None` when no graph was ever pinned. Drives the
    /// always-visible strip above the composer and the `/graph` status view;
    /// never fabricated — it is a read of durable daemon truth.
    pub graph: Option<haider_protocol::graph::GraphStatus>,
    /// L3: the `workflow.graph.state` baseline reduced with its reconnectable
    /// watch suffix for the attached session.
    pub workflow_graph: haider_client::WorkflowGraphProjection,
    /// L2's typed reducer state, isolated behind the client adapter. Rendering
    /// never reads this engine-owned shape directly.
    pub workflow_graph_rpc: haider_client::WorkflowGraphRpcAdapter,
    /// Last typed projection/watch refusal. The last good graph remains
    /// visible while the driver reconnects from `workflow_graph.cursor()`.
    pub workflow_graph_error: Option<String>,
    /// Explicit reject-evidence subview opened from the workflow detail.
    /// This is screen-local and clears on detail/session/pane changes.
    pub workflow_evidence_inspection: Option<WorkflowEvidenceInspection>,
    /// D1 — the Loom registry snapshot (types color the `@type ·` chips; the
    /// workflows annotate the graph screen with tasks + typed I/O).
    pub loom_types: Vec<haider_protocol::loom::LoomAgentType>,
    /// `workflow_catalog_v1` snapshot. This is connection truth and is empty
    /// when the feature is absent; no local built-in fallback is permitted.
    pub workflow_catalog: Vec<haider_rpc::WorkflowCatalogEntryV1>,
    pub loom_workflows: Vec<haider_protocol::loom::LoomWorkflow>,
    /// ADE seam: a session id handed to the CLI (`haider --session <id>`)
    /// that the TUI opens as soon as the live list proves it exists. Cleared
    /// after one attempt — found or honestly flashed as unknown.
    pub initial_session: Option<SessionId>,
    /// W-flow — device PATH presence per declared CLI, from `loom.list`.
    /// A name ABSENT from this map was never probed (older daemon) and must
    /// render as unknown, never as missing.
    pub loom_cli_present: std::collections::BTreeMap<String, bool>,
    pub loom_loaded: bool,
    /// Round 4: the per-connection loom.list dedup latch — separate from
    /// `loom_loaded` (truth) so an in-flight read renders as LOADING.
    pub loom_requested: bool,
    /// D3 — /loom browser state: the flat selection over types then
    /// workflows, and whether the detail pane is open.
    pub loom_selection: usize,
    pub loom_detail: bool,
    /// Round 3: scroll offset inside an open /loom detail pane, clamped
    /// against the render-published ceiling (the plan-surface pattern).
    pub loom_scroll: u16,
    pub loom_scroll_max: std::cell::Cell<u16>,
    /// Round 3: where /loom returns on esc — the screen it was opened from.
    pub loom_return: Option<Screen>,
    /// Which registry pane `Screen::Loom` shows (`/loom` vs `/workflows`).
    pub loom_pane: LoomPane,
    /// v963 L1 — prose/draft/revise/confirm state for the editable typed Loom
    /// document. Registry mutation occurs only on a successful confirm RPC.
    pub loom_authoring: Option<LoomAuthoringState>,
    /// Allocator for reply-fenced Loom editor generations; zero is never
    /// issued so default/missing request context cannot match live state.
    next_loom_authoring_generation: u64,
    /// M2c: the last `graph.inspect` telemetry snapshot for the `/graph`
    /// screen (template rollups, tool-selection stats, evidence provenance with
    /// real workspace-revision provenance). A one-shot read, refetched on open.
    pub graph_inspect: Option<haider_protocol::graph::GraphInspectSnapshot>,
    /// Owner 2026-08-16 (manual retry): a `run.retry` is in flight —
    /// single-flight so a double-click never risks a duplicate command.
    pub retry_inflight: bool,
    /// An honest one-line refusal when the attached daemon predates
    /// `convergence_graph_v1` (set on a feature-absent `/graph`).
    pub graph_unsupported: bool,
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
    /// Cold/reconnect direct snapshots from additive `SessionSummary`
    /// fields, keyed by their exact session id. Active-session state does not
    /// swap this registry, so `/usage` can refresh it in place.
    pub session_metrics:
        std::collections::HashMap<SessionId, haider_protocol::agent::AgentMetricsSnapshot>,
    /// Promoted cache-rate scalars from `SessionSummary`, kept independently
    /// of the retained nested metrics snapshot so the footer reads the
    /// first-class roster facts. The nested path is used only when an older
    /// daemon omits both promoted fields.
    pub session_cache_rates: std::collections::HashMap<SessionId, SessionCacheRates>,
    /// Attention state is roster-only daemon truth. It is kept outside the
    /// checked-in session slots so a summary can update an active session.
    pub session_attention: std::collections::HashMap<SessionId, SessionAttention>,
    /// Durable creation timestamps used only as the recency tie-breaker.
    /// Kept beside roster attention because the active session is checked
    /// out of its ordinary row.
    pub session_created_at_ms: std::collections::HashMap<SessionId, u64>,
    /// Durable last-model aliases retained outside checked-out rows so the
    /// all-sessions search can match the active session before/without a
    /// live model-selection repaint.
    pub session_last_models: std::collections::HashMap<SessionId, String>,
    /// Typed daemon lineage. Missing means an older daemon and remains
    /// top-level for compatibility; an explicit subagent is hidden from the
    /// launcher/browser while its row and metrics remain available to nested
    /// chip surfaces and direct `--session` attachment.
    pub session_kinds: std::collections::HashMap<SessionId, haider_rpc::SessionKindWire>,
    /// Which runtime drives this model (W3c3 M2). Demo by default.
    pub mode: RuntimeMode,
    /// The masked `/login … api` card, while it is open (W3c3 M3).
    pub login: Option<LoginCard>,
    /// The checked-out session's PROTOCOL id (sim `activeId`; `None` =
    /// launcher's no-session state, exactly the sim's `setActiveId(null)`).
    pub active_session: Option<SessionId>,
    /// W-C M1: user-loaded custom slash commands (`.haider/commands` project +
    /// global), merged OVER the built-in registry. Loaded once at live startup
    /// (shell-owned IO — the reducer never touches disk); tests inject via
    /// [`AppModel::set_custom_commands`].
    pub custom_commands: Vec<crate::custom_commands::CustomCommand>,
    /// W-C M1: skip warnings for malformed command files, surfaced once at
    /// startup so a bad drop-in is visible, never a silent loss.
    pub custom_command_warnings: Vec<String>,
    /// W-C M2: terminal focus, for the desktop-notification gate. `focused`
    /// defaults true; `focus_reported` flips once ANY crossterm focus event
    /// arrives, so the fire-anyway fallback holds only on emulators that never
    /// report focus.
    pub focused: bool,
    pub focus_reported: bool,
    /// W-C M2: the desktop-notification on/off toggle (default on), plus a
    /// commit counter the runtime watches to persist a change.
    pub notifications_enabled: bool,
    pub notification_commits: u64,
    /// Model retention (owner 2026-08-15): bumped on every COMMITTED model
    /// pick so the runtime persists the pair and the next boot opens on it.
    pub model_commits: u64,
    /// W-C M2: the attached session's last-seen run state — the edge the
    /// notification fires on (one per turn, never mid-stream).
    pub notification_run_state: Option<RunState>,
    /// W-C M10: per-BACKGROUND-session last-seen run state. The attached
    /// reducer only ever evaluated the active session, so a backgrounded /
    /// parked turn reaching Done/Errored used to notify never; this map gives
    /// each non-active session its own notification edge tracker.
    pub background_notification_states: std::collections::HashMap<SessionId, RunState>,
    /// W-C M2: pending notification lines the runtime drains and emits as
    /// OSC 9 (bounded, masked; the runtime gates emission on a tty).
    pub notifications: Vec<String>,
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
    /// D4: scroll offset into the open `plan` proposal document (the plan
    /// menu's markdown body renders full-height in the transcript area).
    /// A Cell because RENDER owns the new-proposal reset (review round 2:
    /// plan B must open at the top BEFORE any keypress).
    pub plan_scroll: std::cell::Cell<u16>,
    /// The render-computed scroll ceiling for the CURRENT document/viewport;
    /// the key handler clamps against it so overscroll never accumulates.
    pub plan_scroll_max: std::cell::Cell<u16>,
    /// The plan the scroll belongs to — a NEW proposal starts at the top.
    /// Keyed by (menu id, body byte length) so a re-issued id with different
    /// content still reads as a new proposal (round 3).
    pub plan_menu_seen: std::cell::RefCell<Option<(MenuId, u64)>>,
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
    /// `/shells` terminal-registry overlay; activity floats over the body.
    pub shells_open: bool,
    /// Selected shell row in the overlay's keyboard path.
    pub shells_cursor: usize,
    /// `/ssh` profile overlay and its keyboard selection.
    pub ssh_open: bool,
    pub ssh_cursor: usize,
    /// The profile whose destructive overlay removal is awaiting a second
    /// `d`. Moving the cursor or closing the overlay disarms it.
    pub ssh_remove_armed: Option<String>,
    pub ssh_form: Option<SshProfileForm>,
    /// Last known terminal/pane dimensions, seeded before the input pump and
    /// updated on every resize. New SSH PTYs use this initial size.
    pub ssh_terminal_size: haider_rpc::SshPtySizeWire,
    /// Full-body interactive SSH pane. Distinct from the shell-list overlay,
    /// monitor details, and subagent surfaces.
    pub ssh_terminal: Option<SshTerminalPane>,
    /// Existing monitor details floated over the body from the band's
    /// `· N monitors` count.
    pub monitors_open: bool,
    /// Selected monitor row in the overlay's keyboard path.
    pub monitors_cursor: usize,
    /// The monitor whose destructive overlay stop is awaiting a second `x`.
    /// Moving the cursor or closing the overlay disarms it.
    pub monitors_stop_armed: Option<String>,
    /// Monitors this client has seen FIRE whose woken subturn has not yet
    /// completed (owner item 3). A display overlay on daemon truth only —
    /// [`AppModel::monitor_row_state`] is the single reader, and the set is
    /// cleared the moment the session goes idle again.
    pub monitors_firing: std::collections::HashSet<String>,
    /// Small status-line explainer overlay for the active lockdown provider.
    pub lockdown_overlay: bool,
    pub lockdown_status: Option<haider_rpc::LockdownStatusWire>,
    /// Ceiling frozen at the active session's last accepted turn boundary.
    /// Provider roster trust may already describe the following turn.
    pub lockdown_provider: Option<String>,
    pub lockdown_boundary_known: bool,
    /// One-line transient notice shown in the status bar until the next
    /// keystroke (honest stubs: "/tree lands with the daemon").
    pub flash: Option<String>,
    /// A refused/failed IMAGE, shown as its own row INSIDE the composer
    /// band until the next keystroke (970 owner bug 2). It rides the band
    /// rather than the status bar because it answers something the user
    /// just did to the DRAFT — and because the draft is deliberately kept,
    /// the notice has to sit where the draft is. Same shared-predicate
    /// discipline as the attachment chip row: `render::composer_height`
    /// and `render::render_composer` both read THIS field.
    pub composer_notice: Option<ImageNotice>,
    /// A production release newer than this process, discovered by the
    /// shell. Profile-wide and screen-independent: the quiet status-bar
    /// indicator persists until a later check proves the process current or
    /// the process restarts into the installed binary.
    pub update_available: Option<String>,
    /// Persistent profile-level diagnostic, cleared only by an explicit
    /// healthy edge from the daemon after a real write probe succeeds.
    pub profile_diagnostic: Option<haider_protocol::error::ErrorPresentation>,
    /// Latched after sustained unknown-payload/sequence mismatch.
    pub compatibility_diagnostic: Option<haider_protocol::error::ErrorPresentation>,
    /// Post-start microphone failure, persistent until a later Start succeeds.
    pub voice_diagnostic: Option<haider_protocol::error::ErrorPresentation>,
    pub supervisor_diagnostic: Option<haider_protocol::error::ErrorPresentation>,
    /// A durable mutation exhausted its bounded client-side recovery budget.
    pub command_diagnostic: Option<haider_protocol::error::ErrorPresentation>,
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
    pub scroll_back: std::cell::Cell<u64>,
    /// Max scroll-back of the LAST rendered frame — written by the
    /// renderer; wheel notches and sticky jumps clamp against it
    /// (reconcile-then-apply, review r5 P2-2). Starts at 0 (review r2
    /// P2-6).
    pub scroll_max: std::cell::Cell<u64>,
    /// Entry-count watermark for the bottom jump band's "new" counter —
    /// renderer-written (the same frame-feedback `Cell` discipline):
    /// every FOLLOWING frame (scroll-back 0) stamps the transcript entry
    /// count; while scrolled back, the difference is what arrived unseen.
    pub bottom_watermark: std::cell::Cell<usize>,
    /// The transcript viewport of the LAST rendered frame — written by
    /// the renderer beside [`Self::scroll_max`] (the same frame-feedback
    /// `Cell` discipline). The drag-autoscroll edge test reads it at the
    /// dispatch seam; a zero-height rect (nothing rendered yet) disarms
    /// the edges.
    pub transcript_view: std::cell::Cell<ratatui::layout::Rect>,
    /// The status bar width of the LAST rendered frame — the W-INP status
    /// mirror composes [`crate::render::status_left_string`] at exactly
    /// the width the frame wrapped its yields against, so mirror and
    /// screen read the same strip. 0 until a status bar has rendered.
    pub status_width: std::cell::Cell<u16>,
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
    /// Journal timestamp of the current provider-open wait. `Thinking` begins
    /// one request attempt; every other run state clears it. The existing
    /// status strip combines this with the selected provider's open budget so
    /// a slow interactive request is visible rather than looking hung.
    pub provider_wait_started_at_ms: Option<u64>,
    /// Whether the terminal renders 24-bit color — set once at startup by the
    /// runtime (see [`crate::runtime::truecolor_capable`]) and read by render
    /// to pick the Thinking verb's shimmer fidelity: `true` rides the full
    /// three-step falloff, `false` degrades to the two-tone wave (W-E LE6).
    /// Defaults to `true` (the app emits truecolor everywhere; a non-graphics
    /// terminal only downgrades on positive low-color evidence). Never
    /// persisted; a pure presentation capability.
    pub truecolor: bool,
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
    /// 970 — the open Google Antigravity FIRST-LOGIN disclosure, carrying the
    /// add kind it runs on confirmation. `Some` only while this profile has
    /// not yet journalled [`GOOGLE_ANTIGRAVITY_TERMS_SUBJECT`]: the owner's
    /// decision is one warning before the first login, then the standing
    /// `/accounts` badge — never a gate and never a repeated interstitial.
    pub antigravity_consent: Option<AccountAddKind>,
    /// Terms warnings this profile has been shown and accepted, seeded at
    /// boot from [`crate::terms_journal`]. A durable user DECISION, not a
    /// display preference: it lives in the profile's acknowledgement journal
    /// rather than the TUI settings file.
    pub acknowledged_terms: BTreeSet<String>,
    /// Monotonic commit counter for [`Self::acknowledged_terms`] — the
    /// persistence sync writes when it moves (the theme/notification shape,
    /// so a re-affirmation is never silently dropped).
    pub terms_ack_commits: u64,
    /// `/hooks` screen state (H4): the `hooks.list` snapshot, cursor,
    /// confirmation card and in-flight receipt gate. APP-level like
    /// `tools_inventory` — the listing is workspace truth, not session
    /// display state.
    pub hooks: crate::hooks::HooksScreenState,
    /// The ATTACHED session's journaled hook facts + decision-chip state
    /// (H4). Checked in/out with the session exactly like `branch_state`
    /// (the A→B→A law).
    pub hook_facts: crate::hooks::HookFactsLog,
    /// W-A: the ATTACHED session's background task rows (journal
    /// projection). Session-scoped by runtime law — checked in/out whole
    /// with the session, never split per branch.
    pub tasks: crate::taskrows::TaskPanel,
    /// `/resume` browser: the selected row index into
    /// [`AppModel::session_browser_rows`], and the screen to return to on
    /// esc (the browser is reachable from the launcher AND a session).
    pub session_browser_sel: usize,
    /// Search-as-you-type query for the all-sessions browser.
    pub session_browser_query: String,
    pub session_browser_return: Option<Screen>,
    /// W-G: the live token-throughput sampler for the ACTIVE session. Fed on
    /// the existing frame clock (`note_throughput`) while a turn streams,
    /// reset to empty when idle — a pure ring buffer, so idle frames cost
    /// nothing and the readout is probe-reproducible.
    pub throughput: crate::throughput::ThroughputTracker,
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
            peer_agents: Vec::new(),
            peer_list_requested: false,
            ssh_profiles: Vec::new(),
            shells: Vec::new(),
            monitor_count: 0,
            monitors: Vec::new(),
            // Dark is the registry default AND the detection fallback
            // (owner spec §3); main.rs resolves the persisted choice and
            // the detected appearance over this before the first frame.
            theme: ThemeKey::default(),
            theme_choice: ThemeChoice::default(),
            detected_system: ThemeKey::default(),
            theme_picker: None,
            effort_picker: None,
            model_picker: None,
            backtrack: None,
            last_backtrack_escape: None,
            theme_commits: 0,
            sanctum_tier: SanctumTier::default(),
            projection: SessionProjection::new(),
            transcript_layout: std::cell::RefCell::new(Default::default()),
            prompt_history: std::collections::VecDeque::new(),
            cache_usage: crate::cache_usage::SessionUsageFold::default(),
            pending_cache_change: None,
            identity: IdentityLine::default(),
            identity_pinned: false,
            composer: crate::composer::Composer::new(),
            mirrored_input_attachments: None,
            drafts: std::collections::HashMap::new(),
            upload_seq: 0,
            session_title: None,
            session_name: None,
            // The scratch surface's canonical head (the demo script's
            // voice); real sessions claim theirs from the roster.
            session_head: ("Hasan".to_owned(), "(a)".to_owned()),
            msg_queue: Vec::new(),
            queue_panel: QueuePanelState::default(),
            queue_mode: false,
            subturn_mode: false,
            voice: VoiceState::default(),
            listening: false,
            talk: crate::talk::TalkState::default(),
            talk_setup: None,
            talk_config: haider_stt::config::TranscriptionConfig::default(),
            talk_config_error: None,
            launcher_dir: "~/dev/enterprise-suite".to_owned(),
            cwd: "/".to_owned(),
            session_dir: "~/dev/enterprise-suite".to_owned(),
            session_workspace_cwd: None,
            card_seq: 0,
            vfs: vfs_seed(),
            launcher_shellout: None,
            chips: Vec::new(),
            branch_state: crate::branch::BranchState::default(),
            view_path: Vec::new(),
            subtree_collapsed: false,
            fleet: crate::fleet::FleetView::default(),
            graph: None,
            workflow_graph: haider_client::WorkflowGraphProjection::default(),
            workflow_graph_rpc: haider_client::WorkflowGraphRpcAdapter::default(),
            workflow_graph_error: None,
            workflow_evidence_inspection: None,
            loom_types: Vec::new(),
            workflow_catalog: Vec::new(),
            loom_workflows: Vec::new(),
            initial_session: None,
            loom_cli_present: std::collections::BTreeMap::new(),
            loom_loaded: false,
            loom_requested: false,
            loom_selection: 0,
            loom_detail: false,
            loom_scroll: 0,
            loom_scroll_max: std::cell::Cell::new(0),
            loom_return: None,
            loom_pane: LoomPane::default(),
            loom_authoring: None,
            next_loom_authoring_generation: 1,
            graph_inspect: None,
            retry_inflight: false,
            graph_unsupported: false,
            todos_collapsed: false,
            auto_resuming: false,
            aura: AuraModel::seed(),
            mode: RuntimeMode::Demo,
            login: None,
            // The first three generations the allocator can hand out, so a
            // fresh process's seeds are 1-3 exactly as before and
            // `next_ui_generation` continues at 4.
            sessions: seed_session_states(UiGeneration::FIRST.get()),
            session_metrics: std::collections::HashMap::new(),
            session_cache_rates: std::collections::HashMap::new(),
            session_attention: std::collections::HashMap::new(),
            session_created_at_ms: std::collections::HashMap::new(),
            session_last_models: std::collections::HashMap::new(),
            session_kinds: std::collections::HashMap::new(),
            active_session: None,
            custom_commands: Vec::new(),
            custom_command_warnings: Vec::new(),
            focused: true,
            focus_reported: false,
            notifications_enabled: true,
            notification_commits: 0,
            model_commits: 0,
            notification_run_state: None,
            background_notification_states: std::collections::HashMap::new(),
            notifications: Vec::new(),
            last_detached: None,
            next_ui_generation: UiGeneration::FIRST.get() + SEED_SESSION_COUNT,
            roster: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                crate::script::ROSTER_FIRST_CLAIM,
            )),
            demo_requests: Vec::new(),
            menu_selection: 0,
            plan_scroll: std::cell::Cell::new(0),
            plan_scroll_max: std::cell::Cell::new(0),
            plan_menu_seen: std::cell::RefCell::new(None),
            palette_selection: 0,
            palette_scroll: 0,
            palette_dismissed: false,
            help_open: false,
            shells_open: false,
            shells_cursor: 0,
            ssh_open: false,
            ssh_cursor: 0,
            ssh_remove_armed: None,
            ssh_form: None,
            ssh_terminal_size: haider_rpc::SshPtySizeWire {
                cols: 80,
                rows: 22,
                pixel_width: 0,
                pixel_height: 0,
            },
            ssh_terminal: None,
            monitors_open: false,
            monitors_cursor: 0,
            monitors_stop_armed: None,
            monitors_firing: std::collections::HashSet::new(),
            lockdown_overlay: false,
            lockdown_status: None,
            lockdown_provider: None,
            lockdown_boundary_known: false,
            flash: None,
            composer_notice: None,
            update_available: None,
            profile_diagnostic: None,
            compatibility_diagnostic: None,
            voice_diagnostic: None,
            supervisor_diagnostic: None,
            command_diagnostic: None,
            outbox: Vec::new(),
            requests: Vec::new(),
            turn_active: false,
            scroll_back: std::cell::Cell::new(0),
            bottom_watermark: std::cell::Cell::new(0),
            scroll_max: std::cell::Cell::new(0),
            transcript_view: std::cell::Cell::new(ratatui::layout::Rect::default()),
            status_width: std::cell::Cell::new(0),
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
            provider_wait_started_at_ms: None,
            // Assume truecolor until the runtime proves otherwise at startup
            // (the app emits 24-bit color everywhere); tests and demo stay
            // true and render the full-fidelity shimmer.
            truecolor: true,
            // No graphics wordmark until the runtime queries the terminal at
            // startup; every non-graphics terminal and all tests stay None and
            // render falls back to the half-block art in `crate::mark`.
            wordmark: std::cell::RefCell::new(None),
            accounts: AccountsState::default(),
            providers: ProvidersState::default(),
            daemon_features: std::collections::BTreeSet::new(),
            daemon_version: None,
            oauth_add: None,
            oauth_attempt_seq: 0,
            custom_add: None,
            custom_attempt_seq: 0,
            antigravity_consent: None,
            acknowledged_terms: BTreeSet::new(),
            terms_ack_commits: 0,
            hooks: crate::hooks::HooksScreenState::default(),
            hook_facts: crate::hooks::HookFactsLog::default(),
            tasks: crate::taskrows::TaskPanel::default(),
            session_browser_sel: 0,
            session_browser_query: String::new(),
            session_browser_return: None,
            throughput: crate::throughput::ThroughputTracker::new(),
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
            Screen::Loom => DraftKey::Loom,
            _ => self.session_draft_key(),
        }
    }

    /// Install the metadata-only attachment refs carried by an applied
    /// foreign input surface. The session key makes a prior session's chips
    /// invisible immediately on a surface switch.
    pub fn set_mirrored_input_attachments(
        &mut self,
        session: SessionId,
        attachments: Vec<haider_protocol::hook::HookAttachmentMetadata>,
    ) {
        self.mirrored_input_attachments = Some((session, attachments));
        self.dirty = true;
    }

    /// The active session's foreign input refs, if the current composer
    /// mirror applied them. They are render-only and never submit.
    #[must_use]
    pub fn mirrored_input_attachments(&self) -> &[haider_protocol::hook::HookAttachmentMetadata] {
        self.mirrored_input_attachments
            .as_ref()
            .filter(|(session, _)| self.active_session.as_ref() == Some(session))
            .map_or(&[], |(_, attachments)| attachments)
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
                bytes: Some(u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
                artifact: None,
                carried: None,
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

    /// ⌃V / ⌘V / ⌃⇧V — attach the OS clipboard's IMAGE (970 owner bug 2).
    ///
    /// This is `/attach` with a different source of bytes, so it holds
    /// EXACTLY the same gates in the same order, and the bytes land through
    /// the same seam ([`Self::begin_attachment_upload`]) wearing the same
    /// chip. Two differences, both deliberate:
    ///
    /// * the read is issued blind. The reducer cannot know whether the
    ///   clipboard holds an image, text or nothing without performing IO,
    ///   so the vision gate is re-checked in the shell effect once the
    ///   content is actually known — a picture is refused there, and TEXT
    ///   never reaches the gate at all;
    /// * a wrong-surface press is SILENT. ⌃V is a keystroke, not a typed
    ///   command: flashing "session only" at someone who reflex-pasted on
    ///   the launcher would be noise, where `/attach` was a deliberate ask
    ///   that deserves an answer.
    fn paste_clipboard_image(&mut self) {
        if self.screen != Screen::Session {
            return;
        }
        // Same live-mode law as `/attach`: attachments are daemon CAS
        // truth, and the demo has no store to hold bytes.
        if self.mode.fabricates_locally() {
            self.flash =
                Some("· paste — live only; attachments ride the daemon's store".to_owned());
            self.dirty = true;
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_ARTIFACT_PUT_V1) {
            self.flash = Some(self.stale_daemon_note("attachments"));
            self.dirty = true;
            return;
        }
        if self.composer.attachments().len() >= MAX_TURN_ATTACHMENTS {
            self.flash = Some("· 5 attachments a turn — ⌫ at the start removes one".to_owned());
            self.dirty = true;
            return;
        }
        // The vision gate BEFORE the read, when the answer is already
        // known: a pair that declares no vision never pays for a clipboard
        // round trip. The draft is kept, untouched.
        if let Some(notice) = self.image_refusal() {
            self.set_composer_notice(notice);
            return;
        }
        self.requests.push(AppRequest::ClipboardRead);
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

    /// A paste over the pill thresholds — the Claude Code pill (QoL
    /// wave, retiring B4b's paste-as-artifact and the sim's literal
    /// token alike): the draft shows an atomic `[Pasted text #N +K
    /// lines]` placeholder, the content parks on the draft's side store,
    /// and submit expands it back byte-exact at the placeholder's
    /// position ([`crate::composer::Composer::expand_pastes`]). Local on
    /// every surface and mode — no daemon feature, no upload, nothing
    /// fabricated: the daemon receives the full text with the submit.
    /// `/attach` keeps the B4b artifact pipeline; pastes never enter it.
    fn big_paste(&mut self, text: &str, raw_lines: usize) {
        self.composer
            .insert_paste(text.replace("\r\n", "\n").replace('\r', "\n"), raw_lines);
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
        self.close_backtrack();
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
            Screen::Sessions => "haider — sessions".to_owned(),
            Screen::Accounts => "haider — accounts".to_owned(),
            Screen::Tree => "haider — session tree".to_owned(),
            Screen::Tools => "haider — tools".to_owned(),
            Screen::Providers => "haider — providers".to_owned(),
            Screen::Hooks => "haider — hooks".to_owned(),
            Screen::Usage => "haider — usage".to_owned(),
            Screen::Fleet => "haider — fleet".to_owned(),
            Screen::Graph => "haider — graph".to_owned(),
            Screen::Loom => "haider — loom".to_owned(),
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

    /// Structured status fields for the input/status mirror. The same typed
    /// status source builds the visible badge; this never parses its rendered
    /// text back into semantics.
    #[must_use]
    pub fn status_badge_state_detail(&self) -> (String, Option<String>) {
        let (state, detail) = self.projection.status_state_detail();
        let live = tree_live_count(&self.chips);
        if live > 0
            && ((state == "idle" && detail.is_none())
                || self.projection.waiting_on_local_subagent())
        {
            let plural = if live > 1 { "s" } else { "" };
            return (
                "waiting".to_owned(),
                Some(format!("{live} subagent{plural}")),
            );
        }
        if let Some(progress) = self.provider_wait_progress() {
            return (state, Some(progress));
        }
        (state, detail)
    }

    /// Interactive provider-open progress for the existing status line. The
    /// provider roster carries custom overrides; absent facts retain the
    /// lane-O 60-second default.
    #[must_use]
    pub fn provider_wait_progress(&self) -> Option<String> {
        if !self.projection.is_thinking() {
            return None;
        }
        let started_at_ms = self.provider_wait_started_at_ms?;
        let budget_ms = self
            .providers
            .providers
            .iter()
            .find(|summary| summary.provider == self.identity.provider)
            .and_then(|summary| summary.response_open_timeout_ms)
            .unwrap_or(60_000);
        let elapsed_ms = self.clock_ms.saturating_sub(started_at_ms).min(budget_ms);
        let remaining_ms = budget_ms.saturating_sub(elapsed_ms);
        Some(format!(
            "waiting for provider · {} elapsed · {} left",
            crate::format::fmt_elapsed(elapsed_ms),
            crate::format::fmt_elapsed(remaining_ms)
        ))
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
            // Loom: a static registry browse — nothing pulses.
            Screen::Loom => false,
            // Sessions browser: a busy row's dot pulses, exactly as the
            // launcher's does (the same roster truth, more of it).
            Screen::Sessions => self.sessions.iter().any(crate::session::SessionState::busy),
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
            // Fleet: live agents pulse their ◉ glyph / matrix dots on the
            // shared clock (mockup `.fg.live`); a settled or terminal
            // snapshot keeps the gate closed.
            Screen::Fleet => self
                .fleet
                .snapshot
                .as_ref()
                .is_some_and(crate::fleet::has_live),
            // Graph: the current node's `…` beat pulses only while the graph
            // is unfinished; a completed/abandoned reduction is static.
            Screen::Graph => self
                .graph
                .as_ref()
                .is_some_and(haider_protocol::graph::GraphStatus::is_unfinished),
            Screen::Session | Screen::Subagent => {
                // `● thinking…` (tui.js:4458-4462) · the ⚒ running tool
                // glyph (tui.js:4524-4530) · the processing todo's box
                // (tui.js:4694-4697) · chip glyph pulses (tui.js:4823-4834)
                // — plus the viewed chip's own thinking tail and tool rows
                // on the subagent screen.
                //
                // This term MUST track the tail indicator's render gate
                // (`render.rs`, `projection.is_turn_active()`), not the
                // narrower Thinking beat: this function is the ONLY thing that
                // advances `anim_phase`, so a state that renders the pulsing
                // row without registering here would paint it FROZEN — a dead
                // `●` and a static shimmer for the whole stream. Widening it
                // costs no idle wakeups, because every state it adds is one in
                // which a turn is actively running.
                self.projection.is_turn_active()
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
                    // W-A: a RUNNING background task ticks its elapsed
                    // figure in the band above the composer on the same
                    // clock; terminal tasks leave the band and keep the
                    // gate closed.
                    || self.tasks.running_count() > 0
                    || (self.screen == Screen::Subagent
                        && self.viewed_chip().is_some_and(|chip| {
                            // Tracks the chip tail's render gate for the same
                            // reason the session term above does: the row only
                            // breathes while this function keeps the clock
                            // running. Same `display_state()` truth as there.
                            chip.display_state().is_turn_active()
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
                let identity = row.account_identity.as_ref().map_or_else(
                    || row.identity.clone(),
                    haider_protocol::credential::AccountIdentity::summary,
                );
                let created = row.created_at_ms.map_or_else(
                    || "unknown (added before 0.0.964)".to_owned(),
                    |created| created.to_string(),
                );
                let mut desc = format!(
                    "{} · {} · {identity} · added {created}",
                    row.provider,
                    auth_label(row.method)
                );
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
        // W-C M1: the loaded custom commands, as `(name, palette-desc)` rows
        // already ordered by name (the loader sorts). The palette merges them
        // OVER the built-ins and paints them visually distinct.
        let custom_commands = self
            .custom_commands
            .iter()
            .map(|command| (command.name.clone(), command.palette_desc()))
            .collect();
        crate::commands::DynamicSlots {
            providers,
            models,
            accounts,
            efforts,
            custom_commands,
        }
    }

    /// W-C M1: install the loaded custom commands (shell-owned IO does the
    /// disk read; the reducer only ever sees the parsed data). A non-empty
    /// warning list is surfaced once as a flash so a malformed drop-in is
    /// visible instead of silently lost.
    pub fn set_custom_commands(
        &mut self,
        commands: Vec<crate::custom_commands::CustomCommand>,
        warnings: Vec<String>,
    ) {
        self.custom_commands = commands;
        if !warnings.is_empty() {
            let first = warnings.first().cloned().unwrap_or_default();
            self.flash = Some(if warnings.len() == 1 {
                format!("· custom command skipped — {first}")
            } else {
                format!("· {} custom commands skipped — {first}", warnings.len())
            });
        }
        self.custom_command_warnings = warnings;
    }

    /// W-C M1: the loaded custom command matching `name` (namespaced,
    /// case-insensitive), if any.
    #[must_use]
    pub fn custom_command(&self, name: &str) -> Option<&crate::custom_commands::CustomCommand> {
        let lowered = name.to_ascii_lowercase();
        self.custom_commands
            .iter()
            .find(|command| command.name == lowered)
    }

    /// W-C M2: record terminal focus and latch that focus IS reported (so the
    /// fire-anyway fallback only holds on emulators that never report it).
    pub fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        self.focus_reported = true;
    }

    /// W-C M2: seed the notification toggle from persisted settings.
    pub fn set_notifications_enabled(&mut self, enabled: bool) {
        self.notifications_enabled = enabled;
    }

    /// W-C M2: flip (or set) the desktop-notification toggle and bump the
    /// commit counter the runtime watches to persist the change.
    pub fn toggle_notifications(&mut self, enable: Option<bool>) {
        let next = enable.unwrap_or(!self.notifications_enabled);
        if next != self.notifications_enabled {
            self.notifications_enabled = next;
            self.notification_commits += 1;
        }
        self.flash = Some(format!(
            "· notifications {}",
            if next { "on" } else { "off" }
        ));
        self.dirty = true;
    }

    /// W-C M2: the desktop-notification decision for the ATTACHED session's
    /// run state. Edge-triggered — only a NEW transition into a trigger state
    /// (Done / Errored / a permission·input·device-wait park) fires, so a
    /// replay of the same state never re-notifies (one per turn, never
    /// mid-stream). Gated on the toggle and on the terminal being UNFOCUSED
    /// (or focus never reported). The masked line is queued for the runtime,
    /// which owns the tty-gated OSC 9 emission.
    pub fn note_run_state_for_notifications(&mut self, state: &RunState) {
        let previous = self.notification_run_state.replace(state.clone());
        if previous.as_ref() == Some(state) {
            return;
        }
        let Some(attention) = crate::notify::attention_for(state) else {
            return;
        };
        if !self.notifications_enabled {
            return;
        }
        // Fire only when unfocused; a terminal that never reports focus fires
        // regardless (a redundant ping beats a missed one).
        if self.focus_reported && self.focused {
            return;
        }
        let title = self.session_title.as_deref();
        self.notifications
            .push(crate::notify::notification_line(attention, title));
    }

    /// W-C M10: the desktop-notification edge for a BACKGROUND session (one not
    /// checked out on screen). Same trigger set, toggle, and focus gate as the
    /// attached path [`Self::note_run_state_for_notifications`], but keyed per
    /// session so each backgrounded/parked turn's terminal or attention-park
    /// transition is observed exactly once. `title` is the background session's
    /// own title (masked downstream, like the attached line).
    pub fn note_background_run_state_for_notifications(
        &mut self,
        session: &SessionId,
        state: &RunState,
        title: Option<&str>,
    ) {
        let previous = self
            .background_notification_states
            .insert(session.clone(), state.clone());
        if previous.as_ref() == Some(state) {
            return;
        }
        let Some(attention) = crate::notify::attention_for(state) else {
            return;
        };
        if !self.notifications_enabled {
            return;
        }
        if self.focus_reported && self.focused {
            return;
        }
        self.notifications
            .push(crate::notify::notification_line(attention, title));
    }

    /// W-C M2: drain the queued notification lines (the runtime emits each as
    /// OSC 9 to the tty).
    pub fn take_notifications(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notifications)
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

    /// Whether the session's CURRENT pair accepts image attachments, as the
    /// DAEMON projects it (`ModelDetailWire::supports_vision`, itself a
    /// projection of each adapter's `capabilities().vision`).
    ///
    /// `None` is "the daemon says nothing about this pair" — an older
    /// daemon, or a catalog row from before the field existed. The client
    /// holds no tables and must NOT invent the answer: it attaches and lets
    /// the daemon refuse with its typed `vision_unsupported`. Only a
    /// DECLARED `Some(false)` is a refusal this side may act on.
    #[must_use]
    pub fn pair_accepts_images(&self) -> Option<bool> {
        self.current_pair_detail()
            .and_then(|detail| detail.supports_vision)
    }

    /// The refusal to raise when this pair cannot take a picture, or `None`
    /// when an image may proceed. The ONE gate both image entry points use.
    #[must_use]
    pub fn image_refusal(&self) -> Option<ImageNotice> {
        (self.pair_accepts_images() == Some(false)).then(|| ImageNotice::NoVision {
            model: self.identity.model_short.clone(),
        })
    }

    /// Raise an image notice on the composer band, keeping the draft.
    pub fn set_composer_notice(&mut self, notice: ImageNotice) {
        self.composer_notice = Some(notice);
        self.dirty = true;
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
            // W-C M1: a custom command ghosts like a built-in — the whole
            // body is the fragment, so complete the remainder of the name.
            PaletteItem::Custom { name, .. } => {
                let rest = name.strip_prefix(body)?;
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
        self.handle_at(event, std::time::Instant::now());
    }

    /// Deterministic clock seam for rapid Esc backtracking laws.
    pub fn handle_at(&mut self, event: AppEvent, now: std::time::Instant) {
        match event {
            AppEvent::Key(key) => {
                self.dirty = true;
                self.flash = None;
                // The image notice is transient exactly like the flash: it
                // answers ONE gesture, and the next keystroke is the user
                // moving on. Cleared BEFORE dispatch so the very key that
                // raises a new one (⌃V) still shows it.
                self.composer_notice = None;
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
                self.handle_key(key, now);
            }
            AppEvent::Paste(text) => {
                self.dirty = true;
                // The zeroizing wrapper is borrowed, never unwrapped
                // (TUI6.3 fix 2): the wrapped copy wipes when `text`
                // drops at the end of this arm. What flows into the
                // composer — inline text or a pill's side store — is
                // draft content by intent, retained exactly as long as
                // the draft itself.
                let text = text.as_str();
                if self.ssh_terminal.is_some() {
                    if let Some(id) = self
                        .ssh_terminal
                        .as_ref()
                        .and_then(|terminal| terminal.shell_id.clone())
                    {
                        self.queue_ssh_terminal_input(&id, text.as_bytes());
                    }
                    return;
                }
                if self.ssh_form.is_some() {
                    self.ssh_form_paste(text);
                    return;
                }
                // Keys are pasted more often than typed; the paste lands in
                // the masked buffer and NOWHERE else (no pill token, no
                // draft, no ring).
                if let Some(card) = self.login.as_mut() {
                    card.push_str(text);
                    return;
                }
                // The custom-provider card owns paste exactly as it owns
                // typing. Insert at its character-indexed caret; demo cards
                // remain the sim's non-editable fabrication menu.
                if let Some(card) = self.custom_add.as_mut() {
                    if !self.mode.fabricates_locally() {
                        for character in text.chars() {
                            insert_custom_card_character(card, character);
                        }
                    }
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
                self.close_backtrack();
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
                if self.screen == Screen::Loom {
                    if self
                        .loom_authoring
                        .as_ref()
                        .is_some_and(|authoring| authoring.pending)
                    {
                        self.flash = Some(
                            "· Loom editor is locked while validation is in flight".to_owned(),
                        );
                        return;
                    }
                    self.composer
                        .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                    self.note_loom_author_edit();
                    self.palette_dismissed = false;
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
            AppEvent::SurfaceInputReplace { text } => {
                // The Loom editor is a dedicated local surface. A volatile
                // session-input mirror must never replace its bytes.
                if self.screen == Screen::Loom {
                    return;
                }
                self.composer.set_text(text);
                self.dirty = true;
            }
            AppEvent::Envelope(payload) => {
                self.dirty = true;
                if let Some(session_id) = self.active_session.clone() {
                    let activity_ms = wall_clock_ms();
                    self.note_session_activity_at(&session_id, activity_ms);
                    // Bare payloads are the local demo/mock twin and are
                    // reduced while this surface is visibly attached. Keep
                    // its local seen marker level with that activity; live
                    // daemon streams still receive authoritative seen truth
                    // through session.seen summaries.
                    if let Some(attention) = self.session_attention.get_mut(&session_id) {
                        attention.seen_at_ms = Some(activity_ms);
                    }
                }
                if let EventPayload::UserMessage { text, .. } = payload.as_ref() {
                    // A bare payload carries no envelope: this path is the
                    // demo/mock twin, whose prompts have no durable
                    // sequence. Recording one would fabricate a fork cut.
                    self.record_prompt(crate::session::PromptEntry::local(text.clone()));
                }
                self.handle_envelope(&payload);
            }
            AppEvent::UpdateAvailable { version } => {
                self.dirty = true;
                let display = update_version_label(&version);
                self.update_available = Some(version);
                self.flash = Some(format!("· update available — {display} · /update"));
            }
            AppEvent::UpdateCurrent { version } => {
                self.dirty = true;
                self.update_available = None;
                self.flash = Some(format!("· up to date — {}", update_version_label(&version)));
            }
            AppEvent::UpdateFailed { message } => {
                self.dirty = true;
                self.flash = Some(format!("· update failed — {message}"));
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

    // ---- The fleet view (slice 1 — see `crate::fleet`) ----

    /// Whether ⌥F / the summary row can open the fleet view: the session
    /// has a subagent tree to show, or a snapshot already held for it.
    #[must_use]
    pub fn fleet_available(&self) -> bool {
        !self.chips.is_empty() || self.fleet.snapshot.is_some()
    }

    /// Open the fleet view for the CURRENT session (session-born — never a
    /// menu destination). Demo synthesizes the snapshot from the local
    /// chip tree at open and renders it once (the terminal-session shape);
    /// live mode keeps a same-session snapshot warm under the refresh and
    /// issues the `session.fleet` read.
    pub fn open_fleet(&mut self) {
        if !self.fleet_available() {
            self.flash = Some("· no subagents to fleet".to_owned());
            self.dirty = true;
            return;
        }
        if self.mode.fabricates_locally() {
            let session = self
                .active_session
                .clone()
                .unwrap_or_else(|| SessionId::new("demo-session"));
            self.fleet.snapshot = Some(crate::fleet::snapshot_from_chips(
                &self.chips,
                &session,
                self.clock_ms,
            ));
            self.fleet.fetching = false;
        } else {
            if !self.daemon_serves(haider_rpc::FEATURE_SESSION_FLEET_V1) {
                self.flash = Some("· fleet needs a newer daemon (session_fleet_v1)".to_owned());
                self.dirty = true;
                return;
            }
            let Some(active) = self.active_session.clone() else {
                self.flash = Some("· fleet — session only".to_owned());
                self.dirty = true;
                return;
            };
            // A warm snapshot for ANOTHER session must never flash on
            // screen while this session's read is in flight.
            if self
                .fleet
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.session_id != active)
            {
                self.fleet.snapshot = None;
            }
            self.fleet.fetching = true;
            self.requests.push(AppRequest::FleetRefresh);
        }
        self.fleet.stack.clear();
        self.fleet.sel = 0;
        self.fleet.error = None;
        self.switch_surface(Screen::Fleet);
        self.dirty = true;
    }

    /// The current level's navigable count and density — ONE derivation
    /// for keys, clicks and render (the density is a pure function of the
    /// re-rooted subtree size; slice 1 has no manual toggle).
    #[must_use]
    pub fn fleet_nav(&self) -> Option<(usize, crate::fleet::Density)> {
        let snapshot = self.fleet.snapshot.as_ref()?;
        let (level, _) = crate::fleet::resolve(snapshot, &self.fleet.stack);
        let total = crate::fleet::rollup(level).total;
        let density = crate::fleet::density(total);
        let count = match density {
            crate::fleet::Density::List => total,
            crate::fleet::Density::Grid => level.len(),
        };
        Some((count, density))
    }

    /// The agent at the current selection, in the current density's order.
    fn fleet_selected_agent(&self) -> Option<(haider_protocol::ids::AgentId, SessionId, bool)> {
        let snapshot = self.fleet.snapshot.as_ref()?;
        let (level, _) = crate::fleet::resolve(snapshot, &self.fleet.stack);
        let (_, density) = self.fleet_nav()?;
        let node = match density {
            crate::fleet::Density::List => crate::fleet::flatten(level).get(self.fleet.sel)?.node,
            crate::fleet::Density::Grid => level.get(self.fleet.sel)?,
        };
        Some((
            node.agent_id.clone(),
            node.session_id.clone(),
            !node.children.is_empty(),
        ))
    }

    /// ⏎ — re-root on the selected subtree; a LEAF opens its DETAIL frame
    /// (owner 2026-08-16: transcript + the member's own dynamically-made
    /// workflow) and asks the daemon for the member's child-graph status.
    fn fleet_drill(&mut self) {
        let Some((agent, session, has_children)) = self.fleet_selected_agent() else {
            return;
        };
        if !has_children {
            self.fleet.detail = Some(agent);
            self.fleet.detail_graph = None;
            if self.mode == RuntimeMode::Live {
                self.requests.push(AppRequest::FleetMemberGraph { session });
            }
            self.dirty = true;
            return;
        }
        self.fleet.stack.push(agent);
        self.fleet.sel = 0;
    }

    /// Open a fleet member's OWN transcript — the chip view rooted on that
    /// member. Three things this does that the old inline body did not:
    /// it walks the REAL chip path (`path_to_chip`) instead of assuming a
    /// depth-1 member, so a nested member keeps a truthful breadcrumb; it
    /// enters through [`Self::switch_surface`], the documented draft-swap
    /// authority every other subagent door already uses; and it releases
    /// the scroll, so the view opens ON the member's own session instead of
    /// inheriting wherever the previous surface happened to be scrolled.
    /// Returns false when the member is not one of this session's chips.
    fn open_fleet_member_transcript(&mut self, agent: &str) -> bool {
        let Some(path) = path_to_chip(&self.chips, agent) else {
            return false;
        };
        self.view_path = path;
        self.switch_surface(Screen::Subagent);
        self.scroll_back.set(0);
        self.dirty = true;
        true
    }

    /// The armed member's display name — the fleet's own callsign rule,
    /// which falls back to the opaque id rather than inventing a name.
    pub(crate) fn fleet_member_label(&self, agent: &haider_protocol::ids::AgentId) -> String {
        let fallback = || agent.as_str().to_owned();
        let Some(snapshot) = self.fleet.snapshot.as_ref() else {
            return fallback();
        };
        let (level, _) = crate::fleet::resolve(snapshot, &self.fleet.stack);
        crate::fleet::flatten(level)
            .into_iter()
            .find(|row| &row.node.agent_id == agent)
            .map_or_else(fallback, |row| crate::fleet::callsign(row.node).to_owned())
    }

    /// One press of the destroy affordance. The FIRST press only arms it,
    /// naming the member; the second press on the SAME member acts. Offered
    /// only from the member-detail frame — a destroy is never one keystroke
    /// away from a moving list.
    fn fleet_kill_step(&mut self) {
        let Some(agent) = self.fleet.detail.clone() else {
            self.fleet.kill_armed = None;
            return;
        };
        if self.fleet.kill_armed.as_ref() != Some(&agent) {
            let label = self.fleet_member_label(&agent);
            self.fleet.kill_armed = Some(agent);
            self.flash = Some(format!("· destroy {label}? press d again to confirm"));
            return;
        }
        self.fleet.kill_armed = None;
        self.fleet_kill_confirmed(&agent);
    }

    /// A CONFIRMED destroy. Demo mode may only close one of its own locally
    /// fabricated chips. Live mode delegates ownership validation and the
    /// terminal transition to `agent.cancel`; the daemon checks the control
    /// attachment and direct-child boundary. Either way the member KEEPS its
    /// row: a destroyed subagent reads `⊘ cancelled`, it never just vanishes.
    fn fleet_kill_confirmed(&mut self, agent: &haider_protocol::ids::AgentId) {
        let label = self.fleet_member_label(agent);
        if self.mode.fabricates_locally() {
            if find_chip(&self.chips, agent.as_str()).is_none() {
                self.flash = Some(format!(
                    "· {label} runs on its own session — this view cannot destroy it"
                ));
                self.dirty = true;
                return;
            }
            self.requests.push(AppRequest::ChipClose {
                agent: agent.as_str().to_owned(),
            });
            self.flash = Some(format!("· destroying {label} — its row reads ⊘ cancelled"));
        } else if self.daemon_serves(haider_rpc::FEATURE_AGENT_CANCEL_V1) {
            self.requests.push(AppRequest::AgentCancel {
                agent: agent.as_str().to_owned(),
            });
            self.flash = Some(format!("· cancelling {label} — waiting for daemon truth"));
        } else {
            self.flash = Some(self.stale_daemon_note("destroying a subagent"));
        }
        self.dirty = true;
    }

    fn fleet_move(&mut self, delta: isize) {
        let Some((count, _)) = self.fleet_nav() else {
            return;
        };
        if count == 0 {
            self.fleet.sel = 0;
            return;
        }
        let current = self.fleet.sel.min(count - 1) as isize;
        self.fleet.sel =
            usize::try_from((current + delta).clamp(0, count as isize - 1)).unwrap_or(0);
    }

    /// Fleet-screen keys (the F2a full-screen ownership law — every other
    /// key is swallowed, no composer beneath). Esc is drill-scoped: up one
    /// level, and out to the session only from the root.
    fn handle_fleet_key(&mut self, code: KeyCode) {
        self.dirty = true;
        // Every key but the confirming `d` DISARMS a pending destroy: an
        // arm never survives navigation (the `ssh_remove_armed` discipline).
        if !matches!(code, KeyCode::Char('d')) {
            self.fleet.kill_armed = None;
        }
        let grid = matches!(self.fleet_nav(), Some((_, crate::fleet::Density::Grid)));
        let stride = if grid {
            isize::try_from(self.fleet.grid_cols.get().max(1)).unwrap_or(1)
        } else {
            1
        };
        let page = isize::try_from(self.fleet.page_rows.get().max(1)).unwrap_or(1);
        match code {
            KeyCode::Esc => {
                // The detail frame closes FIRST — esc walks back out the
                // way it came (detail → level → parent level → session).
                if self.fleet.detail.take().is_some() {
                    self.fleet.detail_graph = None;
                    self.dirty = true;
                } else if self.fleet.stack.pop().is_none() {
                    self.switch_surface(Screen::Session);
                }
                self.fleet.sel = 0;
            }
            KeyCode::Enter => {
                // From a member's detail frame, ⏎ opens the FULL transcript
                // (the chip view) when this member is one of the active
                // session's own chips; deeper/foreign members keep the
                // honest flash.
                if let Some(agent) = self.fleet.detail.clone() {
                    if !self.open_fleet_member_transcript(agent.as_str()) {
                        self.flash = Some(
                            "· transcript lives on the member's own session — attach to view"
                                .to_owned(),
                        );
                        self.dirty = true;
                    }
                } else {
                    self.fleet_drill();
                }
            }
            // The destroy step — detail-frame only, always two presses.
            KeyCode::Char('d') => self.fleet_kill_step(),
            KeyCode::Up | KeyCode::Char('k') => self.fleet_move(-stride),
            KeyCode::Down | KeyCode::Char('j') => self.fleet_move(stride),
            KeyCode::Left if grid => self.fleet_move(-1),
            KeyCode::Right if grid => self.fleet_move(1),
            KeyCode::PageUp => self.fleet_move(-(page * stride)),
            KeyCode::PageDown => self.fleet_move(page * stride),
            KeyCode::Home => self.fleet.sel = 0,
            KeyCode::End => {
                if let Some((count, _)) = self.fleet_nav() {
                    self.fleet.sel = count.saturating_sub(1);
                }
            }
            _ => {}
        }
    }

    /// A clicked row/cell's index in the current density's order — `None`
    /// when the refreshed snapshot no longer holds the agent the stale
    /// rect was measured on (the hit then does nothing, honestly).
    fn fleet_index_of(&self, agent: &str) -> Option<usize> {
        let snapshot = self.fleet.snapshot.as_ref()?;
        let (level, _) = crate::fleet::resolve(snapshot, &self.fleet.stack);
        let (_, density) = self.fleet_nav()?;
        match density {
            crate::fleet::Density::List => crate::fleet::flatten(level)
                .iter()
                .position(|row| row.node.agent_id.as_str() == agent),
            crate::fleet::Density::Grid => level
                .iter()
                .position(|node| node.agent_id.as_str() == agent),
        }
    }

    /// A committed `session.fleet` snapshot landed. Guarded by session
    /// identity — a stale reply for a session no longer attached installs
    /// nothing.
    pub fn apply_fleet_snapshot(&mut self, snapshot: haider_rpc::SessionFleetSnapshot) {
        if self
            .active_session
            .as_ref()
            .is_some_and(|active| active != &snapshot.session_id)
        {
            return;
        }
        self.fleet.snapshot = Some(snapshot);
        self.fleet.fetching = false;
        self.fleet.error = None;
        // Clamp the selection into the refreshed level; a drill hop that
        // vanished resolves to the nearest surviving ancestor at render.
        if let Some((count, _)) = self.fleet_nav() {
            self.fleet.sel = self.fleet.sel.min(count.saturating_sub(1));
        } else {
            self.fleet.sel = 0;
        }
        self.dirty = true;
    }

    /// The `session.fleet` read failed — rendered honestly on the screen,
    /// never a silent stale view.
    pub fn fleet_failed(&mut self, message: &str) {
        self.fleet.fetching = false;
        self.fleet.error = Some(message.to_owned());
        self.dirty = true;
    }

    /// The socket died: nothing is in flight any more (receipt-free read —
    /// nothing resends it). A held snapshot stays; the reconnect resume
    /// re-reads if the screen is still open.
    pub fn fleet_note_disconnect(&mut self) {
        if self.fleet.fetching {
            self.fleet.fetching = false;
            self.dirty = true;
        }
    }

    /// Install a fresh `graph.status` reduction for the active session.
    /// `None` (never pinned, or abandoned into oblivion) clears the strip.
    /// Stale replies for a since-switched session install nothing.
    pub fn apply_graph_status(
        &mut self,
        session_id: &haider_protocol::ids::SessionId,
        status: Option<haider_protocol::graph::GraphStatus>,
    ) {
        // Owner 2026-08-16 (fleet member detail): a graph answer for the
        // OPEN detail member's session fills the member's workflow section
        // — session-tagged, so a since-closed detail installs nothing.
        if let Some(detail) = &self.fleet.detail {
            let member_session = self.fleet.snapshot.as_ref().and_then(|snapshot| {
                let (_, path) = crate::fleet::resolve(snapshot, &self.fleet.stack);
                let level = path
                    .last()
                    .map_or(snapshot.roots.as_slice(), |node| node.children.as_slice());
                crate::fleet::flatten(level)
                    .into_iter()
                    .find(|row| &row.node.agent_id == detail)
                    .map(|row| row.node.session_id.clone())
            });
            if member_session.as_ref() == Some(session_id) {
                self.fleet.detail_graph = Some((session_id.clone(), status.clone()));
                self.dirty = true;
            }
        }
        if self
            .active_session
            .as_ref()
            .is_some_and(|active| active != session_id)
        {
            return;
        }
        self.graph = status;
        self.graph_unsupported = false;
        self.dirty = true;
    }

    /// Install one complete `workflow.graph.state` baseline for the active
    /// session. A reply crossing a session switch is ignored.
    pub fn apply_workflow_graph_state(
        &mut self,
        session_id: &haider_protocol::ids::SessionId,
        state: Option<haider_protocol::graph::WorkflowGraphState>,
    ) {
        if self.active_session.as_ref() != Some(session_id) {
            return;
        }
        match state {
            Some(state) => {
                let mut adapter = self.workflow_graph_rpc.clone();
                match adapter.replace(state) {
                    Ok(projected) => match self.workflow_graph.replace(projected) {
                        Ok(()) => {
                            self.workflow_graph_rpc = adapter;
                            self.workflow_graph_error = None;
                            self.workflow_evidence_inspection = None;
                        }
                        Err(error) => self.workflow_graph_error = Some(error.to_string()),
                    },
                    Err(error) => self.workflow_graph_error = Some(error.to_string()),
                }
            }
            None => {
                self.workflow_graph.clear();
                self.workflow_graph_rpc.clear();
                self.workflow_graph_error = None;
                self.workflow_evidence_inspection = None;
            }
        }
        if self.screen == Screen::Loom
            && self.loom_pane == LoomPane::Workflows
            && self.loom_selection == 0
            && let Some(index) = self
                .workflow_graph
                .workflow_id()
                .and_then(|workflow_id| self.workflow_row_index(workflow_id))
        {
            self.loom_selection = index;
        }
        self.dirty = true;
    }

    /// Apply one cursor-bounded `workflow.graph.watch` page. A mismatch leaves
    /// the cursor and last good view untouched so the driver can reconnect
    /// from exactly the greatest applied cursor.
    pub fn apply_workflow_graph_page(
        &mut self,
        session_id: &haider_protocol::ids::SessionId,
        page: haider_protocol::graph::WorkflowGraphWatchPage,
    ) -> WorkflowGraphPageOutcome {
        if self.active_session.as_ref() != Some(session_id) {
            return WorkflowGraphPageOutcome::Ignored;
        }
        let Some(cursor) = self.workflow_graph.cursor() else {
            self.workflow_graph_error =
                Some(haider_client::WorkflowGraphRpcAdapterError::BaselineRequired.to_string());
            self.dirty = true;
            return WorkflowGraphPageOutcome::Rebaseline;
        };
        let mut adapter = self.workflow_graph_rpc.clone();
        match adapter.apply_page(cursor, page) {
            Ok(page) => {
                let has_more = page.next_cursor < page.replay_through_cursor;
                match self.workflow_graph.apply_page(page) {
                    Ok(changed) => {
                        self.workflow_graph_rpc = adapter;
                        self.workflow_graph_error = None;
                        if changed
                            && self.workflow_evidence_inspection.as_ref().is_some_and(
                                |inspection| {
                                    self.workflow_graph
                                        .rejection(&inspection.node_id)
                                        .is_none_or(|rejection| {
                                            rejection.cursor != inspection.cursor
                                        })
                                },
                            )
                        {
                            self.workflow_evidence_inspection = None;
                        }
                        self.dirty |= changed;
                        WorkflowGraphPageOutcome::Applied { has_more }
                    }
                    Err(error) => {
                        self.workflow_graph_error = Some(error.to_string());
                        self.dirty = true;
                        WorkflowGraphPageOutcome::Rebaseline
                    }
                }
            }
            Err(error) => {
                self.workflow_graph_error = Some(error.to_string());
                self.dirty = true;
                WorkflowGraphPageOutcome::Rebaseline
            }
        }
    }

    /// M2c: install the `graph.inspect` telemetry snapshot for the active
    /// session (rollups, tool-selection stats, evidence provenance). Ignores a
    /// stale reply for a since-switched session, like `apply_graph_status`.
    /// Owner 2026-08-16 (manual retry): issue `run.retry` for the ACTIVE
    /// terminal-failed session. Honest refusals: nothing failed → flash;
    /// stale daemon → the standard note; already in flight → no-op.
    pub fn issue_run_retry(&mut self) {
        let Some(session) = self.active_session.clone() else {
            self.flash = Some("· /retry — no attached session".to_owned());
            self.dirty = true;
            return;
        };
        if self.retry_inflight {
            return;
        }
        if self.projection.workspace_unavailable().is_some() {
            if !self.daemon_serves(haider_rpc::FEATURE_SESSION_WORKSPACE_SET_V1) {
                self.flash = Some(self.stale_daemon_note("workspace recovery"));
                self.dirty = true;
                return;
            }
            let path = self.cwd.clone();
            let retry_after = self.projection.run_errored() || self.projection.retrying().is_some();
            self.flash = Some(format!("· /retry — re-root to {path}"));
            self.retry_inflight = true;
            self.requests.push(AppRequest::WorkspaceSet {
                session,
                path,
                retry_after,
            });
            self.dirty = true;
            return;
        }
        // Owner 2026-08-17: mid-BACKOFF is retryable too — the daemon's
        // wake seam short-circuits the remaining delay (same run.retry
        // command; attempt numbering preserved). Only idle-never-failed
        // keeps the honest refusal.
        if !self.projection.run_errored() && self.projection.retrying().is_none() {
            self.flash = Some("· /retry — the last run did not fail".to_owned());
            self.dirty = true;
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_RUN_RETRY_V1) {
            self.flash = Some(self.stale_daemon_note("manual retry"));
            self.dirty = true;
            return;
        }
        self.retry_inflight = true;
        self.requests.push(AppRequest::RunRetry { session });
        self.dirty = true;
    }

    /// Grant card: open the exact macOS System Settings pane for the parked
    /// permission (routes to `computer.permission_open_settings`, which maps the
    /// enum to a compiled deep link — the TUI never sends a URL). No-op unless a
    /// card is live and a session is attached.
    pub fn request_permission_open_settings(&mut self) {
        let Some(card) = self.projection.permission_card() else {
            return;
        };
        let Some(session) = self.active_session.clone() else {
            return;
        };
        let request_id = card.request_id.clone();
        let permission = card.permission;
        self.requests.push(AppRequest::OpenPermissionSettings {
            session,
            request_id,
            permission,
        });
        self.dirty = true;
    }

    /// Grant card: recheck the OS permission now by answering the paired
    /// `computer-os-permission` menu's `retry` option — the same durable
    /// menu-answer path the daemon's automatic poll also uses, so there is
    /// exactly one authorization channel. No-op unless the card's menu is open.
    pub fn retry_permission(&mut self) {
        let Some(card) = self.projection.permission_card() else {
            return;
        };
        let menu_id = card.menu_id.clone();
        let Some(menu) = self.projection.open_menu() else {
            return;
        };
        if menu.id != menu_id {
            return;
        }
        let index = menu
            .options
            .iter()
            .position(|option| option.key == "retry")
            .unwrap_or(0);
        self.menu_selection = index;
        self.submit_menu_answer();
    }

    /// The correlated `run.retry` reply: daemon truth — a fresh run is live
    /// on the SAME user turn.
    pub fn apply_run_retried(&mut self, session: &SessionId) {
        if self.active_session.as_ref() == Some(session) {
            self.retry_inflight = false;
            self.flash = Some("· ↻ retrying — same turn, fresh run".to_owned());
            self.dirty = true;
        }
    }

    /// A refused/failed `run.retry`: surface the daemon's reason and re-arm.
    pub fn run_retry_failed(&mut self, message: &str) {
        self.retry_inflight = false;
        self.flash = Some(format!("· ↻ retry refused — {message}"));
        self.dirty = true;
    }

    pub fn apply_graph_inspect(
        &mut self,
        session_id: &haider_protocol::ids::SessionId,
        snapshot: haider_protocol::graph::GraphInspectSnapshot,
    ) {
        if self
            .active_session
            .as_ref()
            .is_some_and(|active| active != session_id)
        {
            return;
        }
        self.graph_inspect = Some(snapshot);
        self.dirty = true;
    }

    /// The attached daemon predates `convergence_graph_v1`: `/graph` refuses
    /// honestly rather than pretending the subsystem exists.
    pub fn graph_unsupported(&mut self) {
        self.graph = None;
        self.graph_unsupported = true;
        self.dirty = true;
    }

    fn handle_key(&mut self, key: KeyEvent, now: std::time::Instant) {
        if self.ssh_terminal.is_some() {
            self.handle_ssh_terminal_key(key);
            return;
        }
        if self.ssh_form.is_some() {
            self.handle_ssh_form_key(key);
            return;
        }
        if self.lockdown_overlay {
            self.lockdown_overlay = false;
            self.dirty = true;
            return;
        }
        if self.screen == Screen::Loom
            && self
                .loom_authoring
                .as_ref()
                .is_some_and(|authoring| authoring.pending)
        {
            self.flash = Some("· Loom editor is locked while validation is in flight".to_owned());
            self.dirty = true;
            return;
        }
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

        // 970 owner bug 2 — the PASTE-IMAGE chord, ahead of the plain ⌃
        // block so it catches every spelling terminals actually send:
        // ⌃V (passed through everywhere), ⌘V (macOS terminals speaking the
        // kitty keyboard protocol, which report SUPER), and ⌃⇧V (the Linux
        // terminal paste chord, when the emulator forwards it instead of
        // answering it). A terminal that answers the chord ITSELF sends a
        // bracketed paste instead — that is the TEXT path, and it is
        // untouched by this arm.
        //
        // Loom keeps its own ⌃V (`/validate`), so it is excluded rather
        // than shadowed.
        if matches!(key.code, KeyCode::Char('v' | 'V'))
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
            && self.screen != Screen::Loom
            && self.composer_owns_input()
        {
            self.paste_clipboard_image();
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
                // The Loom/Workflows tab's registry actions live on ⌃ since
                // the tab grew a live composer (owner 2026-08-22) — and the
                // global ⌃ block runs before the screen dispatch, so they
                // have to be answered here or they never arrive.
                KeyCode::Char('p') if self.screen == Screen::Loom => {
                    if self.loom_authoring.is_some() {
                        self.flash = Some(
                            "· close the Loom editor before changing registry selection".to_owned(),
                        );
                    } else {
                        match self.loom_pane {
                            LoomPane::Workflows => self.pin_selected_workflow(),
                            LoomPane::Types => self.bind_selected_type(),
                        }
                    }
                }
                KeyCode::Char('n') if self.screen == Screen::Loom => {
                    self.seed_loom_authoring();
                }
                KeyCode::Char('s') if self.screen == Screen::Loom => {
                    self.confirm_loom_authoring();
                }
                KeyCode::Char('v') if self.screen == Screen::Loom => {
                    self.validate_loom_document();
                }
                KeyCode::Char('a') if self.screen == Screen::Loom => {
                    self.archive_selected_loom();
                }
                KeyCode::Char('x') if self.screen == Screen::Loom => {
                    self.cancel_loom_install();
                }
                KeyCode::Char('i') if self.screen == Screen::Loom => {
                    self.seed_cli_provisioning();
                }
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
        if self.screen == Screen::Sessions {
            self.handle_sessions_key(key);
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
        if self.shells_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.shells_open = false,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.shells_cursor = self.shells_cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.shells_cursor = self
                        .shells_cursor
                        .saturating_add(1)
                        .min(self.shells.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(shell) = self.shells.get(self.shells_cursor) {
                        self.requests.push(AppRequest::ShellClose {
                            id: shell.id.clone(),
                        });
                        self.flash = Some(format!("· closing shell {}…", shell.id));
                    }
                }
                _ => {}
            }
            self.dirty = true;
            return;
        }
        if self.ssh_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.ssh_open = false;
                    self.ssh_remove_armed = None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.ssh_cursor = self.ssh_cursor.saturating_sub(1);
                    self.ssh_remove_armed = None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.ssh_cursor = self
                        .ssh_cursor
                        .saturating_add(1)
                        .min(self.ssh_profiles.len().saturating_sub(1));
                    self.ssh_remove_armed = None;
                }
                KeyCode::Enter => {
                    if let Some(profile) = self.ssh_profiles.get(self.ssh_cursor) {
                        let profile = profile.name.clone();
                        self.open_ssh_terminal(profile);
                    }
                }
                KeyCode::Char('a') => {
                    self.ssh_form = Some(SshProfileForm::add());
                    self.ssh_remove_armed = None;
                }
                KeyCode::Char('e') => {
                    if let Some(profile) = self.ssh_profiles.get(self.ssh_cursor) {
                        self.ssh_form = Some(SshProfileForm::edit(profile));
                        self.ssh_remove_armed = None;
                    }
                }
                KeyCode::Char('t') => {
                    if let Some(profile) = self.ssh_profiles.get(self.ssh_cursor) {
                        self.requests.push(AppRequest::SshTest {
                            profile: profile.name.clone(),
                        });
                        self.flash = Some(format!("· testing SSH profile {}…", profile.name));
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(profile) = self.ssh_profiles.get(self.ssh_cursor) {
                        let name = profile.name.clone();
                        if self.ssh_remove_armed.as_deref() == Some(name.as_str()) {
                            self.requests.push(AppRequest::SshRemove {
                                profile: name.clone(),
                            });
                            self.ssh_remove_armed = None;
                            self.flash = Some(format!("· removing SSH profile {name}…"));
                        } else {
                            self.ssh_remove_armed = Some(name.clone());
                            self.flash = Some(format!(
                                "· remove SSH profile {name}? press d again to confirm"
                            ));
                        }
                    }
                }
                _ => {}
            }
            self.dirty = true;
            return;
        }
        // 970 owner item 2: keyboard parity with the overlay's click
        // targets — j/k select, x stops (arm-then-confirm), p pauses or
        // resumes by the row's own state, t triggers, e hands the edit to
        // the agent, y copies the id. The overlay owns every key while it
        // is showing, exactly as `/ssh` and `/shells` do.
        if self.monitors_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.monitors_open = false;
                    self.monitors_stop_armed = None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.monitors_cursor = self.monitors_cursor.saturating_sub(1);
                    self.monitors_stop_armed = None;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.monitors_cursor = self
                        .monitors_cursor
                        .saturating_add(1)
                        .min(self.monitors.len().saturating_sub(1));
                    self.monitors_stop_armed = None;
                }
                KeyCode::Char('x') => {
                    if let Some(id) = self.monitors_selected_id() {
                        if self.monitors_stop_armed.as_deref() == Some(id.as_str()) {
                            self.monitors_stop_armed = None;
                            self.monitor_stop(id);
                        } else {
                            self.monitors_stop_armed = Some(id.clone());
                            self.flash =
                                Some(format!("· stop monitor {id}? press x again to confirm"));
                        }
                    }
                }
                KeyCode::Char('p') => {
                    if let Some(id) = self.monitors_selected_id() {
                        self.monitors_stop_armed = None;
                        self.monitor_toggle_pause(id);
                    }
                }
                KeyCode::Char('t') => {
                    if let Some(id) = self.monitors_selected_id() {
                        self.monitors_stop_armed = None;
                        self.monitor_trigger(id);
                    }
                }
                KeyCode::Char('e') => {
                    if let Some(id) = self.monitors_selected_id() {
                        self.monitors_stop_armed = None;
                        self.monitor_edit_with_agent(&id);
                    }
                }
                KeyCode::Char('y') => {
                    if let Some(id) = self.monitors_selected_id() {
                        self.monitors_stop_armed = None;
                        self.monitor_copy_id(&id);
                    }
                }
                _ => {}
            }
            self.dirty = true;
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
        // The fleet view owns its keys entirely (the same full-screen
        // ownership law); esc is drill-scoped — up one level, out to the
        // session only from the root.
        if self.screen == Screen::Fleet {
            self.handle_fleet_key(key.code);
            return;
        }
        // CG-M1: the graph status view is a read-only full screen (the usage
        // precedent). Esc walks back to the session; every other key is
        // swallowed — pin/abandon are `/graph` commands, not hotkeys.
        if self.screen == Screen::Graph {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.screen = Screen::Session;
                self.dirty = true;
            }
            return;
        }
        // D3 — /loom browser: ↑↓ move over types+workflows, ⏎ opens the
        // detail pane, esc backs out (detail → list → where you came from).
        if self.screen == Screen::Loom {
            // The selected live-session model drafts the typed document.
            // Match this before the printable-character arm below.
            if key.code == KeyCode::Char('m') && key.modifiers.contains(KeyModifiers::ALT) {
                self.open_model_picker(String::new());
                self.dirty = true;
                return;
            }
            // Both supported row spaces are fixed-head (synthetic `none`
            // first). A zero workflow total means the catalog feature is
            // unavailable, never an empty catalog.
            let total = match self.loom_pane {
                LoomPane::Types => self.type_row_count(),
                LoomPane::Workflows => self.workflow_row_count(),
            };
            // Round 3: a registry that emptied under an open detail pane
            // leaves no subject — fold the pane so esc means one press.
            if total == 0 {
                self.loom_detail = false;
                self.workflow_evidence_inspection = None;
            }
            // In an open workflow detail, an empty-composer Enter opens the
            // first rejected node's published evidence coordinate. The
            // ordinary Enter arm below still opens a catalog row initially;
            // Esc closes this subview before it closes the row detail.
            if key.code == KeyCode::Enter
                && self.loom_authoring.is_none()
                && !key.modifiers.contains(KeyModifiers::SHIFT)
                && self.composer.text().trim().is_empty()
                && self.open_selected_workflow_rejection_evidence()
            {
                self.dirty = true;
                return;
            }
            let ceiling = self.loom_scroll_max.get();
            match key.code {
                // W-flow: `p` binds the SELECTED row to the bound session —
                // workflows pin by name (`none` abandons); types bind the
                // receipted agent type (`none` clears to plain).
                // Owner 2026-08-22: the tab carries a LIVE composer, so every
                // printable key belongs to it — authoring here is a
                // conversation. Registry actions moved onto modifiers and
                // navigation keys; `p` (pin/bind) is now ⌃P, and the create
                // affordance is the clickable ＋ row or ⌃N.
                // Printable input goes to the composer, exactly as it does on
                // the session screen.
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.composer.insert_str(&c.to_string());
                    self.note_loom_author_edit();
                }
                KeyCode::Backspace => {
                    self.composer.backspace();
                    self.note_loom_author_edit();
                }
                KeyCode::Delete if self.loom_authoring.is_some() => {
                    self.composer.delete_forward();
                    self.note_loom_author_edit();
                }
                KeyCode::Left if self.loom_authoring.is_some() => {
                    self.composer
                        .move_left(key.modifiers.contains(KeyModifiers::SHIFT));
                }
                KeyCode::Right if self.loom_authoring.is_some() => {
                    self.composer
                        .move_right(key.modifiers.contains(KeyModifiers::SHIFT));
                }
                KeyCode::Home if self.loom_authoring.is_some() => {
                    self.composer
                        .line_home(key.modifiers.contains(KeyModifiers::SHIFT));
                }
                KeyCode::End if self.loom_authoring.is_some() => {
                    self.composer
                        .line_end_key(key.modifiers.contains(KeyModifiers::SHIFT));
                }
                KeyCode::Up if self.loom_authoring.is_some() => {
                    let _ = self
                        .composer
                        .line_up(key.modifiers.contains(KeyModifiers::SHIFT));
                }
                KeyCode::Down if self.loom_authoring.is_some() => {
                    let _ = self
                        .composer
                        .line_down(key.modifiers.contains(KeyModifiers::SHIFT));
                }
                // W-flow authoring: describe it, the model makes it.
                KeyCode::Up if self.loom_detail => {
                    self.loom_scroll = self.loom_scroll.min(ceiling).saturating_sub(1);
                }
                KeyCode::Down if self.loom_detail => {
                    self.loom_scroll = self.loom_scroll.saturating_add(1).min(ceiling);
                }
                KeyCode::Up => {
                    self.loom_selection = self.loom_selection.saturating_sub(1);
                }
                KeyCode::Down if total > 0 => {
                    self.loom_selection = (self.loom_selection + 1).min(total - 1);
                }
                // An open editor owns ⏎ even when the user deleted every
                // byte: the empty edit must reach typed validation. Without
                // an editor, nonempty prose starts the draft RPC and an empty
                // composer keeps the registry-detail action unambiguous.
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.composer.insert_str("\n");
                    self.note_loom_author_edit();
                }
                KeyCode::Enter if self.loom_authoring.is_some() => {
                    self.submit_loom_turn();
                }
                KeyCode::Enter if !self.composer.text().trim().is_empty() => {
                    self.submit_loom_turn();
                }
                KeyCode::Enter if total > 0 => {
                    self.loom_detail = !self.loom_detail;
                    if !self.loom_detail {
                        self.workflow_evidence_inspection = None;
                    }
                    self.loom_scroll = 0;
                    // Unknown ceiling until the pane paints once.
                    self.loom_scroll_max.set(u16::MAX);
                }
                KeyCode::Tab => {
                    if self.loom_authoring.is_some() {
                        self.composer.insert_str("  ");
                        self.note_loom_author_edit();
                        self.dirty = true;
                        return;
                    }
                    // ⇄ the sibling registry pane, selection reset — the
                    // sim's two surfaces, one keystroke apart.
                    if self.loom_pane == LoomPane::Types
                        && !self.daemon_serves(haider_rpc::FEATURE_WORKFLOW_CATALOG_V1)
                    {
                        self.flash = Some(self.stale_daemon_note("workflow catalog"));
                        return;
                    }
                    self.loom_pane = match self.loom_pane {
                        LoomPane::Types => LoomPane::Workflows,
                        LoomPane::Workflows => LoomPane::Types,
                    };
                    self.loom_selection = if self.loom_pane == LoomPane::Workflows
                        && self.daemon_serves(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1)
                    {
                        self.workflow_graph
                            .workflow_id()
                            .and_then(|workflow_id| self.workflow_row_index(workflow_id))
                            .unwrap_or(0)
                    } else {
                        0
                    };
                    self.loom_detail = false;
                    self.workflow_evidence_inspection = None;
                    self.loom_scroll = 0;
                    if self.loom_pane == LoomPane::Workflows
                        && self.daemon_serves(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1)
                        && self.active_session.is_some()
                    {
                        self.requests.push(if self.workflow_graph.is_empty() {
                            AppRequest::WorkflowGraphRefresh
                        } else {
                            AppRequest::WorkflowGraphResume
                        });
                    }
                }
                KeyCode::Esc => {
                    if let Some(authoring) = self.loom_authoring.take() {
                        self.composer.clear();
                        self.flash = Some(if authoring.confirmed.is_some() {
                            "· Loom editor closed — confirmed revision remains registered"
                                .to_owned()
                        } else {
                            "· Loom draft closed — no new revision registered".to_owned()
                        });
                    } else if self.workflow_evidence_inspection.take().is_some() {
                        self.loom_scroll = 0;
                    } else if self.loom_detail {
                        self.loom_detail = false;
                        self.loom_scroll = 0;
                    } else {
                        // Round 3: esc returns to the screen /loom was
                        // OPENED from, not blindly to the session.
                        let target = self.loom_return.take().unwrap_or({
                            if self.active_session.is_some() {
                                Screen::Session
                            } else {
                                Screen::Launcher
                            }
                        });
                        self.switch_surface(target);
                    }
                }
                _ => {}
            }
            self.dirty = true;
            return;
        }
        // The computer-permission grant card replaces the blocking menu and
        // owns its action keys: `o` opens the exact Settings pane, `r`/⏎
        // rechecks now. Other keys fall through to the underlying permission
        // menu's ordinary handling.
        if self.screen == Screen::Session && self.projection.permission_card().is_some() {
            match key.code {
                KeyCode::Char('o') | KeyCode::Char('O') => {
                    self.request_permission_open_settings();
                    return;
                }
                KeyCode::Char('r') | KeyCode::Char('R') | KeyCode::Enter => {
                    self.retry_permission();
                    return;
                }
                _ => {}
            }
        }
        // D4: an open `plan` proposal owns the scroll keys — the document
        // fills the transcript area, so ↑↓/PgUp/PgDn page it and Tab cycles
        // the decision; digits/⏎ still answer through the ordinary menu path.
        if self.screen == Screen::Session
            && let Some(menu) = self.projection.open_menu()
            && menu.origin == "plan"
            && !menu.options.is_empty()
        {
            let plan_key = plan_menu_key(menu);
            if self.plan_menu_seen.borrow().as_ref() != Some(&plan_key) {
                *self.plan_menu_seen.borrow_mut() = Some(plan_key);
                self.plan_scroll.set(0);
                // Round 3: the ceiling for an UNPAINTED document is unknown —
                // never discard a keypress against a stale (or zero) max; the
                // next render publishes the real one and clamps.
                self.plan_scroll_max.set(u16::MAX);
            }
            let options = menu.options.len();
            // Review round 2: the STATE clamps too — the render clamp alone
            // let overscroll accumulate invisibly, so ↑ took many presses to
            // bite after paging past the end.
            let ceiling = self.plan_scroll_max.get();
            match key.code {
                KeyCode::Up => {
                    self.plan_scroll
                        .set(self.plan_scroll.get().min(ceiling).saturating_sub(1));
                    self.dirty = true;
                    return;
                }
                KeyCode::Down => {
                    self.plan_scroll
                        .set(self.plan_scroll.get().saturating_add(1).min(ceiling));
                    self.dirty = true;
                    return;
                }
                KeyCode::PageUp => {
                    self.plan_scroll
                        .set(self.plan_scroll.get().min(ceiling).saturating_sub(10));
                    self.dirty = true;
                    return;
                }
                KeyCode::PageDown => {
                    self.plan_scroll
                        .set(self.plan_scroll.get().saturating_add(10).min(ceiling));
                    self.dirty = true;
                    return;
                }
                KeyCode::Tab => {
                    self.menu_selection = (self.menu_selection + 1) % options.max(1);
                    self.dirty = true;
                    return;
                }
                _ => {}
            }
            self.handle_menu_key(key.code);
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
        // The prompt chooser is session chrome. A turn that started while
        // it was open (for example from a concurrent client) closes it and
        // restores Esc's interrupt ownership; otherwise the chooser owns
        // every key until it loads a prompt or closes.
        if self.backtrack.is_some() {
            if self.screen != Screen::Session || self.turn_active {
                self.close_backtrack();
            } else {
                self.handle_backtrack_key(key.code, now);
                return;
            }
        }
        // ⌥F — the fleet view for the CURRENT session (session-born entry,
        // mockup tui.js:3979). Gated on a fleet actually existing so ⌥f
        // keeps its readline word-right meaning everywhere else; the
        // daemon-menu and picker gates above already outranked this.
        if self.screen == Screen::Session
            && key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F'))
            && self.fleet_available()
        {
            self.open_fleet();
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
                            self.set_palette_argument(cmd, &value);
                        }
                        // W-C M1: tab on a custom command completes the name
                        // and opens an argument slot (a trailing space) so
                        // positionals can follow.
                        Some(PaletteItem::Custom { name, .. }) => {
                            self.composer.set_text(format!("/{name} "));
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
                // Esc always cancels a held talk before choosing its
                // streaming/idle meaning. Prompt backtracking must not let
                // the old TalkFire timer survive.
                self.listening = false;
                if self.turn_active {
                    // Esc mid-turn INTERRUPTS (sim, tui.js:2533-2539 +
                    // 1551-1567): the script stops, run → cancelled, badge
                    // ⏸ IDLE (i), a transcript note lands — and the session
                    // stays on screen. Only an idle esc walks back. The
                    // held queue drops with the turn (sim tui.js:1557).
                    self.turn_active = false;
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
                } else if self.composer.is_empty()
                    && self.composer.attachments().is_empty()
                    && self.rapid_backtrack_escape(now)
                {
                    self.open_backtrack(now);
                } else if self.composer.is_empty()
                    && self.composer.attachments().is_empty()
                    && !self.prompt_history.is_empty()
                {
                    // The first empty-idle Esc arms the short Claude Code
                    // gesture. The second opens history; no timer/task is
                    // needed because the next key supplies the clock.
                    self.last_backtrack_escape = Some(now);
                    self.flash = Some("· esc again — previous prompts".to_owned());
                    self.dirty = true;
                } else {
                    // OWNER DIRECTIVE: esc is SESSION-SCOPED — it
                    // interrupts, cancels menus and a held talk (P1-3's
                    // hold-cancel law survives the navigation change),
                    // never navigates. Back is `← main` (and ⌃C).
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
    /// rev933d finding 4: injection from an embedding client applies only
    /// when the PLAIN session composer is the live Enter target — no card
    /// (login, `/talk` setup, an engaged talk session), no menu, no help,
    /// and never the subagent view (whose composer messages the child).
    /// This mirrors the key-dispatch precedence exactly, so a synthesized
    /// Submit can never activate a card row or answer a question.
    pub fn accepts_injected_input(&self) -> bool {
        self.screen == Screen::Session
            && self.login.is_none()
            && self.talk_setup.is_none()
            && !self.talk.engaged()
            && self.composer_owns_input()
    }

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
        self.close_backtrack();
        if self.screen == Screen::Loom {
            self.note_loom_author_edit();
        }
    }

    fn record_prompt(&mut self, entry: crate::session::PromptEntry) {
        self.prompt_history.push_front(entry);
        self.close_backtrack();
    }

    fn rapid_backtrack_escape(&self, now: std::time::Instant) -> bool {
        self.last_backtrack_escape.is_some_and(|prior| {
            now.checked_duration_since(prior)
                .is_some_and(|elapsed| elapsed <= BACKTRACK_ESC_WINDOW)
        })
    }

    fn close_backtrack(&mut self) {
        self.backtrack = None;
        self.last_backtrack_escape = None;
    }

    fn open_backtrack(&mut self, now: std::time::Instant) {
        if self.prompt_history.is_empty() {
            self.flash = Some("· no previous prompts in this session".to_owned());
            self.last_backtrack_escape = None;
            return;
        }
        self.backtrack = Some(PromptBacktrack { selection: 0 });
        self.last_backtrack_escape = Some(now);
        self.flash = None;
        self.dirty = true;
    }

    fn handle_backtrack_key(&mut self, code: KeyCode, now: std::time::Instant) {
        let Some(mut chooser) = self.backtrack else {
            return;
        };
        let last = self.prompt_history.len().saturating_sub(1);
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                chooser.selection = chooser.selection.saturating_sub(1);
                self.backtrack = Some(chooser);
                self.last_backtrack_escape = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                chooser.selection = (chooser.selection + 1).min(last);
                self.backtrack = Some(chooser);
                self.last_backtrack_escape = None;
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = (c as usize) - ('1' as usize);
                if index < self.prompt_history.len() {
                    chooser.selection = index;
                    self.backtrack = Some(chooser);
                }
                self.last_backtrack_escape = None;
            }
            KeyCode::Enter => {
                if let Some(prompt) = self.prompt_history.get(chooser.selection) {
                    let text = prompt.text.clone();
                    self.composer.set_text(text);
                }
                self.close_backtrack();
            }
            // `f` FORKS at the chosen prompt — the same verb the session
            // tree binds for a fork (`tree_fork_selected`). ⏎ keeps its
            // shipped meaning (load into THIS session's composer); the
            // fork instead leaves this session untouched and opens a new
            // one at that prompt.
            KeyCode::Char('f') => self.fork_from_selected_prompt(),
            KeyCode::Esc if self.rapid_backtrack_escape(now) => {
                chooser.selection = (chooser.selection + 1).min(last);
                self.backtrack = Some(chooser);
                self.last_backtrack_escape = Some(now);
            }
            KeyCode::Esc => self.close_backtrack(),
            _ => {}
        }
        self.dirty = true;
    }

    /// Whether a prompt fork could be issued from this surface right now:
    /// a live session on a daemon serving BOTH fork tokens. The chooser's
    /// hint reads this so it never advertises a verb that can only refuse.
    #[must_use]
    pub fn prompt_fork_offered(&self) -> bool {
        !self.mode.fabricates_locally()
            && self.active_session.is_some()
            && self.daemon_serves(haider_rpc::FEATURE_SESSION_FORK_V1)
            && self.daemon_serves(haider_rpc::FEATURE_SESSION_PROMPT_FORK_V1)
    }

    /// Fork at the prompt the Esc-Esc chooser has highlighted (`f`).
    fn fork_from_selected_prompt(&mut self) {
        let Some(chooser) = self.backtrack else {
            return;
        };
        self.issue_prompt_fork(chooser.selection);
    }

    /// `/fork [number]` — the plain-frontend parity door for the Esc-Esc
    /// fork, exactly as `/history [number]` is for the recall (a terminal
    /// that cannot convey rapid double-Esc timing still reaches both).
    /// Bare opens the same chooser; an ordinal forks that row directly.
    fn fork_command(&mut self, remainder: &str) {
        self.dirty = true;
        if self.screen != Screen::Session {
            self.flash = Some("· /fork — session only".to_owned());
            return;
        }
        // The same idle gate `/history` keeps: the chooser is idle chrome
        // and `issue_prompt_fork` refuses a busy session anyway, so opening
        // one mid-turn would only offer a verb that cannot act.
        if self.turn_active {
            self.flash = Some("· /fork — wait for the turn to end".to_owned());
            return;
        }
        if remainder.is_empty() {
            self.open_backtrack(std::time::Instant::now());
            return;
        }
        let Some(index) = remainder
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
        else {
            self.flash = Some("· /fork <number> — fork at a previous prompt".to_owned());
            return;
        };
        self.issue_prompt_fork(index);
    }

    /// THE ONE DOOR to a prompt-oriented `session.fork`.
    ///
    /// `index` is the newest-first ordinal into [`Self::prompt_history`].
    /// Every refusal is an honest notice and NOTHING is installed locally:
    /// the daemon mints the child and the reply opens it. The source
    /// session, its transcript, and its attachment stream are untouched by
    /// construction — this issues no mutation against them.
    fn issue_prompt_fork(&mut self, index: usize) {
        self.dirty = true;
        // A forked child is a daemon-minted session; the demo has no daemon
        // to mint one, and its recalled prompts carry no durable sequence.
        if self.mode.fabricates_locally() {
            self.flash = Some("· fork — live only; the new session is daemon truth".to_owned());
            return;
        }
        // Feature gate BEFORE anything acts (the B2b lesson): BOTH the fork
        // method and the additive prompt-selector shape are required — a
        // daemon serving only the former reads the request as a fork with
        // no coordinates at all.
        if !self.daemon_serves(haider_rpc::FEATURE_SESSION_FORK_V1)
            || !self.daemon_serves(haider_rpc::FEATURE_SESSION_PROMPT_FORK_V1)
        {
            self.flash = Some(self.stale_daemon_note("forking at a previous prompt"));
            return;
        }
        let Some(session) = self.active_session.clone() else {
            self.flash = Some("· fork — no live session attached".to_owned());
            return;
        };
        // Same busy law as `issue_fork`: forking a live turn would split
        // ownership of its open menus, tools and children.
        if self.session_busy() {
            self.flash = Some("· fork — wait for the turn to end".to_owned());
            return;
        }
        let Some(entry) = self.prompt_history.get(index) else {
            self.flash = Some(format!("· /fork {} — no such prompt", index + 1));
            return;
        };
        // NEVER FABRICATE A CUT. A prompt with no committed sequence (the
        // local twin's) offers no coordinate; saying so beats inventing one.
        let Some(seq) = entry.seq else {
            self.flash = Some("· fork — that prompt carries no committed sequence".to_owned());
            return;
        };
        self.requests.push(AppRequest::ForkFromPrompt {
            session,
            source_branch: self.branch_state.active().cloned(),
            seq,
        });
        self.close_backtrack();
        self.flash = Some(format!(
            "· forking at prompt {} — this session stays open",
            index + 1
        ));
    }

    fn history_command(&mut self, remainder: &str) {
        if self.screen != Screen::Session {
            self.flash = Some("· /history — session only".to_owned());
            return;
        }
        if self.turn_active {
            self.flash = Some("· /history — wait for the turn to end".to_owned());
            return;
        }
        if remainder.is_empty() {
            self.open_backtrack(std::time::Instant::now());
            return;
        }
        let Some(index) = remainder
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
        else {
            self.flash = Some("· /history <number> — choose a previous prompt".to_owned());
            return;
        };
        if let Some(prompt) = self.prompt_history.get(index) {
            let text = prompt.text.clone();
            self.composer.set_text(text);
            self.close_backtrack();
        } else {
            self.flash = Some(format!("· /history {} — no such prompt", index + 1));
        }
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
        let display = if is_slash {
            self.composer.take_silent()
        } else {
            self.composer.take_for_submit()
        }
        .trim()
        .to_owned();
        self.palette_selection = 0;
        self.palette_scroll = 0;
        self.palette_dismissed = false;
        if display.is_empty() {
            // Empty ⏎ on the launcher re-attaches the most recently left
            // session (a port law; the detach model keeps it honest by id).
            if self.screen == Screen::Launcher
                && let Some(id) = self.last_detached.clone()
            {
                self.open_session(&id);
            }
            return;
        }
        if display.starts_with('/') {
            self.composer.set_text(display);
            self.execute_slash();
            return;
        }
        // QoL pill: every route below carries a real message, so the paste
        // pills expand HERE — once, after the outer trim (the pasted bytes
        // themselves are never trimmed) and after slash routing (a command
        // keeps its placeholders and their store). Route DECISIONS below
        // keep reading the DISPLAY text: a pasted body opening with `!` or
        // a shell word must never hijack a route the user did not type.
        let text = self.composer.expand_pastes(&display);
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
            && display.starts_with('!')
            && let Some(stripped) = text.strip_prefix('!')
        {
            if self.mode.fabricates_locally() {
                self.flash = Some(
                    "· ! — live shell escape (the demo shell is bare ls · cd · pwd)".to_owned(),
                );
            } else if stripped.trim().is_empty() {
                self.flash = Some("· ! — type a command".to_owned());
            } else if !self.daemon_serves_user_commands() {
                self.flash = Some(self.stale_daemon_note("the ! shell escape"));
            } else {
                self.requests.push(AppRequest::ShellExec {
                    command: stripped.to_owned(),
                    branch: self.branch_state.active().cloned(),
                });
            }
            self.dirty = true;
            return;
        }
        // Shell builtins run against the VFS — local, instant, NO model
        // turn (sim tui.js:1993-2008) — never on the subagent screen, and
        // they never start a session.
        let first_word = display.split_whitespace().next().unwrap_or("");
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
                let mode = if self.subturn_mode {
                    DeliveryMode::Subturn
                } else {
                    DeliveryMode::Steer
                };
                self.projection.apply(&EventPayload::UserMessage {
                    text,
                    attachments: vec![],
                    mode,
                });
                self.projection.push_note(if self.subturn_mode {
                    "· subturn — lands at the next tool call before it executes".to_owned()
                } else {
                    "· steered — delivered at the next safe boundary of the current turn".to_owned()
                });
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
            Ok(identity) => {
                card.stage = LoginStage::Done(identity);
                // The daemon committed the descriptor, but the login
                // receipt carries only its display identity. Re-read the
                // authoritative roster instead of fabricating a row from
                // the submitted provider/alias. This seam is shared by
                // every API-key account kind.
                self.requests.push(AppRequest::AccountsRefresh);
            }
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

    /// Continue a custom-server add after its masked key was staged and the
    /// discovery-backed provider configuration committed. The login card is
    /// reused as the typed account-login result surface, but starts directly
    /// in `Submitting`: it never receives or reconstructs the raw key.
    fn open_staged_login_card(&mut self, provider: &str, alias: String) -> u64 {
        self.stash_draft();
        self.login_attempt_seq += 1;
        let attempt = self.login_attempt_seq;
        let mut card = LoginCard::new(provider.to_owned(), alias, attempt);
        card.stage = LoginStage::Submitting;
        self.login = Some(card);
        self.dirty = true;
        attempt
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
    /// listening STOPS and commits the transcript into the composer for
    /// editing (owner 970: never an auto-send — see
    /// [`Self::talk_commit_stop`]); a press while starting aborts.
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
            crate::talk::TalkPhase::Listening => self.talk_commit_stop(),
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

    /// Stop law: COMMIT INTO THE COMPOSER. Input stops now; the engine
    /// assembles the definitive transcript and [`Self::handle_talk`]'s
    /// `Finished` arm realizes it into the composer AT THE CURSOR, where
    /// it is editable and the user sends it with ⏎.
    ///
    /// 970 owner requirement 1: dictation does NOT auto-send. The ONLY
    /// thing that makes this stop submit a turn is the explicit,
    /// default-off `transcription.auto_send` profile flag — so the
    /// gesture that ends listening hands you text to read, never a turn
    /// you never saw. Both stop gestures (the listening toggle and ⏎
    /// while listening) route here, because "must not auto-send" is a
    /// property of the transcript, not of which key ended it.
    fn talk_commit_stop(&mut self) {
        if self.talk.phase != crate::talk::TalkPhase::Listening {
            return;
        }
        self.talk.phase = crate::talk::TalkPhase::Finishing;
        self.talk.intent = if self.talk_config.auto_send {
            crate::talk::CommitIntent::Submit
        } else {
            crate::talk::CommitIntent::Insert
        };
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
                self.talk_commit_stop();
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
            TalkEvent::SupervisorRestarting { attempt, max } => {
                self.flash = Some(format!(
                    "· talk supervisor restarting — attempt {attempt}/{max}"
                ));
                self.dirty = true;
            }
            TalkEvent::SupervisorFailed { reason } => {
                let ghost = self.talk.ghost.trim().to_owned();
                if !ghost.is_empty() {
                    self.composer
                        .insert_str(crate::talk::clamp_realized(&ghost));
                }
                self.talk.settle();
                self.listening = false;
                self.supervisor_diagnostic = Some(haider_protocol::error::ErrorPresentation::new(
                    "talk-supervisor-unavailable",
                    "Talk unavailable",
                    reason,
                    haider_protocol::error::ErrorScope::Profile,
                    [haider_protocol::error::ErrorAction::Retry],
                ));
                self.dirty = true;
            }
            TalkEvent::Started { generation, .. } => {
                if generation == self.talk.generation && self.talk.phase == TalkPhase::Starting {
                    self.voice_diagnostic = None;
                    self.supervisor_diagnostic = None;
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
                        haider_stt::capture::CaptureHealth::Failed { error } => {
                            let ghost = self.talk.ghost.trim().to_owned();
                            if !ghost.is_empty() {
                                self.composer
                                    .insert_str(crate::talk::clamp_realized(&ghost));
                            }
                            self.talk.settle();
                            self.listening = false;
                            self.voice_diagnostic =
                                Some(haider_protocol::error::ErrorPresentation::new(
                                    "microphone-unavailable",
                                    "Microphone unavailable",
                                    &error,
                                    haider_protocol::error::ErrorScope::Profile,
                                    [haider_protocol::error::ErrorAction::Retry],
                                ));
                            format!("· mic: {error}")
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
                            // Owner 970: the transcript lands AT THE CURSOR
                            // with one separating space, the same shape the
                            // typing-commit path uses. With insert now the
                            // DEFAULT (no auto-send), dictating twice is the
                            // ordinary flow and the two transcripts must not
                            // fuse into one word. `submit_composer` trims, so
                            // the space costs the auto-send path nothing.
                            let mut realized = crate::talk::clamp_realized(&text).to_owned();
                            realized.push(' ');
                            self.composer.insert_str(&realized);
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
        self.accounts.message = self.accounts.adoption_candidate.as_ref().map(|candidate| {
            format!(
                "import {} login? y confirms · n cancels · `haider account import {} --confirm`",
                candidate.source_label, candidate.source
            )
        });
        // P1 MASK LAW (the U2 owner addendum): every open starts masked —
        // a reveal never survives into a later visit, whichever way the
        // last one ended (esc, ⌃C, a screen switch).
        self.accounts.revealed = false;
        self.switch_surface(Screen::Accounts);
        self.requests.push(AppRequest::AccountsRefresh);
        // Local-login detection rides screen entry. The report is
        // metadata-only; copying requires the separate y/confirm action.
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
            | haider_protocol::credential::CredentialStatus::Revoked
            | haider_protocol::credential::CredentialStatus::NeedsAttention { .. } => {
                self.accounts.message = Some(format!(
                    "· {alias} is not usable — /login to re-authenticate"
                ));
                self.dirty = true;
                return;
            }
            haider_protocol::credential::CredentialStatus::Ok
            | haider_protocol::credential::CredentialStatus::Limited { .. } => {}
        }
        let change = PendingCacheChange::Account {
            alias: alias.to_owned(),
        };
        let confirm_new_epoch = self.pending_cache_change.as_ref() == Some(&change);
        self.accounts.pending_select = Some(alias.to_owned());
        self.accounts.message = None;
        self.requests.push(AppRequest::AccountSetActive {
            alias: alias.to_owned(),
            confirm_new_epoch,
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
        if self.pending_cache_change.as_ref()
            == Some(&PendingCacheChange::Account {
                alias: descriptor.alias.as_str().to_owned(),
            })
        {
            self.pending_cache_change = None;
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
        self.auth_label_for(&self.identity.provider)
    }

    /// The same derivation for an ARBITRARY provider key. The auth flavor
    /// is a property of the provider and the account behind it, not of the
    /// session, so a child agent's provider resolves through exactly this
    /// ladder — no second, divergent rule.
    #[must_use]
    fn auth_label_for(&self, provider: &str) -> Option<&'static str> {
        use haider_protocol::credential::AuthMethod;
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

    /// The identity the composer band speaks for the surface being STEERED.
    ///
    /// Every surface but one speaks the SESSION's pair. The SUBAGENT
    /// surface steers a CHILD, so it must speak the CHILD's: the owner's
    /// screenshot showed the parent's `glm-5.2 · api` on the footer under a
    /// child header that correctly read `deepseek-v4-flash`. The bug was
    /// never only the model — the auth label, the reasoning level and the
    /// fast marker were all parent-scoped while a child was being steered.
    ///
    /// The child's model is its MANIFEST fact (`ChipModel::model`, stamped
    /// from `AgentManifest::model_profile`); its auth flavor comes from the
    /// provider it actually billed, or the one the fleet snapshot records.
    /// The session's reasoning level and fast marker are not the child's
    /// and no manifest carries them, so they DROP rather than mislabel the
    /// child — [`crate::fleet::node_metric`]'s law. A child with no model
    /// renders NO identity; the parent's never stands in for it.
    #[must_use]
    pub fn surface_composer_identity(&self, budget: usize) -> Option<String> {
        if self.screen != Screen::Subagent {
            return self.composer_identity(budget);
        }
        let Some(chip) = self.viewed_chip() else {
            return self.composer_identity(budget);
        };
        let model = chip.model.trim();
        if model.is_empty() {
            return None;
        }
        let mut candidates: Vec<String> = Vec::new();
        if let Some(auth) = self.child_auth_label(chip) {
            candidates.push(format!("{model} · {auth}"));
        }
        candidates.push(model.to_owned());
        candidates
            .into_iter()
            .find(|candidate| candidate.chars().count() <= budget)
    }

    /// A viewed child's auth flavor. Truth order: the provider the child
    /// ACTUALLY billed against (its own usage breakdowns carry the method
    /// outright), then the provider the fleet snapshot records for it, run
    /// through the shared [`Self::auth_label_for`] ladder. Breakdowns that
    /// disagree are ambiguous, and ambiguity renders nothing — never a
    /// coin-flip between two true answers.
    #[must_use]
    fn child_auth_label(&self, chip: &ChipModel) -> Option<&'static str> {
        use haider_protocol::credential::AuthMethod;
        if let Some(usage) = chip
            .metrics
            .as_ref()
            .and_then(|metrics| metrics.usage.as_ref())
        {
            let mut seen: Option<AuthMethod> = None;
            let mut ambiguous = false;
            for breakdown in &usage.breakdowns {
                let Some(method) = breakdown.auth_method else {
                    continue;
                };
                match seen {
                    None => seen = Some(method),
                    Some(held) if held == method => {}
                    Some(_) => ambiguous = true,
                }
            }
            if ambiguous {
                return None;
            }
            if let Some(method) = seen {
                return Some(match method {
                    AuthMethod::OAuth => "oauth",
                    AuthMethod::ApiKey => "api",
                });
            }
        }
        let snapshot = self.fleet.snapshot.as_ref()?;
        let provider = crate::fleet::find_node(snapshot, &chip.agent)?
            .provider
            .as_deref()
            .filter(|text| !text.is_empty())?;
        self.auth_label_for(provider)
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

    /// THE ONE DOOR into `/providers` (report §5.2).
    fn enter_providers(&mut self) {
        if self.screen == Screen::Providers {
            return;
        }
        self.providers.message = None;
        self.switch_surface(Screen::Providers);
        self.requests.push(AppRequest::ProvidersRefresh);
        // Re-check for a secret-free adoption offer on provider entry.
        if self.device_discovery_available() {
            self.requests.push(AppRequest::DeviceCandidatesRefresh);
        }
        self.dirty = true;
    }

    /// Esc from `/providers`: same routing as `/accounts`.
    fn exit_providers(&mut self) {
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
    fn enter_usage(&mut self, scope: UsageScope, filter: Option<&str>) {
        self.dirty = true;
        // Bare `/usage` still opens predictably on Accounts; direct scope
        // arguments select their requested destination before any read is
        // queued.
        self.usage.scope = scope;
        self.usage.model_range = UsageModelRange::default();
        self.usage.scroll.set(0);
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
            self.refresh_usage_scope_if_needed();
        }
    }

    fn refresh_usage_scope_if_needed(&mut self) {
        if self.mode.fabricates_locally()
            || !self.daemon_serves(haider_rpc::FEATURE_USAGE_HISTORY_V1)
        {
            return;
        }
        match self.usage.scope {
            UsageScope::History | UsageScope::Models
                if self.usage.history.is_none() && !self.usage.history_fetching =>
            {
                self.usage.history_fetching = true;
                self.requests.push(AppRequest::UsageHistoryRefresh);
            }
            UsageScope::Accounts
            | UsageScope::Global
            | UsageScope::History
            | UsageScope::Models
            | UsageScope::Calendar => {}
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

    /// Keys on `/usage` (U2). KEY-OWNERSHIP: esc closes (never a ⏎ action
    /// — the screen is read-only); ↑/↓ select account groups or scroll the
    /// Models table; ←/→ (and tab/shift-tab) cycle accounts; page/home/end
    /// scroll; `r` reveals identities except on Models where it cycles the
    /// range; `f` re-reads (live). Everything else is swallowed.
    fn handle_usage_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.exit_usage(),
            KeyCode::Up | KeyCode::Char('k') if self.usage.scope == UsageScope::Models => {
                self.usage
                    .scroll
                    .set(self.usage.scroll.get().saturating_sub(1));
                self.dirty = true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.usage.cursor = self.usage.cursor.saturating_sub(1);
                self.usage.follow_cursor.set(true);
                self.dirty = true;
            }
            KeyCode::Down | KeyCode::Char('j') if self.usage.scope == UsageScope::Models => {
                self.usage.scroll.set(
                    self.usage
                        .scroll
                        .get()
                        .saturating_add(1)
                        .min(self.usage.scroll_max.get()),
                );
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
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('<')
            | KeyCode::Char('>') => {
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
                let next = if matches!(code, KeyCode::Right | KeyCode::Tab | KeyCode::Char('>')) {
                    (current + 1) % len
                } else {
                    (current + len - 1) % len
                };
                self.usage.tabs.insert(group.provider.clone(), next);
                self.usage.follow_cursor.set(true);
                self.dirty = true;
            }
            KeyCode::Char('r') if self.usage.scope == UsageScope::Models => {
                self.usage.model_range = self.usage.model_range.next();
                self.usage.scroll.set(0);
                self.dirty = true;
            }
            KeyCode::Char('r') => {
                // Owner addendum: `r` toggles the identity REVEAL for this
                // visit only — the screen always opens masked and closing
                // restores the mask.
                self.usage.revealed = !self.usage.revealed;
                self.dirty = true;
            }
            KeyCode::Char('s') => {
                // 954: `s` cycles the viewing scope ring.
                self.usage.scope = self.usage.scope.next();
                self.usage.scroll.set(0);
                // Entering History live with no held window: read it. The
                // demo never fetches (usage is daemon truth) and a daemon
                // without the feature gets the honest note at render.
                self.refresh_usage_scope_if_needed();
                self.dirty = true;
            }
            // A manual re-read (live only — the demo has nothing to fetch
            // and the honest empty state already says so).
            KeyCode::Char('f') if !self.mode.fabricates_locally() => {
                self.usage.fetching = true;
                self.usage.error = None;
                self.requests.push(AppRequest::UsageRefresh);
                if matches!(self.usage.scope, UsageScope::History | UsageScope::Models)
                    && self.daemon_serves(haider_rpc::FEATURE_USAGE_HISTORY_V1)
                    && !self.usage.history_fetching
                {
                    self.usage.history_fetching = true;
                    self.usage.history_error = None;
                    self.requests.push(AppRequest::UsageHistoryRefresh);
                }
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
                    // G4b: the enterprise builtins edit through their OWN
                    // cards (region / project+location) — the generic edit
                    // card would submit the wrong identity family.
                    match summary.provider.as_str() {
                        "bedrock" => self.open_bedrock_card(),
                        "vertex" => self.open_vertex_card(),
                        _ => self.open_custom_edit(&summary),
                    }
                }
            }
            KeyCode::Char('h') => self.open_huggingface_preset(),
            KeyCode::Char('z') => self.open_opencode_zen_preset(),
            KeyCode::Char('g') => self.open_opencode_go_preset(),
            KeyCode::Char('o') => self.open_ollama_preset(),
            KeyCode::Char('l') => self.open_lmstudio_preset(),
            // G4b enterprise cards.
            KeyCode::Char('a') => self.open_azure_card(),
            KeyCode::Char('b') => self.open_bedrock_card(),
            KeyCode::Char('v') => self.open_vertex_card(),
            // Named API-key builtin. The login card is owned/rendered by
            // /accounts, so the provider shortcut takes the same route as
            // clicking its shared add button.
            KeyCode::Char('d') => {
                self.enter_accounts();
                self.handle_hit(Hit::AccountAdd(AccountAddKind::DeepSeekApi));
            }
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
            KeyCode::Char('t') => {
                if let Some(provider) = self
                    .providers
                    .providers
                    .get(self.providers.cursor)
                    .map(|summary| summary.provider.clone())
                {
                    self.toggle_provider_trust(&provider);
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
            AccountAddKind::GrokOAuth => ("grok-oauth", "Grok — xAI"),
            // 970: the same oauth_start/status/add wire again — the daemon
            // supervises Google's agent behind it, so the card stays
            // flow-agnostic and only its copy names the agent-owned login.
            AccountAddKind::GoogleAntigravity => {
                (GOOGLE_ANTIGRAVITY_PROVIDER, "Google Antigravity")
            }
            AccountAddKind::OpenAiApi
            | AccountAddKind::AnthropicApi
            | AccountAddKind::GeminiApi
            | AccountAddKind::HuggingFace
            | AccountAddKind::OpencodeZen
            | AccountAddKind::OpencodeGo
            | AccountAddKind::Ollama
            | AccountAddKind::LmStudio
            | AccountAddKind::AzureOpenAi
            | AccountAddKind::Bedrock
            | AccountAddKind::Vertex
            | AccountAddKind::Custom
            | AccountAddKind::DeepSeekApi
            | AccountAddKind::HaiderCodeApi
            | AccountAddKind::XaiApi => return,
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

    /// The `/login google` door (970). The FIRST login on this profile opens
    /// the disclosure card — who performs the sign-in, what the agent costs,
    /// and the verbatim terms warning — and starts nothing until the user
    /// confirms. A profile that already journalled the acknowledgement goes
    /// straight to the flow: the owner asked for ONE warning, plus the
    /// standing `/accounts` badge, never a repeated interstitial.
    fn open_antigravity_add(&mut self) {
        if self.oauth_add.is_some()
            || self.custom_add.is_some()
            || self.antigravity_consent.is_some()
        {
            return;
        }
        if self
            .acknowledged_terms
            .contains(GOOGLE_ANTIGRAVITY_TERMS_SUBJECT)
        {
            self.open_oauth_add(AccountAddKind::GoogleAntigravity);
            return;
        }
        self.accounts.message = None;
        self.antigravity_consent = Some(AccountAddKind::GoogleAntigravity);
        self.dirty = true;
    }

    /// `[1]` on the disclosure card: RECORD the acknowledgement, then start
    /// the flow. The record is the durable evidence that this user was shown
    /// the warning and continued; the write itself is the runtime's
    /// (`crate::runtime::sync_terms_persistence`), keyed on the commit
    /// counter bumped here, so the reducer stays IO-free.
    fn confirm_antigravity_consent(&mut self) {
        let Some(kind) = self.antigravity_consent.take() else {
            return;
        };
        if self
            .acknowledged_terms
            .insert(GOOGLE_ANTIGRAVITY_TERMS_SUBJECT.to_owned())
        {
            self.terms_ack_commits = self.terms_ack_commits.saturating_add(1);
        }
        self.dirty = true;
        self.open_oauth_add(kind);
    }

    /// `[2]`/esc on the disclosure card: nothing starts, nothing is
    /// downloaded, and NOTHING is journalled — a declined warning leaves no
    /// record of an acceptance that never happened.
    fn decline_antigravity_consent(&mut self) {
        if self.antigravity_consent.take().is_some() {
            self.accounts.message = Some("· Google Antigravity sign-in cancelled".to_owned());
            self.dirty = true;
        }
    }

    /// Starts the existing OAuth adoption flow against the active alias.
    /// Unlike ordinary add, recovery must preserve the daemon-owned account
    /// identity so a successful flow replaces that descriptor.
    fn open_oauth_relogin(&mut self, provider: &str, alias: String) {
        if self.oauth_add.is_some() || self.custom_add.is_some() {
            return;
        }
        let title = match provider {
            "openai-oauth" => "OpenAI — ChatGPT",
            "anthropic-oauth" => "Anthropic — Claude",
            "kimi-oauth" => "Kimi — Moonshot",
            "grok-oauth" => "Grok — xAI",
            GOOGLE_ANTIGRAVITY_PROVIDER => "Google Antigravity",
            _ => {
                self.accounts.message = Some(format!(
                    "· re-login is not available for {provider}; add it again"
                ));
                return;
            }
        };
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
                            "grok-oauth" => ("grok-oauth", "you@x.ai · SuperGrok"),
                            GOOGLE_ANTIGRAVITY_PROVIDER => (
                                GOOGLE_ANTIGRAVITY_PROVIDER,
                                "you@gmail.com · Gemini subscription",
                            ),
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
                            account_identity: None,
                            created_at_ms: None,
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

    /// Opens the `+ Add custom server` card. The name prefills
    /// with the smallest free `custom[-N]` against the provider registry;
    /// the origin with the sim's demo URL (a real vLLM default).
    fn open_custom_add(&mut self) {
        if self.custom_add.is_some() || self.oauth_add.is_some() {
            return;
        }
        // The custom alias is both the stable provider identity and the
        // immediately registered account alias. Avoid collisions in either
        // daemon-owned namespace before the user starts editing.
        let mut taken = self.accounts.rows.clone();
        taken.extend(self.providers.providers.iter().map(|summary| AccountRow {
            alias: summary.provider.clone(),
            provider: summary.provider.clone(),
            method: haider_protocol::credential::AuthMethod::ApiKey,
            identity: String::new(),
            account_identity: None,
            created_at_ms: None,
            status: haider_protocol::credential::CredentialStatus::Ok,
            selected: false,
            base_url: None,
        }));
        self.custom_attempt_seq += 1;
        self.accounts.message = None;
        let name = smallest_free_alias("custom", &taken);
        let cursor = name.chars().count();
        self.custom_add = Some(CustomProviderCard {
            name,
            origin: "http://127.0.0.1:8000/v1".to_owned(),
            model: String::new(),
            focus: CustomField::Name,
            cursor,
            phase: CustomPhase::Editing { error: None },
            attempt: self.custom_attempt_seq,
            edit: false,
            keyless: false,
            family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
            discover_models: true,
            secret: zeroize::Zeroizing::new(String::new()),
            kind: CustomCardKind::Generic,
            extra: String::new(),
        });
        self.dirty = true;
    }

    /// W10b: the edit card — the SAME custom card prefilled from the
    /// summary with its name locked; ⏎ re-configures origin/model under the
    /// current revision (the daemon refuses unsupported origin drift with a
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
        let model = summary
            .default_model
            .clone()
            .or_else(|| summary.models.first().cloned())
            .unwrap_or_default();
        let cursor = model.chars().count();
        self.custom_add = Some(CustomProviderCard {
            name: summary.provider.clone(),
            origin: summary.endpoint.clone().unwrap_or_default(),
            model,
            focus: CustomField::Model,
            cursor,
            phase: CustomPhase::Editing { error: None },
            attempt: self.custom_attempt_seq,
            edit: true,
            keyless: summary.auth_methods.is_empty(),
            family: summary.api_family,
            discover_models: false,
            secret: zeroize::Zeroizing::new(String::new()),
            kind: CustomCardKind::Generic,
            extra: String::new(),
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

    /// G4b: the Azure OpenAI card — the custom card in Azure shape. The
    /// user pastes the RESOURCE endpoint (`https://{res}.openai.azure.com`)
    /// and the DEPLOYMENT name; submit derives `{endpoint}/openai/v1` and
    /// the daemon speaks the `api-key` header from then on. Live-only: the
    /// demo has no azure fabrication.
    fn open_azure_card(&mut self) {
        if self.custom_add.is_some() || self.oauth_add.is_some() {
            return;
        }
        if self.mode.fabricates_locally()
            || !self.daemon_serves(haider_rpc::FEATURE_PROVIDER_CONFIGURE_V1)
        {
            self.providers.message = Some(self.stale_daemon_note("Azure OpenAI providers"));
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
                account_identity: None,
                created_at_ms: None,
                status: haider_protocol::credential::CredentialStatus::Ok,
                selected: false,
                base_url: None,
            })
            .collect();
        self.custom_attempt_seq += 1;
        self.custom_add = Some(CustomProviderCard {
            name: smallest_free_alias("azure", &taken),
            origin: "https://".to_owned(),
            model: String::new(),
            focus: CustomField::Origin,
            cursor: "https://".chars().count(),
            phase: CustomPhase::Editing { error: None },
            attempt: self.custom_attempt_seq,
            edit: false,
            keyless: false,
            family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
            discover_models: false,
            secret: zeroize::Zeroizing::new(String::new()),
            kind: CustomCardKind::Azure,
            extra: String::new(),
        });
        self.dirty = true;
    }

    /// G4b: the Bedrock card — the builtin `bedrock` profile's REGION
    /// (default `us-east-1`); submit re-configures the mantle endpoint and
    /// chains the bearer-key card. Gated on the daemon actually listing
    /// the builtin (provider-list truth, the B6b Gemini pattern).
    fn open_bedrock_card(&mut self) {
        self.open_enterprise_card(CustomCardKind::Bedrock);
    }

    /// G4b: the Vertex card — the builtin `vertex` profile's PROJECT ID +
    /// LOCATION (default `global`); submit configures the endpoint and
    /// chains an access-token card (~1h tokens; the gcloud device import
    /// auto-refreshes instead).
    fn open_vertex_card(&mut self) {
        self.open_enterprise_card(CustomCardKind::Vertex);
    }

    fn open_enterprise_card(&mut self, kind: CustomCardKind) {
        if self.custom_add.is_some() || self.oauth_add.is_some() {
            return;
        }
        let provider = match kind {
            CustomCardKind::Bedrock => "bedrock",
            CustomCardKind::Vertex => "vertex",
            CustomCardKind::Generic | CustomCardKind::Azure => return,
        };
        if self.mode.fabricates_locally() || !self.daemon_lists_provider(provider) {
            self.providers.message = Some(self.stale_daemon_note("enterprise Claude providers"));
            self.dirty = true;
            return;
        }
        // Prefill from the LISTED profile: a bedrock endpoint parses back
        // to its region; a vertex endpoint to project + location.
        let endpoint = self
            .providers
            .providers
            .iter()
            .find(|summary| summary.provider == provider)
            .and_then(|summary| summary.endpoint.clone());
        let (origin, extra) = match kind {
            CustomCardKind::Bedrock => (
                endpoint
                    .as_deref()
                    .and_then(bedrock_region_of)
                    .unwrap_or("us-east-1")
                    .to_owned(),
                String::new(),
            ),
            _ => {
                let (project, location) = endpoint
                    .as_deref()
                    .and_then(vertex_coordinates_of)
                    .unwrap_or_default();
                (
                    project,
                    if location.is_empty() {
                        "global".to_owned()
                    } else {
                        location
                    },
                )
            }
        };
        self.custom_attempt_seq += 1;
        let cursor = origin.chars().count();
        self.custom_add = Some(CustomProviderCard {
            name: provider.to_owned(),
            origin,
            model: String::new(),
            focus: CustomField::Origin,
            cursor,
            phase: CustomPhase::Editing { error: None },
            attempt: self.custom_attempt_seq,
            edit: false,
            keyless: false,
            family: match kind {
                CustomCardKind::Bedrock | CustomCardKind::Vertex => {
                    haider_rpc::ProviderApiFamilyWire::AnthropicMessages
                }
                CustomCardKind::Generic | CustomCardKind::Azure => {
                    haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions
                }
            },
            discover_models: false,
            secret: zeroize::Zeroizing::new(String::new()),
            kind,
            extra,
        });
        self.dirty = true;
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
                account_identity: None,
                created_at_ms: None,
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
            cursor: 0,
            phase: CustomPhase::Editing { error: None },
            attempt: self.custom_attempt_seq,
            edit: false,
            keyless,
            family: haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
            discover_models: false,
            secret: zeroize::Zeroizing::new(String::new()),
            kind: CustomCardKind::Generic,
            extra: String::new(),
        });
        self.dirty = true;
    }

    fn cancel_custom_add(&mut self) {
        if let Some(card) = self.custom_add.take() {
            if !self.mode.fabricates_locally() {
                self.requests.push(AppRequest::CustomProviderRetired {
                    attempt: card.attempt,
                });
            }
            self.dirty = true;
        }
    }

    /// ⏎ on the live card: `provider.configure` under the CURRENT provider
    /// revision (CAS — a stale snapshot is a typed conflict, never a
    /// silent overwrite). G4b: the enterprise kinds reshape the submit —
    /// azure derives the `/openai/v1` base from the resource endpoint;
    /// bedrock/vertex build their templated endpoints from the card's
    /// coordinates and ECHO the profile's seeded model inventory (the
    /// daemon's shape validators stay the authority; the card only
    /// formats).
    fn submit_custom_add(&mut self) {
        let expected_revision = self.providers.revision.unwrap_or(0);
        let summary_inventory = |providers: &ProvidersState, name: &str| {
            providers
                .providers
                .iter()
                .find(|summary| summary.provider == name)
                .map(|summary| {
                    (
                        summary.models.clone(),
                        summary
                            .default_model
                            .clone()
                            .or_else(|| summary.models.first().cloned()),
                    )
                })
                .unwrap_or_default()
        };
        let Some(card) = self.custom_add.as_mut() else {
            return;
        };
        if !matches!(card.phase, CustomPhase::Editing { .. }) {
            return;
        }
        if !account_alias_ok(&card.name) {
            card.focus_end(CustomField::Name);
            self.dirty = true;
            return;
        }
        if card.origin.trim().is_empty() {
            card.focus_end(CustomField::Origin);
            self.dirty = true;
            return;
        }
        // An ENABLED create requires a model inventory and a default
        // (daemon law) — the card refuses to submit what would bounce.
        // Bedrock/vertex carry the profile's SEEDED inventory instead of a
        // typed model.
        if !card.discover_models
            && matches!(card.kind, CustomCardKind::Generic | CustomCardKind::Azure)
            && card.model.trim().is_empty()
        {
            card.focus_end(CustomField::Model);
            self.dirty = true;
            return;
        }
        if card.discover_models && !card.keyless && card.secret.is_empty() {
            card.focus_end(CustomField::Key);
            self.dirty = true;
            return;
        }
        let attempt = card.attempt;
        let name = card.name.clone();
        let model = card.model.trim().to_owned();
        let keyless = card.keyless;
        let (origin, family, models, default_model) = match card.kind {
            CustomCardKind::Generic => {
                (card.origin.trim().to_owned(), card.family, Vec::new(), None)
            }
            CustomCardKind::Azure => (
                azure_v1_base(card.origin.trim()),
                haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions,
                Vec::new(),
                None,
            ),
            CustomCardKind::Bedrock => {
                let (models, default_model) = summary_inventory(&self.providers, &name);
                (
                    bedrock_mantle_url(card.origin.trim()),
                    haider_rpc::ProviderApiFamilyWire::AnthropicMessages,
                    models,
                    default_model,
                )
            }
            CustomCardKind::Vertex => {
                let location = card.extra.trim();
                let location = if location.is_empty() {
                    "global"
                } else {
                    location
                };
                let (models, default_model) = summary_inventory(&self.providers, &name);
                (
                    vertex_models_url(card.origin.trim(), location),
                    haider_rpc::ProviderApiFamilyWire::AnthropicMessages,
                    models,
                    default_model,
                )
            }
        };
        let Some(card) = self.custom_add.as_mut() else {
            return;
        };
        let secret = (card.discover_models && !card.keyless).then(|| card.take_key());
        card.phase = CustomPhase::Submitting;
        self.requests.push(AppRequest::ProviderConfigure {
            attempt,
            name,
            origin,
            model,
            keyless,
            secret,
            family,
            models,
            default_model,
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
            if card.discover_models && !card.keyless {
                // Submit already moved the only raw-key copy into the
                // zeroizing stage frame. Any failure must request a retype;
                // retaining or reconstructing the key is forbidden.
                card.focus_end(CustomField::Key);
            }
            self.dirty = true;
        }
    }

    /// A committed `provider.configure`: close the card and chain straight
    /// into the masked key card — the provider needs a credential before
    /// it can serve anything (report §4.4: custom = base URL + key).
    /// G4a: a KEYLESS card skips the key card entirely — there is no
    /// credential to add — and goes straight to model discovery.
    pub fn custom_add_committed(&mut self, attempt: u64, credential_staged: bool) -> Option<u64> {
        let card = self.custom_add.take_if(|card| card.attempt == attempt)?;
        if card.keyless {
            self.providers.message = Some(if card.discover_models {
                format!(
                    "✓ provider {} created · no auth · live models discovered",
                    card.name
                )
            } else {
                format!(
                    "✓ provider {} created · keyless — discovering models…",
                    card.name
                )
            });
            // Keyless presets finish at provider.configure (there is no
            // credential/login receipt and intentionally no account row).
            // Still re-read the daemon-owned roster once after a NEW
            // provider commits so every Accounts-screen add path observes
            // authoritative account truth. Editing an existing keyless
            // provider is not an account add and needs no roster read.
            if !card.edit {
                self.requests.push(AppRequest::AccountsRefresh);
            }
            if !card.discover_models {
                self.requests.push(AppRequest::ProviderModelsRefresh {
                    provider: card.name,
                });
            }
            self.dirty = true;
            return None;
        }
        if credential_staged {
            self.accounts.message = Some(format!(
                "✓ provider {} configured · registering its account and live models…",
                card.name
            ));
            let login_attempt = self.open_staged_login_card(&card.name, card.name.clone());
            self.dirty = true;
            return Some(login_attempt);
        }
        // G4b: every keyed kind chains into the masked key card; the copy
        // names what the "key" actually is on each surface.
        self.accounts.message = Some(match card.kind {
            CustomCardKind::Azure => format!(
                "✓ provider {} configured · Azure OpenAI — now add its api-key",
                card.name
            ),
            CustomCardKind::Bedrock => format!(
                "✓ provider {} configured · Bedrock mantle — now add its bearer API key",
                card.name
            ),
            CustomCardKind::Vertex => format!(
                "✓ provider {} configured · Vertex — paste an access token (~1h); detected gcloud credentials auto-link",
                card.name
            ),
            CustomCardKind::Generic => format!(
                "✓ provider {} created · OpenAI-compatible — now add its key",
                card.name
            ),
        });
        self.open_login_card(&card.name, None);
        self.dirty = true;
        None
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
                    card.move_focus(false);
                    self.dirty = true;
                }
            }
            KeyCode::BackTab => {
                if let Some(card) = self.custom_add.as_mut()
                    && matches!(card.phase, CustomPhase::Editing { .. })
                {
                    card.move_focus(true);
                    self.dirty = true;
                }
            }
            KeyCode::Backspace => {
                if let Some(card) = self.custom_add.as_mut()
                    && (card.key_backspace() || card.backspace())
                {
                    self.dirty = true;
                }
            }
            KeyCode::Delete => {
                if let Some(card) = self.custom_add.as_mut()
                    && card.delete_forward()
                {
                    self.dirty = true;
                }
            }
            KeyCode::Left => {
                if let Some(card) = self.custom_add.as_mut()
                    && (card.cycle_choice() || card.move_left())
                {
                    self.dirty = true;
                }
            }
            KeyCode::Right => {
                if let Some(card) = self.custom_add.as_mut()
                    && (card.cycle_choice() || card.move_right())
                {
                    self.dirty = true;
                }
            }
            KeyCode::Char(' ') => {
                if let Some(card) = self.custom_add.as_mut()
                    && card.cycle_choice()
                {
                    self.dirty = true;
                }
            }
            KeyCode::Home => {
                if let Some(card) = self.custom_add.as_mut()
                    && card.can_edit_field(card.focus)
                    && card.cursor != 0
                {
                    card.cursor = 0;
                    self.dirty = true;
                }
            }
            KeyCode::End => {
                if let Some(card) = self.custom_add.as_mut()
                    && card.can_edit_field(card.focus)
                {
                    let end = card
                        .field_value(card.focus)
                        .map_or(0, |value| value.chars().count());
                    if card.cursor != end {
                        card.cursor = end;
                        self.dirty = true;
                    }
                }
            }
            KeyCode::Char(c) => {
                if let Some(card) = self.custom_add.as_mut()
                    && insert_custom_card_character(card, c)
                {
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    /// A mouse press inside a rendered custom-provider value. The hit's
    /// attempt rejects stale frames, and `focus_at` clamps clicks beyond the
    /// current character count while retaining the edit-mode Name lock.
    pub(crate) fn custom_provider_field_press(
        &mut self,
        attempt: u64,
        field: CustomField,
        character: usize,
    ) {
        if !matches!(self.screen, Screen::Accounts | Screen::Providers) {
            return;
        }
        if let Some(card) = self.custom_add.as_mut()
            && card.attempt == attempt
            && card.focus_at(field, character)
        {
            self.dirty = true;
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
            account_identity: None,
            created_at_ms: None,
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
        // 970 — the disclosure card owns the keyboard exactly like the other
        // accounts cards: nothing starts until `[1]`, and `[2]`/esc leaves
        // no record. It closes before `open_oauth_add` runs, so at most one
        // of these three guards can ever be live.
        if self.antigravity_consent.is_some() {
            match code {
                KeyCode::Char('1') => self.confirm_antigravity_consent(),
                KeyCode::Esc | KeyCode::Char('2') => self.decline_antigravity_consent(),
                _ => {}
            }
            return;
        }
        if self.oauth_add.is_some() {
            self.handle_oauth_card_key(code);
            return;
        }
        if self.custom_add.is_some() {
            self.handle_custom_card_key(code);
            return;
        }
        match code {
            KeyCode::Char('y') if self.accounts.adoption_candidate.is_some() => {
                if let Some(candidate) = self.accounts.adoption_candidate.take() {
                    self.accounts.message = Some(format!("importing {} login…", candidate.source));
                    self.requests.push(AppRequest::AccountImportDevice {
                        candidate: candidate.candidate,
                        source: candidate.source,
                    });
                    self.dirty = true;
                }
            }
            KeyCode::Char('n') if self.accounts.adoption_candidate.is_some() => {
                self.accounts.adoption_candidate = None;
                self.accounts.message = Some("local login import cancelled".to_owned());
                if self.device_discovery_available() {
                    self.requests.push(AppRequest::DeviceCandidatesRefresh);
                }
                self.dirty = true;
            }
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
                let total = self.accounts.rows.len();
                if total > 0 {
                    self.accounts.cursor = (self.accounts.cursor + 1).min(total - 1);
                }
                self.dirty = true;
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
                }
            }
            _ => {}
        }
    }

    /// Surfaces one typed, secret-free adoption offer at most once per TUI
    /// session. Import remains a separate explicit y/confirm action.
    pub fn account_adoption_available(
        &mut self,
        notice: &haider_rpc::AccountAdoptionAvailable,
        candidate: haider_rpc::DeviceCredentialCandidateWire,
    ) -> bool {
        let key = format!(
            "{}\u{1f}{}",
            notice.source,
            notice.email.as_deref().unwrap_or("unknown")
        );
        if self.accounts.adoption_candidate.is_some()
            || self.accounts.adoption_noticed.contains(&key)
        {
            return false;
        }
        self.accounts.adoption_noticed.insert(key);
        let identity = candidate.identity.as_ref().map_or_else(
            || "unknown account".to_owned(),
            |identity| identity.summary(),
        );
        let command = format!("haider account import {} --confirm", candidate.source);
        self.flash = Some(format!(
            "· {} login available: {identity} · `{command}`",
            candidate.source_label
        ));
        self.accounts.message = Some(format!(
            "import {} login ({identity})? y confirms · n cancels · `{command}`",
            candidate.source_label
        ));
        self.accounts.adoption_candidate = Some(candidate);
        self.dirty = true;
        true
    }

    /// THE ONE DOOR into `/hooks` (H4). Session-scoped like `/tools`; the
    /// live path is feature-gated BEFORE anything opens (the B2b lesson —
    /// an ungated daemon fabricates nothing, the honest stale-daemon note
    /// names the fix), and the demo path opens a sim-honest EMPTY state
    /// that refuses trust actions.
    /// `/graph [pin|abandon|status]`. Bare (or `status`) opens the graph
    /// status view; `pin`/`abandon` are receipt-backed mutations. Session
    /// only; live only (graph state is daemon truth, never fabricated).
    /// D3 — open the Loom registry browser. Registry truth is daemon-owned:
    /// live-only, feature-gated; the browse list reads the D1 snapshot.
    /// Open the all-sessions browser (`/resume`, the launcher's sessions
    /// row, or `haider resume` at boot). Live-only: the rows ARE daemon
    /// roster truth, and the demo fabricates nothing.
    pub fn enter_sessions(&mut self) {
        self.dirty = true;
        if self.mode.fabricates_locally() {
            self.flash = Some("· /resume — live only; the session list is daemon truth".to_owned());
            return;
        }
        if self.screen != Screen::Sessions {
            self.session_browser_return = Some(self.screen);
        }
        self.session_browser_sel = 0;
        self.session_browser_query.clear();
        self.screen = Screen::Sessions;
    }

    /// Every known session, ordered by ATTENTION (owner 2026-08-21): the
    /// sessions needing a human first, then unseen activity, then the rest
    /// by recency. Within a tier the most recent activity leads, so the row
    /// a user most likely wants is always at the top of the list.
    #[must_use]
    pub fn session_browser_rows(&self) -> Vec<SessionBrowserRow> {
        self.session_rows_for_query(&self.session_browser_query)
    }

    /// Top-level rows in launch/browser order, independent of the browser's
    /// transient filter. The launcher, digit bindings, and `/sessions <n>`
    /// all consume these exact identities so paint and action cannot drift.
    #[must_use]
    pub fn launcher_session_ids(&self) -> Vec<SessionId> {
        self.session_rows_for_query("")
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    /// Durable age text shared by launcher and browser rows.
    #[must_use]
    pub fn session_display_age(&self, session_id: &SessionId, fallback: &str) -> String {
        self.session_attention
            .get(session_id)
            .and_then(|attention| attention.last_activity_ms)
            .map(|activity| format_session_age_at(wall_clock_ms(), activity))
            .unwrap_or_else(|| fallback.to_owned())
    }

    fn session_rows_for_query(&self, query: &str) -> Vec<SessionBrowserRow> {
        let now_ms = wall_clock_ms();
        let needles = query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        let mut rows: Vec<SessionBrowserRow> = self
            .sessions
            .iter()
            .filter(|entry| {
                self.session_kinds.get(&entry.id) != Some(&haider_rpc::SessionKindWire::Subagent)
            })
            .map(|entry| {
                let active = self.active_session.as_ref() == Some(&entry.id);
                let attention = self.session_attention.get(&entry.id);
                let last_activity_ms = attention.and_then(|a| a.last_activity_ms);
                let name = if active {
                    self.session_name.clone()
                } else {
                    entry.name.clone()
                };
                let blurb = if active {
                    self.session_title.clone()
                } else {
                    entry.title.clone()
                };
                let title = name
                    .clone()
                    .or_else(|| blurb.clone())
                    .unwrap_or_else(|| entry.id.as_str().to_owned());
                let dir = if active {
                    self.session_workspace_cwd
                        .clone()
                        .unwrap_or_else(|| self.session_dir.clone())
                } else {
                    entry
                        .workspace_cwd
                        .clone()
                        .unwrap_or_else(|| entry.dir.clone())
                };
                let model_short = if active {
                    self.identity.model_short.clone()
                } else {
                    entry.model_short.clone()
                };
                let search_aliases = format!(
                    "{}\n{}\n{}",
                    name.unwrap_or_default(),
                    blurb.unwrap_or_default(),
                    self.session_last_models
                        .get(&entry.id)
                        .cloned()
                        .unwrap_or_default()
                );
                (
                    SessionBrowserRow {
                        id: entry.id.clone(),
                        title,
                        dir,
                        model_short,
                        agent_type: entry.agent_type.clone(),
                        ago: last_activity_ms
                            .map(|activity| format_session_age_at(now_ms, activity))
                            .unwrap_or_else(|| entry.ago.clone()),
                        busy: if active {
                            self.session_busy()
                        } else {
                            entry.busy()
                        },
                        unseen: attention.is_some_and(SessionAttention::unseen),
                        needs_input: attention.and_then(|a| a.needs_input.clone()),
                        last_activity_ms,
                        created_at_ms: self.session_created_at_ms.get(&entry.id).copied(),
                    },
                    search_aliases,
                )
            })
            .filter(|(row, search_aliases)| {
                if needles.is_empty() {
                    return true;
                }
                let haystack = format!(
                    "{}\n{}\n{}\n{}\n{}",
                    row.title,
                    row.dir,
                    row.model_short,
                    row.id.as_str(),
                    search_aliases
                )
                .to_lowercase();
                needles.iter().all(|needle| haystack.contains(needle))
            })
            .map(|(row, _)| row)
            .collect();
        rows.sort_by(|a, b| {
            let tier = |row: &SessionBrowserRow| {
                if row.needs_input.is_some() {
                    0
                } else if row.unseen {
                    1
                } else {
                    2
                }
            };
            tier(a)
                .cmp(&tier(b))
                .then(b.last_activity_ms.cmp(&a.last_activity_ms))
                .then(b.created_at_ms.cmp(&a.created_at_ms))
        });
        rows
    }

    fn handle_sessions_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::ALT) {
            return;
        }
        let code = key.code;
        let rows = self.session_browser_rows();
        let last = rows.len().saturating_sub(1);
        // One keypress-page. The render window is height-derived, so this is
        // a fixed, predictable jump rather than a guess at the frame.
        const PAGE: usize = 10;
        match code {
            KeyCode::Up => {
                self.session_browser_sel = self.session_browser_sel.saturating_sub(1);
                self.dirty = true;
            }
            KeyCode::Down => {
                self.session_browser_sel = (self.session_browser_sel + 1).min(last);
                self.dirty = true;
            }
            KeyCode::PageUp => {
                self.session_browser_sel = self.session_browser_sel.saturating_sub(PAGE);
                self.dirty = true;
            }
            KeyCode::PageDown => {
                self.session_browser_sel = (self.session_browser_sel + PAGE).min(last);
                self.dirty = true;
            }
            KeyCode::Home => {
                self.session_browser_sel = 0;
                self.dirty = true;
            }
            KeyCode::End => {
                self.session_browser_sel = last;
                self.dirty = true;
            }
            KeyCode::Enter => {
                if let Some(row) = rows.get(self.session_browser_sel) {
                    let id = row.id.clone();
                    // Opening a session is a VIEW: the 936 attention hook
                    // marks it seen for every surface (debounced driver-side).
                    self.open_session(&id);
                }
            }
            KeyCode::Esc => {
                if self.session_browser_query.is_empty() {
                    self.screen = self
                        .session_browser_return
                        .take()
                        .unwrap_or(Screen::Launcher);
                } else {
                    self.session_browser_query.clear();
                    self.session_browser_sel = 0;
                }
                self.dirty = true;
            }
            KeyCode::Backspace => {
                self.session_browser_query.pop();
                self.session_browser_sel = 0;
                self.dirty = true;
            }
            KeyCode::Char(character) if !character.is_control() => {
                self.session_browser_query.push(character);
                self.session_browser_sel = 0;
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn enter_loom(&mut self) {
        self.enter_loom_pane(LoomPane::Types, "loom");
    }

    fn enter_workflows(&mut self) {
        self.enter_loom_pane(LoomPane::Workflows, "workflows");
    }

    fn enter_loom_pane(&mut self, pane: LoomPane, surface: &str) {
        self.dirty = true;
        if self.mode.fabricates_locally() {
            self.flash = Some(format!(
                "· /{surface} — live only; the registry is daemon truth"
            ));
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_LOOM_V1) {
            self.flash = Some(self.stale_daemon_note(surface));
            return;
        }
        if pane == LoomPane::Workflows
            && !self.daemon_serves(haider_rpc::FEATURE_WORKFLOW_CATALOG_V1)
        {
            self.flash = Some(self.stale_daemon_note("workflow catalog"));
            return;
        }
        self.loom_pane = pane;
        self.loom_selection = if pane == LoomPane::Workflows
            && self.daemon_serves(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1)
        {
            self.workflow_graph
                .workflow_id()
                .and_then(|workflow_id| self.workflow_row_index(workflow_id))
                .unwrap_or(0)
        } else {
            0
        };
        self.loom_detail = false;
        self.workflow_evidence_inspection = None;
        self.loom_scroll = 0;
        // W-flow: EVERY pane entry re-reads the registry, so a registration
        // landed by `loom.author.confirm` is visible on return.
        // The Listed-driven fetch stays the once-per-connection hydration
        // path; the reply still rides the connection-epoch fence.
        self.requests.push(AppRequest::LoomRefresh);
        if pane == LoomPane::Workflows
            && self.daemon_serves(haider_rpc::FEATURE_WORKFLOW_GRAPH_V1)
            && self.active_session.is_some()
        {
            self.requests.push(if self.workflow_graph.is_empty() {
                AppRequest::WorkflowGraphRefresh
            } else {
                AppRequest::WorkflowGraphResume
            });
        }
        // Re-entering from the browser itself (pane hop) must keep the
        // ORIGINAL return screen, not overwrite it with Screen::Loom.
        if self.screen != Screen::Loom {
            self.loom_return = Some(self.screen);
        }
        self.switch_surface(Screen::Loom);
    }

    /// W-flow — `p` on the /workflows pane: bind the SELECTED row to the
    /// bound session. Registry/built-in rows pin BY NAME over the existing
    /// `graph.pin` (store resolution: built-in catalog first, then the Loom
    /// registry); the synthetic `none` row abandons the active graph.
    /// Live-and-bound sessions only; a pin over an active graph is the
    /// DAEMON's refusal to make (one-active-graph law) — the flash carries
    /// its error, and nothing here auto-switches.
    fn pin_selected_workflow(&mut self) {
        self.dirty = true;
        if self.mode.fabricates_locally() {
            self.flash = Some("· pin — live only; graphs are daemon truth".to_owned());
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_WORKFLOW_CATALOG_V1) {
            self.flash = Some(self.stale_daemon_note("workflow catalog"));
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1) {
            self.flash = Some(self.stale_daemon_note("graph pin"));
            return;
        }
        if self.active_session.is_none() {
            self.flash = Some("· pin — no bound session; open a session first".to_owned());
            return;
        }
        match self.workflow_row(self.loom_selection) {
            Some(WorkflowRow::None) => {
                if self.graph.is_some() {
                    self.requests.push(AppRequest::GraphAbandon {
                        why: "workflow cleared from /workflows".to_owned(),
                    });
                    self.flash = Some("· clearing workflow…".to_owned());
                } else {
                    self.flash = Some("· already none — no graph pinned".to_owned());
                }
            }
            Some(WorkflowRow::BuiltIn(template)) => {
                self.flash = Some(format!("· pinning {}…", template.name));
                self.requests.push(AppRequest::GraphPin {
                    template: Some(template.name),
                });
            }
            Some(WorkflowRow::Registered(index)) => {
                let name = self.loom_workflows[index].id.clone();
                self.flash = Some(format!("· pinning {name}…"));
                self.requests.push(AppRequest::GraphPin {
                    template: Some(name),
                });
            }
            None => {}
        }
    }

    /// W-flow inline identity — `p` on the /loom Types pane: bind the
    /// SELECTED row's agent type to the bound session over the receipted
    /// `session.select_agent_type` (`none` clears to plain). Live-and-bound
    /// sessions only; the daemon validates the id against the registry (a
    /// miss is a typed refusal the flash carries) and identity moves only
    /// on the `agent_type_selected` fact — nothing installs here.
    /// W-flow (owner 2026-08-22) — "after confirmation, install the required
    /// clis on the device".
    ///
    /// The confirmation is the one we ALREADY audit: this seeds an ordinary
    /// turn, and the install runs as a `process_exec` effect behind the
    /// normal permission card. It deliberately does NOT add a privileged
    /// installer door — a door that takes a program name and runs a package
    /// manager is arbitrary code execution with a friendly label, and it
    /// would bypass the very card that makes the install reviewable.
    ///
    /// The names are not model-authored: they are the type's DECLARED CLIs,
    /// already validated at registration (concrete programs, never a shell
    /// dispatcher), narrowed to the ones this device was PROBED and found to
    /// lack. The operator still sees, and approves, the actual command.
    pub fn seed_cli_provisioning(&mut self) {
        self.dirty = true;
        if self.loom_authoring.is_some() {
            self.flash = Some("· close the Loom editor before preparing an install".to_owned());
            return;
        }
        if self.loom_pane != LoomPane::Types {
            self.flash = Some("· install — agent types carry the CLI grants".to_owned());
            return;
        }
        if self.mode.fabricates_locally() {
            self.flash = Some("· install runs live — the permission card confirms".to_owned());
            return;
        }
        if self.active_session.is_none() {
            self.flash = Some("· no bound session — open a session first".to_owned());
            return;
        }
        let Some(TypeRow::Registered(index)) = self.type_row(self.loom_selection) else {
            self.flash = Some("· install — select a registered agent type".to_owned());
            return;
        };
        let Some(record) = self.loom_types.get(index) else {
            return;
        };
        let missing = crate::render::missing_clis(self, record);
        if missing.is_empty() {
            // Never-probed names land here too, and that is correct: we do
            // not offer to install what we did not check.
            self.flash = Some(format!("· @{} — nothing missing to install", record.id));
            return;
        }
        let id = record.id.clone();
        let list = missing.join(", ");
        let text = format!(
            "The @{id} agent type declares CLIs this device does not have: {list}. \
             Install exactly those programs using this machine's usual package \
             manager, one program per command, and nothing else. Show me each \
             command before you run it, then verify each one is on PATH."
        );
        self.composer.set_text(&text);
        self.flash = Some(format!(
            "· install {} — review the command, the permission card confirms",
            list
        ));
    }

    fn bind_selected_type(&mut self) {
        self.dirty = true;
        if self.mode.fabricates_locally() {
            self.flash = Some("· bind — live only; the binding is daemon truth".to_owned());
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_SESSION_AGENT_TYPE_SELECT_V1) {
            self.flash = Some(self.stale_daemon_note("agent-type binding"));
            return;
        }
        if self.active_session.is_none() {
            self.flash = Some("· bind — no bound session; open a session first".to_owned());
            return;
        }
        match self.type_row(self.loom_selection) {
            Some(TypeRow::None) => {
                if self.identity.agent_type.is_some() {
                    self.flash = Some("· clearing agent type…".to_owned());
                    self.requests
                        .push(AppRequest::SelectAgentType { agent_type: None });
                } else {
                    self.flash = Some("· already plain — no agent type bound".to_owned());
                }
            }
            Some(TypeRow::Registered(index)) => {
                let id = self.loom_types[index].id.clone();
                self.flash = Some(format!("· binding @{id}…"));
                self.requests.push(AppRequest::SelectAgentType {
                    agent_type: Some(id),
                });
            }
            None => {}
        }
    }

    /// Submit prose for a typed draft, or revalidate the current edited text.
    /// Confirmation is separate (`⌃S`) so validation never mutates registry
    /// state and the user always has an explicit final action.
    fn submit_loom_turn(&mut self) {
        if self.mode.fabricates_locally() {
            self.flash = Some("· Loom authoring is daemon-owned".to_owned());
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_LOOM_AUTHORING_V1) {
            self.flash = Some(self.stale_daemon_note("Loom authoring"));
            return;
        }
        if self
            .loom_authoring
            .as_ref()
            .is_some_and(|authoring| authoring.pending)
        {
            self.flash = Some("· Loom authoring request already in flight".to_owned());
            return;
        }
        let text = self.composer.text().to_owned();
        let has_server_draft = self
            .loom_authoring
            .as_ref()
            .and_then(|authoring| authoring.authoring_id.as_ref())
            .is_some();
        if text.trim().is_empty() && !has_server_draft {
            return;
        }
        let drafting_session = self.active_session.clone();
        if self
            .loom_authoring
            .as_ref()
            .and_then(|authoring| authoring.authoring_id.as_ref())
            .is_none()
            && drafting_session.is_none()
        {
            self.flash =
                Some("· no bound session — open one to choose the drafting model".to_owned());
            return;
        }
        if self.loom_authoring.is_none() {
            let kind = match self.loom_pane {
                LoomPane::Types => haider_protocol::loom::LoomAuthorKind::AgentType,
                LoomPane::Workflows => haider_protocol::loom::LoomAuthorKind::Workflow,
            };
            let Some(generation) = self.allocate_loom_authoring_generation() else {
                return;
            };
            self.loom_authoring = Some(LoomAuthoringState {
                generation,
                kind,
                authoring_id: None,
                revision: None,
                errors: Vec::new(),
                confirmed: None,
                pending: false,
                validated: false,
                preview_digest: None,
                install_job: None,
            });
        }
        let kind = self.loom_authoring.as_ref().map_or_else(
            || match self.loom_pane {
                LoomPane::Types => haider_protocol::loom::LoomAuthorKind::AgentType,
                LoomPane::Workflows => haider_protocol::loom::LoomAuthorKind::Workflow,
            },
            |authoring| authoring.kind,
        );
        let generation = self
            .loom_authoring
            .as_ref()
            .map_or(0, |authoring| authoring.generation);
        match self
            .loom_authoring
            .as_ref()
            .and_then(|authoring| authoring.authoring_id.clone().zip(authoring.revision))
        {
            Some((authoring_id, expected_revision)) => {
                self.requests.push(AppRequest::LoomAuthorRevise {
                    generation,
                    authoring_id,
                    expected_revision,
                    kind,
                    text,
                })
            }
            None => {
                let Some(session) = drafting_session else {
                    return;
                };
                self.requests.push(AppRequest::LoomAuthorDraft {
                    generation,
                    session,
                    kind,
                    prose: text,
                });
            }
        }
        if let Some(authoring) = &mut self.loom_authoring {
            authoring.pending = true;
        }
        self.flash = Some("· validating Loom draft…".to_owned());
    }

    /// Seed the tab's composer with the authoring opener for the current
    /// pane. The operator finishes the sentence and presses ⏎ — and STAYS in
    /// the tab, so the proposal, their refinements and the registry are all
    /// in view at once (owner 2026-08-22).
    pub fn seed_loom_authoring(&mut self) {
        self.dirty = true;
        if self.mode.fabricates_locally() {
            self.flash = Some("· Loom authoring is daemon-owned".to_owned());
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_LOOM_AUTHORING_V1) {
            self.flash = Some(self.stale_daemon_note("Loom authoring"));
            return;
        }
        if self.active_session.is_none() {
            self.flash =
                Some("· no bound session — open one to choose the drafting model".to_owned());
            return;
        }
        if self.loom_authoring.is_some() {
            self.flash = Some("· close the current Loom draft before starting another".to_owned());
            return;
        }
        let kind = match self.loom_pane {
            LoomPane::Types => haider_protocol::loom::LoomAuthorKind::AgentType,
            LoomPane::Workflows => haider_protocol::loom::LoomAuthorKind::Workflow,
        };
        let Some(generation) = self.allocate_loom_authoring_generation() else {
            return;
        };
        self.loom_authoring = Some(LoomAuthoringState {
            generation,
            kind,
            authoring_id: None,
            revision: None,
            errors: Vec::new(),
            confirmed: None,
            pending: false,
            validated: false,
            preview_digest: None,
            install_job: None,
        });
        self.flash = Some("· describe it in prose, then press ⏎ for a typed draft".to_owned());
    }

    fn allocate_loom_authoring_generation(&mut self) -> Option<u64> {
        let generation = self.next_loom_authoring_generation;
        let Some(next) = generation.checked_add(1) else {
            self.flash = Some("· Loom editor identity space is exhausted".to_owned());
            return None;
        };
        self.next_loom_authoring_generation = next;
        Some(generation)
    }

    fn note_loom_author_edit(&mut self) {
        if let Some(authoring) = &mut self.loom_authoring {
            authoring.errors.clear();
            authoring.validated = false;
            authoring.confirmed = None;
            authoring.preview_digest = None;
            authoring.install_job = None;
        }
    }

    fn confirm_loom_authoring(&mut self) {
        self.dirty = true;
        if !self.daemon_serves(haider_rpc::FEATURE_LOOM_AUTHORING_V1) {
            self.flash = Some(self.stale_daemon_note("Loom authoring"));
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_LOOM_REGISTRY_CAS_V1) {
            self.flash = Some(self.stale_daemon_note("Loom registry CAS"));
            return;
        }
        let Some(authoring) = &self.loom_authoring else {
            self.flash = Some("· start a Loom draft first (⌃N)".to_owned());
            return;
        };
        if authoring.pending {
            self.flash = Some("· Loom authoring request already in flight".to_owned());
            return;
        }
        let Some(authoring_id) = authoring.authoring_id.clone() else {
            self.flash = Some("· press ⏎ to create the typed draft before confirming".to_owned());
            return;
        };
        let Some(expected_revision) = authoring.revision else {
            self.flash = Some("· validate the typed draft before confirming".to_owned());
            return;
        };
        if !authoring.validated {
            self.flash = Some("· press ⏎ to validate this edit before confirming".to_owned());
            return;
        }
        let text = self.composer.text().to_owned();
        if text.trim().is_empty() {
            self.flash = Some("· an empty Loom draft cannot be confirmed".to_owned());
            return;
        }
        let generation = authoring.generation;
        let kind = authoring.kind;
        let Some((expected_rev, expected_digest)) = self.loom_registry_expectation(kind, &text)
        else {
            self.flash = Some(
                "· cannot recover the registry coordinate — revise or refresh before confirming"
                    .to_owned(),
            );
            return;
        };
        let Some(authoring) = &mut self.loom_authoring else {
            return;
        };
        authoring.pending = true;
        self.requests.push(AppRequest::LoomAuthorConfirm {
            generation,
            authoring_id,
            expected_revision,
            kind,
            text,
            expected_rev,
            expected_digest,
        });
        self.flash = Some("· confirming immutable Loom revision…".to_owned());
    }

    fn validate_loom_document(&mut self) {
        self.dirty = true;
        if !self.daemon_serves(haider_rpc::FEATURE_LOOM_VALIDATION_V1) {
            self.flash = Some(self.stale_daemon_note("Loom validation"));
            return;
        }
        let Some(authoring) = &mut self.loom_authoring else {
            self.flash = Some("· open a Loom editor before validating".to_owned());
            return;
        };
        if authoring.pending {
            self.flash = Some("· Loom authoring request already in flight".to_owned());
            return;
        }
        authoring.pending = true;
        self.requests.push(AppRequest::LoomValidate {
            generation: authoring.generation,
            kind: authoring.kind,
            text: self.composer.text().to_owned(),
        });
        self.flash = Some("· validating without saving…".to_owned());
    }

    fn archive_selected_loom(&mut self) {
        self.dirty = true;
        if !self.daemon_serves(haider_rpc::FEATURE_LOOM_REGISTRY_ARCHIVE_V1) {
            self.flash = Some(self.stale_daemon_note("Loom archive"));
            return;
        }
        let selected = if let Some(authoring) = &self.loom_authoring {
            authoring.confirmed.as_ref().map(|confirmed| {
                let kind = match confirmed.kind {
                    haider_protocol::loom::LoomAuthorKind::AgentType => {
                        haider_protocol::loom::LoomRegistryEntryKind::AgentType
                    }
                    haider_protocol::loom::LoomAuthorKind::Workflow => {
                        haider_protocol::loom::LoomRegistryEntryKind::Workflow
                    }
                };
                (
                    kind,
                    confirmed.registration.id.clone(),
                    confirmed.registration.rev,
                    confirmed.registration.digest.clone(),
                )
            })
        } else {
            match self.loom_pane {
                LoomPane::Types => match self.type_row(self.loom_selection) {
                    Some(TypeRow::Registered(index)) => self.loom_types.get(index).map(|record| {
                        (
                            haider_protocol::loom::LoomRegistryEntryKind::AgentType,
                            record.id.clone(),
                            record.rev,
                            record.digest(),
                        )
                    }),
                    _ => None,
                },
                LoomPane::Workflows => match self.workflow_row(self.loom_selection) {
                    Some(WorkflowRow::Registered(index)) => {
                        self.loom_workflows.get(index).map(|record| {
                            (
                                haider_protocol::loom::LoomRegistryEntryKind::Workflow,
                                record.id.clone(),
                                record.rev,
                                record.digest.clone(),
                            )
                        })
                    }
                    _ => None,
                },
            }
        };
        let Some((kind, id, expected_rev, expected_digest)) = selected else {
            self.flash = Some(if self.loom_authoring.is_some() {
                "· confirm this Loom document before archiving it".to_owned()
            } else {
                "· archive — select a registered user row".to_owned()
            });
            return;
        };
        self.flash = Some(format!("· archiving {id}…"));
        self.requests.push(AppRequest::LoomArchive {
            kind,
            id,
            expected_rev,
            expected_digest,
        });
    }

    fn cancel_loom_install(&mut self) {
        self.dirty = true;
        if !self.daemon_serves(haider_rpc::FEATURE_TYPED_AGENT_INSTALL_CANCEL_V1) {
            self.flash = Some(self.stale_daemon_note("typed-agent install cancellation"));
            return;
        }
        if self
            .loom_authoring
            .as_ref()
            .is_some_and(|authoring| authoring.pending)
        {
            self.flash = Some("· Loom authoring request already in flight".to_owned());
            return;
        }
        let Some((generation, job_id)) = self.loom_authoring.as_ref().and_then(|authoring| {
            if authoring
                .install_job
                .as_ref()
                .is_some_and(|job| job.state.is_terminal())
            {
                return None;
            }
            authoring
                .confirmed
                .as_ref()
                .and_then(|confirmed| confirmed.install_job_id.clone())
                .map(|job_id| (authoring.generation, job_id))
        }) else {
            self.flash = Some("· no cancellable install job in this editor".to_owned());
            return;
        };
        self.requests
            .push(AppRequest::LoomInstallCancel { generation, job_id });
        if let Some(authoring) = &mut self.loom_authoring {
            authoring.pending = true;
        }
        self.flash = Some("· cancelling typed-agent install…".to_owned());
    }

    fn loom_registry_expectation(
        &self,
        kind: haider_protocol::loom::LoomAuthorKind,
        text: &str,
    ) -> Option<(u32, Option<String>)> {
        let signatures = self
            .loom_types
            .iter()
            .map(|record| (record.id.as_str(), record.signature()))
            .collect::<std::collections::HashMap<_, _>>();
        let Ok(spec) = haider_protocol::loom::validate_loom_author_text(text, kind, |id| {
            signatures.get(id).cloned()
        }) else {
            // Confirmation is already gated on successful daemon validation,
            // but a local skew must still fail closed. It cannot turn a
            // missing coordinate into the rev-zero "must be absent" fence.
            return None;
        };
        Some(match spec {
            haider_protocol::loom::ValidatedLoomAuthorSpec::AgentType { record, .. } => self
                .loom_types
                .iter()
                .find(|current| current.id == record.id)
                .map_or((0, None), |current| (current.rev, Some(current.digest()))),
            haider_protocol::loom::ValidatedLoomAuthorSpec::Workflow { source, .. } => {
                let id = haider_protocol::loom::parse_pipe(&source).name;
                id.and_then(|id| self.loom_workflows.iter().find(|current| current.id == id))
                    .map_or((0, None), |current| {
                        (current.rev, Some(current.digest.clone()))
                    })
            }
        })
    }

    fn enter_graph(&mut self, arg: Option<&str>) {
        self.dirty = true;
        if self.screen != Screen::Session && self.screen != Screen::Graph {
            self.flash = Some("· /graph — session only".to_owned());
            return;
        }
        if self.mode.fabricates_locally() {
            self.flash =
                Some("· /graph — live only; convergence graphs are daemon truth".to_owned());
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1) {
            self.graph_unsupported();
            self.flash = Some(self.stale_daemon_note("graph"));
            return;
        }
        match arg {
            Some("pin") => {
                if self.graph.as_ref().is_some_and(|status| {
                    matches!(
                        status.phase,
                        haider_protocol::graph::GraphPhase::Active
                            | haider_protocol::graph::GraphPhase::Blocked
                    )
                }) {
                    self.flash = Some(
                        "· a graph is already active — /graph abandon first, then re-pin"
                            .to_owned(),
                    );
                } else {
                    self.requests.push(AppRequest::GraphPin { template: None });
                    self.flash = Some("· pinning ship-loop…".to_owned());
                }
            }
            Some("abandon") => {
                if self.graph.is_some() {
                    self.requests.push(AppRequest::GraphAbandon {
                        why: "abandoned from /graph".to_owned(),
                    });
                    self.flash = Some("· abandoning graph…".to_owned());
                } else {
                    self.flash = Some("· no graph pinned".to_owned());
                }
            }
            // Bare `/graph`, `/graph status`, or an unknown sub-token all
            // open the read-only status view and refresh it.
            _ => {
                self.graph_unsupported = false;
                self.requests.push(AppRequest::GraphRefresh);
                self.requests.push(AppRequest::GraphInspectRefresh);
                self.screen = Screen::Graph;
            }
        }
    }

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
        // The cwd is CAPTURED AT ISSUANCE (the B2b capture law): prefer the
        // active session's daemon summary coordinate, even when this TUI was
        // launched from another directory. Older daemons fall back to the
        // process cwd.
        self.requests.push(AppRequest::HooksRefresh {
            cwd: self
                .session_workspace_cwd
                .clone()
                .unwrap_or_else(|| self.cwd.clone()),
        });
    }

    /// Keys on the `/hooks` screen. The confirmation card is total while
    /// open: esc cancels the CARD (session-scoped esc law — it never
    /// navigates), ⏎ dispatches; without a card the rows follow the owner
    /// menu law (arrow highlight, digits pick, ⏎ opens the card) and esc
    /// walks back to the session.
    fn handle_hooks_key(&mut self, code: KeyCode) {
        if self.hooks.drilldown.is_some() {
            if code == KeyCode::Esc {
                self.hooks.drilldown = None;
                self.dirty = true;
            }
            return;
        }
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
        let grant = self.hooks.glyph(row) != crate::hooks::TrustGlyph::Trusted;
        self.hooks.confirm = Some(crate::hooks::TrustConfirm {
            digest: row.digest.clone(),
            name: row.name.clone(),
            grant,
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

    /// Replaces only the active argument fragment. Completed slots remain in
    /// order, so two-stage commands such as `/login <provider> <method>` do
    /// not lose the provider when the method row is activated.
    fn set_palette_argument(&mut self, cmd: &str, value: &str) {
        let body = self.composer.text().trim_start_matches('/');
        let trailing_space = body.ends_with(char::is_whitespace);
        let mut words = body.split_whitespace();
        let current_cmd = words.next().unwrap_or_default();
        let args = words.collect::<Vec<_>>();
        let retained = if current_cmd.eq_ignore_ascii_case(cmd) {
            if trailing_space {
                args.len()
            } else {
                args.len().saturating_sub(1)
            }
        } else {
            0
        };
        let mut completed = format!("/{cmd}");
        for argument in args.into_iter().take(retained) {
            completed.push(' ');
            completed.push_str(argument);
        }
        completed.push(' ');
        completed.push_str(value);
        self.composer.set_text(completed);
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
                self.set_palette_argument(cmd, &value);
                self.execute_slash();
            }
            // W-C M1: activating a custom command preserves any args the user
            // already typed, then runs it (execute_slash resolves the loaded
            // command and submits its expanded body as a user turn).
            PaletteItem::Custom { name, .. } => {
                let args: String = self
                    .composer
                    .text()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(" ");
                self.composer.set_text(if args.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {args}")
                });
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
            // Explicit parity for terminals/frontends where a rapid double
            // Esc cannot be distinguished. Bare opens the same chooser;
            // an ordinal loads that durable prompt verbatim.
            "history" => self.history_command(&remainder),
            // The same parity door for the FORK verb the chooser binds to
            // `f`: bare opens the chooser, an ordinal forks that prompt
            // into a new session and leaves this one open.
            "fork" => self.fork_command(&remainder),
            // W-C M2: the desktop-notification toggle. Works everywhere (it is
            // a display preference, not a session command); bare `/notifications`
            // flips it, `on`/`off` set it explicitly, and the runtime persists
            // the change to the TUI settings file.
            "notifications" | "notify" => match arg.as_deref() {
                Some("on") => self.toggle_notifications(Some(true)),
                Some("off") => self.toggle_notifications(Some(false)),
                None | Some("toggle") => self.toggle_notifications(None),
                Some(other) => {
                    self.flash = Some(format!("· /notifications {other} — try on · off"));
                }
            },
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
                let provider = words.next().unwrap_or("").to_ascii_lowercase();
                let method = words.next().unwrap_or("").to_ascii_lowercase();
                let alias = words.next().map(str::to_owned);
                match (provider.as_str(), method.as_str()) {
                    ("", _) => {
                        self.flash = Some(
                            "· /login <provider> <oauth|api> — e.g. /login anthropic api"
                                .to_owned(),
                        );
                    }
                    // The custom route is the existing Accounts add card:
                    // name + base URL + auth choice + optional masked key.
                    // Its feature gate, alias allocation, keyless branch,
                    // staging and daemon configure/login chain remain owned
                    // by the one AccountAdd arm.
                    ("custom", "api") => {
                        self.enter_accounts();
                        self.handle_hit(Hit::AccountAdd(AccountAddKind::Custom));
                    }
                    ("kimi" | "grok", "api") => {
                        self.flash = Some(format!(
                            "· /login {provider} api — no API-key flow for this provider; try /login {provider} oauth"
                        ));
                    }
                    // 970 — Google Antigravity. `/login google` is the
                    // PREFERRED shortcut and needs no method word (the login
                    // is Google's agent's to perform, so there is only one);
                    // the explicit existing grammar reaches the same door.
                    ("google" | GOOGLE_ANTIGRAVITY_PROVIDER, "" | "oauth") => {
                        self.enter_accounts();
                        self.handle_hit(Hit::AccountAdd(AccountAddKind::GoogleAntigravity));
                    }
                    // Every other method word is refused, and the refusal
                    // names the OTHER Google account a user may actually
                    // want: the API-key `gemini` provider is a separate
                    // credential, not a spelling of this one.
                    ("google" | GOOGLE_ANTIGRAVITY_PROVIDER, _) => {
                        self.flash = Some(format!(
                            "· /login {provider} {method} — Google Antigravity is agent-owned OAuth only; try /login google · the API-key Gemini provider is a separate account (/login gemini api)"
                        ));
                    }
                    (provider, "api") => self.open_login_card(provider, alias),
                    // B6b/B2b-m3: every `/login <provider> oauth` mirrors
                    // its account-add button EXACTLY by routing through the
                    // same hit arm — jump to /accounts first (the card
                    // renders and owns keys there), then the arm's feature
                    // gate and card open run unchanged (mirror by
                    // construction, never a second dispatch). The daemon
                    // owns every flow: loopback PKCE for openai/anthropic,
                    // and the device-code grants for kimi/grok.
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
                    ("grok", "oauth") => {
                        self.enter_accounts();
                        self.handle_hit(Hit::AccountAdd(AccountAddKind::GrokOAuth));
                    }
                    (provider, "oauth") => {
                        self.flash = Some(format!(
                            "· /login {provider} oauth — no OAuth flow for this provider; try /login {provider} api"
                        ));
                    }
                    // A provider row is the FIRST slot, not a complete
                    // command. Keep the chosen provider in the composer and
                    // visibly advance to the provider-aware method slot. This
                    // covers both palette activation and a directly typed
                    // `/login <provider>` after the palette was dismissed.
                    (provider, "") => {
                        self.composer.set_text(format!("/login {provider} "));
                        self.palette_selection = 0;
                        self.palette_scroll = 0;
                        self.palette_dismissed = false;
                        self.dirty = true;
                    }
                    (provider, _) => {
                        self.flash = Some(format!(
                            "· /login {provider} <oauth|api> — pick a supported method"
                        ));
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
            "update" if self.mode.fabricates_locally() => {
                // Demo remains a UI-only world. It must never run release,
                // filesystem, daemon, or process effects.
                self.flash = Some(
                    "· /update — UI ready; lands with the gates wave (live mode installs)"
                        .to_owned(),
                );
            }
            "update" => {
                if let Some(version) = self.update_available.as_deref() {
                    self.requests.push(AppRequest::RunUpdate);
                    self.flash = Some(format!("· updating to {}", update_version_label(version)));
                } else {
                    self.requests.push(AppRequest::CheckForUpdate);
                    self.flash = Some("· checking for updates…".to_owned());
                }
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
            "checkpoints" => self.checkpoint_command(None),
            "undo" => self.checkpoint_mutation_command(false, &remainder),
            "redo" => self.checkpoint_mutation_command(true, &remainder),
            "rollback" => self.checkpoint_command(Some(if remainder.trim().is_empty() {
                "current".to_owned()
            } else {
                remainder.trim().to_owned()
            })),
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
                            self.subturn_mode = false;
                            self.projection.push_note(
                                "· mid-turn input → STEER — delivered at the next safe boundary"
                                    .to_owned(),
                            );
                        }
                        Some("turn" | "queue") => {
                            self.queue_mode = true;
                            self.subturn_mode = false;
                            self.projection.push_note(
                                "· mid-turn input → QUEUE — held until the turn ends, then consumed without idling"
                                    .to_owned(),
                            );
                        }
                        Some("subturn") => {
                            self.queue_mode = false;
                            self.subturn_mode = true;
                            self.projection.push_note(
                                "· mid-turn input → SUBTURN — held for the next tool call, then injected before execution"
                                    .to_owned(),
                            );
                        }
                        _ => {
                            let mode = if self.queue_mode {
                                "queue (after turn)"
                            } else if self.subturn_mode {
                                "subturn (next tool call)"
                            } else {
                                "steer (safe boundary)"
                            };
                            self.projection.push_note(format!(
                                "· mid-turn input mode is {mode} — /queue steer|subturn|turn"
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
            // DEMO keeps its honest daemon-truth refusal; live mode opens
            // the full selectable browser and can reach every listed row.
            "sessions" if !self.mode.fabricates_locally() => {
                if remainder.is_empty() {
                    self.enter_sessions();
                } else {
                    self.open_listed_session(&remainder);
                }
            }
            // Owner 2026-08-21: the sessions screen the demo stub once
            // called unbuilt IS built — the all-sessions browser. Both
            // names open it; the demo says so honestly instead of
            // fabricating a roster.
            "sessions" => self.enter_sessions(),
            "accounts" => self.enter_accounts(),
            "peer" | "peers" => self.peer_command(&remainder),
            "ssh" => self.ssh_command(&remainder),
            "shells" => self.shells_command(&remainder),
            "monitors" => self.monitors_command(&remainder),
            "providers" => self.enter_providers(),
            "hooks" => self.enter_hooks(),
            // CG-M1: `/graph [pin|abandon|status]`.
            "graph" => self.enter_graph(arg.as_deref()),
            "loom" => self.enter_loom(),
            // Owner 2026-08-21: every session on the machine, with the
            // shared attention state visible (unseen dot, needs-you chip).
            "resume" => self.enter_sessions(),
            "workflows" => self.enter_workflows(),
            // Owner 2026-08-16: manual retry of the failed turn — the
            // keyboard path to the ambient retry row's click.
            "retry" => self.issue_run_retry(),
            // `/usage [history|models|calendar|global|accounts] [provider]`: a
            // leading scope lands directly; otherwise the first token keeps
            // the existing provider-prefix meaning.
            "usage" => {
                let mut usage_args = remainder.split_whitespace();
                let first = usage_args.next();
                let requested_scope = first.and_then(UsageScope::from_name);
                let scope = requested_scope.unwrap_or_default();
                let filter = if requested_scope.is_some() {
                    usage_args.next()
                } else {
                    first
                };
                self.enter_usage(scope, filter);
            }
            // W5e-3: choose from the DISCOVERED catalog. Both are
            // feature-gated BEFORE shipping this time (the W5e-1b lesson).
            "model" => {
                // F2a: `/model [query]` opens the FULL-SCREEN picker — exact
                // OAuth subscription rows plus one API choice per model
                // slug, query pre-filled. An empty registry keeps the honest
                // flash (stale daemon named when undiscoverable).
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
                    let (name, health) = (name.clone(), health.clone());
                    self.select_provider(name, health);
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
                        let names: Vec<String> = self
                            .accounts
                            .rows
                            .iter()
                            .map(|row| {
                                let identity = row.account_identity.as_ref().map_or_else(
                                    || row.identity.clone(),
                                    haider_protocol::credential::AccountIdentity::summary,
                                );
                                let created = row.created_at_ms.map_or_else(
                                    || "unknown (added before 0.0.964)".to_owned(),
                                    |created| created.to_string(),
                                );
                                format!(
                                    "{} [{} · {} · added {created}]",
                                    row.alias, row.provider, identity
                                )
                            })
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
                // W-C M1: a user-loaded custom command merges OVER (never
                // replaces) the built-ins, so it is resolved only after every
                // built-in arm missed. Found → expand + submit as a turn;
                // otherwise fall through to the stub/typo notes.
                if let Some(command) = self.custom_command(other).cloned() {
                    self.run_custom_command(&command, &remainder);
                    return;
                }
                // No catalog command is a stub any more — `/fork` was the
                // last one and now reaches the prompt-fork door (review r1
                // P2's honest-typo note is what remains).
                self.flash = Some(format!("· unknown command /{other} — /help lists commands"));
            }
        }
    }

    fn checkpoint_command(&mut self, rollback: Option<String>) {
        self.dirty = true;
        if self.screen != Screen::Session {
            self.flash = Some("· checkpoint commands are session only".to_owned());
            return;
        }
        if self.mode.fabricates_locally() {
            self.flash =
                Some("· checkpoints are live daemon history; demo mode fabricates none".to_owned());
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_CHECKPOINT_V1) {
            self.flash = Some(self.stale_daemon_note("workspace checkpoints"));
            return;
        }
        self.requests.push(AppRequest::Checkpoints {
            branch: self.branch_state.active().cloned(),
            rollback,
        });
        self.flash = Some("· reading durable checkpoints…".to_owned());
    }

    fn checkpoint_mutation_command(&mut self, redo: bool, argument: &str) {
        self.dirty = true;
        if self.screen != Screen::Session {
            self.flash = Some("· checkpoint commands are session only".to_owned());
            return;
        }
        if self.mode.fabricates_locally() {
            self.flash =
                Some("· checkpoints are live daemon history; demo mode fabricates none".to_owned());
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_CHECKPOINT_V1) {
            self.flash = Some(self.stale_daemon_note("workspace checkpoints"));
            return;
        }
        let target = if argument.trim().is_empty() {
            "last".to_owned()
        } else {
            argument.trim().to_owned()
        };
        let branch = self.branch_state.active().cloned();
        self.requests.push(if redo {
            AppRequest::CheckpointRedo { branch, target }
        } else {
            AppRequest::CheckpointUndo { branch, target }
        });
        self.flash = Some(if redo {
            "· redoing checkpoint…".to_owned()
        } else {
            "· undoing checkpoint…".to_owned()
        });
    }

    /// `/peer` is an inline transcript surface: bare lists peers and each row
    /// includes the exact send spelling; arguments send without fabricating a
    /// user/assistant transcript entry. `/peers` remains an input alias.
    fn peer_command(&mut self, argument: &str) {
        self.dirty = true;
        if self.mode.fabricates_locally() {
            self.flash = Some("· /peer — live only; the registry is daemon truth".to_owned());
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_PEER_MESSAGING_V1) {
            self.flash = Some(self.stale_daemon_note("peer messaging"));
            return;
        }
        let argument = argument.trim();
        if argument.is_empty() {
            self.peer_list_requested = true;
            self.requests.push(AppRequest::PeerList);
            self.flash = Some("· reading live peers…".to_owned());
            return;
        }
        let Some((to, message)) = argument.split_once(char::is_whitespace) else {
            self.flash = Some(format!(
                "· /peer {argument} <message> — message is required"
            ));
            return;
        };
        let message = message.trim_start();
        if message.is_empty() {
            self.flash = Some(format!("· /peer {to} <message> — message is required"));
            return;
        }
        self.requests.push(AppRequest::PeerSend {
            to: to.to_owned(),
            message: message.to_owned(),
        });
        self.flash = Some(format!("· sending peer message to {to}…"));
    }

    fn ssh_command(&mut self, argument: &str) {
        self.dirty = true;
        if self.mode.fabricates_locally() {
            self.flash = Some("· /ssh — live only; profiles are daemon truth".into());
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_SSH_PROFILES_V1) {
            self.flash = Some(self.stale_daemon_note("SSH profiles"));
            return;
        }
        let argument = argument.trim();
        if argument.is_empty() {
            self.ssh_open = true;
            self.requests.push(AppRequest::SshList);
            self.flash = Some("· reading SSH profiles…".into());
            return;
        }
        if let Some(value) = argument.strip_prefix("scope ") {
            let value = value.trim();
            let scope = match value {
                "all" => haider_rpc::SshScopeWire::All,
                "none" => haider_rpc::SshScopeWire::None,
                _ => {
                    let names = value
                        .split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(ToOwned::to_owned)
                        .collect::<Vec<_>>();
                    if names.is_empty() {
                        self.flash = Some("· /ssh scope all|none|name[,name…]".into());
                        return;
                    }
                    haider_rpc::SshScopeWire::Allow { names }
                }
            };
            self.requests.push(AppRequest::SshSetScope { scope });
            self.flash = Some("· updating this session's SSH scope…".into());
            return;
        }
        if let Some(profile) = argument.strip_prefix("shell ") {
            let profile = profile.trim();
            if profile.is_empty() {
                self.flash = Some("· /ssh shell <profile>".into());
                return;
            }
            self.open_ssh_terminal(profile.to_owned());
            return;
        }
        if let Some(profile) = argument.strip_prefix("test ") {
            let profile = profile.trim();
            if !profile.is_empty() {
                self.requests.push(AppRequest::SshTest {
                    profile: profile.to_owned(),
                });
                self.flash = Some(format!("· testing SSH profile {profile}…"));
                return;
            }
        }
        if let Some(profile) = argument.strip_prefix("rm ") {
            let profile = profile.trim();
            if !profile.is_empty() {
                self.requests.push(AppRequest::SshRemove {
                    profile: profile.to_owned(),
                });
                self.flash = Some(format!("· removing SSH profile {profile}…"));
                return;
            }
        }
        self.flash = Some(
            "· /ssh [scope all|none|name,… | shell <profile> | test <profile> | rm <profile>]"
                .into(),
        );
    }

    fn shells_command(&mut self, argument: &str) {
        self.dirty = true;
        if self.mode.fabricates_locally() {
            self.flash = Some("· /shells — live only; terminals are daemon truth".into());
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_SHELL_REGISTRY_V1) {
            self.flash = Some(self.stale_daemon_note("shell registry"));
            return;
        }
        let argument = argument.trim();
        if let Some(id) = argument.strip_prefix("close ") {
            self.requests.push(AppRequest::ShellClose {
                id: id.trim().to_owned(),
            });
            self.flash = Some("· closing shell…".into());
        } else if argument.is_empty() {
            self.shells_open = true;
            self.requests.push(AppRequest::ShellList);
            self.flash = Some("· reading terminal registry…".into());
        } else {
            self.flash = Some("· /shells [close <id>]".into());
        }
    }

    /// The band task line's LIVE shell count — starting/running only, the
    /// same filter the retired status segment applied. One truth for the
    /// row, its hit map, and the tests.
    #[must_use]
    pub fn live_shell_count(&self) -> usize {
        self.shells
            .iter()
            .filter(|shell| {
                matches!(
                    &shell.status,
                    haider_rpc::ShellStatusWire::Starting | haider_rpc::ShellStatusWire::Running
                )
            })
            .count()
    }

    /// The right-aligned counts on the `▾ subagents` band row (970 owner
    /// item 1). Empty when there is nothing running — the row then collapses
    /// exactly as it did before.
    #[must_use]
    pub fn band_counts(&self) -> Vec<crate::taskrows::BandCount> {
        crate::taskrows::band_counts(self.live_shell_count(), self.monitor_count)
    }

    /// `/monitors` and its control subcommands. A bare `/monitors` opens the
    /// overlay against fresh daemon truth; `stop|pause|resume <id>` act on one
    /// row without opening anything (owner item 4).
    fn monitors_command(&mut self, argument: &str) {
        self.dirty = true;
        if self.mode.fabricates_locally() {
            self.flash = Some("· /monitors — live only; monitors are daemon truth".into());
            return;
        }
        if self.screen != Screen::Session || self.active_session.is_none() {
            self.flash = Some("· /monitors — attach a session first".into());
            return;
        }
        if !self.daemon_serves(haider_rpc::FEATURE_MONITOR_CONTROL_V1) {
            self.flash = Some(self.stale_daemon_note("monitor details"));
            return;
        }
        let argument = argument.trim();
        if argument.is_empty() {
            self.monitors_open = true;
            self.monitors_cursor = 0;
            self.requests.push(AppRequest::MonitorList);
            self.flash = Some("· reading existing monitor registry…".into());
            return;
        }
        let (verb, id) = argument
            .split_once(char::is_whitespace)
            .unwrap_or((argument, ""));
        let id = id.trim();
        if id.is_empty() {
            self.flash = Some("· /monitors [stop|pause|resume <id>]".into());
            return;
        }
        match verb {
            "stop" | "remove" => self.monitor_stop(id.to_owned()),
            "pause" => self.monitor_pause(id.to_owned()),
            "resume" => self.monitor_resume(id.to_owned()),
            _ => self.flash = Some("· /monitors [stop|pause|resume <id>]".into()),
        }
    }

    pub(crate) fn monitor_stop(&mut self, monitor_id: String) {
        self.flash = Some(format!("· stopping monitor {monitor_id}…"));
        self.requests.push(AppRequest::MonitorRemove { monitor_id });
        self.dirty = true;
    }

    pub(crate) fn monitor_pause(&mut self, monitor_id: String) {
        self.flash = Some(format!("· pausing monitor {monitor_id}…"));
        self.requests.push(AppRequest::MonitorPause { monitor_id });
        self.dirty = true;
    }

    pub(crate) fn monitor_resume(&mut self, monitor_id: String) {
        self.flash = Some(format!("· resuming monitor {monitor_id}…"));
        self.requests.push(AppRequest::MonitorResume { monitor_id });
        self.dirty = true;
    }

    pub(crate) fn monitor_trigger(&mut self, monitor_id: String) {
        self.flash = Some(format!("· triggering monitor {monitor_id}…"));
        self.requests
            .push(AppRequest::MonitorTrigger { monitor_id });
        self.dirty = true;
    }

    /// Pause or resume by the row's OWN state — one key/click for the pair,
    /// so the overlay never asks the user which verb applies.
    pub(crate) fn monitor_toggle_pause(&mut self, monitor_id: String) {
        let paused = self
            .monitors
            .iter()
            .find(|monitor| monitor.monitor_id == monitor_id)
            .is_some_and(|monitor| matches!(monitor.state, haider_rpc::MonitorStateWire::Paused));
        if paused {
            self.monitor_resume(monitor_id);
        } else {
            self.monitor_pause(monitor_id);
        }
    }

    /// The prefill that hands one monitor's edit to the AGENT (owner item 2):
    /// the composer opens on `/monitor edit <id>: ` and the user types what to
    /// change in prose — the model then calls `monitor.update`.
    #[must_use]
    pub fn monitor_edit_prefill(monitor_id: &str) -> String {
        format!("/monitor edit {monitor_id}: ")
    }

    pub(crate) fn monitor_edit_with_agent(&mut self, monitor_id: &str) {
        self.monitors_open = false;
        self.composer
            .set_text(Self::monitor_edit_prefill(monitor_id));
        // The draft opens on a slash, but what follows is PROSE for the
        // agent — the palette must not sit over it offering commands.
        self.palette_dismissed = true;
        self.flash = Some("· describe the change — the agent edits the monitor".into());
        self.dirty = true;
    }

    pub(crate) fn monitor_copy_id(&mut self, monitor_id: &str) {
        // The reducer already holds the exact text (TUI5 item 5), so it
        // travels in the request and the runtime runs the shared
        // pbcopy + OSC 52 path.
        self.requests
            .push(AppRequest::CopyText(monitor_id.to_owned()));
        self.flash = Some(format!("· copied {monitor_id}"));
        self.dirty = true;
    }

    /// The overlay's selected row id, if the registry is non-empty.
    #[must_use]
    pub fn monitors_selected_id(&self) -> Option<String> {
        self.monitors
            .get(self.monitors_cursor)
            .map(|monitor| monitor.monitor_id.clone())
    }

    /// The state one row RENDERS as: daemon truth, except that a monitor this
    /// client has seen fire reads `firing` until the woken subturn completes
    /// (owner item 3).
    #[must_use]
    pub fn monitor_row_state(
        &self,
        monitor: &haider_rpc::MonitorRegistrationWire,
    ) -> haider_rpc::MonitorStateWire {
        if self.monitors_firing.contains(&monitor.monitor_id) {
            haider_rpc::MonitorStateWire::Firing
        } else {
            monitor.state
        }
    }

    /// One monitor delivery reaching this session: an ambient transcript note
    /// and a `firing` chip that stands until the woken subturn completes.
    /// Never a modal — the fire is news, not a question.
    pub fn apply_monitor_fired(&mut self, report: &haider_rpc::MonitorDeliveryReportWire) {
        self.monitors_firing.insert(report.monitor_id.clone());
        self.projection.push_note(monitor_fired_note(report));
        self.dirty = true;
    }

    /// The woken subturn finished: every row this client marked `firing`
    /// falls back to daemon truth.
    pub(crate) fn clear_monitor_firing(&mut self) {
        if !self.monitors_firing.is_empty() {
            self.monitors_firing.clear();
            self.dirty = true;
        }
    }

    pub fn apply_monitor_list(&mut self, receipt: haider_rpc::MonitorListReceiptWire) {
        self.monitors = match receipt.outcome {
            haider_rpc::MonitorListOutcomeWire::Listed { monitors } => monitors,
            haider_rpc::MonitorListOutcomeWire::Rejected { .. }
            | haider_rpc::MonitorListOutcomeWire::Unknown => Vec::new(),
            _ => Vec::new(),
        };
        self.monitor_count = self.monitors.len();
        self.monitors_cursor = self
            .monitors_cursor
            .min(self.monitors.len().saturating_sub(1));
        self.flash = None;
        self.dirty = true;
    }

    /// A pause/resume/trigger/update receipt: the returned row REPLACES the
    /// one we hold, so the chip follows daemon truth rather than an optimistic
    /// local guess.
    pub fn apply_monitor_mutate(&mut self, receipt: haider_rpc::MonitorMutateReceiptWire) {
        match receipt.outcome {
            haider_rpc::MonitorMutateOutcomeWire::Updated { monitor }
            | haider_rpc::MonitorMutateOutcomeWire::Paused { monitor }
            | haider_rpc::MonitorMutateOutcomeWire::Resumed { monitor } => {
                self.monitors_firing.remove(&monitor.monitor_id);
                if let Some(slot) = self
                    .monitors
                    .iter_mut()
                    .find(|row| row.monitor_id == monitor.monitor_id)
                {
                    *slot = monitor;
                } else {
                    self.monitors.push(monitor);
                }
                self.monitor_count = self.monitors.len();
                self.flash = None;
            }
            haider_rpc::MonitorMutateOutcomeWire::Triggered { monitor_id } => {
                self.flash = Some(format!("· triggered {monitor_id}"));
            }
            haider_rpc::MonitorMutateOutcomeWire::Rejected { rejection } => {
                self.flash = Some(monitor_rejection_note(&rejection));
            }
            _ => self.flash = Some("· monitor control refused".into()),
        }
        self.dirty = true;
    }

    pub fn apply_monitor_remove(&mut self, receipt: haider_rpc::MonitorRemoveReceiptWire) {
        match receipt.outcome {
            haider_rpc::MonitorRemoveOutcomeWire::Removed { monitor_id } => {
                self.monitors.retain(|row| row.monitor_id != monitor_id);
                self.monitors_firing.remove(&monitor_id);
                self.monitor_count = self.monitors.len();
                self.monitors_cursor = self
                    .monitors_cursor
                    .min(self.monitors.len().saturating_sub(1));
                self.flash = Some(format!("· stopped {monitor_id}"));
            }
            haider_rpc::MonitorRemoveOutcomeWire::Rejected { rejection } => {
                self.flash = Some(monitor_rejection_note(&rejection));
            }
            _ => self.flash = Some("· monitor control refused".into()),
        }
        self.dirty = true;
    }

    pub(crate) fn apply_ssh_list(&mut self, profiles: Vec<haider_rpc::SshProfileWire>) {
        self.ssh_profiles = profiles;
        self.ssh_remove_armed = None;
        self.ssh_cursor = self
            .ssh_cursor
            .min(self.ssh_profiles.len().saturating_sub(1));
        self.flash = None;
        self.dirty = true;
    }

    fn open_ssh_terminal(&mut self, profile: String) {
        self.ssh_open = false;
        let terminal = SshTerminalPane::opening(profile.clone(), self.ssh_terminal_size);
        self.requests.push(AppRequest::SshShellOpen {
            profile,
            size: terminal.size,
        });
        self.ssh_terminal = Some(terminal);
        self.flash = Some("· opening remote terminal…".into());
        self.dirty = true;
    }

    fn handle_ssh_form_key(&mut self, key: KeyEvent) {
        let mut save = None;
        let mut close = false;
        {
            let Some(form) = self.ssh_form.as_mut() else {
                return;
            };
            form.error = None;
            match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Tab | KeyCode::Down => {
                    form.focus = (form.focus + 1) % SshProfileForm::FIELD_COUNT;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    form.focus = form
                        .focus
                        .checked_sub(1)
                        .unwrap_or(SshProfileForm::FIELD_COUNT - 1);
                }
                KeyCode::Left if form.focus == 5 => form.cycle_auth(false),
                KeyCode::Right if form.focus == 5 => form.cycle_auth(true),
                KeyCode::Enter if form.focus + 1 < SshProfileForm::FIELD_COUNT => {
                    form.focus += 1;
                }
                KeyCode::Enter if form.focus == 7 => {
                    save = Some(form.take_request());
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    save = Some(form.take_request());
                }
                KeyCode::Backspace => ssh_form_backspace(form),
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    ssh_form_push(form, character);
                }
                _ => {}
            }
        }
        if close {
            self.ssh_form = None;
        }
        if let Some(result) = save {
            match result {
                Ok((mutation, secret)) => {
                    self.requests
                        .push(AppRequest::SshProfileSave { mutation, secret });
                    self.ssh_form = None;
                    self.flash = Some("· saving SSH profile…".into());
                }
                Err(error) => {
                    if let Some(form) = self.ssh_form.as_mut() {
                        form.error = Some(error);
                    }
                }
            }
        }
        self.dirty = true;
    }

    fn ssh_form_paste(&mut self, text: &str) {
        let Some(form) = self.ssh_form.as_mut() else {
            return;
        };
        form.error = None;
        let secret_field = form.focus == 6
            && matches!(
                form.auth,
                SshFormAuthKind::Password | SshFormAuthKind::KeyMaterial
            );
        for character in text.chars() {
            if secret_field || !character.is_control() {
                ssh_form_push(form, character);
            }
        }
        self.dirty = true;
    }

    pub(crate) fn apply_ssh_shell_opened(&mut self, shell: haider_rpc::ShellWire) {
        if let Some(terminal) = self.ssh_terminal.as_mut()
            && matches!(
                &shell.kind,
                haider_rpc::ShellKindWire::Ssh { profile } if profile == &terminal.profile
            )
        {
            terminal.shell_id = Some(shell.id);
            self.flash = Some("· remote terminal connected · ⌃] close".into());
            self.dirty = true;
        }
    }

    pub(crate) fn apply_ssh_shell_output(&mut self, id: &str, bytes: &[u8]) {
        if let Some(terminal) = self.ssh_terminal.as_mut()
            && terminal.shell_id.as_deref() == Some(id)
        {
            terminal.push_output(bytes);
            self.dirty = true;
        }
    }

    pub(crate) fn apply_ssh_shell_terminal_state(&mut self, shell: &haider_rpc::ShellWire) {
        let closes = self
            .ssh_terminal
            .as_ref()
            .and_then(|terminal| terminal.shell_id.as_deref())
            == Some(shell.id.as_str())
            && matches!(
                &shell.status,
                haider_rpc::ShellStatusWire::Exited { .. } | haider_rpc::ShellStatusWire::Closed
            );
        if closes {
            self.ssh_terminal = None;
            self.flash = Some(match &shell.status {
                haider_rpc::ShellStatusWire::Exited { code } => code.map_or_else(
                    || "· remote terminal exited".into(),
                    |code| format!("· remote terminal exited {code}"),
                ),
                _ => "· remote terminal closed".into(),
            });
            self.dirty = true;
        }
    }

    fn handle_ssh_terminal_key(&mut self, key: KeyEvent) {
        let Some(id) = self
            .ssh_terminal
            .as_ref()
            .and_then(|terminal| terminal.shell_id.clone())
        else {
            return;
        };
        if key.code == KeyCode::Char(']') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.requests.push(AppRequest::ShellClose { id });
            self.flash = Some("· closing remote terminal…".into());
            return;
        }
        if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.requests.push(AppRequest::SshShellEof { id });
            self.flash = Some("· remote terminal input closed…".into());
            return;
        }
        if let Some(bytes) = ssh_terminal_key_bytes(key) {
            self.queue_ssh_terminal_input(&id, &bytes);
        }
    }

    fn queue_ssh_terminal_input(&mut self, id: &str, bytes: &[u8]) {
        for chunk in bytes.chunks(haider_rpc::SSH_PTY_INPUT_MAX_BYTES) {
            self.requests.push(AppRequest::SshShellInput {
                id: id.to_owned(),
                input: SshTerminalInput::new(chunk.to_vec()),
            });
        }
    }

    pub(crate) fn apply_shell_list(&mut self, shells: Vec<haider_rpc::ShellWire>) {
        self.shells = shells;
        self.shells_cursor = self.shells_cursor.min(self.shells.len().saturating_sub(1));
        self.flash = None;
        self.dirty = true;
    }

    pub(crate) fn apply_shell_event(&mut self, shell: haider_rpc::ShellWire) {
        self.apply_ssh_shell_terminal_state(&shell);
        if let Some(existing) = self.shells.iter_mut().find(|entry| entry.id == shell.id) {
            *existing = shell;
        } else {
            self.shells.push(shell);
        }
        self.dirty = true;
    }

    /// Public only so `tests/` can pin the peer-list affordance without a
    /// daemon (the same precedent as [`crate::link::CommandContext`]).
    #[doc(hidden)]
    pub fn apply_peer_list(&mut self, agents: Vec<haider_protocol::peer::PeerDescriptor>) {
        self.peer_agents = agents;
        if !std::mem::take(&mut self.peer_list_requested) {
            self.dirty = true;
            return;
        }
        if self.peer_agents.is_empty() {
            self.projection.push_note("· no live peers".to_owned());
        } else {
            self.projection
                .push_note("· live peers — send with /peer <address> <message>".to_owned());
            for peer in &self.peer_agents {
                let kind = match peer.kind {
                    haider_protocol::peer::PeerKind::HaiderSession => "haider_session",
                    haider_protocol::peer::PeerKind::External => "external",
                };
                let state = match peer.state {
                    haider_protocol::peer::PeerState::Idle => "idle",
                    haider_protocol::peer::PeerState::Busy => "busy",
                };
                self.projection.push_note(format!(
                    "  {} · {} · {} · {} · last seen {} — /peer {} <message>",
                    peer.name, kind, peer.workspace, state, peer.last_seen, peer.id
                ));
            }
        }
        self.flash = None;
        self.dirty = true;
    }

    /// W-C M1: expand a custom command and submit its body as a user turn.
    ///
    /// There is NO inline shell execution this wave — a custom command becomes
    /// a PROMPT ONLY. A `model:` frontmatter override is applied through the
    /// SAME G3 `session.select_model` path `/model` uses (never a new one); an
    /// unknown model is ignored with a note.
    fn run_custom_command(
        &mut self,
        command: &crate::custom_commands::CustomCommand,
        arg_str: &str,
    ) {
        self.dirty = true;
        let expansion = command.expand(arg_str);
        let body = expansion.body.trim().to_owned();
        if body.is_empty() {
            self.flash = Some(format!(
                "· /{} — nothing to send (empty after expansion)",
                command.name
            ));
            return;
        }
        // The per-command model/pair override, resolved BEFORE the turn so the
        // select rides ahead of the submit on the request queue.
        let note = expansion
            .model
            .as_deref()
            .map(|model| self.apply_custom_command_model(command, model));
        // Submit the expanded body as an ordinary user turn — never re-parsed
        // as a slash command.
        let submitted = self.submit_custom_command_body(body);
        // A model note wins the flash only when the turn actually went out;
        // an unsubmitted turn keeps the submit helper's own explanation.
        if submitted && let Some(note) = note {
            self.flash = Some(note);
        }
    }

    /// W-C M1: resolve a custom command's `model:` override against the
    /// discovered catalog (like `/model`) and apply it via the receipted
    /// `session.select_model`. Returns the human note either way.
    fn apply_custom_command_model(
        &mut self,
        command: &crate::custom_commands::CustomCommand,
        model: &str,
    ) -> String {
        let resolved = self.providers.providers.iter().find_map(|summary| {
            summary
                .models
                .iter()
                .find(|slug| slug.eq_ignore_ascii_case(model))
                .map(|slug| (summary.provider.clone(), slug.clone()))
        });
        let Some((provider, resolved_model)) = resolved else {
            return format!(
                "· /{} — model “{model}” unknown; using the current model",
                command.name
            );
        };
        // The G3 path applies to an ATTACHED live session, exactly like the
        // picker: the launcher/demo has no session to select on yet.
        let live_session = (!self.mode.fabricates_locally() && self.screen == Screen::Session)
            .then(|| self.active_session.clone())
            .flatten();
        match live_session {
            Some(session) => {
                // Reuse the G3 gate: a stale daemon that cannot serve
                // cross-provider selection gets an honest note, never a
                // request it will reject.
                if !self.daemon_serves(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1) {
                    return format!("· /{} — model override needs a newer daemon", command.name);
                }
                let change = PendingCacheChange::Model {
                    session: session.clone(),
                    provider: provider.clone(),
                    model: resolved_model.clone(),
                };
                let confirm_new_epoch = self.pending_cache_change.as_ref() == Some(&change);
                self.requests.push(AppRequest::SelectModel {
                    session,
                    model: resolved_model.clone(),
                    provider: provider.clone(),
                    confirm_new_epoch,
                });
                format!(
                    "· /{} — model → {resolved_model} · {provider}",
                    command.name
                )
            }
            None => {
                // M9: the launcher has no attached session YET, but the very
                // next `CreateSession` mints the session from the identity pair
                // (live.rs reads `identity.provider`/`identity.model_short`).
                // Set that pair NOW — exactly like the `/model` picker's
                // launcher branch — so the FIRST turn uses the override instead
                // of the stale default. (The old code only returned a note and
                // the first turn ran on the old pair.)
                self.identity.provider = provider.clone();
                self.identity.model_short = resolved_model.clone();
                self.identity_pinned = true;
                self.refresh_context_window();
                format!(
                    "· /{} — model → {resolved_model} · {provider} (applies to the new session)",
                    command.name
                )
            }
        }
    }

    /// W-C M1: submit `body` as an ordinary user turn. Returns whether a turn
    /// was actually issued. Only the Session and Launcher surfaces submit;
    /// other screens (and the demo, which has no daemon) say so honestly.
    fn submit_custom_command_body(&mut self, body: String) -> bool {
        if self.mode.fabricates_locally() {
            self.flash =
                Some("· custom commands run live — they submit a turn to the daemon".to_owned());
            return false;
        }
        match self.screen {
            Screen::Session => {
                self.turn_active = true;
                self.scroll_back.set(0);
                self.requests.push(AppRequest::SubmitText {
                    text: body,
                    voice: false,
                    title: self.session_title.is_none(),
                    branch: self.branch_state.active().cloned(),
                    attachments: Vec::new(),
                });
                true
            }
            Screen::Launcher => {
                // Mirror the launcher submit (R11 cut 4): nothing local
                // happens — the daemon mints the session and its events drive
                // the screen flip, so no fabricated row can need reconciling.
                self.requests.push(AppRequest::CreateSession { text: body });
                true
            }
            _ => {
                self.flash =
                    Some("· custom commands run from a session or the launcher".to_owned());
                false
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

    /// W-G: feed the throughput tracker one observation on the frame clock —
    /// called at the existing clock-advance sites (every applied ACTIVE-session
    /// envelope and the live anim tick), so there is NO new timer. While the
    /// turn streams it samples the cumulative output-token count at
    /// `clock_ms`, preferring provider usage and falling back to an
    /// approximate text-derived count (rendered `~`) when no incremental usage
    /// is reported. Off-stream it resets the tracker ONCE to the empty resting
    /// shape, so idle frames stay byte-identical (WG3).
    pub fn note_throughput(&mut self) {
        // PERSISTENCE (owner 2026-08-15): off-stream the tracker is left
        // untouched, so the last turn's readout stays visible at rest — it is
        // SETTLED, not cleared, and the next turn's epoch starts it over.
        //
        // tpsfix (v0.0.970): the estimator is fed the smooth streamed-character
        // signal plus the provider's exact per-turn total, and the turn epoch
        // that owns both. It samples through the WHOLE live turn (thinking and
        // tool time included) so a mid-turn pause never publishes a final
        // figure; the generation clock inside the estimator still starts at the
        // first output token, so TTFT stays excluded.
        let now = self.clock_ms;
        let turn = self.projection.turn_epoch();
        let exact = self.projection.turn_output_tokens_exact();
        if self.projection.turn_live() {
            self.throughput
                .observe(now, turn, self.projection.streamed_output_chars(), exact);
        } else {
            self.throughput.settle(now, turn, exact);
        }
    }

    /// W-G: the throughput row's data, or `None` when the row must not show —
    /// off-stream (the WG3 gate) or before a rate is established. The render
    /// and plain layers both build their line from this readout.
    #[must_use]
    pub fn throughput_readout(&self) -> Option<crate::throughput::ThroughputReadout> {
        if !self.projection.is_streaming() {
            return None;
        }
        self.throughput.readout()
    }

    /// The ALWAYS-VISIBLE throughput readout for the composer identity line —
    /// the last measured rate persists at rest (owner: keep it visible even
    /// when not streaming). `None` only before any rate has been established.
    /// Idle frames stay byte-identical because a rest readout is static.
    #[must_use]
    pub fn throughput_pill(&self) -> Option<crate::throughput::ThroughputReadout> {
        self.throughput.readout()
    }

    fn note_session_activity_at(&mut self, session_id: &SessionId, activity_ms: u64) {
        let attention = self
            .session_attention
            .entry(session_id.clone())
            .or_insert_with(|| SessionAttention {
                seen_at_ms: None,
                last_activity_ms: None,
                waiting_why: None,
                needs_input: None,
            });
        attention.last_activity_ms = attention
            .last_activity_ms
            .into_iter()
            .chain(Some(activity_ms))
            .max();
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
                if let Ok(EventPayload::ClientDiagnostic { code, message, .. }) =
                    envelope.payload.decode_event()
                    && code == "client-daemon-incompatible"
                {
                    self.compatibility_diagnostic =
                        Some(haider_protocol::error::ErrorPresentation::new(
                            code,
                            "Client/daemon incompatible — update",
                            message,
                            haider_protocol::error::ErrorScope::Session,
                            [haider_protocol::error::ErrorAction::None],
                        ));
                }
                // S4: applied journal truth advances the render clock —
                // the first paint after a spawn reads a clock already
                // inside the journal's own time base, tick or no tick.
                self.clock_ms = self.clock_ms.max(envelope.committed_at_ms);
                self.note_session_activity_at(&envelope.session_id, envelope.committed_at_ms);
                // M10: a BACKGROUND session's terminal/park transition also
                // warrants a desktop notification. The attached reducer
                // (`handle_envelope`) only ever evaluated the ACTIVE session, so
                // a backgrounded turn reaching Done/Errored used to notify
                // never. The active session keeps its own edge tracker, so only
                // NON-active sessions are evaluated here (no double-fire).
                if self.active_session.as_ref() != Some(&envelope.session_id)
                    && let Ok(EventPayload::RunState(state)) = envelope.payload.decode_event()
                {
                    let title = self
                        .sessions
                        .iter()
                        .find(|entry| entry.id == envelope.session_id)
                        .and_then(|entry| entry.title.clone());
                    self.note_background_run_state_for_notifications(
                        &envelope.session_id,
                        &state,
                        title.as_deref(),
                    );
                }
                // W-G: sample throughput for the ACTIVE session on the same
                // applied-envelope clock that just advanced (the frame clock —
                // no new timer). A background session's stream never feeds the
                // active row.
                if self.active_session.as_ref() == Some(&envelope.session_id) {
                    self.note_throughput();
                }
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
                if matches!(note, crate::branch::AdmittedNote::Content) {
                    match envelope.payload.decode_event() {
                        Ok(payload) => {
                            // The origin marker is intentionally `ui=false`:
                            // consume it as display metadata for the linked
                            // visible command, never as its own row.
                            if envelope.agent_id.is_none()
                                && let Some(origin) =
                                    crate::projection::user_command_origin(&payload)
                            {
                                self.branch_state.mark_user_command(
                                    &mut self.projection,
                                    envelope.branch_id.as_ref(),
                                    &origin,
                                );
                            }
                            if envelope.agent_id.is_none()
                                && let EventPayload::UserMessage { text, .. } = &payload
                            {
                                self.record_prompt(crate::session::PromptEntry::committed(
                                    text.clone(),
                                    envelope.seq,
                                ));
                            }
                            if admission == Admission::Apply {
                                self.route_admitted(
                                    &payload,
                                    envelope.branch_id.as_ref(),
                                    envelope.agent_id.as_ref(),
                                    envelope.committed_at_ms,
                                );
                            }
                        }
                        // S3/W-A/L3: additive agent, task, and workflow graph
                        // event unions ride raw envelopes OUTSIDE
                        // `EventPayload` — try them before counting the
                        // payload unknown (both twins).
                        Err(_) if admission == Admission::Apply => {
                            if !crate::session::route_agent_event(
                                &mut self.branch_state,
                                &mut self.projection,
                                &mut self.chips,
                                envelope,
                            ) && !crate::session::route_task_event(
                                &mut self.tasks,
                                &mut self.branch_state,
                                &mut self.projection,
                                envelope,
                            ) && !crate::session::route_workflow_graph_event(envelope)
                                && !crate::session::route_permission_event(
                                    &mut self.projection,
                                    envelope,
                                )
                                && !crate::session::route_workspace_event(
                                    &mut self.projection,
                                    envelope,
                                )
                            {
                                self.projection.count_unknown_payload();
                            }
                        }
                        Err(_) => {}
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
        if let EventPayload::Usage(usage) = payload {
            self.cache_usage.note(usage);
        }
        // 954 queue panel: deltas maintain the panel in place (Enqueued
        // carries the complete row — the render-complete law). A delta the
        // panel cannot interpret (a newer daemon's Unknown change) forces
        // an honest fresh list instead of a silently wrong panel.
        if let EventPayload::QueueChanged(delta) = payload {
            if self.queue_panel.apply_delta(delta) {
                self.requests.push(AppRequest::QueueList);
            }
            self.dirty = true;
        }
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
            Destination::Session => {
                if let EventPayload::RunState(state) = payload {
                    self.provider_wait_started_at_ms =
                        matches!(state, RunState::Thinking).then_some(at_ms);
                }
                self.handle_envelope(payload);
            }
        }
    }

    fn handle_envelope(&mut self, payload: &EventPayload) {
        // W-UI: a child's workflow rollup rides the parent stream as a
        // typed extension item (`agent_graph_rollup_v1`). It is CHIP state,
        // not transcript prose — route it to the chip and swallow both
        // marker halves so the transcript never shows the raw fact.
        if let EventPayload::Item(event) = payload {
            let item = match event {
                haider_protocol::item::ItemEvent::Started { item, .. }
                | haider_protocol::item::ItemEvent::Completed { item, .. } => Some(item),
                haider_protocol::item::ItemEvent::Delta { .. } => None,
            };
            if let Some(haider_protocol::item::TurnItem::Extension { kind, data }) = item
                && kind == haider_protocol::agent::AGENT_GRAPH_ROLLUP_EXTENSION_KIND
            {
                if matches!(event, haider_protocol::item::ItemEvent::Completed { .. })
                    && let Ok(roll) = serde_json::from_value::<
                        haider_protocol::agent::AgentGraphRollupV1,
                    >(data.clone())
                    && let Some(chip) = find_chip_mut(&mut self.chips, roll.agent.as_str())
                {
                    chip.note_event_at(self.clock_ms);
                    chip.graph = Some(roll);
                    self.dirty = true;
                }
                return;
            }
        }
        // Recovery consequences are derived from the still-open typed card,
        // but dispatched only after this committed MenuAnswered fact applies.
        // A stale/rejected local click therefore cannot start an account op.
        let committed_recovery = if let EventPayload::MenuAnswered(answer) = payload {
            self.projection.open_menu().and_then(|menu| {
                if menu.id != answer.menu {
                    return None;
                }
                let MenuKind::ErrorRecovery {
                    option_actions,
                    provider,
                    account,
                    presentation,
                    ..
                } = &menu.kind
                else {
                    return None;
                };
                let index = answer.option_key.as_deref().map_or_else(
                    || usize::try_from(answer.option_index).ok(),
                    |key| menu.options.iter().position(|option| option.key == key),
                )?;
                option_actions.get(index).copied().map(|action| {
                    let provider = if matches!(action, ErrorAction::Relogin | ErrorAction::TopUp) {
                        provider.clone()
                    } else {
                        None
                    };
                    let account = if action == ErrorAction::Relogin {
                        account.as_ref().map(|alias| alias.as_str().to_owned())
                    } else {
                        None
                    };
                    let reset_at_ms = if action == ErrorAction::Wait {
                        presentation.reset_at_ms
                    } else {
                        None
                    };
                    (action, provider, account, reset_at_ms)
                })
            })
        } else {
            None
        };
        if let EventPayload::Usage(usage) = payload {
            self.cache_usage.note(usage);
        }
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
        if let EventPayload::RunState(state) = payload {
            // W-C M2: the desktop-notification edge — terminal + attention-park
            // states only, never mid-stream, gated on the focus/toggle inside.
            self.note_run_state_for_notifications(state);
            if state.is_terminal() {
                self.turn_active = false;
                self.auto_resuming = false;
                // 970 owner item 3: the woken subturn is over, so every row
                // this client painted `firing` falls back to daemon truth.
                self.clear_monitor_firing();
                // The `♪ speaking` tag ends where the TURN ends. A trailing
                // `Voice(false)` beat could not: a branch parked on a menu
                // never reaches its own tail, so later ordinary rows kept
                // rendering as spoken (review P2-10).
                self.projection.set_voice_live(false);
            }
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
        if let Some((action, provider, account, reset_at_ms)) = committed_recovery {
            self.recovery_card_answered(action, provider, account, reset_at_ms);
        }
    }

    fn recovery_card_answered(
        &mut self,
        action: ErrorAction,
        provider: Option<String>,
        account: Option<String>,
        reset_at_ms: Option<u64>,
    ) {
        match action {
            ErrorAction::Relogin => {
                if let (Some(provider), Some(alias)) = (provider, account) {
                    self.enter_accounts();
                    self.open_oauth_relogin(&provider, alias);
                } else {
                    self.projection
                        .push_note("· open Accounts and sign in again".into());
                }
            }
            ErrorAction::Reimport => {
                self.enter_accounts();
                self.requests.push(AppRequest::DeviceCandidatesRefresh);
                self.accounts.message = Some("· looking for credentials to re-adopt…".into());
            }
            ErrorAction::SwitchAccount => {
                self.enter_accounts();
                self.accounts.message = Some("· choose another usable account".into());
            }
            ErrorAction::EditKey => {
                self.enter_accounts();
                self.accounts.message = Some(
                    "· replace this key by removing the rejected account, then add it again".into(),
                );
            }
            ErrorAction::TopUp => self.projection.push_note(format!(
                "· add credits in the {} billing portal, then retry",
                provider.as_deref().unwrap_or("provider")
            )),
            ErrorAction::Wait => self.projection.push_note(reset_at_ms.map_or_else(
                || "· wait for the provider limit to reset, then retry".into(),
                |reset| format!("· wait until Unix time {reset} ms, then retry"),
            )),
            ErrorAction::Retry => self
                .projection
                .push_note("· submit the prompt again to retry".into()),
            ErrorAction::ChooseModel => {
                self.projection
                    .push_note("· open Models and choose a compatible model".into());
            }
            ErrorAction::ContactAdmin => self
                .projection
                .push_note("· contact the account administrator for access".into()),
            ErrorAction::ContinuePartial | ErrorAction::RetryFresh | ErrorAction::None => {}
        }
        self.dirty = true;
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
        *self.transcript_layout.get_mut() = Default::default();
        self.prompt_history.clear();
        self.close_backtrack();
        self.branch_state = crate::branch::BranchState::default();
        self.hook_facts = crate::hooks::HookFactsLog::default();
        self.tasks = crate::taskrows::TaskPanel::default();
        self.throughput.reset();
        self.session_title = None;
        self.session_name = None;
        self.session_workspace_cwd = None;
        self.lockdown_provider = None;
        self.lockdown_boundary_known = false;
        self.lockdown_status = None;
        self.lockdown_overlay = false;
        self.turn_active = false;
        // Monitors are SESSION truth — a new session inherits none of the
        // previous one's registry, cursor, or firing overlay.
        self.monitors.clear();
        self.monitor_count = 0;
        self.monitors_open = false;
        self.monitors_cursor = 0;
        self.monitors_stop_armed = None;
        self.monitors_firing.clear();
        self.msg_queue.clear();
        self.queue_mode = false;
        self.subturn_mode = false;
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

    /// `/sessions <n|id>` — open ANY listed session, including the ones
    /// past the launcher's painted rows (W3c3.1 r2, P2-D).
    ///
    /// Without this, R11's "cold sessions … listable and READABLE" held
    /// only for the first nine: rows past the digit span had no hit target
    /// and no key, so raising the bound from three to nine moved the
    /// defect rather than closing it. The read itself is the attach's own
    /// replay, so opening is all that was missing.
    fn open_listed_session(&mut self, arg: &str) {
        let rows = self.session_rows_for_query("");
        let by_ordinal = arg
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .and_then(|n| rows.get(n - 1))
            .map(|row| row.id.clone());
        let target = by_ordinal.or_else(|| {
            rows.iter()
                .find(|row| row.id.as_str() == arg)
                .map(|row| row.id.clone())
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
        if let Some(id) = self.launcher_session_ids().get(index).cloned() {
            self.open_session(&id);
        }
    }

    /// Sim `openSession` (tui.js:1606-1615): sweep closed chips whose 5 s
    /// removal never fired, attach, and NOTHING else — no turn starts
    /// (owner item 1), and the one left behind keeps running.
    pub fn open_session(&mut self, id: &SessionId) {
        if self.active_session.as_ref() == Some(id) {
            self.switch_surface(Screen::Session);
            self.note_session_view();
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
        *self.transcript_layout.get_mut() = Default::default();
        self.prompt_history = std::mem::take(&mut slot.prompt_history);
        self.cache_usage = std::mem::take(&mut slot.cache_usage);
        self.chips = std::mem::take(&mut slot.chips);
        // B2b: the branch registry/active/parked views travel as ONE unit
        // with the session — the A→B→A checkout law.
        self.branch_state = std::mem::take(&mut slot.branch_state);
        // H4: the journaled hook facts + decision-chip state travel the
        // same way.
        self.hook_facts = std::mem::take(&mut slot.hook_facts);
        // CG-M1: the graph reduction travels whole with the session so the
        // strip reflects the session it belongs to, never the last one seen.
        self.graph = slot.graph.take();
        self.workflow_graph = std::mem::take(&mut slot.workflow_graph);
        self.workflow_graph_rpc = std::mem::take(&mut slot.workflow_graph_rpc);
        self.workflow_graph_error = None;
        self.workflow_evidence_inspection = None;
        self.graph_unsupported = false;
        // W-A: the background task rows travel whole with the session.
        self.tasks = std::mem::take(&mut slot.tasks);
        self.lockdown_provider = slot.lockdown_provider.take();
        self.lockdown_boundary_known = slot.lockdown_boundary_known;
        self.lockdown_status = None;
        self.lockdown_overlay = false;
        self.msg_queue = std::mem::take(&mut slot.msg_queue);
        self.queue_mode = slot.queue_mode;
        self.subturn_mode = slot.subturn_mode;
        self.turn_active = slot.turn_active;
        self.auto_resuming = slot.auto_resuming;
        self.subtree_collapsed = slot.subtree_collapsed;
        self.todos_collapsed = slot.todos_collapsed;
        self.session_title = slot.title.take();
        self.session_name = slot.name.take();
        self.session_head = std::mem::take(&mut slot.head);
        self.session_dir = std::mem::take(&mut slot.dir);
        self.session_workspace_cwd = slot.workspace_cwd.take();
        self.sessions[index] = slot;
        self.active_session = Some(id.clone());
        self.menu_selection = 0;
        self.view_path.clear();
        // CG-M1: read this session's graph reduction so the strip reflects a
        // graph pinned earlier (single-flight in the driver). Live only and
        // feature-gated: demo has no graph truth, and an old daemon has no
        // `graph.status` — so neither pollutes the request stream.
        if !self.mode.fabricates_locally()
            && self.daemon_serves(haider_rpc::FEATURE_CONVERGENCE_GRAPH_V1)
        {
            self.requests.push(AppRequest::GraphRefresh);
        }
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
        self.note_session_view();
    }

    /// Returns the provider whose ceiling the status bar must display. Before
    /// the first accepted turn the provider roster is the best available
    /// truth; afterward the accepted boundary remains authoritative even if
    /// an administrator has already toggled trust for the following turn.
    pub(crate) fn active_lockdown_provider(&self) -> Option<&str> {
        if self.lockdown_boundary_known {
            return self.lockdown_provider.as_deref();
        }
        self.providers
            .providers
            .iter()
            .find(|provider| provider.provider == self.identity.provider)
            .filter(|provider| !matches!(provider.trust, haider_rpc::ProviderTrustWire::Full))
            .map(|provider| provider.provider.as_str())
    }

    /// Freezes the UI chip at the same accepted-turn boundary used by the
    /// daemon. A later roster update changes only the next boundary.
    pub fn note_lockdown_turn_boundary(&mut self) {
        self.lockdown_provider = self
            .providers
            .providers
            .iter()
            .find(|provider| provider.provider == self.identity.provider)
            .filter(|provider| !matches!(provider.trust, haider_rpc::ProviderTrustWire::Full))
            .map(|provider| provider.provider.clone());
        self.lockdown_boundary_known = true;
        self.lockdown_status = None;
        self.dirty = true;
    }

    pub fn note_session_lockdown_turn_boundary(&mut self, session_id: &SessionId) {
        if self.active_session.as_ref() == Some(session_id) {
            self.note_lockdown_turn_boundary();
        }
    }

    /// A view/read acknowledgement is deliberately a request, not local
    /// unseen bookkeeping. The live driver debounces it and holds it until
    /// the control attachment is established; another surface then receives
    /// the same durable truth through ordinary roster summaries.
    fn note_session_view(&mut self) {
        if self.mode.fabricates_locally()
            || !self.daemon_serves(haider_rpc::FEATURE_SESSION_SEEN_V1)
        {
            return;
        }
        if let Some(session) = self.active_session.clone() {
            self.requests.push(AppRequest::Seen { session });
        }
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
            *self.transcript_layout.get_mut() = Default::default();
            slot.prompt_history = std::mem::take(&mut self.prompt_history);
            slot.cache_usage = std::mem::take(&mut self.cache_usage);
            slot.chips = std::mem::take(&mut self.chips);
            slot.branch_state = std::mem::take(&mut self.branch_state);
            slot.hook_facts = std::mem::take(&mut self.hook_facts);
            slot.graph = self.graph.take();
            slot.workflow_graph = std::mem::take(&mut self.workflow_graph);
            slot.workflow_graph_rpc = std::mem::take(&mut self.workflow_graph_rpc);
            self.workflow_graph_error = None;
            self.workflow_evidence_inspection = None;
            slot.tasks = std::mem::take(&mut self.tasks);
            slot.lockdown_provider = self.lockdown_provider.take();
            slot.lockdown_boundary_known = std::mem::take(&mut self.lockdown_boundary_known);
            slot.msg_queue = std::mem::take(&mut self.msg_queue);
            slot.queue_mode = std::mem::take(&mut self.queue_mode);
            slot.subturn_mode = std::mem::take(&mut self.subturn_mode);
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
            slot.workspace_cwd = self.session_workspace_cwd.take();
        }
        self.last_detached = Some(active);
        self.lockdown_status = None;
        self.lockdown_overlay = false;
        self.msg_queue.clear();
        self.queue_mode = false;
        self.subturn_mode = false;
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

    /// Open a daemon-minted fork child as a NEW session surface and seed its
    /// composer with the editable, unsent draft (the owner's ask: a fork
    /// leaves the original transcript and terminal where they were and opens
    /// a new one).
    ///
    /// The source session is not touched here: [`Self::open_session`] parks
    /// its draft into its own slot and its row, transcript and chips stay on
    /// the roster exactly as they were.
    ///
    /// `reachable` is whether this client actually holds (or is opening) a
    /// control attachment to the child. When it does not — a dead socket
    /// takes no attach — the child is still committed daemon truth, so the
    /// notice names `haider resume <id>` rather than losing it silently.
    ///
    /// Returns `false` when the surface could not move to the child at all;
    /// that notice names the same door.
    pub fn open_forked_session(
        &mut self,
        child: &SessionId,
        draft: &haider_protocol::session_fork::SessionForkDraft,
        reachable: bool,
    ) -> bool {
        self.dirty = true;
        self.upsert_live_session(child);
        self.open_session(child);
        if self.active_session.as_ref() != Some(child) {
            // Never a silent loss: the fork committed, so name the door.
            self.flash = Some(format!(
                "· forked — could not open the new session here · `haider resume {}`",
                child.as_str()
            ));
            return false;
        }
        // The draft is UNSENT: the user edits it and submits when ready. It
        // is seeded even when the child is unreachable — the bytes are this
        // client's, and a later reattach finds them parked and waiting.
        self.composer.set_text(draft.text.clone());
        let mut unrepresentable = 0_usize;
        for block in &draft.attachments {
            if self.composer.attachments().len() >= MAX_TURN_ATTACHMENTS {
                unrepresentable += 1;
                continue;
            }
            self.upload_seq += 1;
            match crate::composer::PendingAttachment::carrying(self.upload_seq, block.clone()) {
                // Already in the CAS and already verified: the chip carries
                // the daemon's exact block, so no upload is issued.
                Some(chip) => self.composer.push_attachment(chip),
                None => unrepresentable += 1,
            }
        }
        let mut note = if reachable {
            "· forked — new session · the prompt is an editable draft".to_owned()
        } else {
            format!(
                "· forked — new session, not attached here · `haider resume {}`",
                child.as_str()
            )
        };
        if unrepresentable > 0 {
            note.push_str(&format!(
                " · {unrepresentable} attachment(s) could not be carried"
            ));
        }
        self.flash = Some(note);
        true
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
        if let Some(metadata) = &summary.metadata {
            let prior = self
                .session_created_at_ms
                .insert(summary.session_id.clone(), metadata.created_at_ms);
            if prior != Some(metadata.created_at_ms) {
                self.dirty = true;
            }
        }
        if let Some(kind) = summary.kind {
            let prior = self.session_kinds.insert(summary.session_id.clone(), kind);
            if prior != Some(kind) {
                self.dirty = true;
            }
        }
        if let Some(last_model) = &summary.last_model {
            let prior = self
                .session_last_models
                .insert(summary.session_id.clone(), last_model.clone());
            if prior.as_ref() != Some(last_model) {
                self.dirty = true;
            }
        }
        if self.daemon_serves(haider_rpc::FEATURE_SESSION_SEEN_V1) {
            let attention = SessionAttention {
                seen_at_ms: summary.seen_at_ms,
                last_activity_ms: self
                    .session_attention
                    .get(&summary.session_id)
                    .and_then(|held| held.last_activity_ms)
                    .into_iter()
                    .chain(summary.last_activity_ms)
                    .max(),
                waiting_why: summary.waiting_why.clone(),
                needs_input: summary.needs_input.clone(),
            };
            if self.session_attention.get(&summary.session_id) != Some(&attention) {
                self.session_attention
                    .insert(summary.session_id.clone(), attention);
                self.dirty = true;
            }
        }
        if let Some(workspace) = &summary.workspace_cwd {
            if self.active_session.as_ref() == Some(&summary.session_id) {
                if self.session_workspace_cwd.as_ref() != Some(workspace) {
                    self.session_workspace_cwd = Some(workspace.clone());
                    self.dirty = true;
                }
            } else if let Some(entry) = self
                .sessions
                .iter_mut()
                .find(|row| row.id == summary.session_id)
                && entry.workspace_cwd.as_ref() != Some(workspace)
            {
                entry.workspace_cwd = Some(workspace.clone());
                self.dirty = true;
            }
        }
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
        if let Some(metrics) = &summary.agent_metrics {
            let replace = self
                .session_metrics
                .get(&summary.session_id)
                .is_none_or(|held| held.head_seq < metrics.head_seq);
            if replace {
                self.session_metrics
                    .insert(summary.session_id.clone(), metrics.clone());
                self.dirty = true;
            }
        }
        let nested_cache = summary
            .agent_metrics
            .as_ref()
            .and_then(|metrics| metrics.usage.as_ref());
        let promoted_present = summary.cache_lifetime_hit_basis_points.is_some()
            || summary.cache_reread_hit_basis_points.is_some();
        let cache_rates = SessionCacheRates {
            head_seq: summary.head_seq,
            lifetime_basis_points: if promoted_present {
                summary.cache_lifetime_hit_basis_points
            } else {
                nested_cache.and_then(|usage| usage.cache_hit_basis_points)
            },
            reread_basis_points: if promoted_present {
                summary.cache_reread_hit_basis_points
            } else {
                nested_cache.and_then(|usage| usage.cache_reread_hit_basis_points)
            },
        };
        let replace_cache = self
            .session_cache_rates
            .get(&summary.session_id)
            .is_none_or(|held| held.head_seq < summary.head_seq);
        if replace_cache {
            self.session_cache_rates
                .insert(summary.session_id.clone(), cache_rates);
            self.dirty = true;
        }
        // Model truth (owner 2026-08-15): the roster wears the model the
        // session ACTUALLY runs — the daemon's journal-folded `last_model` —
        // never this client's own identity (the old seed made every row wear
        // the CLIENT's current model). Absence hydrates nothing (older
        // daemon); the ACTIVE session's identity follows the live
        // ModelSelected lane instead.
        if let Some(last_model) = &summary.last_model
            && self.active_session.as_ref() != Some(&summary.session_id)
            && let Some(entry) = self
                .sessions
                .iter_mut()
                .find(|row| row.id == summary.session_id)
            && entry.model_short != *last_model
        {
            entry.model_short = last_model.clone();
            self.dirty = true;
        }
        // W-flow inline identity: the roster row's bound agent type. Unlike
        // the count fields, ABSENCE from a feature-serving daemon means
        // PLAIN (a clear must propagate to the row), so the field copies
        // whole under the gate; an older daemon never serves the feature
        // and hydrates nothing.
        if self.daemon_serves(haider_rpc::FEATURE_SESSION_AGENT_TYPE_SELECT_V1)
            && let Some(entry) = self
                .sessions
                .iter_mut()
                .find(|row| row.id == summary.session_id)
            && entry.agent_type != summary.agent_type
        {
            entry.agent_type = summary.agent_type.clone();
            self.dirty = true;
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

    /// Freshest direct metrics for one chip: the live parent-journal mirror
    /// wins, with the child `SessionSummary` as cold/older-daemon fallback.
    #[must_use]
    pub fn chip_metrics<'a>(
        &'a self,
        chip: &'a ChipModel,
    ) -> Option<&'a haider_protocol::agent::AgentMetricsSnapshot> {
        let summary = chip
            .child_session
            .as_deref()
            .and_then(|session| self.session_metrics.get(&SessionId::new(session)));
        match (chip.metrics.as_ref(), summary) {
            (Some(live), Some(cold)) if cold.head_seq > live.head_seq => Some(cold),
            (Some(live), _) => Some(live),
            (None, summary) => summary,
        }
    }

    #[must_use]
    pub fn main_agent_metrics(&self) -> Option<&haider_protocol::agent::AgentMetricsSnapshot> {
        self.active_session
            .as_ref()
            .and_then(|session| self.session_metrics.get(session))
    }

    /// The cache-health headline for the active session, sourced from the
    /// promoted roster field (with a pre-promotion-daemon fallback applied
    /// while hydrating [`Self::session_cache_rates`]).
    #[must_use]
    pub fn main_cache_reread_hit_basis_points(&self) -> Option<u32> {
        self.active_session
            .as_ref()
            .and_then(|session| self.session_cache_rates.get(session))
            .and_then(|rates| rates.reread_basis_points)
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
        if self.shells_open && !matches!(hit, Hit::ShellClose(_) | Hit::ShellStatus) {
            return;
        }
        if self.ssh_open {
            return;
        }
        if self.monitors_open
            && !matches!(
                hit,
                Hit::MonitorStatus
                    | Hit::MonitorRow(_)
                    | Hit::MonitorStop(_)
                    | Hit::MonitorPause(_)
                    | Hit::MonitorTrigger(_)
                    | Hit::MonitorEdit(_)
                    | Hit::MonitorCopyId(_)
            )
        {
            return;
        }
        if self.lockdown_overlay {
            self.lockdown_overlay = false;
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
        if self.screen == Screen::Loom
            && self
                .loom_authoring
                .as_ref()
                .is_some_and(|authoring| authoring.pending)
        {
            self.flash = Some("· Loom editor is locked while validation is in flight".to_owned());
            return;
        }
        match hit {
            // M2c: a click on the always-visible graph strip opens the
            // `/graph` telemetry screen — the same effect as the command
            // (fetch status + one-shot graph.inspect, then show the view).
            Hit::RetryRun => {
                self.issue_run_retry();
            }
            Hit::PermissionOpenSettings => {
                self.request_permission_open_settings();
            }
            Hit::PermissionRetry => {
                self.retry_permission();
            }
            Hit::GraphStrip => {
                self.graph_unsupported = false;
                self.requests.push(AppRequest::GraphRefresh);
                self.requests.push(AppRequest::GraphInspectRefresh);
                if self.screen == Screen::Loom {
                    self.switch_surface(Screen::Graph);
                } else {
                    self.screen = Screen::Graph;
                }
            }
            Hit::LockdownStatus => {
                self.lockdown_overlay = true;
                self.lockdown_status = None;
                self.requests.push(AppRequest::LockdownStatus {
                    provider: Some(
                        self.active_lockdown_provider()
                            .unwrap_or(self.identity.provider.as_str())
                            .to_owned(),
                    ),
                });
            }
            Hit::RevealPath(path) if matches!(self.screen, Screen::Session | Screen::Subagent) => {
                self.requests.push(AppRequest::RevealPath { path });
            }
            // Every hit below re-checks its OWNING SURFACE: the map may be
            // one frame stale, so a rect from a screen we have since left
            // must never act (review P1-5 — the law documented above was
            // only honored by the palette/menu hits).
            Hit::AttachSession(id)
                if matches!(self.screen, Screen::Launcher | Screen::Sessions) =>
            {
                self.attach_session_id(&id);
            }
            Hit::LoomNew if self.screen == Screen::Loom => self.seed_loom_authoring(),
            Hit::ExtraRow(which) if self.screen == Screen::Launcher => match which {
                LauncherRow::Aura => self.enter_aura(),
                LauncherRow::Accounts => self.enter_accounts(),
                LauncherRow::Peers => self.peer_command(""),
                LauncherRow::Workflows => self.enter_workflows(),
                LauncherRow::Loom => self.enter_loom(),
                LauncherRow::Sessions => self.enter_sessions(),
            },
            // `/accounts` rows: click = make active for its provider (sim
            // tui.js:3604 onClick useAccount). Value-carrying alias, and
            // NEVER an optimistic flip — select_account only requests.
            Hit::AccountRow(alias) if self.screen == Screen::Accounts => {
                self.select_account(&alias);
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
                    AccountAddKind::DeepSeekApi => {
                        if self.daemon_lists_provider("deepseek") {
                            self.open_login_card("deepseek", None);
                        } else {
                            self.accounts.message =
                                Some(self.stale_daemon_note("DeepSeek accounts"));
                            self.dirty = true;
                        }
                    }
                    AccountAddKind::HaiderCodeApi => {
                        if self.daemon_lists_provider("haider-code") {
                            self.open_login_card("haider-code", None);
                        } else {
                            self.accounts.message =
                                Some(self.stale_daemon_note("Haider Code accounts"));
                            self.dirty = true;
                        }
                    }
                    AccountAddKind::XaiApi => {
                        if self.daemon_lists_provider("xai") {
                            self.open_login_card("xai", None);
                        } else {
                            self.accounts.message = Some(self.stale_daemon_note("xAI accounts"));
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
                    // B6b: device flows ride their own feature bit (shipped
                    // beside kimi-oauth, then shared by grok-oauth), with
                    // the same §4.1 gate as the PKCE pair above.
                    AccountAddKind::KimiOAuth => {
                        if self.daemon_serves(haider_rpc::FEATURE_ACCOUNT_OAUTH_DEVICE_V1) {
                            self.open_oauth_add(kind);
                        } else {
                            self.accounts.message =
                                Some(self.stale_daemon_note("Kimi OAuth sign-in"));
                            self.dirty = true;
                        }
                    }
                    AccountAddKind::GrokOAuth => {
                        if self.daemon_serves(haider_rpc::FEATURE_ACCOUNT_OAUTH_DEVICE_V1) {
                            self.open_oauth_add(kind);
                        } else {
                            self.accounts.message =
                                Some(self.stale_daemon_note("Grok OAuth sign-in"));
                            self.dirty = true;
                        }
                    }
                    // 970: the ACP adapter ships with NO feature bit (the
                    // B6a/Gemini precedent), so `provider.list` truth is its
                    // capability signal — a daemon that does not list the
                    // class would bounce the eventual account obscurely.
                    AccountAddKind::GoogleAntigravity => {
                        if self.daemon_lists_provider(GOOGLE_ANTIGRAVITY_PROVIDER) {
                            self.open_antigravity_add();
                        } else {
                            self.accounts.message =
                                Some(self.stale_daemon_note("Google Antigravity accounts"));
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
                    AccountAddKind::AzureOpenAi => {
                        self.open_azure_card();
                    }
                    AccountAddKind::Bedrock => {
                        self.open_bedrock_card();
                    }
                    AccountAddKind::Vertex => {
                        self.open_vertex_card();
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
            Hit::UsageScope(scope) if self.screen == Screen::Usage => {
                self.usage.scope = scope;
                self.usage.scroll.set(0);
                self.refresh_usage_scope_if_needed();
                self.dirty = true;
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
            Hit::ModelPickerRow {
                provider,
                model,
                api_group,
            } if self.model_picker.is_some() => {
                let in_provider_stage = self
                    .model_picker
                    .as_ref()
                    .is_some_and(|picker| picker.provider_stage.is_some());
                if let Some(row) = self.model_picker_rows().into_iter().find(|row| {
                    row.provider == provider
                        && row.model == model
                        && api_group
                            == (!in_provider_stage && row.auth == "api" && !row.model.is_empty())
                }) {
                    self.activate_model_picker_row(&row);
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
            Hit::ShellStatus => self.shells_command(""),
            Hit::MonitorStatus => self.monitors_command(""),
            Hit::ShellClose(id) if self.shells_open => {
                self.requests.push(AppRequest::ShellClose { id });
                self.flash = Some("· closing shell…".into());
            }
            // 970 owner item 2: the overlay's row actions. Every arm
            // re-checks the overlay flag — local chrome never acts from a
            // hit map drawn for a screen that is no longer showing.
            Hit::MonitorRow(id) if self.monitors_open => {
                if let Some(index) = self
                    .monitors
                    .iter()
                    .position(|monitor| monitor.monitor_id == id)
                {
                    self.monitors_cursor = index;
                }
                self.monitors_stop_armed = None;
            }
            Hit::MonitorStop(id) if self.monitors_open => {
                if self.monitors_stop_armed.as_deref() == Some(id.as_str()) {
                    self.monitors_stop_armed = None;
                    self.monitor_stop(id);
                } else {
                    self.flash = Some(format!("· stop monitor {id}? click stop again to confirm"));
                    self.monitors_stop_armed = Some(id);
                }
            }
            Hit::MonitorPause(id) if self.monitors_open => {
                self.monitors_stop_armed = None;
                self.monitor_toggle_pause(id);
            }
            Hit::MonitorTrigger(id) if self.monitors_open => {
                self.monitors_stop_armed = None;
                self.monitor_trigger(id);
            }
            Hit::MonitorEdit(id) if self.monitors_open => {
                self.monitors_stop_armed = None;
                self.monitor_edit_with_agent(&id);
            }
            Hit::MonitorCopyId(id) if self.monitors_open => {
                self.monitors_stop_armed = None;
                self.monitor_copy_id(&id);
            }
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
            // The collapsed subagents summary row — the fleet's clickable
            // door (⌥F's twin), on the panel's two host screens.
            Hit::FleetSummary
                if matches!(self.screen, Screen::Session | Screen::Subagent)
                    && !self.subtree_collapsed =>
            {
                self.open_fleet();
            }
            // A fleet row/cell: select it, and drill when it has children
            // — the hit carries the agent id it was RENDERED for, so a
            // refreshed snapshot can never drill a different agent.
            Hit::FleetNode(agent) if self.screen == Screen::Fleet => {
                if let Some(index) = self.fleet_index_of(&agent) {
                    self.fleet.sel = index;
                    self.fleet.kill_armed = None;
                    // A click is now an ACTIVATION, not a dead selection:
                    // it re-roots a subtree and opens a leaf's detail frame
                    // exactly as ⏎ does. `sel` was just resolved FROM the
                    // clicked agent id, so the shared drill acts on the row
                    // that was clicked and no other.
                    self.fleet_drill();
                }
            }
            // The detail frame's transcript door, by mouse — the same door
            // ⏎ opens, never a parallel one.
            Hit::FleetTranscript(agent)
                if self.screen == Screen::Fleet && find_chip(&self.chips, &agent).is_some() =>
            {
                self.open_fleet_member_transcript(&agent);
            }
            // A member whose transcript lives on its own session keeps the
            // keyboard door's honest refusal.
            Hit::FleetTranscript(_) if self.screen == Screen::Fleet => {
                self.flash = Some(
                    "· transcript lives on the member's own session — attach to view".to_owned(),
                );
            }
            // Clicking `✕ destroy` ARMS it; the confirming press is what
            // acts, so no single stray click can destroy a subagent.
            Hit::FleetKill(agent)
                if self.screen == Screen::Fleet
                    && self
                        .fleet
                        .detail
                        .as_ref()
                        .map(haider_protocol::ids::AgentId::as_str)
                        == Some(agent.as_str()) =>
            {
                self.fleet_kill_step();
            }
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
            Hit::HookFiring(menu) if self.screen == Screen::Hooks => {
                if self.hook_facts.has_menu(&menu)
                    && let Some(card) = self.hook_facts.menu(&menu).cloned()
                {
                    self.hooks.drilldown = Some(card);
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
                self.note_session_view();
            }
            Hit::JumpToBottom if matches!(self.screen, Screen::Session | Screen::Subagent) => {
                // 954: return to follow; the next FOLLOWING frame stamps
                // the watermark, which is what clears the unseen counter —
                // the reducer never fabricates a "seen" count itself.
                self.scroll_back.set(0);
                self.note_session_view();
            }
            Hit::QueueRowSteer(id) if matches!(self.screen, Screen::Session | Screen::Subagent) => {
                // Fenced: the revision we hold rides the mutation; a stale
                // one comes back as a typed conflict and the panel re-reads.
                if let Some(revision) = self.queue_panel.revision {
                    self.requests
                        .push(AppRequest::QueuePromoteSteer { id, revision });
                    self.dirty = true;
                }
            }
            Hit::QueueRowToggle(id)
                if matches!(self.screen, Screen::Session | Screen::Subagent) =>
            {
                // Leg one of remove+resubmit. The held text and its NEXT
                // mode park in pending_toggle so no crash window between
                // the legs can silently lose the user's words.
                let staged = self
                    .queue_panel
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .map(|row| {
                        let next = match row.mode {
                            haider_protocol::DeliveryMode::Queue => {
                                haider_protocol::DeliveryMode::Subturn
                            }
                            _ => haider_protocol::DeliveryMode::Queue,
                        };
                        (row.text.clone(), next)
                    });
                if let (Some((text, next)), Some(revision)) = (staged, self.queue_panel.revision) {
                    self.queue_panel.pending_toggle = Some((id.clone(), text, next));
                    self.requests
                        .push(AppRequest::QueueToggleRemove { id, revision });
                    self.dirty = true;
                }
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
        // The all-sessions browser scrolls under the wheel: it is a long
        // list by construction (every session on the machine), so the wheel
        // is the expected gesture and paging by keyboard alone is not enough.
        if self.screen == Screen::Sessions {
            let last = self.session_browser_rows().len().saturating_sub(1);
            self.session_browser_sel = if up {
                self.session_browser_sel.saturating_sub(3)
            } else {
                (self.session_browser_sel + 3).min(last)
            };
            self.dirty = true;
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
        // Fleet: the wheel walks the selection (selection-follow is the
        // view's only scroll authority in slice 1).
        if self.screen == Screen::Fleet {
            self.fleet_move(if up { -3 } else { 3 });
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
        self.note_session_view();
    }

    /// One drag-autoscroll step (QoL wave): a held transcript selection
    /// dragged to the viewport's edge keeps scrolling — one line per drag
    /// event (crossterm reports Drag only on movement; no timer exists
    /// for an edge HOLD, by design), reconcile-then-apply clamped exactly
    /// like the wheel. The selection itself is untouched: it is
    /// screen-space ([`crate::select`]), so the anchor CELL stays parked
    /// while the content slides beneath it and the copy-on-release reads
    /// the final frame.
    pub fn drag_autoscroll(&mut self, up: bool) {
        if !matches!(self.screen, Screen::Session | Screen::Subagent) {
            return;
        }
        self.dirty = true;
        // A drag scroll is a real scroll (the handle_wheel law).
        self.sticky_suppressed.set(false);
        let max = self.scroll_max.get();
        let current = self.scroll_back.get().min(max);
        let next = if up {
            current.saturating_add(1).min(max)
        } else {
            current.saturating_sub(1)
        };
        self.scroll_back.set(next);
        self.note_session_view();
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

    pub fn handle_terminal_resize(&mut self, cols: u16, rows: u16) {
        self.handle_resize();
        let size = haider_rpc::SshPtySizeWire {
            cols: u32::from(cols.max(1)),
            // The SSH pane uses the full body width. Reserve one row for the
            // global status strip and one for the pane's own title so the
            // remote PTY receives its drawable rows rather than the outer
            // terminal height.
            rows: u32::from(rows.saturating_sub(2).max(1)),
            pixel_width: 0,
            pixel_height: 0,
        };
        self.ssh_terminal_size = size;
        if let Some(terminal) = self.ssh_terminal.as_mut() {
            terminal.size = size;
            if let Some(id) = terminal.shell_id.clone() {
                self.requests.push(AppRequest::SshShellResize { id, size });
            }
        }
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

    /// The daemon's exact model × provider pairs in `provider.list` order.
    /// This is the lossless source for both picker stages; presentation-only
    /// grouping never changes the request authority below.
    fn model_picker_pair_rows(&self) -> Vec<ModelPickerRow> {
        use haider_protocol::credential::AuthMethod;
        let mut rows = Vec::new();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
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
            let inventory_age_ms = now_ms
                .zip(summary.inventory_fetched_at_ms)
                .map(|(now, fetched_at)| now.saturating_sub(fetched_at));
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
                    // An unauthenticated endpoint is still API-side for the
                    // picker's binary metering decision: only OAuth is a
                    // distinct paid subscription that must remain exact.
                    [] => "api",
                    [AuthMethod::OAuth] => "oauth",
                    _ => "api",
                }
            };
            if summary.models.is_empty() {
                rows.push(ModelPickerRow {
                    provider: summary.provider.clone(),
                    providers: vec![summary.provider.clone()],
                    available_providers: 0,
                    lockdown_providers: usize::from(!matches!(
                        summary.trust,
                        haider_rpc::ProviderTrustWire::Full
                    )),
                    default_providers: 0,
                    current_provider: None,
                    context_window_varies: false,
                    lockdown: !matches!(summary.trust, haider_rpc::ProviderTrustWire::Full),
                    model: String::new(),
                    auth,
                    context_window: None,
                    inventory_age_ms,
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
                    providers: vec![summary.provider.clone()],
                    available_providers: usize::from(available),
                    lockdown_providers: usize::from(!matches!(
                        summary.trust,
                        haider_rpc::ProviderTrustWire::Full
                    )),
                    default_providers: usize::from(summary.default_model.as_deref() == Some(model)),
                    current_provider: (self.identity.provider == summary.provider
                        && self.identity.model_short == *model)
                        .then(|| summary.provider.clone()),
                    context_window_varies: false,
                    lockdown: !matches!(summary.trust, haider_rpc::ProviderTrustWire::Full),
                    model: model.clone(),
                    auth,
                    context_window: self.providers.declared_window(&summary.provider, model),
                    inventory_age_ms,
                    available,
                    reason: reason.clone(),
                    is_default: summary.default_model.as_deref() == Some(model),
                    is_current: self.identity.provider == summary.provider
                        && self.identity.model_short == *model,
                    selectable: true,
                });
            }
            // A session may legitimately run a caller-configured passthrough
            // id that a custom server omitted from its advisory catalog. Keep
            // the current pair visible and typed as unlisted, but do not add
            // it to the discovered inventory or pretend it is an available
            // picker row.
            if self.identity.provider == summary.provider
                && matches!(
                    summary.inventory_authority,
                    haider_rpc::ModelInventoryAuthorityWire::Advisory
                )
                && matches!(
                    summary.model_inventory_status(&self.identity.model_short),
                    haider_rpc::ModelInventoryStatusWire::Unlisted
                )
            {
                rows.push(ModelPickerRow {
                    provider: summary.provider.clone(),
                    providers: vec![summary.provider.clone()],
                    available_providers: 0,
                    lockdown_providers: usize::from(!matches!(
                        summary.trust,
                        haider_rpc::ProviderTrustWire::Full
                    )),
                    default_providers: 0,
                    current_provider: Some(summary.provider.clone()),
                    context_window_varies: false,
                    lockdown: !matches!(summary.trust, haider_rpc::ProviderTrustWire::Full),
                    model: self.identity.model_short.clone(),
                    auth,
                    context_window: None,
                    inventory_age_ms,
                    available: false,
                    reason: Some("unlisted by advisory provider catalog".to_owned()),
                    is_default: false,
                    is_current: true,
                    selectable: false,
                });
            }
        }
        rows
    }

    /// Rows visible in the picker's current stage. The top level preserves
    /// every OAuth pair and every provider placeholder in source order, but
    /// emits each API model slug once at its first pair's position. The
    /// provider stage returns the exact API pairs for its chosen slug.
    #[must_use]
    pub fn model_picker_rows(&self) -> Vec<ModelPickerRow> {
        let pair_rows = self.model_picker_pair_rows();
        if let Some(stage) = self
            .model_picker
            .as_ref()
            .and_then(|picker| picker.provider_stage.as_ref())
        {
            return pair_rows
                .into_iter()
                .filter(|row| row.auth == "api" && row.model == stage.model)
                .collect();
        }

        enum TopEntry {
            Exact(ModelPickerRow),
            ApiGroup(usize),
        }

        let mut entries = Vec::new();
        let mut groups: Vec<Vec<ModelPickerRow>> = Vec::new();
        for row in pair_rows {
            if row.auth != "api" || row.model.is_empty() {
                entries.push(TopEntry::Exact(row));
                continue;
            }
            if let Some(index) = groups
                .iter()
                .position(|group| group.first().is_some_and(|first| first.model == row.model))
            {
                groups[index].push(row);
            } else {
                let index = groups.len();
                groups.push(vec![row]);
                entries.push(TopEntry::ApiGroup(index));
            }
        }

        entries
            .into_iter()
            .filter_map(|entry| match entry {
                TopEntry::Exact(row) => Some(row),
                TopEntry::ApiGroup(index) => Self::collapse_api_group(&groups[index]),
            })
            .collect()
    }

    fn collapse_api_group(group: &[ModelPickerRow]) -> Option<ModelPickerRow> {
        let first = group.first()?;
        let ready = |row: &&ModelPickerRow| row.available && row.selectable;
        let available_providers = group.iter().filter(ready).count();
        let fact_rows: Vec<&ModelPickerRow> = if available_providers > 0 {
            group.iter().filter(ready).collect()
        } else {
            group.iter().collect()
        };
        let first_context_window = fact_rows.first().and_then(|row| row.context_window);
        let context_window_varies = fact_rows
            .iter()
            .any(|row| row.context_window != first_context_window);
        let context_window = if context_window_varies {
            None
        } else {
            first_context_window
        };
        let inventory_age_ms = fact_rows
            .iter()
            .filter_map(|row| row.inventory_age_ms)
            .min();
        let reason = if available_providers == 0 {
            if group.len() == 1 {
                first
                    .reason
                    .clone()
                    .or_else(|| Some("provider unavailable".to_owned()))
            } else {
                Some(format!(
                    "all {} API providers unavailable — {}",
                    group.len(),
                    group
                        .iter()
                        .map(|row| format!(
                            "{}: {}",
                            row.provider,
                            row.reason.as_deref().unwrap_or("provider unavailable")
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ))
            }
        } else {
            None
        };
        let lockdown_providers = group.iter().filter(|row| row.lockdown).count();
        let default_providers = group.iter().filter(|row| row.is_default).count();
        let current_provider = group
            .iter()
            .find(|row| row.is_current)
            .map(|row| row.provider.clone());

        Some(ModelPickerRow {
            provider: first.provider.clone(),
            providers: group.iter().map(|row| row.provider.clone()).collect(),
            available_providers,
            lockdown_providers,
            default_providers,
            current_provider,
            context_window_varies,
            lockdown: lockdown_providers == group.len(),
            model: first.model.clone(),
            auth: "api",
            context_window,
            inventory_age_ms,
            available: available_providers > 0,
            reason,
            is_default: default_providers > 0,
            is_current: group.iter().any(|row| row.is_current),
            selectable: group.iter().any(|row| row.selectable),
        })
    }

    /// The picker's LIVE search: case-insensitive; every whitespace-
    /// separated token must substring-match the row's model + every provider
    /// represented by it (+ auth flavor) haystack. Grouping happens before
    /// filtering, so a provider-name query never changes aggregate facts.
    #[must_use]
    pub fn model_picker_filtered(&self, query: &str) -> Vec<ModelPickerRow> {
        let needle = query.to_ascii_lowercase();
        let tokens: Vec<&str> = needle.split_whitespace().collect();
        self.model_picker_rows()
            .into_iter()
            .filter(|row| {
                let haystack = format!("{} {} {}", row.model, row.providers.join(" "), row.auth)
                    .to_ascii_lowercase();
                tokens.iter().all(|token| haystack.contains(token))
            })
            .collect()
    }

    /// KEY-OWNERSHIP LAW (F2a, heeded history): while the picker is open
    /// it owns every key — ⏎ selects the HIGHLIGHTED row (never an
    /// exact-match jump), esc backs out one stage before closing WITHOUT
    /// selecting, characters edit the search, ↑/↓ move the highlight
    /// (wrapping).
    fn handle_model_picker_key(&mut self, code: KeyCode) {
        self.dirty = true;
        match code {
            KeyCode::Esc => {
                let Some(picker) = self.model_picker.as_mut() else {
                    return;
                };
                if let Some(stage) = picker.provider_stage.take() {
                    picker.query = stage.parent_query;
                    picker.selection = stage.parent_selection;
                    picker.scroll.set(stage.parent_scroll);
                    picker.error = None;
                } else {
                    // Top-level esc closes WITHOUT selecting — nothing else
                    // moves. A provider-stage esc only returned here first.
                    self.model_picker = None;
                }
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
                    self.activate_model_picker_row(&row);
                }
            }
            KeyCode::Tab => {
                let Some((query, selection, pending)) = self.model_picker.as_ref().map(|picker| {
                    (
                        picker.query.clone(),
                        picker.selection,
                        picker.pending.is_some(),
                    )
                }) else {
                    return;
                };
                if pending {
                    return;
                }
                if let Some(row) = self.model_picker_filtered(&query).get(selection).cloned() {
                    let is_top_api_choice = self
                        .model_picker
                        .as_ref()
                        .is_some_and(|picker| picker.provider_stage.is_none())
                        && row.auth == "api"
                        && !row.model.is_empty();
                    if is_top_api_choice {
                        self.activate_model_picker_row(&row);
                    } else {
                        self.toggle_provider_trust(&row.provider);
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.query.pop();
                    picker.selection = 0;
                    picker.scroll.set(0);
                    picker.error = None;
                }
            }
            KeyCode::Char(c) => {
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.query.push(c);
                    picker.selection = 0;
                    picker.scroll.set(0);
                    picker.error = None;
                }
            }
            _ => {}
        }
    }

    /// Act on the highlighted visible row without ever sending an aggregate
    /// provider identity to the existing pair-selection authority.
    fn activate_model_picker_row(&mut self, row: &ModelPickerRow) {
        let is_top_api = self
            .model_picker
            .as_ref()
            .is_some_and(|picker| picker.provider_stage.is_none())
            && row.auth == "api"
            && !row.model.is_empty();
        if !is_top_api {
            self.select_model_row(row);
            return;
        }
        if !row.available || !row.selectable {
            // An all-unavailable group refuses with its aggregate,
            // provider-qualified reason instead of opening a useless stage.
            let reason = row
                .reason
                .clone()
                .unwrap_or_else(|| "API providers unavailable".to_owned());
            if let Some(picker) = self.model_picker.as_mut() {
                picker.error = Some(format!("{} — {reason}", row.model));
            }
            return;
        }
        self.enter_model_provider_stage(&row.model);
    }

    fn enter_model_provider_stage(&mut self, model: &str) {
        let Some(picker) = self.model_picker.as_mut() else {
            return;
        };
        if picker.provider_stage.is_some() {
            return;
        }
        picker.provider_stage = Some(ModelProviderStage {
            model: model.to_owned(),
            parent_query: std::mem::take(&mut picker.query),
            parent_selection: picker.selection,
            parent_scroll: picker.scroll.get(),
        });
        picker.selection = 0;
        picker.scroll.set(0);
        picker.error = None;
    }

    fn toggle_provider_trust(&mut self, provider_name: &str) {
        let Some(summary) = self
            .providers
            .providers
            .iter()
            .find(|summary| summary.provider == provider_name)
            .cloned()
        else {
            return;
        };
        // An unknown future value is never interpreted as Full authority;
        // the first toggle normalizes it to today's Lockdown value.
        let trust = if matches!(summary.trust, haider_rpc::ProviderTrustWire::Lockdown) {
            haider_rpc::ProviderTrustWire::Full
        } else {
            haider_rpc::ProviderTrustWire::Lockdown
        };
        if self.mode.fabricates_locally() {
            if let Some(provider) = self
                .providers
                .providers
                .iter_mut()
                .find(|provider| provider.provider == summary.provider)
            {
                provider.trust = trust;
            }
            self.providers.message = Some(format!("{} trust toggled (demo)", summary.provider));
        } else if self.daemon_serves(haider_rpc::FEATURE_PROVIDER_LOCKDOWN_V1) {
            self.providers.message = Some(format!("changing {} trust…", summary.provider));
            self.requests.push(AppRequest::ProviderSetTrust {
                provider: summary.provider,
                trust,
                expected_revision: self.providers.revision.unwrap_or(0),
            });
        } else {
            self.providers.message = Some(self.stale_daemon_note("provider lockdown"));
        }
        self.dirty = true;
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
        // sets the default pair used by the next CreateSession. W-flow: the
        // loom authoring input's ⌥m hop selects for the BOUND session too —
        // the receipted select is the authoring model choice.
        let live_session = (!self.mode.fabricates_locally()
            && (self.screen == Screen::Session || self.screen == Screen::Loom))
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
        let change = PendingCacheChange::Model {
            session: session.clone(),
            provider: row.provider.clone(),
            model: row.model.clone(),
        };
        let confirm_new_epoch = self.pending_cache_change.as_ref() == Some(&change);
        self.requests.push(AppRequest::SelectModel {
            session,
            model: row.model.clone(),
            provider: row.provider.clone(),
            confirm_new_epoch,
        });
    }

    /// `/provider <name>`. At the LAUNCHER this pins the default pair the
    /// next session is created with — client-owned view state, and the same
    /// ownership flip `/model` carries. In an ATTACHED session the provider
    /// is DAEMON TRUTH, so it has to be committed by the receipted select:
    /// assigning `identity.provider` locally and flashing success announced a
    /// switch the daemon never heard, and the next turn still ran on the old
    /// provider.
    fn select_provider(&mut self, name: String, health: String) {
        self.dirty = true;
        let live_session = (!self.mode.fabricates_locally()
            && (self.screen == Screen::Session || self.screen == Screen::Loom))
            .then(|| self.active_session.clone())
            .flatten();
        let Some(session) = live_session else {
            self.identity.provider = name.clone();
            self.refresh_context_window();
            self.identity_pinned = true;
            self.flash = Some(format!("· provider → {name} · {health}"));
            return;
        };
        if !self.daemon_serves(haider_rpc::FEATURE_SESSION_MODEL_SELECT_V1) {
            self.flash = Some(self.stale_daemon_note("cross-provider model selection"));
            return;
        }
        // A provider is committed by selecting a MODEL on it. The daemon
        // publishes each provider's default; when it declares NONE we must
        // not invent one. Reaching for the first catalog entry would commit a
        // choice the daemon never sanctioned — the same lie this fix removes,
        // relocated one line down.
        let Some(model) = self
            .providers
            .providers
            .iter()
            .find(|summary| summary.provider.eq_ignore_ascii_case(&name))
            .and_then(|summary| summary.default_model.clone())
        else {
            self.flash = Some(format!(
                "· {name} declares no default model — /model to choose one"
            ));
            return;
        };
        let change = PendingCacheChange::Model {
            session: session.clone(),
            provider: name.clone(),
            model: model.clone(),
        };
        let confirm_new_epoch = self.pending_cache_change.as_ref() == Some(&change);
        // IN FLIGHT, not done. The resolved pair is rendered by
        // `apply_model_selected` from daemon truth; claiming the switch here
        // would be the original defect with a longer message.
        self.flash = Some(format!(
            "· provider → {name} · {health} · selecting {model}…"
        ));
        self.requests.push(AppRequest::SelectModel {
            session,
            model,
            provider: name,
            confirm_new_epoch,
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
        self.pending_cache_change = None;
        // Model retention: a COMMITTED pick is what the next boot opens on.
        self.model_commits += 1;
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

    /// [`Self::record_session_error`] with the failure's TYPED presentation
    /// (E8 visual pass): the transcript row gets the card-shaped err
    /// treatment — bold title, railed dim detail, muted fact line — instead
    /// of the baseline one-line ✗.
    pub(crate) fn record_session_error_card(
        &mut self,
        session: &SessionId,
        presentation: haider_protocol::error::ErrorPresentation,
    ) {
        if self.active_session.as_ref() == Some(session) {
            self.projection.record_local_error_card(presentation);
        } else if let Some(entry) = self.sessions.iter_mut().find(|entry| &entry.id == session) {
            entry.projection.record_local_error_card(presentation);
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
        } else if ladder.contains(&requested) {
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
        let change = PendingCacheChange::Fast {
            session: session.clone(),
            enabled,
        };
        let confirm_new_epoch = self.pending_cache_change.as_ref() == Some(&change);
        self.requests.push(AppRequest::SelectFast {
            session,
            enabled,
            confirm_new_epoch,
        });
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
        let change = PendingCacheChange::Effort {
            session: session.clone(),
            effort: effort.clone(),
        };
        let confirm_new_epoch = self.pending_cache_change.as_ref() == Some(&change);
        self.requests.push(AppRequest::SelectEffort {
            session,
            effort,
            confirm_new_epoch,
        });
    }

    /// The RESOLVED effort committed (G3/R2): render daemon truth. On
    /// anthropic pairs the flash notes the prompt-cache re-warm (decision
    /// 5 — changing effort invalidates the prompt cache).
    pub fn apply_effort_selected(&mut self, effort: Option<&str>) {
        self.identity.reasoning = effort.map(str::to_owned);
        self.effort_picker = None;
        self.pending_cache_change = None;
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
        self.pending_cache_change = None;
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

    /// The daemon preflighted a warmed cache epoch. Nothing changed; the
    /// exact selection is retained so repeating it is an explicit confirm.
    pub fn cache_epoch_confirmation_required(&mut self, change: PendingCacheChange, message: &str) {
        self.pending_cache_change = Some(change);
        let notice = format!("{message} · repeat this selection to create the new epoch");
        match &mut self.pending_cache_change {
            Some(PendingCacheChange::Model { .. }) => {
                if let Some(picker) = self.model_picker.as_mut() {
                    picker.pending = None;
                    picker.error = Some(notice.clone());
                }
            }
            Some(PendingCacheChange::Effort { .. }) => {
                if let Some(picker) = self.effort_picker.as_mut() {
                    picker.pending = None;
                    picker.error = Some(notice.clone());
                }
            }
            Some(PendingCacheChange::Account { .. }) => {
                self.accounts.pending_select = None;
                self.accounts.message = Some(notice.clone());
            }
            Some(PendingCacheChange::Fast { .. }) | None => {}
        }
        self.projection.push_note(format!("· {notice}"));
        self.flash = Some("· cache epoch change needs confirmation".to_owned());
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
            "core     fs_read fs_edit process_exec agent_spawn request_input … (13, always on)"
                .to_owned(),
            "custom   notify_slack (fire-and-forget) · preview_deploy (await) · preview_smoke (deferred)"
                .to_owned(),
            "dispatch each custom tool declares a mode: how the turn treats its result".to_owned(),
            "register adding a tool is itself a menu-answerable action — a local agent can provision another"
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

#[cfg(test)]
#[path = "custom_provider_tests.rs"]
mod custom_provider_tests;

#[cfg(test)]
#[path = "ssh_shell_registry_tests.rs"]
mod ssh_shell_registry_tests;

#[cfg(test)]
#[allow(clippy::expect_used)]
mod e3_recovery_tests {
    use super::*;
    use haider_protocol::error::{ErrorPresentation, ErrorScope};
    use haider_protocol::ids::CredentialAlias;
    use haider_protocol::menu::ErrorRecoveryCardKind;

    /// LAW E3b: only the committed menu answer dispatches the existing OAuth
    /// start operation. MUTATION: removing `recovery_card_answered` from the
    /// committed-event seam leaves the request queue empty.
    #[test]
    fn e3b_oauth_expired_committed_action_dispatches_oauth_start() {
        let mut model = AppModel::default();
        let menu = Menu {
            id: MenuId::new("oauth-recovery-law"),
            kind: MenuKind::ErrorRecovery {
                card: ErrorRecoveryCardKind::OauthExpired,
                presentation: ErrorPresentation::new(
                    "oauth-expired",
                    "Sign-in expired",
                    "Sign in again.",
                    ErrorScope::Account,
                    [ErrorAction::Relogin],
                ),
                option_actions: vec![ErrorAction::Relogin],
                provider: Some("openai-oauth".into()),
                account: Some(CredentialAlias::new("openai-oauth")),
                source_run: None,
                source_item: None,
            },
            title: "Sign-in expired".into(),
            body: vec!["Sign in again.".into()],
            options: vec![MenuOption {
                key: "relogin".into(),
                label: "Re-login".into(),
                detail: None,
                decision: None,
            }],
            blocking: false,
            scope: MenuScope::Session,
            origin: "error-recovery".into(),
            ttl_ms: None,
            timeout_option: None,
        };
        model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuOpened(
            menu.clone(),
        ))));
        assert!(
            !model
                .requests
                .iter()
                .any(|request| matches!(request, AppRequest::OAuthAddStart { .. }))
        );
        model.handle(AppEvent::Envelope(Box::new(EventPayload::MenuAnswered(
            MenuAnswer {
                menu: menu.id,
                option_key: Some("relogin".into()),
                option_index: 0,
                value: None,
                via: AnswerVia::Tui,
            },
        ))));
        assert!(model.requests.iter().any(|request| matches!(
            request,
            AppRequest::OAuthAddStart { provider, alias, .. }
                if provider == "openai-oauth" && alias == "openai-oauth"
        )));
    }
}
