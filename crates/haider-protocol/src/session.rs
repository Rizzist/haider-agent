//! Durable, typed session configuration.

use serde::{Deserialize, Serialize};

/// The immutable output pair fixed for a logical run. Headless admission and
/// native worker pickup write this same fact before any provider resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutputPairPinnedV1 {
    pub provider: String,
    pub model: String,
    pub max_tokens: u64,
    pub source: crate::output_budget::SessionOutputBudgetSourceV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_route: Option<RunOutputProviderRouteV1>,
    /// Complete boundary, including an explicitly absent selector. Missing only
    /// in older journals, which must be reconstructed from historical metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_snapshot: Option<RunOutputRouteSnapshotV1>,
}

/// Fail closed when an older journal cannot prove a run's original route.
/// The stable error code remains decodable by older schema-33 daemons.
pub fn run_route_history_unavailable(detail: &str) -> crate::error::HaiderError {
    crate::error::HaiderError::new(
        crate::error::ErrorCode::StoreCorrupt,
        format!("run_route_history_unavailable: {detail}"),
        false,
    )
    .with_presentation(crate::error::ErrorPresentation::new(
        "run-route-history-unavailable",
        "Run route history unavailable",
        detail,
        crate::error::ErrorScope::Session,
        [],
    ))
}

/// Complete route provenance owned by a run. An absent account remains absent;
/// resolved aliases describe observations and never pin the default account pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutputRouteSnapshotV1 {
    pub rebind_id: Option<String>,
    pub base_url: Option<String>,
    pub account: Option<String>,
    pub resolved_alias: Option<String>,
    pub resolved_seen: bool,
    pub selection_epoch: Option<u64>,
    pub reset_epoch: Option<u64>,
}

impl RunOutputRouteSnapshotV1 {
    pub const RESOLVED_KIND: &str = "run_output_route_snapshot_v1";

    #[must_use]
    pub fn capture(metadata: &SessionMetadataV1) -> Self {
        Self {
            rebind_id: metadata.provider_rebind_id.clone(),
            base_url: metadata.provider_base_url.clone(),
            account: metadata.account_alias.clone(),
            resolved_alias: metadata.resolved_route_alias.clone(),
            resolved_seen: metadata.resolved_route_seen,
            selection_epoch: metadata.selection_epoch,
            reset_epoch: metadata.route_reset_epoch,
        }
    }

    pub fn restore(&self, metadata: &mut SessionMetadataV1) {
        metadata.provider_rebind_id.clone_from(&self.rebind_id);
        metadata.provider_base_url.clone_from(&self.base_url);
        metadata.account_alias.clone_from(&self.account);
        metadata
            .resolved_route_alias
            .clone_from(&self.resolved_alias);
        metadata.resolved_route_seen = self.resolved_seen;
        metadata.selection_epoch = self.selection_epoch;
        metadata.route_reset_epoch = self.reset_epoch;
    }

    /// An automatic cross-provider switch intentionally releases selectors,
    /// but retains the run's own selection provenance.
    pub fn clear_selectors(&mut self) {
        self.rebind_id = None;
        self.base_url = None;
        self.account = None;
        self.resolved_alias = None;
        self.resolved_seen = false;
    }
}

impl From<RunOutputProviderRouteV1> for RunOutputRouteSnapshotV1 {
    fn from(route: RunOutputProviderRouteV1) -> Self {
        Self {
            rebind_id: Some(route.rebind_id),
            base_url: route.base_url,
            account: route.account,
            resolved_alias: route.resolved_alias,
            resolved_seen: route.resolved_seen,
            selection_epoch: route.selection_epoch,
            reset_epoch: route.reset_epoch,
        }
    }
}

/// Compatibility projection of an explicit rebind for older readers. New pins
/// also carry `route_snapshot`, which covers account-only and unselected routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutputProviderRouteV1 {
    pub rebind_id: String,
    pub base_url: Option<String>,
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_alias: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolved_seen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_epoch: Option<u64>,
}

impl RunOutputProviderRouteV1 {
    pub const RESOLVED_KIND: &str = "run_output_route_resolved_v1";

    pub fn capture(metadata: &SessionMetadataV1) -> Option<Self> {
        metadata.provider_rebind_id.as_ref().map(|rebind_id| Self {
            rebind_id: rebind_id.clone(),
            base_url: metadata.provider_base_url.clone(),
            account: metadata.account_alias.clone(),
            resolved_alias: metadata.resolved_route_alias.clone(),
            resolved_seen: metadata.resolved_route_seen,
            selection_epoch: metadata.selection_epoch,
            reset_epoch: metadata.route_reset_epoch,
        })
    }
}

impl RunOutputPairPinnedV1 {
    pub const KIND: &str = "run_output_pair_pinned_v1";

    pub fn to_event(
        &self,
        run_id: &crate::ids::RunId,
    ) -> Result<crate::EventPayload, serde_json::Error> {
        Ok(crate::EventPayload::Item(
            crate::item::ItemEvent::Completed {
                item_id: crate::ids::ItemId::new(format!("output-pair-{run_id}")),
                item: crate::item::TurnItem::Extension {
                    kind: Self::KIND.into(),
                    data: serde_json::to_value(self)?,
                },
            },
        ))
    }
}

/// Whether a session may stop for a human interaction.
///
/// The default is deliberately interactive so legacy metadata and every UI
/// session preserve their existing behavior. Non-interactive callers must
/// opt into `Autonomous` explicitly at durable session creation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionInteractionModeV1 {
    #[default]
    Interactive,
    Autonomous,
}

impl SessionInteractionModeV1 {
    #[must_use]
    pub const fn is_interactive(&self) -> bool {
        matches!(self, Self::Interactive)
    }
}

/// Optional, durable permission policy selected when a session is created by
/// a non-interactive client.
///
/// These are ordinary policy overrides, not evidence that a human typed or
/// approved a particular effect. The daemon therefore applies them as
/// `AuthorizationVerdict::Allow`, never as `PreAuthorized(UserTyped)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPermissionOverridesV1 {
    /// Explicitly deny model-initiated filesystem writes and every execution
    /// class that could write the workspace indirectly (local/remote process,
    /// Git, desktop control, and writable-peer operations). This is the durable
    /// representation of `haider run --read-only`; it takes precedence over
    /// every allow field below. Automatic hooks are suppressed separately by
    /// the daemon while this session policy is active.
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    /// Allow model-initiated filesystem writes and patches without a menu.
    #[serde(default)]
    pub allow_writes: bool,
    /// Allow model-initiated process execution without a menu.
    #[serde(default)]
    pub allow_exec: bool,
    /// Allow access to the explicitly activated Android transport without a
    /// permission menu. This grants only SMS read, mobile observation, and
    /// mobile control; it does not lift any desktop, filesystem, process, or
    /// network permission.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_mobile: bool,
    /// Auto-allow mode (the Codex `--full-auto` analogue): resolve EVERY
    /// effect class the model can reach to `Allow` for the session, not just
    /// writes/exec — computer control, web fetch, task-kill, and any future
    /// class included. It is a policy default flip, never a `PreAuthorized`
    /// credential and never a suppression of the deny path: an explicit deny
    /// rule still wins (the broker checks the denylist first), every effect is
    /// still journaled, and the macOS TCC gate still applies to computer
    /// actions (auto-allow only lifts Haider's own menu; autonomous sessions
    /// fail a missing OS grant closed), and the "controlling your screen"
    /// banner still shows.
    ///
    /// Omitted from the wire while `false` so a pre-auto-allow overrides value
    /// keeps its exact historical bytes; the field only appears when enabled.
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto_allow: bool,
}

impl SessionPermissionOverridesV1 {
    /// Whether this value grants no permissions and is equivalent to absence.
    #[must_use]
    pub fn is_empty(self) -> bool {
        !self.read_only
            && !self.allow_writes
            && !self.allow_exec
            && !self.allow_mobile
            && !self.auto_allow
    }
}

/// Sanitised launch-origin path vocabulary
/// (`docs/design/dated-workspace-v1.md` §4, O6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchOriginPathKindV1 {
    /// The client's own home or a descendant, shown as `~`/`~/suffix`.
    HomeRelative,
    /// An absolute path outside any recognised home form.
    Absolute,
    /// Another user's home form with the user component masked.
    Redacted,
    /// The launch directory could not be captured.
    Unavailable,
}

/// Upper bound on a serialized origin display (UTF-8 bytes). Ingress
/// revalidation rejects anything longer without corrupting the session.
pub const LAUNCH_ORIGIN_DISPLAY_MAX_BYTES: usize = 4096;

/// One sanitised origin path. `display` is optional only for
/// [`LaunchOriginPathKindV1::Unavailable`]. The value is display data —
/// never a filesystem argument and never daemon proof a path exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchOriginPathV1 {
    pub kind: LaunchOriginPathKindV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

impl LaunchOriginPathV1 {
    /// Ingress revalidation (O6): bounded display, no raw control/newline
    /// bytes, and `display` present exactly when the kind requires it.
    pub fn validate(&self) -> Result<(), String> {
        match (&self.kind, &self.display) {
            (LaunchOriginPathKindV1::Unavailable, _) => {}
            (_, None) => return Err("origin display is required for this kind".to_string()),
            (_, Some(_)) => {}
        }
        if let Some(display) = &self.display {
            if display.len() > LAUNCH_ORIGIN_DISPLAY_MAX_BYTES {
                return Err(format!(
                    "origin display is {} bytes; limit is {LAUNCH_ORIGIN_DISPLAY_MAX_BYTES}",
                    display.len()
                ));
            }
            if display.chars().any(|character| {
                character.is_control()
                    || matches!(
                        character as u32,
                        0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069
                    )
            }) {
                return Err("origin display contains unescaped control text".to_string());
            }
        }
        Ok(())
    }
}

/// The CURRENT launch-origin snapshot for one session (O5): the latest
/// successful foreground registration, stored as the typed-metadata
/// projection and returned by `session.attach`. Client-reported context
/// only — never workspace authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchOriginV1 {
    /// The session this origin belongs to; copied parent history can never
    /// supply a child's origin.
    pub subject_session_id: String,
    /// Client-minted open identity (first foreground activation in one
    /// local TUI process).
    pub open_id: String,
    /// Monotonic registration revision, starting at 1. CAS token for
    /// replacement; `expected_revision = 0` means absence.
    pub revision: u64,
    pub path: LaunchOriginPathV1,
    /// Daemon commit time, Unix milliseconds (never client clocks).
    pub recorded_at_ms: u64,
    /// Journal sequence of the accepted registration event.
    pub selected_seq: u64,
}

/// Optional origin registration riding `session.attach` (O1–O3). Absent
/// field bytes are identical to a pre-feature attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchOriginRegistrationV1 {
    /// Receipt identity: an identical retry replays the original result.
    pub command_id: String,
    pub open_id: String,
    pub worker_generation: u64,
    /// Current origin revision this registration replaces (0 = absence).
    /// A stale value is a conflict, never latest-timestamp-wins.
    pub expected_revision: u64,
    pub path: LaunchOriginPathV1,
    /// Whether the resolved workspace path existed on disk at registration
    /// time (L6): `Some(false)` records a resolved-but-unmaterialised
    /// dated workspace without creating it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_materialized: Option<bool>,
}

/// Additive replay fact appended atomically with a committed origin
/// registration, its metadata projection update, and its receipt (O4).
/// Raw history keeps every accepted registration; "replaced" refers only
/// to the current UI/model-context slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLaunchOriginSelected {
    pub subject_session_id: String,
    pub open_id: String,
    pub revision: u64,
    pub path: LaunchOriginPathV1,
    pub recorded_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_materialized: Option<bool>,
}

/// Dated-workspace allocation facts persisted beside session metadata
/// (`docs/design/dated-workspace-v1.md` §2–3). Legacy metadata omits it;
/// recording these facts never creates the paths they name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceAllocationV1 {
    /// Canonical `<base>/Haider/<hijri-date>` organizing root.
    pub daily_root: String,
    /// Canonical session leaf (the tool workspace).
    pub leaf: String,
    /// 128-bit lowercase-hex allocation id (distinct from the session id).
    pub allocation_id: String,
    /// Calendar identifier (`islamic-civil` in v1).
    pub calendar_id: String,
    /// islamic-civil `YYYY-MM-DD` label used for the daily root.
    pub hijri_date: String,
    /// Sampled Gregorian `YYYY-MM-DD` civil date.
    pub gregorian_date: String,
    /// UTC allocation instant, Unix milliseconds.
    pub allocated_at_ms: u64,
    /// Offset seconds east of UTC observed at allocation time.
    pub offset_seconds: i32,
}

/// Authoritative metadata stored in `sessions.meta_json` for live sessions.
///
/// The version suffix is intentional: old rows contain `{}` and decode as no
/// typed metadata, while a future incompatible shape can be added without
/// silently reinterpreting committed configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadataV1 {
    /// Durable sequence of the current model, budget, or route selection.
    /// Zero is the initial binding before a selection fact is committed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_epoch: Option<u64>,
    /// Sequence at which this route was created or explicitly reset. Absence
    /// on legacy metadata does not prove that the session was never served.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_reset_epoch: Option<u64>,
    /// Epoch whose durable selection fact records the current budget clamp.
    /// Missing legacy/inherited notices can be published on first route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_clamp_notice_epoch: Option<u64>,
    /// Last account route actually resolved for a turn. This is not an
    /// account pin: unpinned sessions continue to follow the active account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_route_alias: Option<String>,
    /// Distinguishes an observed keyless route from a route not yet resolved.
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolved_route_seen: bool,
    /// Canonical absolute UTF-8 workspace path.
    pub cwd: String,
    /// Provider adapter name (`anthropic`, `openai`, `openai-compatible`, or
    /// `fake` in injected tests).
    ///
    /// Sessions are provider-agnostic: this field is plumbing for the CURRENT
    /// model selection, never session identity. The user selects a model; the
    /// provider rides along as an attribute of the selected row and both may
    /// change together through `session.select_model`.
    pub provider: String,
    /// Per-session endpoint override committed by `session.provider.rebind`.
    /// Absence leaves the provider registry and credential endpoint authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_base_url: Option<String>,
    /// Durable command identity of the latest explicit provider rebind.
    /// Ordinary model selection does not advance this request-boundary marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_rebind_id: Option<String>,
    /// Optional account alias pinned by `session.create` or
    /// `session.provider.rebind`. When present, provider resolution uses this
    /// exact credential rather than the provider's mutable active-account selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_alias: Option<String>,
    /// Full provider model identifier — the current model selection.
    pub model: String,
    /// Maximum output tokens for each provider request.
    pub max_tokens: u64,
    /// Whether `max_tokens` was derived from the selected model or set by the
    /// user. `None` for metadata written before 973; readers classify it with
    /// [`crate::output_budget::SessionOutputBudgetSourceV1::classify`].
    /// Absence stays off the wire so legacy metadata bytes are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_source: Option<crate::output_budget::SessionOutputBudgetSourceV1>,
    /// Version of the deterministic daemon-owned system policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_version: Option<String>,
    /// Optional headless creation policy. Absence preserves legacy session and
    /// receipt bytes and means the daemon registry defaults remain authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_overrides: Option<SessionPermissionOverridesV1>,
    /// Durable human-availability contract for root runs, restart recovery,
    /// and any session fork. Omitted legacy metadata remains interactive.
    #[serde(
        default,
        skip_serializing_if = "SessionInteractionModeV1::is_interactive"
    )]
    pub interaction_mode: SessionInteractionModeV1,
    /// Optional user-facing session title (G2). `None` for legacy rows and
    /// untitled sessions — absence stays OFF the wire so pre-G2 metadata
    /// bytes are unchanged. Normalized by the daemon: trimmed, control
    /// characters stripped, ≤ 80 chars, empty collapses to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Explicit per-pair reasoning-effort selection (G3). `None` means "the
    /// provider's own default" and is skipped on the wire so pre-G3 metadata
    /// rows stay byte-identical. The value is a provider-vocabulary STRING
    /// validated against the CURRENT pair's declared ladder at selection
    /// time — never a global enum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Anthropic fast-mode flag (G3). Skipped while false so pre-G3 metadata
    /// rows stay byte-identical.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fast: bool,
    /// Cache-destruction warning policy (CM3). Absent legacy metadata uses
    /// the balanced default and its configurable cold-cost threshold.
    #[serde(
        default,
        skip_serializing_if = "crate::cache::CachePolicySettingsV1::is_default"
    )]
    pub cache_policy: crate::cache::CachePolicySettingsV1,
    /// Bound Loom agent-type id (W-flow inline identity). `None` means a
    /// plain session and stays off the wire so earlier metadata rows are
    /// byte-identical. Validated against the Loom registry at selection
    /// time; the type's job rides the volatile prompt tail — binding or
    /// clearing never moves the conversation tree or the cache epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Conversation-level estimated token savings. This is additive metadata;
    /// legacy rows omit it and decode to an empty counter.
    #[serde(
        default,
        skip_serializing_if = "crate::context::ContextEconomy::is_empty"
    )]
    pub context_economy: crate::context::ContextEconomy,
    /// Current launch-origin snapshot (O5). `None` for legacy rows and
    /// sessions never foreground-opened; absence stays off the wire so
    /// pre-feature metadata bytes are unchanged. Display context only —
    /// workspace authority remains `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_origin: Option<LaunchOriginV1>,
    /// Dated-workspace allocation facts (§2–3). Absent for legacy rows,
    /// preserved-cwd sessions, and explicit workspaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_allocation: Option<WorkspaceAllocationV1>,
    /// Durable creation time in Unix milliseconds.
    pub created_at_ms: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// Additive replay fact emitted atomically with a committed live-session
/// model selection and its command receipt.
///
/// Sessions are provider-agnostic: the fact records the RESOLVED pair the
/// next logical turn resolves through — the model the user selected plus the
/// provider attribute of that row — not a change of session identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelected {
    /// Selection version; may remain unchanged for a true no-op command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_epoch: Option<u64>,
    /// Provider attribute of the selected model row.
    pub provider: String,
    /// The selected full model identifier.
    pub model: String,
    /// This fact publishes route provenance without changing the model pair.
    /// The first resolved route keeps the captured epoch; a rotation advances it.
    /// Older readers may still treat it as a model selection; new readers
    /// keep their known output budget and omit the model-change note.
    #[serde(default, skip_serializing_if = "is_false")]
    pub route_only: bool,
    /// Explicit account pin cleared by this cross-provider selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_account_pin: Option<String>,
    /// Explicit endpoint override cleared by a cross-provider selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_provider_endpoint: Option<String>,
    /// User budget reduced by this selection, retained for external clients
    /// and replay even after a later selection restores the requested value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_budget_clamp: Option<crate::output_budget::OutputBudgetClampV1>,
}

/// Shared disclosure for choices automatically cleared by a model pick.
#[must_use]
pub fn model_selection_clear_notice(
    account: Option<&str>,
    endpoint: Option<&str>,
    provider: &str,
) -> Option<String> {
    let choices = match (account, endpoint) {
        (Some(alias), Some(url)) => format!("account pin {alias} cleared; endpoint {url} cleared"),
        (Some(alias), None) => format!("account pin {alias} cleared"),
        (None, Some(url)) => format!("endpoint {url} cleared"),
        (None, None) => return None,
    };
    Some(format!(
        "{choices} — {provider} uses its active account and default endpoint"
    ))
}

/// Additive replay fact emitted atomically with a committed live-session
/// effort selection and its command receipt (G3).
///
/// `None` reverts the session to the provider default. The fact is a pure
/// session-config journal movement: it never moves the conversation tree,
/// which is what lets the F3 compaction head CAS tolerate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortSelected {
    /// The selected effort, or `None` for "provider default".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Additive replay fact emitted atomically with a committed live-session
/// fast-mode toggle and its command receipt (G3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FastModeSelected {
    /// Whether fast mode is on after this fact.
    pub enabled: bool,
}

/// Durable endpoint/account selection for the session's next provider request.
/// The model and conversation are unchanged. Credentials never enter this fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProviderRebound {
    /// Durable route-selection version, equal to this fact's journal sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_epoch: Option<u64>,
    pub rebind_id: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Budget for the durable queued next-turn pair after this route change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_budget: Option<crate::output_budget::SessionOutputBudgetV1>,
    /// Budget installed for the active run after this rebind, even when its
    /// model matches the queued next-turn model in `output_budget`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_output_budget: Option<ActiveRunOutputBudgetV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveRunOutputBudgetV1 {
    pub run_id: crate::ids::RunId,
    pub model: String,
    pub budget: crate::output_budget::SessionOutputBudgetV1,
}

/// Additive replay fact emitted atomically with a committed live-session
/// agent-type binding and its command receipt (W-flow inline identity).
///
/// `None` reverts the session to plain. Like effort, this is a pure
/// session-config journal movement: it never moves the conversation tree,
/// and the bound job rides the volatile prompt tail, so the cache epoch
/// is untouched in both directions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTypeSelected {
    /// The bound Loom agent-type id, or `None` for a plain session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

/// Additive session-configuration event union kept separate from
/// [`crate::EventPayload`] so existing exhaustive Rust consumers remain
/// source compatible. Readers should try this decoder before treating an
/// unknown core event kind as opaque (the same contract as
/// [`crate::branch::BranchEventPayload`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionConfigEventPayload {
    ModelSelected(ModelSelected),
    /// Additive replay fact emitted atomically with a committed session
    /// rename and its command receipt (G2). `None` = the title was cleared;
    /// absence stays OFF the wire.
    SessionRenamed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Durable attention acknowledgement. This config fact deliberately does
    /// not count as session activity: it only records that a surface has
    /// observed activity which was already committed.
    SessionSeen {
        seen_at_ms: u64,
    },
    EffortSelected(EffortSelected),
    FastModeSelected(FastModeSelected),
    SessionProviderRebound(SessionProviderRebound),
    AgentTypeSelected(AgentTypeSelected),
    /// Committed launch-origin registration (O4). Session config only: no
    /// conversation node, run, cache-epoch, or seen/activity movement.
    SessionLaunchOriginSelected(SessionLaunchOriginSelected),
}

impl SessionConfigEventPayload {
    /// Encodes one committed rename fact.
    pub fn session_renamed_value(
        title: Option<String>,
    ) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(Self::SessionRenamed { title })
    }

    /// Decodes a `session_renamed` fact: `Some(title)` when the payload is
    /// one, `None` for every other payload.
    #[must_use]
    pub fn session_renamed_from_value(value: &serde_json::Value) -> Option<Option<String>> {
        match serde_json::from_value::<Self>(value.clone()).ok()? {
            Self::SessionRenamed { title } => Some(title),
            _ => None,
        }
    }

    /// Encodes one committed attention acknowledgement.
    pub fn session_seen_value(seen_at_ms: u64) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(Self::SessionSeen { seen_at_ms })
    }
}

impl SessionLaunchOriginSelected {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(SessionConfigEventPayload::SessionLaunchOriginSelected(
            self.clone(),
        ))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<SessionConfigEventPayload>(value.clone()).ok()? {
            SessionConfigEventPayload::SessionLaunchOriginSelected(selected) => Some(selected),
            _ => None,
        }
    }
}

impl ModelSelected {
    /// Additive complete route witness. Older readers ignore the extra key.
    pub fn to_payload_value_with_route(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<serde_json::Value, serde_json::Error> {
        let mut value = self.to_payload_value()?;
        value["route_snapshot"] =
            serde_json::to_value(RunOutputRouteSnapshotV1::capture(metadata))?;
        Ok(value)
    }

    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(SessionConfigEventPayload::ModelSelected(self.clone()))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<SessionConfigEventPayload>(value.clone()).ok()? {
            SessionConfigEventPayload::ModelSelected(selected) => Some(selected),
            _ => None,
        }
    }
}

impl AgentTypeSelected {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(SessionConfigEventPayload::AgentTypeSelected(self.clone()))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<SessionConfigEventPayload>(value.clone()).ok()? {
            SessionConfigEventPayload::AgentTypeSelected(selected) => Some(selected),
            _ => None,
        }
    }
}

impl EffortSelected {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(SessionConfigEventPayload::EffortSelected(self.clone()))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<SessionConfigEventPayload>(value.clone()).ok()? {
            SessionConfigEventPayload::EffortSelected(selected) => Some(selected),
            _ => None,
        }
    }
}

impl FastModeSelected {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(SessionConfigEventPayload::FastModeSelected(self.clone()))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<SessionConfigEventPayload>(value.clone()).ok()? {
            SessionConfigEventPayload::FastModeSelected(selected) => Some(selected),
            _ => None,
        }
    }
}

impl SessionProviderRebound {
    pub fn to_payload_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(SessionConfigEventPayload::SessionProviderRebound(
            self.clone(),
        ))
    }

    #[must_use]
    pub fn from_payload_value(value: &serde_json::Value) -> Option<Self> {
        match serde_json::from_value::<SessionConfigEventPayload>(value.clone()).ok()? {
            SessionConfigEventPayload::SessionProviderRebound(rebound) => Some(rebound),
            _ => None,
        }
    }

    /// Rebuild the routing projection from a replayed committed fact.
    pub fn apply_to_metadata(&self, metadata: &mut SessionMetadataV1) {
        if let Some(epoch) = self.selection_epoch {
            metadata.selection_epoch = Some(epoch);
        }
        if metadata.provider != self.provider
            || metadata.provider_base_url != self.base_url
            || metadata.account_alias != self.account
        {
            metadata.resolved_route_alias = None;
            metadata.resolved_route_seen = false;
            metadata.route_reset_epoch = self.selection_epoch;
        }
        metadata.provider_rebind_id = Some(self.rebind_id.clone());
        metadata.provider.clone_from(&self.provider);
        metadata.provider_base_url.clone_from(&self.base_url);
        metadata.account_alias.clone_from(&self.account);
        if let Some(budget) = self.output_budget {
            metadata.max_tokens = budget.max_tokens;
            metadata.max_tokens_source = Some(budget.source);
        }
    }
}
