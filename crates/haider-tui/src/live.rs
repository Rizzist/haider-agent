//! The LIVE driver (W3c3 M2 — report R11 cut 4).
//!
//! `DemoDriver` plays a canned script; `LiveDriver` speaks to a real daemon.
//! Everything the two share — the reducer, the projections, per-surface
//! status derivation, `animated()`, render, select/clipboard/mark, the hit
//! map, the sticky band and the input pump — stays exactly where it was
//! (the W3C seam report's verified stays-put list). Only the SOURCE of
//! events changes, plus the coordinates a live mutation needs.
//!
//! ## Shape: a pure core with a thin IO shell
//!
//! [`LiveDriver`] is a state machine. It consumes [`LiveReply`] (a response,
//! an envelope, a disconnect) and produces [`LiveCommand`] (an RPC to
//! issue), mutating the [`AppModel`] on the way. It performs no IO and
//! awaits nothing, so every law below is testable without a daemon:
//! the working set, LRU eviction, reconnect cursors, the launcher's
//! create→attach→submit order, and menu coordinates. `run_live` (in
//! [`crate::runtime`]) is the shell that performs the IO.
//!
//! ## What the driver owns (R11 cut 4)
//!
//! * per-session attachment and the last-applied cursor — the CLIENT's
//!   cursor bookkeeping deliberately lives here, not in `haider-client`;
//! * an attachment working set capped at the server's per-connection limit,
//!   priority-ordered (active first, then running/pending-menu), LRU-detach
//!   before the cap would be exceeded, with cold sessions represented by
//!   list/read metadata;
//! * durable command ids and the outbox that survives a reconnect;
//! * the menu-answer coordinates committed by each `MenuOpened`.
//!
//! The cursor authority itself is `SessionProjection` (see
//! [`crate::projection::SessionProjection::last_applied`]): the driver
//! never keeps a second copy, because a second copy is how a reattach ends
//! up asking for history the reducer already applied — or worse, skipping
//! history it never did.

use std::collections::HashMap;

use haider_protocol::DeliveryMode;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::ids::{MenuId, RunId, SessionId};
use haider_rpc::{AttachmentId, CommandId, MenuInput, SessionSummary, SubmitDisposition};

use crate::app::{AppModel, AppRequest, OutboundAnswer};
use crate::projection::RawOutcome;

/// The daemon's per-connection attachment ceiling
/// (`haider-daemon/src/session_hub/mod.rs`: `max_attachments_per_connection`).
///
/// The client holds AT MOST this many attachments and LRU-detaches before a
/// new attach would need a 17th slot, so the ceiling is never reached from
/// this side and the daemon never has to reject one of our attaches. (The
/// report says "capped below the server's limit"; the brief and §6.3's test
/// say "16, LRU-detach before the 17th". They agree on the observable: we
/// never ask for a 17th.)
pub const ATTACHMENT_CAP: usize = 16;

/// One page size for the launcher's session listing.
pub const LIST_PAGE: u32 = 64;

/// One outbound operation the IO shell must perform.
///
/// `Attach`/`Detach`/`List`/`Read` are connection-scoped reads: losing one
/// costs a round trip and nothing else. The rest are DURABLE MUTATIONS and
/// carry a [`CommandId`]: the daemon deduplicates by receipt, so a lost
/// response is retried under the SAME id and cannot duplicate the effect
/// (§6.4: "a lost mutation response does not duplicate session, turn,
/// cancel, or login").
#[derive(Debug, Clone, PartialEq)]
pub enum LiveCommand {
    List {
        cursor: Option<String>,
    },
    Attach {
        session: SessionId,
        after_seq: u64,
    },
    Detach {
        attachment: AttachmentId,
    },
    Create {
        command_id: CommandId,
        cwd: String,
        provider: String,
        model: String,
        max_tokens: u64,
        /// Carried through the round trip so the first turn can follow the
        /// create WITHOUT the model having fabricated anything.
        first_text: String,
    },
    Submit {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        text: String,
        mode: DeliveryMode,
        /// The branch captured at ISSUANCE (B2b): `Some` encodes the
        /// branch-capable `turn.submit` decode form; `None` keeps the
        /// legacy main-branch bytes byte-for-byte historical.
        branch: Option<haider_protocol::ids::BranchId>,
        /// Ready attachment blocks captured at issuance (B4b). They ride
        /// BOTH `turn.submit` wire forms; an empty vector keeps the
        /// pre-B4 bytes historical (`attachments: []` is B4a's decode
        /// default).
        attachments: Vec<haider_protocol::tool::AttachmentBlock>,
    },
    /// `artifact.put` — receipt-free, content-addressed byte ingress
    /// (B4b). Deliberately NO command id and never outboxed: repeating
    /// the same bytes is naturally idempotent, and a socket loss simply
    /// drops the flight (the driver sweeps its chip with an honest
    /// notice instead of silently resending). `upload`/`surface` are
    /// CLIENT-side correlation the link's request context carries back;
    /// only `bytes` reaches the wire.
    ArtifactPut {
        upload: u64,
        surface: crate::app::DraftKey,
        bytes: crate::app::ArtifactBytes,
    },
    Cancel {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        run_id: RunId,
        /// Captured at issuance (B2b). CLIENT-side identity only: the wire
        /// `turn.cancel` pins the run by `run_id`, which is already
        /// branch-pinned by its acceptance.
        branch: Option<haider_protocol::ids::BranchId>,
    },
    /// `session.compact` — receipt-backed, idle-only manual compaction
    /// (W7b). The daemon's own journal events drive every visible state
    /// change; the reply only retires the outbox entry.
    Compact {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        /// Captured at issuance (B2b): `Some` encodes the branch-capable
        /// `session.compact` decode form; `None` the legacy bytes.
        branch: Option<haider_protocol::ids::BranchId>,
    },
    /// `branch.create` — one durable named ref at EXACT committed
    /// coordinates (B2b). Receipt-backed: a lost response retries under
    /// the same command id and returns the original branch. The response
    /// installs NOTHING locally — the daemon's `BranchCreated` journal
    /// fact is the only materializer.
    BranchCreate {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        source_branch: Option<haider_protocol::ids::BranchId>,
        fork_node_id: haider_protocol::ids::NodeId,
        fork_seq: u64,
        name: Option<String>,
    },
    /// `agent.message` — one parent-authored message to a DIRECT child
    /// (S1's wire; S3 rides it from the chip composer). Receipt-backed:
    /// the daemon dedupes by command id, chooses steer-vs-queued ITSELF,
    /// and journals the `AgentMessaged` fact plus the chip's user row —
    /// the response receipt only names what it did and retires the outbox
    /// entry. Nothing is painted locally.
    AgentMessage {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        /// The chip's opaque agent id (never the callsign — §5.1).
        agent: String,
        text: String,
    },
    /// `shell.exec` — the W8b `!` escape: one exact user command for the
    /// session daemon's workspace, receipt-backed and PreAuthorized
    /// (UserTyped). Zero provider requests; the committed
    /// `CommandExecution` events render the row.
    ShellExec {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        command: String,
    },
    /// `tools.inventory` — a READ of the daemon's canonical tool registry
    /// + remembered session grants (W8b /tools). No durable identity.
    ToolsInventory {
        session: SessionId,
    },
    /// `hooks.list` — a READ of the daemon's hook discovery for one
    /// workspace (H4 /hooks). No durable identity; the cwd was captured at
    /// issuance by the reducer.
    HooksList {
        cwd: String,
    },
    /// `usage.report` — a READ of the cross-provider usage snapshot
    /// (U2 /usage, U1's wire). Parameterless, receipt-free, never outboxed:
    /// a socket loss simply leaves the screen on its honest fetching /
    /// stale state and `r` re-reads.
    UsageReport,
    /// `hooks.trust` / `hooks.revoke` — a receipted digest pin or
    /// revocation (H3's R2 pattern). DURABLE: a lost response retries under
    /// the same command id and replays the same committed change. The
    /// response installs NOTHING locally — the driver chains a fresh
    /// `hooks.list` and daemon truth moves the rows.
    HooksTrust {
        command_id: CommandId,
        digest: String,
        /// `true` encodes `hooks.trust`, `false` `hooks.revoke`.
        trusted: bool,
    },
    /// `account.remove` — durable, revision-fenced (W10b).
    AccountRemove {
        command_id: CommandId,
        alias: String,
        expected_revision: Option<u64>,
    },
    /// `provider.remove` — durable custom-provider removal (W10b).
    ProviderRemove {
        command_id: CommandId,
        provider: String,
        expected_revision: u64,
    },
    /// Stage a raw secret in connection-scoped daemon memory (R7/R10).
    /// Deliberately NON-durable and NOT in the outbox: no command receipt
    /// may ever contain a secret, so a lost response is answered by
    /// staging a FRESH one, never by replaying this.
    Stage {
        stage_id: String,
        secret: haider_rpc::SecretWire,
        provider: String,
        alias: Option<String>,
        /// The login ATTEMPT this stage serves (TUI6.4, review r4
        /// finding 1). CLIENT-SIDE identity only — `request_body` never
        /// sends it; the link's `CommandContext` carries it back into
        /// the decoded reply, so stage correlation is by IDENTITY, never
        /// by queue position (r4's lesson: position-based popping on a
        /// wire that also delivers uncorrelated no-id frames is how a
        /// cancelled attempt's vault reference cross-bound to a live
        /// card).
        attempt: u64,
    },
    /// Commit an API login against an already-staged reference. DURABLE:
    /// its command identity deliberately EXCLUDES the ephemeral
    /// `vault_reference`, so a retry supplies a freshly staged reference
    /// UNDER THE SAME COMMAND ID and still recovers the original committed
    /// result — see the `Staged` arm of [`LiveDriver::apply`], which
    /// re-stages under `login_command` and replaces the outbox entry
    /// instead of minting a second.
    ///
    /// A RECONNECT IS NOT A RETRY. Staging is connection-scoped and the
    /// card wipes its copy of the key on submit, so a fresh socket has
    /// neither the reference nor anything to re-stage: the pending entry is
    /// retired and the card asks for a retype (`LiveDriver::abandon_login`).
    LoginApi {
        command_id: CommandId,
        provider: String,
        alias: Option<String>,
        vault_reference: String,
        /// The attempt identity, uniform with [`Self::Stage`] (TUI6.4) —
        /// client-side only, never on the wire.
        attempt: u64,
    },
    /// A menu answer at its EXACT committed opening coordinates.
    Answer {
        command_id: CommandId,
        session: SessionId,
        menu: MenuId,
        request_seq: u64,
        worker_generation: u64,
        option_key: String,
        option_index: u32,
        input: Option<MenuInput>,
    },
    /// `account.list` for the `/accounts` screen (W5d). A read.
    AccountList,
    /// `account.device_candidates` (D2) — the daemon's metadata-only
    /// discovery of first-party CLI credential stores. A read, issued on
    /// screen entry only; secrets never ride its response by D1's wire
    /// contract.
    DeviceCandidates,
    /// `account.import_device` (D2). DURABLE + receipted: the daemon
    /// dedupes by command id and re-reads the local store itself — the
    /// candidate id is the only payload, and the response installs
    /// NOTHING locally (the chained `account.list` is the materializer).
    DeviceImport {
        command_id: CommandId,
        candidate: String,
    },
    /// `account.set_active` (W5d). DURABLE mutation: the same command id
    /// replays the same committed result. `alias` rides along client-side
    /// so a failure reply can clear the exact pending row.
    AccountSetActive {
        command_id: CommandId,
        alias: String,
        confirm_new_epoch: bool,
    },
    /// `provider.list` for the `/providers` screen (W5d). A read.
    ProviderList,
    /// `provider.models_refresh` (W5f-2d): discover the provider's live
    /// catalog from its subscription source. Triggered when an active OAuth
    /// account has no discovered models yet — the fetch needs its token.
    RefreshProviderModels {
        provider: String,
    },
    /// `account.oauth_start` (W5e-1). Transient — never outboxed; a lost
    /// response is answered by a fresh card, never a replay.
    OAuthStart {
        provider: String,
        desired_alias: String,
        attempt_id: String,
    },
    /// `account.oauth_status` poll for the bound flow.
    OAuthStatus {
        flow_id: haider_rpc::OAuthFlowId,
        attempt_id: String,
    },
    /// `account.oauth_cancel` (idempotent).
    OAuthCancel {
        flow_id: haider_rpc::OAuthFlowId,
        attempt_id: String,
    },
    /// `account.add` for a READY flow reference. DURABLE: the reference is
    /// excluded from the semantic digest, so a retry replays the committed
    /// descriptor.
    AccountAddOAuth {
        command_id: CommandId,
        provider: String,
        alias: String,
        flow_id: haider_rpc::OAuthFlowId,
        attempt_id: String,
        oauth_reference: haider_rpc::OAuthReadyRefWire,
    },
    /// `account.set_default_model` under the expected-revision CAS (W5d).
    SetDefaultModel {
        command_id: CommandId,
        provider: String,
        model: String,
        expected_revision: u64,
    },
    /// `session.select_model` (F2a/F1): receipted live-session pair
    /// selection. DURABLE — a reconnect resends under the same command id
    /// and the daemon replays the committed receipt.
    SelectModel {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        model: String,
        provider: String,
        confirm_new_epoch: bool,
    },
    /// `session.rename` (G2): receipted live-session rename. DURABLE — a
    /// reconnect resends under the same command id and the daemon replays
    /// the committed receipt.
    Rename {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        title: String,
    },
    /// `session.select_effort` (G3): receipted per-pair effort selection.
    /// DURABLE — a reconnect resends under the same command id and the
    /// daemon replays the committed receipt. `None` reverts to the
    /// provider default.
    SelectEffort {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        effort: Option<String>,
        confirm_new_epoch: bool,
    },
    /// `session.select_fast` (G3): the receipted fast-mode toggle. DURABLE.
    SelectFast {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        enabled: bool,
        confirm_new_epoch: bool,
    },
    /// `provider.configure` for a custom OpenAI-compatible provider
    /// (W5g-4) or a G4b enterprise builtin. Identity fields are fixed by
    /// the card: the stated family, api-key auth (or auth NONE for the G4a
    /// keyless presets), enabled. The served model seeds the inventory and
    /// the default in one stroke (an enabled create requires both — daemon
    /// law, W5g-5); the G4b enterprise cards echo an EXPLICIT inventory
    /// instead.
    ConfigureProvider {
        command_id: CommandId,
        provider: String,
        origin: String,
        model: String,
        /// G4a: `auth_requirement: none` on the wire when true.
        keyless: bool,
        /// G4b: chat-completions for customs/azure, anthropic-messages for
        /// the enterprise builtins.
        family: haider_rpc::ProviderApiFamilyWire,
        /// G4b: explicit inventory echo; EMPTY derives `[model]`.
        models: Vec<String>,
        default_model: Option<String>,
        expected_revision: u64,
    },
    /// `transcription.secret_get` (T2): read the vaulted Deepgram key for
    /// the TUI-resident engine. A READ — no durable identity; the raw
    /// secret answer rides the same protected UDS surface as
    /// `vault.stage`.
    TranscriptionSecretGet,
    /// `transcription.secret_set` (T2): vault (or, with `clear`, delete)
    /// the key. Deliberately NON-durable — no receipt may carry a secret;
    /// the vault file is the durable truth (T1 daemon law), so no command
    /// id and never outboxed.
    TranscriptionSecretSet {
        secret: haider_rpc::SecretWire,
        clear: bool,
    },
}

/// Which transcription-secret RPC an error reply belongs to (link-context
/// identity — neither request carries a durable command id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptionOp {
    Get,
    Set,
}

impl LiveCommand {
    /// The durable idempotency key, for the mutations that have one.
    #[must_use]
    pub const fn command_id(&self) -> Option<&CommandId> {
        match self {
            Self::Create { command_id, .. }
            | Self::Submit { command_id, .. }
            | Self::Cancel { command_id, .. }
            | Self::Compact { command_id, .. }
            | Self::BranchCreate { command_id, .. }
            | Self::AgentMessage { command_id, .. }
            | Self::ShellExec { command_id, .. }
            | Self::AccountRemove { command_id, .. }
            | Self::ProviderRemove { command_id, .. }
            | Self::Answer { command_id, .. } => Some(command_id),
            Self::HooksTrust { command_id, .. } => Some(command_id),
            Self::LoginApi { command_id, .. } => Some(command_id),
            Self::AccountSetActive { command_id, .. } => Some(command_id),
            Self::DeviceImport { command_id, .. } => Some(command_id),
            Self::SetDefaultModel { command_id, .. } => Some(command_id),
            Self::SelectModel { command_id, .. } => Some(command_id),
            Self::Rename { command_id, .. } => Some(command_id),
            Self::SelectEffort { command_id, .. } => Some(command_id),
            Self::SelectFast { command_id, .. } => Some(command_id),
            Self::ConfigureProvider { command_id, .. } => Some(command_id),
            Self::AccountAddOAuth { command_id, .. } => Some(command_id),
            Self::List { .. }
            | Self::Attach { .. }
            | Self::Detach { .. }
            | Self::AccountList
            | Self::DeviceCandidates
            | Self::ProviderList
            | Self::RefreshProviderModels { .. }
            | Self::OAuthStart { .. }
            | Self::OAuthStatus { .. }
            | Self::OAuthCancel { .. }
            | Self::ToolsInventory { .. }
            | Self::HooksList { .. }
            // U2: the usage snapshot is a read (see above).
            | Self::UsageReport
            // A stage carries no durable identity BY DESIGN (see above).
            | Self::Stage { .. }
            // T2: reads + the deliberately receipt-free secret set (no
            // receipt may carry a secret — the vault file is the truth).
            | Self::TranscriptionSecretGet
            | Self::TranscriptionSecretSet { .. }
            // Content-addressed and receipt-free BY DESIGN (B4b): the
            // bytes are their own idempotency key.
            | Self::ArtifactPut { .. } => None,
        }
    }
}

/// One inbound fact the driver reduces.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveReply {
    /// A `session.list` page.
    Listed {
        sessions: Vec<SessionSummary>,
        next_cursor: Option<String>,
    },
    /// `session.attach` answered. The attach RESPONSE always precedes the
    /// first event for its attachment (the daemon's register→replay seam);
    /// this is where the attachment id becomes routable.
    Attached {
        session: SessionId,
        attachment: AttachmentId,
        worker_generation: u64,
        replay_through_seq: u64,
    },
    Detached {
        attachment: AttachmentId,
    },
    /// `session.create` answered: the daemon's own session id.
    Created {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        cwd: String,
        model: String,
    },
    Submitted {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        disposition: SubmitDisposition,
    },
    /// `artifact.put` answered with the daemon's VERIFIED content address
    /// (B4b). `upload`/`surface` are the client-side identity the link's
    /// request context carried through (the wire is receipt-free), so the
    /// chip completes on the ISSUING draft — never guessed from queue
    /// position or the currently displayed surface.
    ArtifactUploaded {
        upload: u64,
        surface: crate::app::DraftKey,
        artifact: haider_protocol::ids::ArtifactRef,
    },
    /// `artifact.put` answered with a wire-level error (B4b): the chip
    /// dies (submitting it would name bytes the CAS never accepted) and
    /// the notice says so. Identity-carried like [`Self::ArtifactUploaded`].
    ArtifactUploadFailed {
        upload: u64,
        surface: crate::app::DraftKey,
        message: String,
    },
    Answered {
        command_id: CommandId,
    },
    /// `vault.stage` answered with an opaque single-use reference. The
    /// `attempt` is the CLIENT-side identity the link's request context
    /// carried through (TUI6.4): the driver gates on it — a reply whose
    /// attempt is retired, or is not the live one, mints nothing and
    /// emits no vault reference.
    Staged {
        vault_reference: String,
        provider: String,
        alias: Option<String>,
        attempt: u64,
    },
    /// `vault.stage` answered with a wire-level ERROR (TUI6.4): identity-
    /// carried like [`Self::Staged`] — stage requests hold no durable
    /// command id, but the link's context knows which attempt asked, so
    /// the failure reaches the RIGHT card (or a ghost dies silently)
    /// instead of being guessed from queue position (the 6.3b bug).
    StageFailed {
        attempt: u64,
        code: String,
        message: String,
    },
    /// `account.login_api` committed; `identity` is the descriptor's
    /// display identity (never a secret).
    LoggedIn {
        command_id: CommandId,
        identity: String,
    },
    Cancelled {
        command_id: CommandId,
    },
    /// `session.compact` accepted; the compaction events arrive on the
    /// attachment stream like any other run.
    Compacted {
        command_id: CommandId,
    },
    /// `branch.create` answered with the daemon's stable coordinates
    /// (B2b). The receipt retires the outbox entry and ACTIVATES the
    /// branch in the originating session once the `BranchCreated` journal
    /// fact has installed it — it installs nothing itself (no live branch
    /// before daemon truth).
    BranchForked {
        command_id: CommandId,
        session: SessionId,
        branch_id: haider_protocol::ids::BranchId,
        name: String,
    },
    /// `agent.message` answered with the daemon's delivery receipt (S1
    /// wire, S3 consumer): which child, steer-into-the-running-turn versus
    /// a queued fresh child turn, and the child run's coordinates. The
    /// receipt retires the outbox entry and paints ONLY the transient
    /// flash — the timeline rows ride the journal facts on the attachment
    /// stream, so nothing here fabricates transcript state.
    AgentMessaged {
        command_id: CommandId,
        receipt: haider_protocol::agent::AgentMessageReceipt,
    },
    /// `shell.exec` accepted; the `CommandExecution` events arrive on the
    /// attachment stream.
    ShellAccepted {
        command_id: CommandId,
    },
    /// `tools.inventory` snapshot — daemon registry truth for /tools.
    ToolsInventory {
        session: SessionId,
        snapshot: Box<haider_protocol::tool::ToolInventorySnapshot>,
    },
    /// `hooks.list` answered — discovery truth for /hooks (H4).
    Hooks {
        policy: String,
        hooks: Vec<haider_rpc::HookSummaryWire>,
    },
    /// `hooks.list` FAILED. Identity-tagged from the link's request context
    /// because the read carries no durable command id (the oauth_start /
    /// stage precedent): the failure lands on the hooks screen, not a
    /// launcher flash.
    HooksListFailed {
        message: String,
    },
    /// `usage.report` answered — the committed cross-provider snapshot
    /// (U2). Boxed: a many-account report is the largest read this enum
    /// carries.
    UsageReport {
        report: Box<haider_protocol::usage::UsageReportV1>,
    },
    /// `usage.report` FAILED. Identity-tagged from the link's request
    /// context (the read has no durable command id — the hooks.list
    /// precedent): the typed message lands on the usage screen, never a
    /// bare flash.
    UsageReportFailed {
        message: String,
    },
    /// `hooks.trust` / `hooks.revoke` committed: the daemon's receipt. It
    /// retires the outbox entry and chains a fresh `hooks.list` — the rows
    /// themselves move only on daemon truth (the branch discipline).
    HookTrustChanged {
        command_id: CommandId,
        digest: String,
        trusted: bool,
    },
    /// A `provider.models_refresh` failed — lands on the provider ROW
    /// (availability reason), never the status-bar flash.
    ModelsRefreshFailed {
        provider: String,
        message: String,
    },
    /// `account.remove` committed.
    AccountRemoved {
        command_id: CommandId,
        removed_alias: String,
        replacement_active_alias: Option<String>,
        revision: u64,
    },
    /// `provider.remove` committed.
    ProviderRemoved {
        command_id: CommandId,
        provider: String,
        revision: u64,
    },
    /// One committed envelope for an attachment.
    Event {
        attachment: AttachmentId,
        session: SessionId,
        envelope: Box<RawEnvelope>,
    },
    /// The daemon dropped this attachment under backpressure. `Lagged`'s
    /// `last_queued_seq` is server TELEMETRY, never resume authority — we
    /// reattach from our own greatest fully applied sequence (R9's cursor
    /// law).
    Lagged {
        attachment: AttachmentId,
    },
    /// The daemon finished replaying `(after_seq, high_water_seq]` for this
    /// attachment. It is the CATCH-UP BOUNDARY, and the only thing that can
    /// expose a replay whose LAST envelopes were lost: a hole in the middle
    /// is caught by the next sequence, a hole at the end has no next
    /// sequence at all (review W3c3 P1-2 — this marker used to be
    /// discarded at the link).
    CaughtUp {
        attachment: AttachmentId,
        high_water_seq: u64,
    },
    /// The client's inbound frame channel overflowed and DROPPED
    /// uncorrelated frames. A drop must become a gap, never silence: we do
    /// not know which attachment lost what, so every held attachment
    /// reattaches from its own cursor (review W3c3 P1-2 — `lost_events()`
    /// had no production caller).
    EventsLost {
        count: u64,
    },
    /// An `session.attach` failed. Carried separately from `Failed`
    /// because an attach has no durable command id to correlate by, and a
    /// silent failure would leave the session un-attachable for the life
    /// of the connection (review P1-5).
    AttachFailed {
        session: SessionId,
        code: String,
        message: String,
        retryable: bool,
    },
    /// A correlated operation failed.
    Failed {
        command_id: Option<CommandId>,
        code: String,
        message: String,
        retryable: bool,
    },
    /// `account.list` answered (W5d `/accounts`).
    Accounts {
        descriptors: Vec<haider_protocol::credential::CredentialDescriptor>,
        revision: Option<u64>,
    },
    /// `account.device_candidates` answered (D2): the daemon's
    /// metadata-only discovery report. `discovery_disabled` is the honest
    /// configured-off state, never an empty-device claim.
    DeviceCandidates {
        discovery_disabled: bool,
        candidates: Vec<haider_rpc::DeviceCredentialCandidateWire>,
    },
    /// `account.import_device` committed (D2): the daemon's receipt. It
    /// retires the outbox entry and chains the `account.list` refresh —
    /// the descriptor itself installs NOTHING locally (daemon truth is
    /// the only materializer, the branch discipline).
    DeviceImported {
        command_id: CommandId,
        descriptor: haider_protocol::credential::CredentialDescriptor,
        revision: u64,
    },
    /// `account.set_active` committed: the selected descriptor + the
    /// management revision the mutation finalized at.
    AccountSelected {
        command_id: CommandId,
        descriptor: haider_protocol::credential::CredentialDescriptor,
        revision: u64,
    },
    /// `provider.list` answered (W5d `/providers`).
    Providers {
        providers: Vec<haider_rpc::ProviderSummaryWire>,
        revision: u64,
    },
    /// `provider.models_refresh` answered (W5f-2d): one provider's summary,
    /// now carrying its discovered catalog.
    ProviderModelsRefreshed {
        provider: haider_rpc::ProviderSummaryWire,
        revision: u64,
    },
    /// `account.set_default_model` committed.
    DefaultModelSet {
        command_id: CommandId,
        provider: haider_rpc::ProviderSummaryWire,
        revision: u64,
    },
    /// `session.select_model` committed (F2a): the RESOLVED pair — never
    /// an echo of the request.
    ModelSelected {
        command_id: CommandId,
        session: SessionId,
        provider: String,
        model: String,
        worker_generation: u64,
    },
    /// `session.rename` committed (G2): the NORMALIZED title — never an
    /// echo of the request.
    Renamed {
        command_id: CommandId,
        session: SessionId,
        title: Option<String>,
        worker_generation: u64,
    },
    /// `session.select_effort` committed (G3): the RESOLVED value — never
    /// an echo of the request.
    EffortSelected {
        command_id: CommandId,
        session: SessionId,
        effort: Option<String>,
        worker_generation: u64,
    },
    /// `session.select_fast` committed (G3).
    FastSelected {
        command_id: CommandId,
        session: SessionId,
        enabled: bool,
        worker_generation: u64,
    },
    /// `provider.configure` committed (W5g-4).
    ProviderConfigured {
        command_id: CommandId,
        provider: haider_rpc::ProviderSummaryWire,
        revision: u64,
    },
    /// `account.oauth_start` FAILED. Identity-tagged because the request is
    /// non-durable and its error reply has no command_id to correlate by.
    OAuthStartFailed {
        attempt_id: String,
        code: String,
        message: String,
    },
    /// `account.oauth_start` answered (attempt-tagged via context).
    OAuthStarted {
        attempt_id: String,
        availability: haider_rpc::OAuthAvailabilityWire,
        flow_id: Option<haider_rpc::OAuthFlowId>,
        authorization_url: Option<String>,
        provider_origin: Option<String>,
    },
    /// `account.oauth_status` answered.
    OAuthFlowStatus {
        flow_id: haider_rpc::OAuthFlowId,
        status: haider_rpc::OAuthFlowStatusWire,
    },
    /// `account.add` committed the OAuth descriptor.
    AccountAdded {
        command_id: CommandId,
        descriptor: haider_protocol::credential::CredentialDescriptor,
    },
    /// `transcription.secret_get` answered (T2): the vaulted key, or an
    /// honest `None`. The wrapper redacts Debug and zeroizes on drop; the
    /// reducer routes it by its recorded intent and drops it promptly.
    TranscriptionSecret {
        secret: Option<haider_rpc::SecretWire>,
    },
    /// `transcription.secret_set` answered (T2): whether a secret is
    /// present AFTER the operation.
    TranscriptionSecretStored {
        present: bool,
    },
    /// A transcription-secret RPC FAILED. Identity-tagged from the link's
    /// request context (neither request has a durable command id — the
    /// hooks-list / oauth-start precedent), so the failure lands on the
    /// talk flow, never an uncorrelated flash.
    TranscriptionSecretFailed {
        op: TranscriptionOp,
        message: String,
    },
    /// The connection died; the shell will dial again.
    Disconnected {
        reason: String,
    },
    /// A fresh connection is negotiated. Every attachment is gone with the
    /// old socket, so the working set is rebuilt from the reducer's cursors
    /// and the outbox is resent under its durable ids.
    Reconnected,
    /// The daemon entered its drain window.
    Draining {
        reason: String,
    },
}

/// A durable mutation awaiting its response — the outbox.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    command_id: CommandId,
    command: LiveCommand,
}

/// The driver's OAuth add flight (W5e-1). One at a time — the card is total
/// over the accounts screen. Poll cadence bounded by [`OAUTH_POLL_INTERVAL`].
#[derive(Debug)]
struct OAuthFlight {
    attempt: u64,
    attempt_id: String,
    provider: String,
    alias: String,
    flow: Option<haider_rpc::OAuthFlowId>,
    last_poll: Option<std::time::Instant>,
    /// The authorize/verification URL `oauth_start` answered with — kept so
    /// a later `WaitingDevice` status can render the device-honest "enter
    /// the code at <url>" copy (the status wire itself carries no URL).
    url: String,
    /// The provider origin beside it (same source, same reason).
    origin: String,
    /// The durable add command once READY fired (correlates its failure).
    add_command: Option<CommandId>,
}

/// `account.oauth_status` poll cadence while the browser owns the flow.
const OAUTH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1500);

/// The committed coordinates of one open menu (report R11 cut 4): a live
/// answer is built from the OPENING envelope, never from local state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuCoordinates {
    pub session: SessionId,
    /// The `seq` of the committed `MenuOpened` envelope — the CAS fence.
    pub request_seq: u64,
    /// The `worker_generation` that envelope carried.
    pub worker_generation: u64,
    /// Minted at the FIRST answer attempt and reused on every retry, so a
    /// resend after a lost response resolves the same menu once.
    command_id: Option<CommandId>,
}

/// A live session row the launcher knows about but is not attached to.
///
/// "Cold" is a WORKING-SET state, not a lesser one: the row is listable
/// with its committed head, and SELECTING it attaches and replays its full
/// history through the same router a hot session uses (R11 cut 4's
/// list/read metadata — the read is the attach's own replay, so there is
/// no second, divergent projector and no separate `session.read` path to
/// keep honest).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cold {
    /// The committed head the daemon reported at list time — how far this
    /// session has progressed while we hold no attachment.
    head_seq: u64,
}

impl Cold {
    /// The committed head at list time.
    const fn head_seq(&self) -> u64 {
        self.head_seq
    }
}

/// The live driver. See the module charter.
#[derive(Debug)]
pub struct LiveDriver {
    /// Attachment ids by session, and the reverse map used to REJECT an
    /// event for an attachment we do not hold (report §6.3: "unknown
    /// attachment ids are rejected").
    attachments: HashMap<SessionId, AttachmentId>,
    routes: HashMap<AttachmentId, SessionId>,
    /// Working set in LRU order: front is coldest, back is most recently
    /// used. Bounded by [`ATTACHMENT_CAP`].
    lru: Vec<SessionId>,
    /// Sessions the daemon listed that we hold no attachment for.
    cold: HashMap<SessionId, Cold>,
    /// Attaches issued and not yet answered, each remembering the CURSOR it
    /// asked from. Two laws ride this one map:
    ///
    /// * **one attach in flight per session** — the latch lives with
    ///   [`Self::ensure_attached`], the single `Attach` emitter, so no
    ///   caller can open a second attachment by forgetting to check
    ///   (review W3c3 P1-3 / D1-1: the latch used to live in the CALLER
    ///   while four sites emitted, and `run_live`'s loop tail attached
    ///   again on every gap, `Lagged` and reconnect);
    /// * **the strict gap covers the FIRST envelope** — the remembered
    ///   `after_seq` seeds the projection's cursor when the attach is
    ///   answered, so a replay that starts at `after_seq + 2` stops and
    ///   reattaches instead of painting (review W3c3 P1-1).
    attaching: HashMap<SessionId, u64>,
    /// The ONE durable command of the open login card, so a failure can be
    /// correlated to it instead of merely coinciding with it (P2-2) and a
    /// retry re-stages UNDER IT rather than minting a second (P1-4).
    login_command: Option<CommandId>,
    /// The LIVE attempt this driver is staging/logging-in for (TUI6.3
    /// fix 1): bound when the `LoginApi` request drains, cleared by a
    /// retire, an abandon, or completion. Reply gates compare against it
    /// AND against the open card's own attempt — both must agree or the
    /// reply is a ghost from a cancelled attempt and is ignored whole.
    /// Since TUI6.4 the stage leg carries the attempt in the REPLY
    /// itself (the link's request context), so there is no positional
    /// correlation state at all — r4 proved a FIFO tag queue lets an
    /// uncorrelated no-id frame shift the alignment and cross-bind a
    /// cancelled vault reference.
    login_attempt: Option<u64>,
    /// Login command ids retired BEFORE their reply arrived (a close or
    /// abandon raced the daemon). Their late replies — success or
    /// failure — are consumed SILENTLY: the user already saw the cancel,
    /// and r4's P2 was exactly a retired failure painting a misleading
    /// global flash. Entries are consumed on their (single) reply and
    /// the set dies with the connection (no reply can cross a socket).
    retired_logins: std::collections::HashSet<CommandId>,
    /// When the card's non-durable `vault.stage` was issued. A Stage
    /// swallowed by a disconnect or a wedged daemon would otherwise park
    /// the card at "validating…" with `accepts_input() == false` forever
    /// (review W3c3 D2-5).
    login_started: Option<std::time::Instant>,
    /// The in-flight `account.set_active`, so a failure clears the exact
    /// pending row (W5d) instead of only flashing.
    pending_account_select: Option<(CommandId, String)>,
    /// The in-flight `account.set_default_model`: (command, provider).
    pending_default_model: Option<(CommandId, String)>,
    /// In-flight `session.select_model` (F2a): the REQUESTED pair, so a
    /// typed refusal can land on the exact selection that asked.
    pending_model_select: Option<(CommandId, SessionId, String, String)>,
    /// In-flight `session.rename` (G2): (command, session), so a typed
    /// refusal lands on the exact session that asked.
    pending_rename: Option<(CommandId, SessionId)>,
    /// In-flight `session.select_effort` (G3): failure correlation.
    pending_effort_select: Option<(CommandId, SessionId, Option<String>)>,
    /// In-flight `session.select_fast` (G3): failure correlation.
    pending_fast_select: Option<(CommandId, SessionId, bool)>,
    /// W10b in-flight removals: (command, alias/provider) — failures
    /// surface on the owning screen; successes come as typed replies.
    pending_account_remove: Option<(CommandId, String)>,
    pending_provider_remove: Option<(CommandId, String)>,
    /// The in-flight `provider.configure`: (command, card attempt).
    pending_custom: Option<(CommandId, u64)>,
    /// The in-flight `hooks.trust`/`hooks.revoke`: (command, digest) — a
    /// failure surfaces on the hooks screen and releases its gate (H4).
    pending_hook_trust: Option<(CommandId, String)>,
    /// The cwd the last `hooks.list` was issued for — what a trust receipt
    /// chains its refresh against (captured at issuance by the reducer).
    hooks_cwd: Option<String>,
    /// The in-flight `account.import_device` (D2), so a typed failure
    /// releases the exact pending candidate and lands its honest reason
    /// in the section. Durable — it survives a reconnect in the outbox.
    pending_device_import: Option<(CommandId, String)>,
    /// The one OAuth add flight (W5e-1): the card's whole driver state.
    oauth_flight: Option<OAuthFlight>,
    /// Durable mutations awaiting a response, in issue order.
    outbox: Vec<Pending>,
    /// Committed menu coordinates, by menu id.
    menus: HashMap<MenuId, MenuCoordinates>,
    /// Latest worker generation per session (create/attach/submit report it).
    generations: HashMap<SessionId, u64>,
    /// The run a session is CURRENTLY executing, learned from the committed
    /// envelopes' `run_id` and dropped the moment the run terminalizes.
    /// `turn.cancel` needs it: cancelling by guess would either name a run
    /// that already ended (a no-op the user reads as "Esc did nothing") or,
    /// worse, a later one.
    active_run: HashMap<SessionId, RunId>,
    /// Command-id minting: `{instance}-{n}`. The instance segment is random
    /// per process so two clients never mint the same durable id.
    instance: String,
    next_command: u64,
    /// Generation minting for live session rows.
    connected: bool,
    /// Sessions whose first turn is waiting for their attachment. Keyed by
    /// SESSION, not by the create's command id: the turn cannot be
    /// submitted until the attach RESPONSE lands, because `turn.submit`
    /// requires an established control attachment to that session.
    pending_first_turn: HashMap<SessionId, String>,
    /// Text handed to `session.create`, held until the daemon names the
    /// session it created.
    creating: HashMap<CommandId, String>,
    /// Providers a model-refresh has already been requested for this
    /// connection (W5f-2d) — so an active OAuth account with no discovered
    /// catalog triggers ONE fetch, not one per snapshot. Cleared on redial
    /// (a fresh daemon may answer differently).
    models_requested: std::collections::HashSet<String>,
    /// The pass clock. `live_pass` stamps it once per pass so the driver's
    /// deadlines are a pure function of the value it was handed — a test
    /// moves time by calling [`Self::set_now`], never by sleeping.
    now: std::time::Instant,
}

/// How long the login card may sit in `Submitting` before it says so —
/// the deadline covers BOTH transactions (`vault.stage` then
/// `account.login_api`).
///
/// `vault.stage` is deliberately NOT durable (no receipt may contain a
/// secret), so nothing retries it: a stage swallowed by a dead socket has
/// no later response to arrive. The card must therefore time itself out
/// and ask for the key again rather than advertise "validating…" forever
/// behind `accepts_input() == false`.
pub const LOGIN_STAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Ceiling on the OUTPUT-token budget `session.create` requests (W5f-2).
///
/// `session.create`'s `max_tokens` reaches the providers as the per-request
/// OUTPUT cap (`max_output_tokens` / `max_tokens`) — it was being fed the
/// identity's CONTEXT window (200k), which Anthropic rejects outright and
/// OpenAI clamps unpredictably. 30k sits inside every current subscription
/// model's output limit while leaving real headroom; a context window
/// smaller than the ceiling still wins.
pub const SESSION_OUTPUT_CAP: u64 = 30_000;

/// The output budget a new session may request: the ceiling, bounded by the
/// (smaller) context window when one is declared.
#[must_use]
pub fn session_output_cap(context_window: u64) -> u64 {
    SESSION_OUTPUT_CAP.min(context_window.max(1))
}

impl LiveDriver {
    /// A driver for one client instance. `instance` must be unique per
    /// process (the client instance id serves).
    #[must_use]
    pub fn new(instance: impl Into<String>) -> Self {
        Self {
            attachments: HashMap::new(),
            routes: HashMap::new(),
            lru: Vec::new(),
            cold: HashMap::new(),
            attaching: HashMap::new(),
            login_command: None,
            login_attempt: None,
            retired_logins: std::collections::HashSet::new(),
            login_started: None,
            pending_account_select: None,
            pending_default_model: None,
            pending_model_select: None,
            pending_rename: None,
            pending_effort_select: None,
            pending_fast_select: None,
            pending_account_remove: None,
            pending_provider_remove: None,
            pending_custom: None,
            pending_hook_trust: None,
            hooks_cwd: None,
            pending_device_import: None,
            oauth_flight: None,
            outbox: Vec::new(),
            menus: HashMap::new(),
            generations: HashMap::new(),
            active_run: HashMap::new(),
            instance: instance.into(),
            next_command: 0,
            connected: true,
            pending_first_turn: HashMap::new(),
            creating: HashMap::new(),
            models_requested: std::collections::HashSet::new(),
            now: std::time::Instant::now(),
        }
    }

    /// Stamp the pass clock. [`crate::runtime::live_pass`] calls this once
    /// per pass with `Instant::now()`; a test calls it with
    /// `base + Duration` to move time deterministically.
    pub const fn set_now(&mut self, now: std::time::Instant) {
        self.now = now;
    }

    /// The WORKING SET: which sessions we want attached, coldest first.
    ///
    /// Between a disconnect and its reconnect this is the set to RESTORE,
    /// not the set currently held — [`Self::is_attached`] answers that.
    #[must_use]
    pub fn working_set(&self) -> &[SessionId] {
        &self.lru
    }

    /// True while this session has a live attachment.
    #[must_use]
    pub fn is_attached(&self, session: &SessionId) -> bool {
        self.attachments.contains_key(session)
    }

    /// Sessions known from `session.list` but not attached — still listable
    /// and readable (R11 cut 4: "cold sessions represented by list/read
    /// metadata").
    #[must_use]
    pub fn is_cold(&self, session: &SessionId) -> bool {
        self.cold.contains_key(session)
    }

    /// A cold session's committed head at list time — what the launcher
    /// knows about a session it holds no attachment for.
    #[must_use]
    pub fn cold_head_seq(&self, session: &SessionId) -> Option<u64> {
        self.cold.get(session).map(Cold::head_seq)
    }

    /// Durable mutations still awaiting a response.
    #[must_use]
    pub fn outbox_len(&self) -> usize {
        self.outbox.len()
    }

    /// Working-set members that are neither attached nor waiting for an
    /// attach — the state that must not exist while connected (W3c3.1 r2,
    /// P1-B).
    ///
    /// A ghost holds a slot nothing will ever fill, and at the cap it is
    /// chosen as the eviction victim forever: `ensure_attached` finds no
    /// attachment to detach and refuses every later attach, so the
    /// ATTACHED surface stops loading with nothing on screen to say why.
    /// Exposed so the invariant is assertable rather than argued.
    ///
    /// A DISCONNECT is the documented exception: attachments die with the
    /// socket while the working set — which sessions to restore — survives
    /// on purpose, and `resume` re-attaches every member.
    #[must_use]
    pub fn ghost_slots(&self) -> Vec<SessionId> {
        if !self.connected {
            return Vec::new();
        }
        self.lru
            .iter()
            .filter(|session| {
                !self.attachments.contains_key(*session) && !self.attaching.contains_key(*session)
            })
            .cloned()
            .collect()
    }

    /// The committed coordinates of an open menu, if this connection saw it
    /// open.
    #[must_use]
    pub fn menu_coordinates(&self, menu: &MenuId) -> Option<&MenuCoordinates> {
        self.menus.get(menu)
    }

    /// The boot sequence: list sessions. Attaching happens on SELECTION —
    /// entering live mode must not attach to everything it can see.
    #[must_use]
    pub fn boot(&self) -> Vec<LiveCommand> {
        // The FIRST connect needs the same front-door truth as a redial:
        // the identity bootstrap fires when account/provider snapshots
        // APPLY, and `resume()` only runs on reconnects — the live probe
        // caught the launcher sitting on demo seeds because boot never
        // asked (W5f-2c).
        vec![
            LiveCommand::List { cursor: None },
            LiveCommand::AccountList,
            LiveCommand::ProviderList,
        ]
    }

    // ---------------------------------------------------------- commands --

    fn mint(&mut self) -> CommandId {
        self.next_command += 1;
        CommandId::new(format!("{}-{}", self.instance, self.next_command))
    }

    /// Queue a durable mutation in the outbox and return it for issue.
    fn enqueue(&mut self, command: LiveCommand) -> LiveCommand {
        if let Some(id) = command.command_id() {
            self.outbox.push(Pending {
                command_id: id.clone(),
                command: command.clone(),
            });
        }
        command
    }

    fn retire(&mut self, command_id: &CommandId) {
        self.outbox
            .retain(|pending| &pending.command_id != command_id);
    }

    // ------------------------------------------------------- working set --

    /// Attach whatever the user has SELECTED, if it is not attached yet.
    ///
    /// R11 cut 4: "entering live mode … attaches only on selection". The
    /// launcher lists cold sessions from `session.list`; opening one is the
    /// moment its history is actually wanted, so this is called once per
    /// loop pass and is a no-op unless the attached surface changed. It is
    /// also what makes a SECOND terminal see the same session's contiguous
    /// events (§6.4) — its launcher row is cold until it is chosen.
    pub fn sync_selection(&mut self, model: &AppModel) -> Vec<LiveCommand> {
        let Some(active) = model.active_session.clone() else {
            return Vec::new();
        };
        self.ensure_attached(model, &active)
    }

    /// THE ONE PLACE AN `Attach` IS EMITTED, evicting the coldest EVICTABLE
    /// session first when the cap is already full (report §6.3:
    /// "LRU-detaches before the 17th attach").
    ///
    /// Priority (R11 cut 4): the ACTIVE session is never evicted, and a
    /// session that is running or holds an unanswered menu is only evicted
    /// when nothing colder is available — losing its attachment would mean
    /// missing the very events the user is waiting on.
    ///
    /// The in-flight latch lives HERE, with the emitter, and every caller
    /// (`sync_selection`, `Lagged`, `AppRequest::Reattach`, `resume`, the
    /// create response) goes through it. "One attach in flight per
    /// session" is then unrepresentable to break rather than a rule five
    /// call sites have to remember — which is exactly how the double
    /// attach survived M3.2's fix (review W3c3 P1-3 / D1-1).
    pub fn ensure_attached(&mut self, model: &AppModel, session: &SessionId) -> Vec<LiveCommand> {
        if self.attachments.contains_key(session) {
            self.touch(session);
            return Vec::new();
        }
        if self.attaching.contains_key(session) {
            return Vec::new();
        }
        // A DEAD SOCKET TAKES NO ATTACH. The link drops commands issued
        // while disconnected (there is nothing to write them to), so an
        // attach latched here would never be answered and never released:
        // the session would be unattachable for the rest of the process.
        // `resume` is what restores the working set, and it runs with the
        // fresh connection in hand.
        if !self.connected {
            return Vec::new();
        }
        let mut commands = Vec::new();
        // A session ALREADY in the working set needs no new slot: that is
        // what makes `resume` — which rebuilds `lru` wholesale and then
        // attaches each member — reattach its own set instead of evicting
        // it one member at a time.
        if !self.lru.iter().any(|held| held == session) && self.lru.len() >= ATTACHMENT_CAP {
            let Some(victim) = self.evictable(model) else {
                return Vec::new();
            };
            let Some(attachment) = self.attachments.get(&victim).cloned() else {
                // Every slot is an attach we have already asked for.
                // Asking for a 17th would earn `overloaded`; the loop
                // calls this again next pass, when a response has landed.
                return Vec::new();
            };
            self.drop_attachment(&attachment);
            self.cold.insert(
                victim.clone(),
                Cold {
                    head_seq: cursor_of(model, &victim).unwrap_or(0),
                },
            );
            commands.push(LiveCommand::Detach { attachment });
        }
        let after_seq = cursor_of(model, session).unwrap_or(0);
        // The working set WANTS it from the moment we ask, not from the
        // moment the daemon answers: a disconnect in between must still
        // restore it. Slot and latch move together — see `claim_slot`.
        self.claim_slot(session, after_seq);
        commands.push(LiveCommand::Attach {
            session: session.clone(),
            after_seq,
        });
        commands
    }

    /// The coldest session we may detach: never the attached surface, and
    /// hot (running / pending-menu) sessions only once nothing else is
    /// left.
    fn evictable(&self, model: &AppModel) -> Option<SessionId> {
        let active = model.active_session.clone();
        let mut fallback = None;
        for candidate in &self.lru {
            if Some(candidate) == active.as_ref() {
                continue;
            }
            if is_hot(model, candidate) {
                fallback.get_or_insert_with(|| candidate.clone());
                continue;
            }
            return Some(candidate.clone());
        }
        fallback
    }

    fn touch(&mut self, session: &SessionId) {
        self.lru.retain(|held| held != session);
        self.lru.push(session.clone());
    }

    /// CLAIM a working-set slot and the in-flight latch TOGETHER — the only
    /// place either is set for a session we do not yet hold.
    ///
    /// They are inseparable on purpose (W3c3.1 r2, P1-B). Setting the LRU
    /// entry without the latch produces a GHOST: a slot nothing will ever
    /// fill, which at the cap is chosen as the eviction victim on every
    /// pass, whereupon `ensure_attached` finds no attachment to detach and
    /// refuses every later attach — the attached surface stops loading
    /// with nothing on screen to say why. Setting the latch without the
    /// slot is the mirror: an attach whose response has no reserved place
    /// in the working set.
    fn claim_slot(&mut self, session: &SessionId, after_seq: u64) {
        self.attaching.insert(session.clone(), after_seq);
        self.touch(session);
    }

    /// RELEASE both, for a session that now holds neither an attachment nor
    /// a pending attach. See [`Self::claim_slot`].
    fn release_slot(&mut self, session: &SessionId) {
        self.attaching.remove(session);
        self.lru.retain(|held| held != session);
    }

    fn drop_attachment(&mut self, attachment: &AttachmentId) {
        if let Some(session) = self.routes.remove(attachment) {
            self.attachments.remove(&session);
            self.lru.retain(|held| held != &session);
        }
    }

    // ------------------------------------------------------------ replies --

    /// Reduce one inbound fact, mutating the model, and return the RPCs the
    /// shell must now issue.
    #[allow(clippy::too_many_lines)]
    pub fn apply(&mut self, model: &mut AppModel, reply: LiveReply) -> Vec<LiveCommand> {
        match reply {
            LiveReply::Listed {
                sessions,
                next_cursor,
            } => {
                for summary in sessions {
                    model.upsert_live_session(&summary.session_id);
                    // Launcher fix 2: the additive turn/footprint fields
                    // hydrate the row's counts at list time — tolerantly
                    // (an older daemon's summary stores nothing).
                    model.note_summary_counts(&summary);
                    self.generations
                        .insert(summary.session_id.clone(), summary.worker_generation);
                    if !self.attachments.contains_key(&summary.session_id) {
                        self.cold.insert(
                            summary.session_id,
                            Cold {
                                head_seq: summary.head_seq,
                            },
                        );
                    }
                }
                model.dirty = true;
                next_cursor.map_or_else(Vec::new, |cursor| {
                    vec![LiveCommand::List {
                        cursor: Some(cursor),
                    }]
                })
            }
            LiveReply::Attached {
                session,
                attachment,
                worker_generation,
                ..
            } => {
                self.cold.remove(&session);
                // THE STRICT GAP LAW COVERS THE FIRST ENVELOPE (review
                // W3c3 P1-1). Continuity used to be checked only once
                // `last_seq` was set, so a fresh attach at cursor 0 that
                // was answered with seq 2 APPLIED seq 2 as its first event
                // — a hole painted as history, with no later sequence able
                // to expose it. Seeding the reducer with the cursor we
                // asked from makes the first admitted envelope checked
                // like every other one.
                if let Some(after_seq) = self.attaching.remove(&session) {
                    model.seed_cursor(&session, after_seq);
                }
                self.generations.insert(session.clone(), worker_generation);
                self.routes.insert(attachment.clone(), session.clone());
                self.attachments.insert(session.clone(), attachment);
                self.touch(&session);
                // Everything that was waiting for exactly this attachment:
                // a freshly created session's first turn, and any durable
                // mutation the outbox is still holding for this session
                // (the reconnect resend — review P1-4).
                let mut commands: Vec<LiveCommand> = self
                    .outbox
                    .iter()
                    .filter(|pending| command_session(&pending.command) == Some(&session))
                    .map(|pending| pending.command.clone())
                    .collect();
                if let Some(text) = self.pending_first_turn.remove(&session) {
                    // A freshly created session's founding turn is always
                    // legacy/main — no branch (and no attachment chip)
                    // can exist before it.
                    commands.push(self.submit(model, &session, text, None, Vec::new()));
                }
                commands
            }
            LiveReply::Detached { attachment } => {
                self.drop_attachment(&attachment);
                Vec::new()
            }
            LiveReply::Created {
                command_id,
                session,
                worker_generation,
                cwd,
                model: model_name,
            } => {
                self.retire(&command_id);
                self.generations.insert(session.clone(), worker_generation);
                // THE LAUNCHER ORDER (R11 cut 4). Only now — with the
                // daemon's own id in hand — does a row exist. Nothing was
                // fabricated locally, so nothing has to be reconciled.
                model.upsert_live_session(&session);
                if let Some(row) = model.sessions.iter_mut().find(|row| row.id == session) {
                    // The ROW shows the display form; `cwd` is the absolute
                    // path the daemon was given.
                    let _ = cwd;
                    row.model_short = model_name;
                }
                let commands = self.ensure_attached(model, &session);
                model.open_session(&session);
                // THE ORDER (R11 cut 4): create response → attach response
                // → turn.submit. The turn waits for the ATTACHMENT, not
                // merely for the attach to be requested: the daemon rejects
                // a submit from a connection with no control attachment to
                // that session, and issuing both in one batch races.
                if let Some(text) = self.creating.remove(&command_id) {
                    self.pending_first_turn.insert(session, text);
                }
                model.dirty = true;
                commands
            }
            LiveReply::Submitted {
                command_id,
                session,
                worker_generation,
                ..
            } => {
                self.retire(&command_id);
                self.generations.insert(session, worker_generation);
                Vec::new()
            }
            LiveReply::ArtifactUploaded {
                upload,
                surface,
                artifact,
            } => {
                // The daemon's VERIFIED content address completes the chip
                // on the ISSUING draft (B4b). A chip the user removed
                // mid-flight stays removed — the reply dies silently (the
                // CAS holds unreferenced bytes, nothing more).
                model.complete_upload(surface, upload, artifact);
                Vec::new()
            }
            LiveReply::ArtifactUploadFailed {
                upload,
                surface,
                message,
            } => {
                if let Some(label) = model.fail_upload(surface, upload) {
                    model.flash = Some(format!("· {label} — upload failed: {message}"));
                    model.dirty = true;
                }
                Vec::new()
            }
            LiveReply::ToolsInventory { session, snapshot } => {
                // Committed daemon truth for the ACTIVE session only — a
                // stale reply for another session must not paint this one.
                if model.active_session.as_ref() == Some(&session) {
                    model.tools_inventory = Some(*snapshot);
                    model.dirty = true;
                }
                Vec::new()
            }
            LiveReply::Hooks { policy, hooks } => {
                // The ONLY writer of the /hooks rows (H4): daemon
                // discovery truth, trusted rows pinning the trust baseline.
                model.hooks.apply_snapshot(policy, hooks);
                model.dirty = true;
                Vec::new()
            }
            LiveReply::HooksListFailed { message } => {
                model.hooks.list_failed(&message);
                model.dirty = true;
                Vec::new()
            }
            // U2: the ONLY writer of the /usage snapshot — committed
            // daemon truth, installed whole.
            LiveReply::UsageReport { report } => {
                model.usage.apply_report(*report);
                model.dirty = true;
                Vec::new()
            }
            LiveReply::UsageReportFailed { message } => {
                model.usage.read_failed(&message);
                model.dirty = true;
                Vec::new()
            }
            LiveReply::HookTrustChanged {
                command_id,
                digest,
                trusted,
            } => {
                // The receipt retires the gate and INSTALLS NOTHING — the
                // chained `hooks.list` is what moves the rows (the branch
                // discipline: no local truth before daemon truth).
                self.retire(&command_id);
                if self
                    .pending_hook_trust
                    .as_ref()
                    .is_some_and(|(pending, _)| pending == &command_id)
                {
                    self.pending_hook_trust = None;
                }
                model.hooks.note_receipt(&digest, trusted);
                model.dirty = true;
                match self.hooks_cwd.clone() {
                    Some(cwd) => vec![LiveCommand::HooksList { cwd }],
                    None => Vec::new(),
                }
            }
            LiveReply::ModelsRefreshFailed { provider, message } => {
                for summary in &mut model.providers.providers {
                    if summary.provider == provider {
                        summary.availability = haider_rpc::ProviderAvailabilityWire::Unavailable;
                        summary.availability_reason = Some(message.clone());
                    }
                }
                model.dirty = true;
                Vec::new()
            }
            LiveReply::AccountRemoved {
                command_id,
                removed_alias,
                replacement_active_alias,
                revision,
            } => {
                self.pending_account_remove = None;
                self.retire(&command_id);
                model.accounts.rows.retain(|row| row.alias != removed_alias);
                model.accounts.cursor = model
                    .accounts
                    .cursor
                    .min(model.accounts.rows.len().saturating_sub(1));
                if let Some(active) = &replacement_active_alias {
                    for row in &mut model.accounts.rows {
                        row.selected = row.alias == *active;
                    }
                }
                model.accounts.revision = Some(revision);
                model.accounts.message = Some(match replacement_active_alias {
                    Some(active) => {
                        format!("removed `{removed_alias}` · active → `{active}`")
                    }
                    None => format!("removed `{removed_alias}`"),
                });
                model.dirty = true;
                vec![LiveCommand::AccountList, LiveCommand::ProviderList]
            }
            LiveReply::ProviderRemoved {
                command_id,
                provider,
                revision,
            } => {
                self.pending_provider_remove = None;
                self.retire(&command_id);
                model.providers.providers.retain(|s| s.provider != provider);
                model.providers.cursor = model
                    .providers
                    .cursor
                    .min(model.providers.providers.len().saturating_sub(1));
                model.providers.revision = Some(revision);
                model.providers.message = Some(format!("removed `{provider}`"));
                model.dirty = true;
                vec![LiveCommand::ProviderList]
            }
            LiveReply::Answered { command_id }
            | LiveReply::Cancelled { command_id }
            | LiveReply::Compacted { command_id }
            | LiveReply::ShellAccepted { command_id } => {
                self.retire(&command_id);
                Vec::new()
            }
            LiveReply::AgentMessaged {
                command_id,
                receipt,
            } => {
                self.retire(&command_id);
                // DAEMON TRUTH ONLY (S3): the flash names the receipt's
                // own delivery kind — steer landed inside the child's
                // running turn, queued started a fresh child turn. The
                // chip's user row and the parent's `→ messaged` marker
                // both arrive as journal facts; nothing is painted here
                // beyond this transient line.
                let who = crate::app::find_chip(&model.chips, receipt.agent.as_str())
                    .filter(|chip| !chip.callsign.is_empty())
                    .map_or_else(
                        || receipt.agent.as_str().to_owned(),
                        crate::app::chip_display_name,
                    );
                model.flash = Some(match receipt.delivery {
                    haider_protocol::agent::AgentMessageDelivery::DeliveredSteer => {
                        format!("· messaged {who} — delivered as a steer into the running turn")
                    }
                    haider_protocol::agent::AgentMessageDelivery::DeliveredQueued => {
                        format!("· messaged {who} — queued as a fresh child turn")
                    }
                    haider_protocol::agent::AgentMessageDelivery::DeliveredSubturn => {
                        format!("· messaged {who} — subturn lands at the next tool call")
                    }
                });
                model.dirty = true;
                Vec::new()
            }
            LiveReply::BranchForked {
                command_id,
                session,
                branch_id,
                name,
            } => {
                self.retire(&command_id);
                // Activation touches ONLY the originating session (law) and
                // installs nothing: if the `BranchCreated` journal fact has
                // already materialized the branch, switch to it now; if the
                // receipt outran the event, ARM the activation and let the
                // install itself take effect. A typed failure never reaches
                // here, so topology stays untouched on refusal.
                if model.active_session.as_ref() == Some(&session) {
                    if model.branch_state.contains(&branch_id) {
                        if model.switch_branch(Some(&branch_id)).is_some() {
                            model.flash = Some(format!("· forked → {name}"));
                        }
                    } else {
                        model.branch_state.arm_activation(branch_id);
                    }
                } else if let Some(slot) = model.sessions.iter_mut().find(|slot| slot.id == session)
                {
                    if slot.branch_state.contains(&branch_id) {
                        slot.switch_branch(Some(&branch_id));
                    } else {
                        slot.branch_state.arm_activation(branch_id);
                    }
                }
                model.dirty = true;
                Vec::new()
            }
            LiveReply::Staged {
                vault_reference,
                provider,
                alias,
                attempt,
            } => {
                // TUI6.4 (review r4 finding 1): correlation is by the
                // reply's OWN attempt identity — carried through the
                // link's request context, never guessed from queue
                // position (r4's probe shifted a FIFO queue with an
                // uncorrelated no-id frame and cross-bound a CANCELLED
                // vault reference to the live card). The attempt must be
                // the live one on both sides — driver binding AND the
                // open card — or the reply is a ghost: dropped whole, no
                // mint, no vault reference emitted, no flash (the user
                // saw the cancel).
                let live = self.login_attempt == Some(attempt)
                    && model.login.as_ref().map(|card| card.attempt) == Some(attempt);
                if !live {
                    return Vec::new();
                }
                // Transaction two. The staged reference is single-use and
                // expiring, so the login follows immediately.
                //
                // A RETRY RE-STAGES UNDER THE ORIGINAL COMMAND ID (review
                // W3c3 P1-4 / D2-5), which is exactly what this command's
                // charter has always claimed: `LoginApi`'s durable identity
                // EXCLUDES the ephemeral `vault_reference` precisely so a
                // fresh reference can recover the original committed
                // result. Minting a second id made the daemon see a second
                // login while the first stayed pending forever.
                let command_id = self.login_command.clone().unwrap_or_else(|| self.mint());
                self.login_command = Some(command_id.clone());
                // One pending entry per card: the re-stage REPLACES the
                // stale reference under the same id rather than queueing a
                // second unanswerable command.
                self.retire(&command_id);
                vec![self.enqueue(LiveCommand::LoginApi {
                    command_id,
                    provider,
                    alias,
                    vault_reference,
                    attempt,
                })]
            }
            LiveReply::LoggedIn {
                command_id,
                identity,
            } => {
                self.retire(&command_id);
                // A retired attempt's late SUCCESS is as silent as its
                // failure (TUI6.4): consume the remembered id and move on.
                self.retired_logins.remove(&command_id);
                // TUI6.3 fix 1(c): the result lands only on the card that
                // ASKED — the command must be the live login command and
                // the open card must be the live attempt. A stale
                // LoggedIn (its attempt retired, or a newer card open)
                // touches nothing: the r3 probe had an old result marking
                // a NEW provider/alias card successful.
                let owns = self.login_command.as_ref() == Some(&command_id)
                    && self.login_attempt.is_some()
                    && model.login.as_ref().map(|card| card.attempt) == self.login_attempt;
                let provider = owns
                    .then(|| model.login.as_ref().map(|card| card.provider.clone()))
                    .flatten();
                if owns {
                    self.close_login(&command_id);
                    self.login_attempt = None;
                    model.login_result(Ok(identity));
                }
                if provider.as_deref() == Some("deepseek") {
                    self.models_requested.insert("deepseek".to_owned());
                    vec![
                        LiveCommand::AccountList,
                        LiveCommand::RefreshProviderModels {
                            provider: "deepseek".to_owned(),
                        },
                    ]
                } else {
                    Vec::new()
                }
            }
            LiveReply::Accounts {
                descriptors,
                revision,
            } => {
                let rows = descriptors
                    .iter()
                    .map(crate::app::AccountRow::from_descriptor)
                    .collect();
                if model.accounts.apply_snapshot(rows, revision) {
                    // Daemon truth landed: an unpinned identity follows the
                    // active account (W5f-2) so the first session can serve.
                    model.bootstrap_identity_from_daemon();
                    model.dirty = true;
                }
                // An active OAuth account with no catalog yet needs one
                // discovered before the picker or the bootstrap can work.
                self.provider_model_refreshes(model)
            }
            LiveReply::DeviceCandidates {
                discovery_disabled,
                candidates,
            } => {
                // Metadata only, wholesale (D2): the section renders what
                // the daemon reported and nothing else — an empty or
                // switched-off report simply keeps the section absent.
                model.device.apply(candidates, discovery_disabled);
                model.dirty = true;
                Vec::new()
            }
            LiveReply::DeviceImported {
                command_id,
                descriptor,
                revision: _,
            } => {
                self.retire(&command_id);
                if self
                    .pending_device_import
                    .as_ref()
                    .is_some_and(|(pending, _)| *pending == command_id)
                {
                    self.pending_device_import = None;
                }
                model.device.pending_import = None;
                // The receipt NAMES what the daemon committed; the rows
                // themselves land via the chained refresh below — nothing
                // is inserted here (D2's installs-nothing-locally law).
                // P1 MASK LAW: the identity rides the receipt MASKED-
                // ALWAYS (one authority — `mask_identity`); receipts are
                // transient chrome with no reveal loop of their own.
                let action = if model
                    .accounts
                    .rows
                    .iter()
                    .any(|row| row.alias == descriptor.alias.as_str())
                {
                    "re-adopted"
                } else {
                    "imported"
                };
                model.device.message = Some(format!(
                    "✓ {action} {} → {} · {}",
                    descriptor.provider,
                    descriptor.alias,
                    crate::format::mask_identity(&descriptor.identity)
                ));
                model.dirty = true;
                vec![LiveCommand::AccountList, LiveCommand::ProviderList]
            }
            LiveReply::AccountSelected {
                command_id,
                descriptor,
                revision,
            } => {
                self.retire(&command_id);
                if self
                    .pending_account_select
                    .as_ref()
                    .is_some_and(|(id, _)| *id == command_id)
                {
                    self.pending_account_select = None;
                }
                model.apply_account_selected(&descriptor, revision);
                Vec::new()
            }
            LiveReply::Providers {
                providers,
                revision,
            } => {
                if model.providers.apply_snapshot(providers, revision) {
                    // The provider snapshot may complete the bootstrap the
                    // account snapshot started (either order works).
                    model.bootstrap_identity_from_daemon();
                    // A PINNED identity skips the bootstrap but still wants
                    // its real declared window (W5g-1).
                    model.refresh_context_window();
                    model.dirty = true;
                }
                // Providers may have arrived before accounts — re-check
                // whether an active OAuth provider still needs its catalog.
                self.provider_model_refreshes(model)
            }
            LiveReply::ProviderModelsRefreshed { provider, revision } => {
                if model.providers.apply_models_refresh(provider, revision) {
                    // The catalog is here: NOW the bootstrap can adopt the
                    // provider's real default model (W5f-2d).
                    model.bootstrap_identity_from_daemon();
                    // A PINNED identity skips the bootstrap but still wants
                    // its real declared window (W5g-1).
                    model.refresh_context_window();
                    model.dirty = true;
                }
                Vec::new()
            }
            LiveReply::ModelSelected {
                command_id,
                session,
                provider,
                model: model_name,
                worker_generation,
            } => {
                self.retire(&command_id);
                if self
                    .pending_model_select
                    .as_ref()
                    .is_some_and(|(id, _, _, _)| *id == command_id)
                {
                    self.pending_model_select = None;
                }
                self.generations.insert(session, worker_generation);
                model.apply_model_selected(&provider, &model_name);
                Vec::new()
            }
            LiveReply::Renamed {
                command_id,
                session,
                title,
                worker_generation,
            } => {
                self.retire(&command_id);
                if self
                    .pending_rename
                    .as_ref()
                    .is_some_and(|(id, _)| *id == command_id)
                {
                    self.pending_rename = None;
                }
                self.generations.insert(session.clone(), worker_generation);
                model.apply_renamed(&session, title);
                Vec::new()
            }
            LiveReply::EffortSelected {
                command_id,
                session,
                effort,
                worker_generation,
            } => {
                self.retire(&command_id);
                if self
                    .pending_effort_select
                    .as_ref()
                    .is_some_and(|(id, _, _)| *id == command_id)
                {
                    self.pending_effort_select = None;
                }
                self.generations.insert(session, worker_generation);
                model.apply_effort_selected(effort.as_deref());
                Vec::new()
            }
            LiveReply::FastSelected {
                command_id,
                session,
                enabled,
                worker_generation,
            } => {
                self.retire(&command_id);
                if self
                    .pending_fast_select
                    .as_ref()
                    .is_some_and(|(id, _, _)| *id == command_id)
                {
                    self.pending_fast_select = None;
                }
                self.generations.insert(session, worker_generation);
                model.apply_fast_selected(enabled);
                Vec::new()
            }
            LiveReply::DefaultModelSet {
                command_id,
                provider,
                revision,
            } => {
                self.retire(&command_id);
                if self
                    .pending_default_model
                    .as_ref()
                    .is_some_and(|(id, _)| *id == command_id)
                {
                    self.pending_default_model = None;
                }
                model.apply_default_model_set(provider, revision);
                Vec::new()
            }
            LiveReply::ProviderConfigured {
                command_id,
                provider,
                revision,
            } => {
                self.retire(&command_id);
                let Some((_, attempt)) = self
                    .pending_custom
                    .take_if(|(pending, _)| *pending == command_id)
                else {
                    return Vec::new();
                };
                // Upsert semantics — the created profile joins the list
                // under the commit's revision.
                model.providers.apply_models_refresh(provider, revision);
                model.custom_add_committed(attempt);
                model.dirty = true;
                Vec::new()
            }
            LiveReply::OAuthStarted {
                attempt_id,
                availability,
                flow_id,
                authorization_url,
                provider_origin,
            } => {
                let Some(flight) = self
                    .oauth_flight
                    .as_mut()
                    .filter(|flight| flight.attempt_id == attempt_id)
                else {
                    return Vec::new(); // retired card's ghost — silent
                };
                if !availability.available {
                    let attempt = flight.attempt;
                    let reason = availability
                        .reason
                        .unwrap_or_else(|| "OAuth is unavailable for this provider".to_owned());
                    self.oauth_flight = None;
                    model.oauth_add_failed(attempt, &reason);
                    return Vec::new();
                }
                let (Some(flow_id), Some(url)) = (flow_id, authorization_url) else {
                    let attempt = flight.attempt;
                    self.oauth_flight = None;
                    model.oauth_add_failed(attempt, "the daemon returned no authorization URL");
                    return Vec::new();
                };
                flight.flow = Some(flow_id);
                flight.last_poll = Some(self.now);
                flight.url = url.clone();
                flight.origin = provider_origin.clone().unwrap_or_default();
                let attempt = flight.attempt;
                model.oauth_add_phase(
                    attempt,
                    crate::app::OAuthAddPhase::WaitingBrowser {
                        url: url.clone(),
                        origin: provider_origin.unwrap_or_default(),
                    },
                );
                // The authorize hop: open the user's browser (runtime effect).
                model.requests.push(AppRequest::OpenUrl { url });
                Vec::new()
            }
            LiveReply::OAuthStartFailed {
                attempt_id,
                code,
                message,
            } => {
                let Some(flight) = self
                    .oauth_flight
                    .take_if(|flight| flight.attempt_id == attempt_id)
                else {
                    return Vec::new(); // a retired card's ghost — silent
                };
                model.oauth_add_failed(flight.attempt, &format!("{code} — {message}"));
                Vec::new()
            }
            LiveReply::OAuthFlowStatus { flow_id, status } => {
                let Some(flight) = self
                    .oauth_flight
                    .as_mut()
                    .filter(|flight| flight.flow.as_ref() == Some(&flow_id))
                else {
                    return Vec::new();
                };
                let attempt = flight.attempt;
                match status {
                    haider_rpc::OAuthFlowStatusWire::WaitingBrowser => Vec::new(),
                    // B2b-m3 polish (c): the device grant reports its OWN
                    // waiting phase — mapped to device-honest copy ("enter
                    // the code at <verification url>"), never left to fall
                    // through the tolerant arm while the card shows the
                    // loopback's "your browser opened…" line. The reducer
                    // ignores a no-op re-report (the 1.5 s poll).
                    haider_rpc::OAuthFlowStatusWire::WaitingDevice => {
                        model.oauth_add_phase(
                            attempt,
                            crate::app::OAuthAddPhase::WaitingDevice {
                                url: flight.url.clone(),
                                origin: flight.origin.clone(),
                            },
                        );
                        Vec::new()
                    }
                    haider_rpc::OAuthFlowStatusWire::Exchanging => {
                        model.oauth_add_phase(attempt, crate::app::OAuthAddPhase::Exchanging);
                        Vec::new()
                    }
                    haider_rpc::OAuthFlowStatusWire::Ready {
                        oauth_reference, ..
                    } => {
                        if flight.add_command.is_some() {
                            return Vec::new(); // add already in flight
                        }
                        self.next_command += 1;
                        let command_id =
                            CommandId::new(format!("{}-{}", self.instance, self.next_command));
                        flight.add_command = Some(command_id.clone());
                        let command = LiveCommand::AccountAddOAuth {
                            command_id,
                            provider: flight.provider.clone(),
                            alias: flight.alias.clone(),
                            flow_id,
                            attempt_id: flight.attempt_id.clone(),
                            oauth_reference,
                        };
                        model.oauth_add_phase(attempt, crate::app::OAuthAddPhase::Adding);
                        vec![self.enqueue(command)]
                    }
                    haider_rpc::OAuthFlowStatusWire::Failed { public_code } => {
                        self.oauth_flight = None;
                        model
                            .oauth_add_failed(attempt, &format!("authorize failed: {public_code}"));
                        Vec::new()
                    }
                    haider_rpc::OAuthFlowStatusWire::Expired => {
                        self.oauth_flight = None;
                        model.oauth_add_failed(
                            attempt,
                            "the authorize window expired — start again",
                        );
                        Vec::new()
                    }
                    haider_rpc::OAuthFlowStatusWire::Cancelled => {
                        self.oauth_flight = None;
                        model.oauth_add_failed(attempt, "the flow was cancelled");
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            LiveReply::AccountAdded {
                command_id,
                descriptor,
            } => {
                self.retire(&command_id);
                if let Some(flight) = self
                    .oauth_flight
                    .take_if(|flight| flight.add_command.as_ref() == Some(&command_id))
                {
                    model.oauth_add_completed(flight.attempt, &descriptor);
                }
                Vec::new()
            }
            LiveReply::Event {
                attachment,
                session,
                envelope,
            } => self.on_event(model, &attachment, &session, &envelope),
            LiveReply::Lagged { attachment } => {
                // The daemon dropped us; reattach from OUR cursor, not from
                // the telemetry the frame carries.
                let Some(session) = self.routes.get(&attachment).cloned() else {
                    return Vec::new();
                };
                self.drop_attachment(&attachment);
                self.ensure_attached(model, &session)
            }
            LiveReply::CaughtUp {
                attachment,
                high_water_seq,
            } => {
                let Some(session) = self.routes.get(&attachment).cloned() else {
                    return Vec::new();
                };
                // The replay announced `H`; if the reducer never applied
                // that far, envelopes were lost between the daemon's sink
                // and our reducer, and the LAST ones leave no trace of
                // their own absence. Reattach from what we really applied.
                if cursor_of(model, &session).unwrap_or(0) >= high_water_seq {
                    return Vec::new();
                }
                self.drop_attachment(&attachment);
                let mut commands = vec![LiveCommand::Detach { attachment }];
                commands.extend(self.ensure_attached(model, &session));
                commands
            }
            LiveReply::EventsLost { count } => {
                model.flash = Some(format!("· resynchronizing — {count} frames dropped"));
                model.dirty = true;
                // Working-set order, so the command stream is deterministic
                // (a `HashMap` walk is not).
                let held: Vec<(SessionId, AttachmentId)> = self
                    .lru
                    .iter()
                    .filter_map(|session| {
                        self.attachments
                            .get(session)
                            .map(|attachment| (session.clone(), attachment.clone()))
                    })
                    .collect();
                let mut commands = Vec::new();
                for (session, attachment) in held {
                    self.drop_attachment(&attachment);
                    commands.push(LiveCommand::Detach { attachment });
                    commands.extend(self.ensure_attached(model, &session));
                }
                commands
            }
            LiveReply::AttachFailed {
                session,
                code,
                message,
                retryable,
            } => {
                // THE WORKING SET HOLDS NO GHOSTS (W3c3.1 r2, P1-B). While
                // connected, every `lru` member is either attached or has
                // an attach in flight — `ensure_attached` now claims the
                // slot at REQUEST time, so a failure that released only the
                // latch left a member that was neither. Nothing retried it,
                // and worse: at cap it became `evictable`, whereupon
                // `ensure_attached` found no attachment to detach and
                // silently refused EVERY later attach. One background
                // `overloaded` during a reconnect could make the attached
                // surface permanently unattachable, with nothing on screen
                // to say why. The slot is released either way; a retry
                // re-claims it.
                self.release_slot(&session);
                self.cold.insert(
                    session.clone(),
                    Cold {
                        head_seq: cursor_of(model, &session).unwrap_or(0),
                    },
                );
                // Retryable classes (overloaded, a transient cap) are worth
                // one more try on the next loop pass; a permanent one is
                // reported and the row stays cold rather than pretending.
                model.flash = Some(format!("· attach {code} — {message}"));
                model.dirty = true;
                // Only the ATTACHED SURFACE retries here. A background
                // session that took a transient `overloaded` goes cold and
                // is re-attached by the next selection, which is reachable
                // and visible; retrying every member of a 16-session
                // working set against a daemon that just said "overloaded"
                // would be a client-side amplification of its overload.
                if retryable && model.active_session.as_ref() == Some(&session) {
                    return self.ensure_attached(model, &session);
                }
                // A PERMANENT refusal of the attached surface DESELECTS it
                // (W3c3.2). The daemon said retrying is futile, but the
                // loop tail's `sync_selection` re-attaches whatever is
                // selected — so leaving the row selected turns every
                // failure reply into the next attach, an infinite
                // attach/fail ping-pong at wire speed (reachable with a
                // stale roster: a session another store knew, attached
                // after a reconnect). Deselecting keeps the arm's own
                // promise — the row stays COLD, the flash names the code,
                // and every later retry is the user's own selection, which
                // the released latch accepts cleanly.
                if !retryable && model.active_session.as_ref() == Some(&session) {
                    model.back_to_launcher();
                }
                Vec::new()
            }
            LiveReply::StageFailed {
                attempt,
                code,
                message,
            } => {
                // TUI6.4: the stage-level error arrives identity-tagged
                // (the link's context knows which attempt staged), so the
                // LIVE attempt's card takes the recovery immediately —
                // the 6.3b liveness case, now solved by identity instead
                // of the positional pop r4 proved unsound. A ghost
                // attempt's error dies silently: no card paint, no flash
                // (the user already saw the cancel).
                let live = self.login_attempt == Some(attempt)
                    && model.login.as_ref().map(|card| card.attempt) == Some(attempt);
                if live {
                    self.login_started = None;
                    self.login_attempt = None;
                    model.login_result(Err((code, message)));
                    model.dirty = true;
                }
                Vec::new()
            }
            LiveReply::Failed {
                command_id,
                code,
                message,
                retryable,
            } => {
                // TUI6.4 (review r4): no-id failures NEVER touch login
                // state — 6.3b's positional consumption is REMOVED, and
                // stage-level errors arrive as the identity-tagged
                // `StageFailed` instead. The honest residual: an
                // uncorrelated ProtocolError that kills a stage leaves no
                // reply to correlate at all — the stage's answer simply
                // never arrives and the 30s deadline abandons with the
                // retype recovery. Fail-closed (nothing ever mints on
                // ambiguity), deadline-bounded, and no longer guessable
                // into the wrong attempt.
                //
                // r4's P2: a late reply for a RETIRED login command is
                // consumed SILENTLY — no card paint, no flash. The retire
                // remembered the id precisely so this reply could be told
                // apart from a genuinely unrelated failure (which still
                // deserves its flash below).
                if let Some(id) = &command_id
                    && self.retired_logins.remove(id)
                {
                    self.retire(id);
                    model.dirty = true;
                    return Vec::new();
                }
                // A failed OAuth `account.add` fails the card in place.
                if let Some(id) = &command_id
                    && let Some(flight) = self
                        .oauth_flight
                        .take_if(|flight| flight.add_command.as_ref() == Some(id))
                {
                    if !retryable {
                        self.retire(id);
                    }
                    model.oauth_add_failed(flight.attempt, &message);
                    return Vec::new();
                }
                // W10b: a failed removal surfaces the daemon's typed
                // reason on its screen (builtin refusal, blocking aliases,
                // revision conflict) — the client never re-judges.
                if let Some(id) = &command_id
                    && self
                        .pending_account_remove
                        .as_ref()
                        .is_some_and(|(pending, _)| pending == id)
                    && let Some((_, alias)) = self.pending_account_remove.take()
                {
                    if !retryable {
                        self.retire(id);
                    }
                    model.accounts.message = Some(format!("`{alias}` not removed — {message}"));
                    model.dirty = true;
                    return Vec::new();
                }
                // D2: a failed `account.import_device` releases the exact
                // pending candidate and lands the daemon's honest typed
                // reason inside the section (both screens render it).
                // Nothing moved locally, so nothing rolls back.
                if let Some(id) = &command_id
                    && self
                        .pending_device_import
                        .as_ref()
                        .is_some_and(|(pending, _)| pending == id)
                    && self.pending_device_import.take().is_some()
                {
                    if !retryable {
                        self.retire(id);
                    }
                    model.device.pending_import = None;
                    model.device.message = Some(format!("✗ import failed — {message}"));
                    model.dirty = true;
                    return Vec::new();
                }
                if let Some(id) = &command_id
                    && self
                        .pending_provider_remove
                        .as_ref()
                        .is_some_and(|(pending, _)| pending == id)
                    && let Some((_, provider)) = self.pending_provider_remove.take()
                {
                    if !retryable {
                        self.retire(id);
                    }
                    model.providers.message = Some(format!("`{provider}` not removed — {message}"));
                    model.dirty = true;
                    return Vec::new();
                }
                // A failed `session.select_model` (F2a): the typed public
                // reason lands on the exact selection that asked — inline
                // in the picker when it is open, as a session-view error
                // line otherwise. The row stays selectable for a retry.
                if let Some(id) = &command_id
                    && self
                        .pending_model_select
                        .as_ref()
                        .is_some_and(|(pending, _, _, _)| pending == id)
                    && let Some((_, session, provider, model_name)) =
                        self.pending_model_select.take()
                {
                    if !retryable {
                        self.retire(id);
                    }
                    if code == haider_rpc::ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED {
                        model.cache_epoch_confirmation_required(
                            crate::app::PendingCacheChange::Model {
                                session,
                                provider,
                                model: model_name,
                            },
                            &message,
                        );
                    } else {
                        model.model_select_failed(&provider, &model_name, &code, &message);
                    }
                    return Vec::new();
                }
                // A failed `session.rename` (G2): the typed public reason
                // lands on the exact session that asked, never a silent
                // IDLE.
                if let Some(id) = &command_id
                    && self
                        .pending_rename
                        .as_ref()
                        .is_some_and(|(pending, _)| pending == id)
                    && let Some((_, session)) = self.pending_rename.take()
                {
                    if !retryable {
                        self.retire(id);
                    }
                    model.rename_failed(&session, &code, &message);
                    return Vec::new();
                }
                // A failed `session.select_effort` (G3): the typed public
                // reason lands inline in the picker when it is open, as a
                // session-view error line otherwise.
                if let Some(id) = &command_id
                    && self
                        .pending_effort_select
                        .as_ref()
                        .is_some_and(|(pending, _, _)| pending == id)
                    && let Some((_, session, effort)) = self.pending_effort_select.take()
                {
                    if !retryable {
                        self.retire(id);
                    }
                    if code == haider_rpc::ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED {
                        model.cache_epoch_confirmation_required(
                            crate::app::PendingCacheChange::Effort { session, effort },
                            &message,
                        );
                    } else {
                        model.effort_select_failed(&code, &message);
                    }
                    return Vec::new();
                }
                // A failed `session.select_fast` (G3).
                if let Some(id) = &command_id
                    && self
                        .pending_fast_select
                        .as_ref()
                        .is_some_and(|(pending, _, _)| pending == id)
                    && let Some((_, session, enabled)) = self.pending_fast_select.take()
                {
                    if !retryable {
                        self.retire(id);
                    }
                    if code == haider_rpc::ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED {
                        model.cache_epoch_confirmation_required(
                            crate::app::PendingCacheChange::Fast { session, enabled },
                            &message,
                        );
                    } else {
                        model.fast_select_failed(&code, &message);
                    }
                    return Vec::new();
                }
                // A failed `account.set_default_model` releases its gate;
                // a revision_conflict also refreshes (the CAS proved the
                // snapshot stale).
                if let Some(id) = &command_id
                    && self
                        .pending_default_model
                        .as_ref()
                        .is_some_and(|(pending, _)| pending == id)
                    && let Some((_, provider)) = self.pending_default_model.take()
                {
                    if !retryable {
                        self.retire(id);
                    }
                    let refresh = code == haider_rpc::ERROR_CODE_REVISION_CONFLICT;
                    model.default_model_failed(&provider, &message, refresh);
                    return Vec::new();
                }
                // A failed `provider.configure` returns the card to its
                // editable fields with the public reason (W5g-4). A
                // revision_conflict also refreshes the snapshot so the
                // retry submits under fresh truth.
                if let Some(id) = &command_id
                    && self
                        .pending_custom
                        .as_ref()
                        .is_some_and(|(pending, _)| pending == id)
                    && let Some((_, attempt)) = self.pending_custom.take()
                {
                    if !retryable {
                        self.retire(id);
                    }
                    model.custom_add_failed(attempt, &message);
                    if code == haider_rpc::ERROR_CODE_REVISION_CONFLICT {
                        return vec![self.enqueue(LiveCommand::ProviderList)];
                    }
                    return Vec::new();
                }
                // A failed hook trust/revoke lands its typed reason on the
                // hooks screen and releases the one-at-a-time gate (H4).
                // Nothing moved locally, so nothing rolls back.
                if let Some(id) = &command_id
                    && self
                        .pending_hook_trust
                        .as_ref()
                        .is_some_and(|(pending, _)| pending == id)
                    && let Some((_, digest)) = self.pending_hook_trust.take()
                {
                    if !retryable {
                        self.retire(id);
                    }
                    model.hooks.trust_failed(&digest, &message);
                    model.dirty = true;
                    return Vec::new();
                }
                // A failed `account.set_active` clears its exact pending
                // row (W5d) — the model's rows never moved, so this is
                // gate-release + honest message, no rollback.
                if let Some(id) = &command_id
                    && self
                        .pending_account_select
                        .as_ref()
                        .is_some_and(|(pending, _)| pending == id)
                    && let Some((_, alias)) = self.pending_account_select.take()
                {
                    if !retryable {
                        self.retire(id);
                    }
                    if code == haider_rpc::ERROR_CODE_CACHE_EPOCH_CONFIRMATION_REQUIRED {
                        model.cache_epoch_confirmation_required(
                            crate::app::PendingCacheChange::Account { alias },
                            &message,
                        );
                    } else {
                        model.account_select_failed(&alias, &message);
                    }
                    return Vec::new();
                }
                // A rejected TURN must release the optimistic mid-turn UI,
                // or the session sits in "running" forever with no envelope
                // ever coming to clear it — and Esc, which now cancels only
                // a run the stream named, has nothing to cancel (P1-4).
                if let Some(id) = &command_id {
                    let submit_session = self.outbox.iter().find_map(|pending| {
                        if &pending.command_id != id {
                            return None;
                        }
                        match &pending.command {
                            LiveCommand::Submit { session, .. } => Some(session.clone()),
                            _ => None,
                        }
                    });
                    if let Some(session) = submit_session {
                        model.turn_active = false;
                        // F2e: the rejected turn's public reason reaches
                        // the SESSION VIEW, not just a transient flash —
                        // an api/oauth/endpoint rejection must never end
                        // as a silent IDLE.
                        model.record_session_error(
                            &session,
                            format!("submit rejected — {code}: {message}"),
                        );
                    }
                    if !retryable {
                        self.retire(id);
                    }
                }
                // A card that is waiting takes the typed recovery text —
                // but ONLY for a failure that belongs to it. "A card is
                // open" is not correlation: an unrelated `capability_denied`
                // would otherwise show a login recovery message for a
                // failure that had nothing to do with the login (P2-2).
                let owns = command_id.as_ref() == self.login_command.as_ref()
                    && self.login_command.is_some()
                    && model.login.as_ref().map(|card| card.attempt) == self.login_attempt
                    && self.login_attempt.is_some();
                if owns && model.login.is_some() {
                    model.login_result(Err((code, message)));
                } else {
                    model.flash = Some(format!("· {code} — {message}"));
                }
                if let Some(id) = &command_id
                    && !retryable
                {
                    // A permanent failure ends the transaction; a retryable
                    // one keeps the id so the retype re-stages UNDER IT.
                    self.close_login(id);
                }
                if owns {
                    self.login_started = None;
                }
                model.dirty = true;
                Vec::new()
            }
            LiveReply::Disconnected { reason } => {
                self.connected = false;
                self.attaching.clear();
                // The run survives the socket; the ATTACHMENT does not.
                // `active_run` is stream-derived and the reattach replays
                // whatever moved while we were away.
                // Every attachment died with the socket, but the WORKING SET
                // — which sessions we want attached, in priority order — is
                // exactly what the resume has to restore, so `lru` survives
                // the disconnect. Clearing it here is how a reconnect
                // silently comes back attached to nothing but the active
                // session.
                self.attachments.clear();
                self.routes.clear();
                // The login's staged secret died with the socket: staging is
                // connection-scoped and deliberately non-durable, so nothing
                // can retry it and no response will ever arrive. A card left
                // at "validating…" refuses input until Esc, which is a dead
                // end the user has no way to read (review W3c3 D2-5).
                self.abandon_login(model, "the connection dropped mid-validation");
                // No reply crosses a socket: retired ids awaiting silent
                // consumption die with the connection (TUI6.4).
                self.retired_logins.clear();
                // B4b: `artifact.put` is receipt-free — an in-flight
                // upload's reply can never arrive and nothing resends it,
                // so its chip dies HERE, named, instead of spinning
                // "uploading" forever.
                let dropped = model.drop_uploading_attachments();
                model.flash = Some(if dropped > 0 {
                    format!(
                        "· reconnecting — {reason} ({dropped} in-flight attachment upload(s) dropped — /attach again)"
                    )
                } else {
                    format!("· reconnecting — {reason}")
                });
                model.dirty = true;
                Vec::new()
            }
            LiveReply::Reconnected => {
                self.connected = true;
                model.flash = None;
                model.dirty = true;
                self.resume(model)
            }
            LiveReply::Draining { reason } => {
                model.flash = Some(format!("· daemon draining — {reason}"));
                model.dirty = true;
                Vec::new()
            }
            // T2 transcription-secret answers: pure reducer routing — the
            // intent recorded at issuance decides where each one lands
            // (talk start, setup presence, setup probe). No driver state.
            LiveReply::TranscriptionSecret { secret } => {
                model.talk_secret_answer(secret);
                Vec::new()
            }
            LiveReply::TranscriptionSecretStored { present } => {
                model.talk_secret_stored(present);
                Vec::new()
            }
            LiveReply::TranscriptionSecretFailed { op, message } => {
                model.talk_secret_failed(op, message);
                Vec::new()
            }
        }
    }

    /// End the open login transaction, if `command_id` is the one it owns.
    fn close_login(&mut self, command_id: &CommandId) {
        if self.login_command.as_ref() == Some(command_id) {
            self.login_command = None;
            self.login_started = None;
            self.login_attempt = None;
        }
    }

    /// Give up on an in-flight login and hand the card an honest recovery.
    ///
    /// The staged reference is single-use, connection-scoped and holds the
    /// only copy of the key (the card wiped its own on submit), so there is
    /// nothing to resend: the only recovery is a retype, and saying so is
    /// the whole job.
    fn abandon_login(&mut self, model: &mut AppModel, why: &str) {
        // TUI6.3b lesson, restated for the identity mechanism (TUI6.4):
        // the attempt binding dies on EVERY abandon path, BEFORE the
        // early-return — there is no positional queue left to strand,
        // and a stage reply that arrives after an abandon fails the
        // identity gate on the dead binding.
        self.login_attempt = None;
        if self.login_started.is_none() && self.login_command.is_none() {
            return;
        }
        self.login_started = None;
        if let Some(id) = self.login_command.take() {
            self.retire(&id);
            // Its reply may still arrive on a LIVE connection (timeout
            // abandon): consume it silently when it does.
            self.retired_logins.insert(id);
        }
        if model.login.is_some() {
            model.login_result(Err((
                haider_rpc::ERROR_CODE_RESTAGE_REQUIRED.to_owned(),
                why.to_owned(),
            )));
        }
    }

    /// The next instant this driver has something to do with no inbound
    /// reply to trigger it — the shell's wakeup, so a deadline is BOUNDED
    /// rather than dependent on unrelated traffic (W3c3.1 r2, P2-B).
    ///
    /// `expire_login` used to run only when `live_pass` happened to run,
    /// and `live_pass` runs only when the select loop wakes: a keypress, a
    /// reply, or a tick gated on `model.dirty`/`model.animated()` — none of
    /// which a quiet terminal with a wedged daemon produces. The card sat
    /// at "validating…", closed to input, until the user pressed a key.
    #[must_use]
    pub fn next_deadline(&self) -> Option<std::time::Instant> {
        let login = self
            .login_started
            .map(|started| started + LOGIN_STAGE_TIMEOUT);
        // The OAuth poll cadence: wake when the next status poll is due.
        let oauth = self
            .oauth_flight
            .as_ref()
            .filter(|flight| flight.flow.is_some() && flight.add_command.is_none())
            .and_then(|flight| flight.last_poll)
            .map(|last| last + OAUTH_POLL_INTERVAL);
        match (login, oauth) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The OAuth poll sweep (W5e-1), driven by the same pass that expires
    /// logins: when the card is waiting on the browser and the cadence is
    /// due, emit one `account.oauth_status`.
    pub(crate) fn oauth_poll(&mut self) -> Vec<LiveCommand> {
        let Some(flight) = self
            .oauth_flight
            .as_mut()
            .filter(|flight| flight.add_command.is_none())
        else {
            return Vec::new();
        };
        let Some(flow_id) = flight.flow.clone() else {
            return Vec::new();
        };
        let due = flight
            .last_poll
            .is_none_or(|last| self.now.duration_since(last) >= OAUTH_POLL_INTERVAL);
        if !due {
            return Vec::new();
        }
        flight.last_poll = Some(self.now);
        vec![LiveCommand::OAuthStatus {
            flow_id,
            attempt_id: flight.attempt_id.clone(),
        }]
    }

    /// The pass's deadline sweep — see [`LOGIN_STAGE_TIMEOUT`]. Driven by
    /// [`crate::runtime::live_pass`], which stamps [`Self::set_now`] first.
    pub(crate) fn expire_login(&mut self, model: &mut AppModel) {
        if self
            .login_started
            .is_some_and(|started| self.now.duration_since(started) >= LOGIN_STAGE_TIMEOUT)
        {
            self.abandon_login(model, "validation timed out");
        }
    }

    /// One committed envelope: route it, record menu coordinates, and honor
    /// the reducer's strict gap law.
    fn on_event(
        &mut self,
        model: &mut AppModel,
        attachment: &AttachmentId,
        session: &SessionId,
        envelope: &RawEnvelope,
    ) -> Vec<LiveCommand> {
        // Report §6.3: unknown attachment ids are rejected. An event for an
        // attachment we do not hold is a frame from a detached (or
        // never-established) subscription; applying it would resurrect a
        // session we deliberately let go cold.
        let Some(routed) = self.routes.get(attachment) else {
            return Vec::new();
        };
        if routed != session || &envelope.session_id != session {
            return Vec::new();
        }
        self.touch(session);
        // Driver-side bookkeeping happens ONLY for envelopes the reducer
        // actually applied (review P1: `record_menu` used to run ahead of
        // the strict gate, so a re-delivered `MenuOpened` reset a menu's
        // durable command id — breaking the same-command retry law — and a
        // GAPPED `RunState` set a cancel target the user's screen never
        // showed).
        // ONE reattach authority. `route_raw` pushes `AppRequest::Reattach`
        // on a gap and `handle_request` performs the detach+attach; issuing
        // a second pair here would open an attachment the daemon never
        // hears a detach for — a permanent slot against its 16-per-
        // connection ceiling, plus duplicate delivery of every later
        // envelope for that session (review P1-3).
        if model.route_raw(envelope) == RawOutcome::Applied {
            self.record_menu(session, envelope);
            self.apply_tuning_fact(model, session, envelope);
        }
        Vec::new()
    }

    /// G3: committed `effort_selected`/`fast_mode_selected` facts populate
    /// the identity's tuning segment for the ACTIVE session — this is what
    /// makes "daemon truth on session attach" real: an attach replays the
    /// journal, and the latest fact wins in replay order. Select replies
    /// land the same values; both writers agree because both are committed
    /// daemon truth.
    fn apply_tuning_fact(&self, model: &mut AppModel, session: &SessionId, envelope: &RawEnvelope) {
        if model.active_session.as_ref() != Some(session) {
            return;
        }
        let Ok(payload) = serde_json::from_value::<
            haider_protocol::session::SessionConfigEventPayload,
        >(envelope.payload.clone()) else {
            return;
        };
        match payload {
            haider_protocol::session::SessionConfigEventPayload::EffortSelected(selected) => {
                if model.identity.reasoning != selected.effort {
                    model.identity.reasoning = selected.effort;
                    model.dirty = true;
                }
            }
            haider_protocol::session::SessionConfigEventPayload::FastModeSelected(selected) => {
                if model.identity.fast != selected.enabled {
                    model.identity.fast = selected.enabled;
                    model.dirty = true;
                }
            }
            haider_protocol::session::SessionConfigEventPayload::ModelSelected(_) => {}
            // G2 renames land through the correlated `session.rename` reply
            // and `session.list` summaries, never this raw-fact lane.
            haider_protocol::session::SessionConfigEventPayload::SessionRenamed { .. } => {}
        }
    }

    /// Record a menu's COMMITTED opening coordinates, and retire its answer
    /// from the outbox once the resolution itself is committed.
    ///
    /// Coordinates come from the opening envelope, before reduction, so an
    /// answer can be built even if the reducer routes the card into a
    /// subagent transcript. Retirement comes from the committed
    /// `MenuAnswered`/`MenuClosed`, which is the ONLY authority that a
    /// durable answer landed — a correlated echo would be a second one.
    fn record_menu(&mut self, session: &SessionId, envelope: &RawEnvelope) {
        let Ok(payload) =
            serde_json::from_value::<haider_protocol::EventPayload>(envelope.payload.clone())
        else {
            return;
        };
        match payload {
            haider_protocol::EventPayload::MenuOpened(menu) => {
                self.menus.insert(
                    menu.id.clone(),
                    MenuCoordinates {
                        session: session.clone(),
                        request_seq: envelope.seq,
                        worker_generation: envelope.worker_generation,
                        command_id: None,
                    },
                );
            }
            haider_protocol::EventPayload::MenuAnswered(answer) => {
                self.retire_menu(&answer.menu);
            }
            haider_protocol::EventPayload::MenuClosed { menu, .. } => self.retire_menu(&menu),
            // The run a cancel would name. A terminal state releases it, so
            // Esc after a turn ends cancels nothing rather than a later run.
            haider_protocol::EventPayload::RunState(state) => {
                if state.is_terminal() {
                    self.active_run.remove(session);
                } else if let Some(run) = envelope.run_id.clone() {
                    self.active_run.insert(session.clone(), run);
                }
            }
            _ => {}
        }
    }

    fn retire_menu(&mut self, menu: &MenuId) {
        if let Some(coordinates) = self.menus.remove(menu)
            && let Some(command_id) = coordinates.command_id
        {
            self.retire(&command_id);
        }
    }

    /// Rebuild the world on a fresh connection: reattach every session that
    /// was in the working set, each from ITS OWN last applied cursor, then
    /// resend the outbox under its durable ids.
    /// Discover the catalog for any ACTIVE OAuth account whose provider has
    /// no models yet (W5f-2d): the picker and the identity bootstrap both
    /// need real slugs, and the fetch needs the account's token. One request
    /// per provider per connection — the dedup set stops a snapshot storm.
    fn provider_model_refreshes(&mut self, model: &AppModel) -> Vec<LiveCommand> {
        let mut commands = Vec::new();
        for row in &model.accounts.rows {
            // Any SELECTED account whose provider has no catalog asks for
            // one — OAuth subscriptions AND api-key accounts alike (W5g-5:
            // a custom provider's models discover from its origin under
            // its key, and nothing else would ever trigger that).
            if !row.selected {
                continue;
            }
            let has_models = model
                .providers
                .providers
                .iter()
                .find(|summary| summary.provider == row.provider)
                .is_some_and(|summary| !summary.models.is_empty());
            if has_models || self.models_requested.contains(&row.provider) {
                continue;
            }
            self.models_requested.insert(row.provider.clone());
            commands.push(LiveCommand::RefreshProviderModels {
                provider: row.provider.clone(),
            });
        }
        // G4a: KEYLESS custom providers have no account row at all — their
        // discovery trigger is the provider summary itself: an enabled
        // chat-completions custom with a stored origin, no auth methods,
        // and no models yet asks once per connection.
        for summary in &model.providers.providers {
            let keyless = matches!(
                summary.api_family,
                haider_rpc::ProviderApiFamilyWire::OpenAiChatCompletions
            ) && summary.endpoint.is_some()
                && summary.auth_methods.is_empty()
                && summary.enabled;
            if !keyless
                || !summary.models.is_empty()
                || self.models_requested.contains(&summary.provider)
            {
                continue;
            }
            self.models_requested.insert(summary.provider.clone());
            commands.push(LiveCommand::RefreshProviderModels {
                provider: summary.provider.clone(),
            });
        }
        commands
    }

    fn resume(&mut self, model: &mut AppModel) -> Vec<LiveCommand> {
        // A fresh socket's daemon may answer discovery differently — let it
        // be asked again (W5f-2d).
        self.models_requested.clear();
        // Account + provider truth ride the connect (W5f-2): the identity
        // bootstrap fires when their snapshots APPLY, so without asking at
        // the front door the launcher would sit on demo seeds until the
        // user happened to open /accounts. Reads, never in the outbox.
        let mut commands = vec![
            LiveCommand::List { cursor: None },
            LiveCommand::AccountList,
            LiveCommand::ProviderList,
        ];
        // Priority order: the attached surface first, then the sessions the
        // user is waiting on, then the rest — so a capped working set
        // rebuilds the ones that matter.
        let mut wanted: Vec<SessionId> = self.lru.clone();
        if let Some(active) = model.active_session.clone()
            && !wanted.contains(&active)
        {
            wanted.push(active);
        }
        wanted.sort_by_key(|session| {
            let active = model.active_session.as_ref() == Some(session);
            let hot = is_hot(model, session);
            (!active, !hot)
        });
        // The working set is REBUILT, not cleared: a second disconnect
        // before these attaches are acknowledged must not collapse it to
        // the active session alone (review P2-3).
        self.lru = wanted.iter().take(ATTACHMENT_CAP).cloned().collect();
        // Through the SINGLE emitter, so a reconnect races nothing: the
        // loop tail's `sync_selection` finds the active session already
        // latched and adds no second attach (review W3c3 D1-1 case C).
        for session in self.lru.clone() {
            commands.extend(self.ensure_attached(model, &session));
        }
        // A pending `LoginApi` CANNOT ride the reconnect: its
        // `vault_reference` is single-use and connection-scoped, so the
        // fresh socket's daemon has never heard of it, and the card wiped
        // the key on submit so nothing can re-stage it either. Resending it
        // verbatim on every reconnect — which is what used to happen — buys
        // a guaranteed `restage_required` and leaves the entry pending
        // forever (review W3c3 D2-5).
        self.abandon_login(model, "the connection dropped mid-validation");
        // Session-scoped mutations WAIT for their attachment: `turn.submit`,
        // `turn.cancel` and `MenuAnswer` all require an established control
        // attachment, and a resend issued alongside the attach races it into
        // a non-retryable `capability_denied` (review P1-4 — the same race
        // the create→attach→submit path already fixed). Unscoped commands
        // have no such dependency and go now.
        commands.extend(
            self.outbox
                .iter()
                .filter(|pending| command_session(&pending.command).is_none())
                .map(|pending| pending.command.clone()),
        );
        commands
    }

    // ----------------------------------------------------------- requests --

    /// Translate one reducer request into live RPCs (report R11 cut 4:
    /// "the common `AppModel` emits semantic requests").
    pub fn handle_request(
        &mut self,
        model: &mut AppModel,
        request: AppRequest,
    ) -> Vec<LiveCommand> {
        let label = demo_only_label(&request);
        match request {
            AppRequest::CreateSession { text } => {
                let command_id = self.mint();
                self.creating.insert(command_id.clone(), text.clone());
                vec![self.enqueue(LiveCommand::Create {
                    command_id,
                    cwd: model.cwd.clone(),
                    provider: model.identity.provider.clone(),
                    model: model.identity.model_short.clone(),
                    max_tokens: session_output_cap(model.identity.context_window),
                    first_text: text,
                })]
            }
            AppRequest::SubmitText {
                text,
                branch,
                attachments,
                ..
            } => match model.active_session.clone() {
                // The branch AND the attachments were captured at ISSUANCE
                // by the reducer — a switch (or a chip removal) between
                // issuance and this drain must not retarget the turn
                // (research risk 4; B4b extends the same law to blocks).
                Some(session) => vec![self.submit(model, &session, text, branch, attachments)],
                None => Vec::new(),
            },
            // S3: the chip composer rides S1's `agent.message` wire. The
            // AGENT was captured at issuance by the reducer (the viewed
            // chip); the daemon owns delivery (steer vs queued), the
            // journal facts paint the rows, and the receipt's flash is
            // the only client-side touch.
            AppRequest::ChipSubmit { agent, text } => match model.active_session.clone() {
                Some(session) => {
                    let command_id = self.mint();
                    let worker_generation =
                        self.generations.get(&session).copied().unwrap_or_default();
                    vec![self.enqueue(LiveCommand::AgentMessage {
                        command_id,
                        session,
                        worker_generation,
                        agent,
                        text,
                    })]
                }
                None => Vec::new(),
            },
            // B4b: the receipt-free upload — no mint, no outbox (see the
            // `LiveCommand::ArtifactPut` charter). The reducer already
            // chipped the draft; only the reply mutates it further.
            AppRequest::AttachUpload {
                upload,
                surface,
                bytes,
            } => vec![LiveCommand::ArtifactPut {
                upload,
                surface,
                bytes,
            }],
            AppRequest::LoginApi {
                attempt,
                provider,
                alias,
                secret,
            } => {
                // The stage id is an EPHEMERAL same-connection retry nonce,
                // not a durable key: the same id with the same bytes
                // returns the same reference, and a reconnect must stage
                // afresh because the daemon's staged memory is
                // connection-scoped.
                self.next_command += 1;
                // TUI6.3 fix 1: bind the driver to THIS attempt — every
                // later reply must correlate to it or die. TUI6.4: the
                // stage COMMAND carries the attempt too, so its reply
                // comes back identity-tagged by the link's context.
                self.login_attempt = Some(attempt);
                // The card is now UNANSWERABLE-BY-ITSELF until a response
                // lands: arm the deadline that covers BOTH transactions.
                self.login_started = Some(self.now);
                vec![LiveCommand::Stage {
                    stage_id: format!("{}-stage-{}", self.instance, self.next_command),
                    secret,
                    provider,
                    alias,
                    attempt,
                }]
            }
            AppRequest::LoginRetired { attempt } => {
                // TUI6.3 fix 1(b): the card closed — retire the attempt.
                // Whatever the transaction had in flight (a stage awaiting
                // its reference, the login command awaiting commit, the
                // deadline) is invalidated; the reply gates then drop the
                // ghosts silently — stage replies by their own carried
                // attempt (TUI6.4), login replies by the retired command
                // id remembered below (r4's P2: a late Failed for a
                // retired id must not even flash).
                if self.login_attempt == Some(attempt) {
                    self.login_attempt = None;
                    self.login_started = None;
                    if let Some(id) = self.login_command.take() {
                        self.retire(&id);
                        self.retired_logins.insert(id);
                    }
                }
                Vec::new()
            }
            // `/accounts` (W5d): a read — never in the outbox.
            AppRequest::AccountsRefresh => vec![LiveCommand::AccountList],
            // A read — never outboxed; the reducer already gated on the
            // feature bit and pushes it on screen entry only (D2).
            AppRequest::DeviceCandidatesRefresh => vec![LiveCommand::DeviceCandidates],
            AppRequest::DeviceImport { candidate } => {
                let command_id = self.mint();
                self.pending_device_import = Some((command_id.clone(), candidate.clone()));
                vec![self.enqueue(LiveCommand::DeviceImport {
                    command_id,
                    candidate,
                })]
            }
            AppRequest::ProvidersRefresh => vec![LiveCommand::ProviderList],
            AppRequest::OAuthAddStart {
                provider,
                alias,
                attempt,
            } => {
                self.next_command += 1;
                let attempt_id = format!("{}-oauth-{}", self.instance, self.next_command);
                self.oauth_flight = Some(OAuthFlight {
                    attempt,
                    attempt_id: attempt_id.clone(),
                    provider: provider.clone(),
                    alias: alias.clone(),
                    flow: None,
                    last_poll: None,
                    url: String::new(),
                    origin: String::new(),
                    add_command: None,
                });
                vec![LiveCommand::OAuthStart {
                    provider,
                    desired_alias: alias,
                    attempt_id,
                }]
            }
            AppRequest::OAuthAddCancel { attempt } => {
                let Some(flight) = self
                    .oauth_flight
                    .take_if(|flight| flight.attempt == attempt)
                else {
                    return Vec::new();
                };
                match flight.flow {
                    Some(flow_id) => vec![LiveCommand::OAuthCancel {
                        flow_id,
                        attempt_id: flight.attempt_id,
                    }],
                    None => Vec::new(),
                }
            }
            // OpenUrl is runtime-owned (like CopySelection) — never a wire
            // command; reaching here means a headless drain: no-op.
            AppRequest::OpenUrl { .. } => Vec::new(),
            AppRequest::SetDefaultModel {
                provider,
                model,
                expected_revision,
            } => {
                let command_id = self.mint();
                self.pending_default_model = Some((command_id.clone(), provider.clone()));
                vec![self.enqueue(LiveCommand::SetDefaultModel {
                    command_id,
                    provider,
                    model,
                    expected_revision,
                })]
            }
            AppRequest::SelectModel {
                session,
                model: model_name,
                provider,
                confirm_new_epoch,
            } => {
                let command_id = self.mint();
                let worker_generation = self.generations.get(&session).copied().unwrap_or_default();
                self.pending_model_select = Some((
                    command_id.clone(),
                    session.clone(),
                    provider.clone(),
                    model_name.clone(),
                ));
                vec![self.enqueue(LiveCommand::SelectModel {
                    command_id,
                    session,
                    worker_generation,
                    model: model_name,
                    provider,
                    confirm_new_epoch,
                })]
            }
            AppRequest::Rename { session, title } => {
                let command_id = self.mint();
                let worker_generation = self.generations.get(&session).copied().unwrap_or_default();
                self.pending_rename = Some((command_id.clone(), session.clone()));
                vec![self.enqueue(LiveCommand::Rename {
                    command_id,
                    session,
                    worker_generation,
                    title,
                })]
            }
            AppRequest::SelectEffort {
                session,
                effort,
                confirm_new_epoch,
            } => {
                let command_id = self.mint();
                let worker_generation = self.generations.get(&session).copied().unwrap_or_default();
                self.pending_effort_select =
                    Some((command_id.clone(), session.clone(), effort.clone()));
                vec![self.enqueue(LiveCommand::SelectEffort {
                    command_id,
                    session,
                    worker_generation,
                    effort,
                    confirm_new_epoch,
                })]
            }
            AppRequest::SelectFast {
                session,
                enabled,
                confirm_new_epoch,
            } => {
                let command_id = self.mint();
                let worker_generation = self.generations.get(&session).copied().unwrap_or_default();
                self.pending_fast_select = Some((command_id.clone(), session.clone(), enabled));
                vec![self.enqueue(LiveCommand::SelectFast {
                    command_id,
                    session,
                    worker_generation,
                    enabled,
                    confirm_new_epoch,
                })]
            }
            AppRequest::ProviderConfigure {
                attempt,
                name,
                origin,
                model: served_model,
                keyless,
                family,
                models,
                default_model,
                expected_revision,
            } => {
                let command_id = self.mint();
                self.pending_custom = Some((command_id.clone(), attempt));
                vec![self.enqueue(LiveCommand::ConfigureProvider {
                    command_id,
                    provider: name,
                    origin,
                    model: served_model,
                    keyless,
                    family,
                    models,
                    default_model,
                    expected_revision,
                })]
            }
            // G4a: an explicit models re-discovery (the `f` key and the
            // keyless commit chain). A read — not outboxed, no receipt.
            AppRequest::ProviderModelsRefresh { provider } => {
                self.models_requested.insert(provider.clone());
                vec![LiveCommand::RefreshProviderModels { provider }]
            }
            AppRequest::AccountSetActive {
                alias,
                confirm_new_epoch,
            } => {
                let command_id = self.mint();
                self.pending_account_select = Some((command_id.clone(), alias.clone()));
                vec![self.enqueue(LiveCommand::AccountSetActive {
                    command_id,
                    alias,
                    confirm_new_epoch,
                })]
            }
            // The request's `after_seq` is the reducer's own last fully
            // applied sequence — the same value [`cursor_of`] reads, from
            // the same authority. `ensure_attached` re-reads it rather
            // than carrying a copy, because a second cursor is how a
            // reattach asks for history the reducer already applied.
            AppRequest::Reattach { session, .. } => {
                let mut commands = Vec::new();
                if let Some(attachment) = self.attachments.get(&session).cloned() {
                    self.drop_attachment(&attachment);
                    commands.push(LiveCommand::Detach { attachment });
                }
                commands.extend(self.ensure_attached(model, &session));
                commands
            }
            AppRequest::Interrupt { branch } => {
                // Esc cancels the run the COMMITTED stream says is running.
                // With no such run there is nothing to cancel, and an
                // invented run id would be a command the daemon can only
                // reject.
                let Some(session) = model.active_session.clone() else {
                    return Vec::new();
                };
                let Some(run_id) = self.active_run.get(&session).cloned() else {
                    return Vec::new();
                };
                let command_id = self.mint();
                let worker_generation = self.generations.get(&session).copied().unwrap_or_default();
                vec![self.enqueue(LiveCommand::Cancel {
                    command_id,
                    session,
                    worker_generation,
                    run_id,
                    branch,
                })]
            }
            AppRequest::ShellExec { command } => {
                // W8b law 5/6: one durable daemon command, no UserMessage,
                // no client-side spawn, no shell re-quoting — the exact
                // bytes travel once.
                let Some(session) = model.active_session.clone() else {
                    return Vec::new();
                };
                let command_id = self.mint();
                let worker_generation = self.generations.get(&session).copied().unwrap_or_default();
                vec![self.enqueue(LiveCommand::ShellExec {
                    command_id,
                    session,
                    worker_generation,
                    command,
                })]
            }
            AppRequest::ToolsRefresh => {
                let Some(session) = model.active_session.clone() else {
                    return Vec::new();
                };
                vec![self.enqueue(LiveCommand::ToolsInventory { session })]
            }
            AppRequest::HooksRefresh { cwd } => {
                // A read — never outboxed; the cwd is remembered so a
                // trust receipt can chain its refresh at the same
                // coordinates.
                self.hooks_cwd = Some(cwd.clone());
                vec![LiveCommand::HooksList { cwd }]
            }
            // U2: a read — never outboxed (the hooks.list discipline).
            AppRequest::UsageRefresh => vec![
                // Refresh direct/root agent snapshots through the additive
                // SessionSummary field at the same time as account totals.
                LiveCommand::List { cursor: None },
                LiveCommand::UsageReport,
            ],
            AppRequest::HooksTrust { digest, trusted } => {
                let command_id = self.mint();
                self.pending_hook_trust = Some((command_id.clone(), digest.clone()));
                vec![self.enqueue(LiveCommand::HooksTrust {
                    command_id,
                    digest,
                    trusted,
                })]
            }
            AppRequest::AccountRemove { alias } => {
                let command_id = self.mint();
                let expected_revision = model.accounts.revision;
                self.pending_account_remove = Some((command_id.clone(), alias.clone()));
                vec![self.enqueue(LiveCommand::AccountRemove {
                    command_id,
                    alias,
                    expected_revision,
                })]
            }
            AppRequest::ProviderRemove { provider } => {
                let command_id = self.mint();
                let expected_revision = model.providers.revision.unwrap_or(0);
                self.pending_provider_remove = Some((command_id.clone(), provider.clone()));
                vec![self.enqueue(LiveCommand::ProviderRemove {
                    command_id,
                    provider,
                    expected_revision,
                })]
            }
            AppRequest::Compact { branch } => {
                // W7b: receipt-backed idle-only `session.compact`. The
                // daemon is the state authority — a busy worker answers
                // with a typed refusal that lands as a flash; nothing is
                // fabricated locally (the P1-A law).
                let Some(session) = model.active_session.clone() else {
                    return Vec::new();
                };
                let command_id = self.mint();
                let worker_generation = self.generations.get(&session).copied().unwrap_or_default();
                vec![self.enqueue(LiveCommand::Compact {
                    command_id,
                    session,
                    worker_generation,
                    branch,
                })]
            }
            AppRequest::BranchCreate {
                session,
                source_branch,
                fork_node_id,
                fork_seq,
                name,
            } => {
                // B2b fork issuance: the reducer captured EXACT coordinates
                // (session, source branch, node, seq) at issuance; this
                // drain only adds the durable command identity and the
                // session's worker generation. Nothing is installed until
                // the daemon's `BranchCreated` fact arrives.
                let command_id = self.mint();
                let worker_generation = self.generations.get(&session).copied().unwrap_or_default();
                vec![self.enqueue(LiveCommand::BranchCreate {
                    command_id,
                    session,
                    worker_generation,
                    source_branch,
                    fork_node_id,
                    fork_seq,
                    name,
                })]
            }
            // T2: the secret RPCs — a read and a deliberately receipt-free
            // set (no receipt may carry a secret; the daemon's vault file
            // is the durable truth). Never outboxed: a socket loss simply
            // drops the flight and the talk flow's error path says so.
            AppRequest::TranscriptionSecretRead => vec![LiveCommand::TranscriptionSecretGet],
            AppRequest::TranscriptionSecretStore { secret, clear } => {
                vec![LiveCommand::TranscriptionSecretSet { secret, clear }]
            }
            // Runtime-owned effects: `live_pass` hands these BACK to the
            // shell (they need the terminal, the process, or — for the
            // B4b attach read — the filesystem; TalkShell needs the mic
            // and the stt supervisor), so reaching here at all would be a
            // routing bug, not a discard.
            AppRequest::CopySelection
            | AppRequest::CopyText(_)
            | AppRequest::AttachRead { .. }
            | AppRequest::TalkShell(_)
            | AppRequest::Quit => Vec::new(),
            // DEMO-ONLY VOCABULARY. The reducer refuses every one of these
            // upstream in live mode (`AppModel::refuse_demo_only`), so this
            // arm is unreachable by design — but it must never be a SILENT
            // discard. Returning an empty vector quietly is exactly how
            // `/compact` fabricated `turn_active = true` and then wedged
            // the session forever (W3c3.1 r2, P1-A): the request vanished
            // and no surface said so. If a future reducer path forgets its
            // gate, the user sees a flash instead of a dead UI, and the
            // pinned test sees a failure instead of silence.
            AppRequest::Talk
            | AppRequest::ChipClose { .. }
            | AppRequest::AuraSubmit { .. }
            | AppRequest::AuraTalk
            | AppRequest::ResetAura => {
                // Undo the optimistic local state the reducer's gate should
                // have prevented, so a missed gate costs a flash rather
                // than a session that is mid-turn or listening forever.
                model.turn_active = false;
                model.listening = false;
                model.flash = Some(format!(
                    "· {label} — demo only; the live runtime has no behavior for it"
                ));
                model.dirty = true;
                Vec::new()
            }
            // A genuine NO-OP, not a discard: `ResetAllSessions` cancels the
            // demo driver's arms and clears its token meters. Live mode has
            // neither, so there is nothing to do and nothing to say.
            AppRequest::ResetAllSessions => Vec::new(),
        }
    }

    fn submit(
        &mut self,
        model: &AppModel,
        session: &SessionId,
        text: String,
        branch: Option<haider_protocol::ids::BranchId>,
        attachments: Vec<haider_protocol::tool::AttachmentBlock>,
    ) -> LiveCommand {
        let command_id = self.mint();
        let worker_generation = self.generations.get(session).copied().unwrap_or_default();
        // `/queue turn` holds mid-turn input to the end of the turn;
        // `/queue steer` (the default) delivers it at the next safe
        // boundary. The MODE is the user's standing choice, so it rides
        // every submit — the daemon, not the client, decides when.
        let mode = if model.queue_mode {
            DeliveryMode::Queue
        } else if model.subturn_mode {
            DeliveryMode::Subturn
        } else {
            DeliveryMode::Steer
        };
        self.enqueue(LiveCommand::Submit {
            command_id,
            session: session.clone(),
            worker_generation,
            text,
            mode,
            branch,
            attachments,
        })
    }

    /// Drain the model's answer outbox into live `MenuAnswer` frames at the
    /// COMMITTED opening coordinates (report R11 cut 4).
    ///
    /// The demo's epoch-only `OutboundAnswer` is demo-only: a live answer
    /// must carry `command_id`, session, menu id, `request_seq` and
    /// `worker_generation` so the daemon's compare-and-set can fence a
    /// stale answer. The command id is minted ONCE per menu and reused on
    /// every retry, so a resend after a lost response resolves the same
    /// menu exactly once.
    pub fn drain_answers(&mut self, model: &mut AppModel) -> Vec<LiveCommand> {
        let mut commands = Vec::new();
        for pending in std::mem::take(&mut model.outbox) {
            if let Some(command) = self.answer_command(&pending) {
                commands.push(self.enqueue(command));
            }
        }
        commands
    }

    fn answer_command(&mut self, pending: &OutboundAnswer) -> Option<LiveCommand> {
        let menu = pending.answer.menu.clone();
        let coordinates = self.menus.get(&menu)?;
        let (session, request_seq, worker_generation) = (
            coordinates.session.clone(),
            coordinates.request_seq,
            coordinates.worker_generation,
        );
        let command_id = match &coordinates.command_id {
            Some(existing) => existing.clone(),
            None => {
                let minted = self.mint();
                if let Some(slot) = self.menus.get_mut(&menu) {
                    slot.command_id = Some(minted.clone());
                }
                minted
            }
        };
        Some(LiveCommand::Answer {
            command_id,
            session,
            menu,
            request_seq,
            worker_generation,
            option_key: pending
                .answer
                .option_key
                .clone()
                .unwrap_or_else(|| pending.answer.option_index.to_string()),
            option_index: pending.answer.option_index,
            input: pending
                .answer
                .value
                .clone()
                .map(|text| MenuInput::Text { text }),
        })
    }
}

/// What to CALL a demo-only request when live mode has to refuse it — the
/// user's word for the thing, not the variant's.
///
/// Every request gets one, so the refusal arm can never be reached with
/// nothing to say (W3c3.1 r2, P1-A: the silent discard is what let
/// `/compact` wedge a live session).
const fn demo_only_label(request: &AppRequest) -> &'static str {
    match request {
        AppRequest::Talk => "push-to-talk",
        AppRequest::ChipClose { .. } => "closing a subagent",
        AppRequest::AuraSubmit { .. } | AppRequest::AuraTalk | AppRequest::ResetAura => "Aura Mode",
        _ => "that",
    }
}

/// The session a command is scoped to, if any. Session-scoped mutations
/// require an established control attachment; unscoped ones do not.
const fn command_session(command: &LiveCommand) -> Option<&SessionId> {
    match command {
        LiveCommand::Submit { session, .. }
        | LiveCommand::Cancel { session, .. }
        | LiveCommand::Compact { session, .. }
        | LiveCommand::BranchCreate { session, .. }
        | LiveCommand::AgentMessage { session, .. }
        | LiveCommand::ShellExec { session, .. }
        | LiveCommand::ToolsInventory { session }
        | LiveCommand::SelectModel { session, .. }
        | LiveCommand::Rename { session, .. }
        | LiveCommand::SelectEffort { session, .. }
        | LiveCommand::SelectFast { session, .. }
        | LiveCommand::Answer { session, .. } => Some(session),
        _ => None,
    }
}

/// A session's greatest fully applied sequence — read from the reducer,
/// which is the sole cursor authority (a driver-side copy is how a
/// reattach ends up asking for the wrong history).
fn cursor_of(model: &AppModel, session: &SessionId) -> Option<u64> {
    if model.active_session.as_ref() == Some(session) {
        return model.projection.last_applied();
    }
    model
        .sessions
        .iter()
        .find(|row| &row.id == session)
        .and_then(|row| row.projection.last_applied())
}

/// A session the user is waiting on: mid-turn, live subagents, or holding
/// an unanswered card. Evicting one loses exactly the events that matter.
fn is_hot(model: &AppModel, session: &SessionId) -> bool {
    if model.active_session.as_ref() == Some(session) {
        return true;
    }
    model
        .sessions
        .iter()
        .find(|row| &row.id == session)
        .is_some_and(|row| row.busy() || row.projection.open_menu().is_some())
}
