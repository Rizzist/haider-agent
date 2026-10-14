//! Content-free benchmark scopes, disabled unless `HAIDER_PHASE_TRACE_DIR` is set.
//!
//! CPU is *thread* time during active polls, with nested scopes subtracted.
//! A future may migrate threads or wait without charging unrelated worker CPU.
//! Wall intervals include waits and may overlap; consumers must not sum them.
//! Records are bounded and buffered until the executable's exit guard flushes.
//! Windows currently emits no records: consumers must report unavailable data.

use std::future::Future;

/// Frozen caller families for journal page reads. No caller-supplied text or
/// durable identifier enters a trace.
#[derive(Clone, Copy, Debug, Default)]
pub enum StoreReadCaller {
    #[default]
    Unattributed,
    AttachmentReplay,
    UsageReport,
    UsageBackfill,
    PromptHistory,
    CoreRecovery,
    ActorNarrative,
    ActorToolRepair,
    ActorControl,
    ActorAgentSpawn,
    TurnRecovery,
    StartupHydration,
    NativePipe,
    WarmJournalProjection,
    TurnSetupReduction,
    WorkerRunHeads,
    WorkerUserCommand,
    WorkerUserMessages,
    WorkerEffects,
    WorkerQueue,
    WorkerWorkspace,
    WorkerCompaction,
    WorkerPlanMenus,
    WorkerCacheContext,
    WorkerBudget,
    WorkerToolState,
    WorkerPermission,
    WorkerReplay,
    ProviderRebind,
    SessionHubActor,
    SessionHubRpc,
    DescendantStream,
    HookProjection,
    ForegroundCapture,
    Delegation,
    Completion,
    Tasks,
    Monitor,
    PeerDelivery,
    CachePolicy,
    Accounts,
    Evidence,
}

impl StoreReadCaller {
    #[cfg(unix)]
    fn name(self) -> Option<&'static str> {
        match self {
            Self::Unattributed => None,
            Self::AttachmentReplay => Some("attachment_replay"),
            Self::UsageReport => Some("usage_report"),
            Self::UsageBackfill => Some("usage_backfill"),
            Self::PromptHistory => Some("prompt_history"),
            Self::CoreRecovery => Some("core_recovery"),
            Self::ActorNarrative => Some("actor_narrative"),
            Self::ActorToolRepair => Some("actor_tool_repair"),
            Self::ActorControl => Some("actor_control"),
            Self::ActorAgentSpawn => Some("actor_agent_spawn"),
            Self::TurnRecovery => Some("turn_recovery"),
            Self::StartupHydration => Some("startup_hydration"),
            Self::NativePipe => Some("native_pipe"),
            Self::WarmJournalProjection => Some("warm_journal_projection"),
            Self::TurnSetupReduction => Some("turn_setup_reduction"),
            Self::WorkerRunHeads => Some("worker_run_heads"),
            Self::WorkerUserCommand => Some("worker_user_command"),
            Self::WorkerUserMessages => Some("worker_user_messages"),
            Self::WorkerEffects => Some("worker_effects"),
            Self::WorkerQueue => Some("worker_queue"),
            Self::WorkerWorkspace => Some("worker_workspace"),
            Self::WorkerCompaction => Some("worker_compaction"),
            Self::WorkerPlanMenus => Some("worker_plan_menus"),
            Self::WorkerCacheContext => Some("worker_cache_context"),
            Self::WorkerBudget => Some("worker_budget"),
            Self::WorkerToolState => Some("worker_tool_state"),
            Self::WorkerPermission => Some("worker_permission"),
            Self::WorkerReplay => Some("worker_replay"),
            Self::ProviderRebind => Some("provider_rebind"),
            Self::SessionHubActor => Some("session_hub_actor"),
            Self::SessionHubRpc => Some("session_hub_rpc"),
            Self::DescendantStream => Some("descendant_stream"),
            Self::HookProjection => Some("hook_projection"),
            Self::ForegroundCapture => Some("foreground_capture"),
            Self::Delegation => Some("delegation"),
            Self::Completion => Some("completion"),
            Self::Tasks => Some("tasks"),
            Self::Monitor => Some("monitor"),
            Self::PeerDelivery => Some("peer_delivery"),
            Self::CachePolicy => Some("cache_policy"),
            Self::Accounts => Some("accounts"),
            Self::Evidence => Some("evidence"),
        }
    }
}

/// Frozen report vocabulary. No caller-supplied text enters a trace.
#[derive(Clone, Copy, Debug)]
pub enum Phase {
    ClientControl,
    TurnControl,
    TurnSetup,
    LockdownBindActivate,
    StoreAccess,
    StoreOther,
    StoreOwnerLockWait,
    StoreConnectionLockWait,
    StoreQueryReplay,
    StoreQueryReducer,
    StoreQueryPoint,
    StoreEventDecode,
    StoreProviderView,
    StoreReceiptAttempt,
    CasReadHash,
    CasReverify,
    Spawn,
    RuntimeInit,
    StoreOpen,
    DirectoryPrep,
    SocketHandshake,
    CapabilityCatalog,
    Submit,
    Rpc,
    StoreJournal,
    ProjectionDigest,
    ProviderAssembly,
    StreamDecode,
    ToolDispatch,
    CompletionRender,
    Teardown,
}

impl Phase {
    #[cfg(unix)]
    fn name(self) -> &'static str {
        match self {
            Self::ClientControl => "client_control",
            Self::TurnControl => "turn_control",
            Self::TurnSetup => "turn_setup",
            Self::LockdownBindActivate => "lockdown_bind_activate",
            Self::StoreAccess => "store_access",
            Self::StoreOther => "store_other",
            Self::StoreOwnerLockWait => "store_owner_lock_wait",
            Self::StoreConnectionLockWait => "store_connection_lock_wait",
            Self::StoreQueryReplay => "store_query_replay",
            Self::StoreQueryReducer => "store_query_reducer",
            Self::StoreQueryPoint => "store_query_point",
            Self::StoreEventDecode => "store_event_decode",
            Self::StoreProviderView => "store_provider_view",
            Self::StoreReceiptAttempt => "store_receipt_attempt",
            Self::CasReadHash => "cas_read_hash",
            Self::CasReverify => "cas_reverify",
            Self::Spawn => "spawn",
            Self::RuntimeInit => "runtime_init",
            Self::StoreOpen => "store_open",
            Self::DirectoryPrep => "directory_prep",
            Self::SocketHandshake => "socket_handshake",
            Self::CapabilityCatalog => "capability_catalog",
            Self::Submit => "submit",
            Self::Rpc => "rpc",
            Self::StoreJournal => "store_journal",
            Self::ProjectionDigest => "projection_digest",
            Self::ProviderAssembly => "provider_assembly",
            Self::StreamDecode => "stream_decode",
            Self::ToolDispatch => "tool_dispatch",
            Self::CompletionRender => "completion_render",
            Self::Teardown => "teardown",
        }
    }
}

/// Synchronous scope guard; intentionally not Send on supported platforms.
/// Do not retain this guard across `.await`; use [`measure`] there.
#[must_use]
pub struct Scope {
    #[cfg(unix)]
    _native: native::Scope,
}

/// Synchronous wait guard with zero attributed CPU.
///
/// Unlike [`Scope`], this marks its interval as waiting so concurrent active
/// work wins wall attribution. It is intended only for blocking lock calls.
#[must_use]
pub struct WaitScope {
    #[cfg(unix)]
    _native: native::WaitScope,
}

/// Trace-only counters for one CAS hash operation.
///
/// The wrapper deliberately exposes no content or artifact identity. When the
/// phase trace is disabled, [`Self::enabled`] is false and the note methods are
/// no-ops, so callers can retain their ordinary uncounted read loop.
#[must_use]
pub struct CasHashScope {
    _scope: Scope,
}

impl CasHashScope {
    pub fn enabled(&self) -> bool {
        #[cfg(unix)]
        {
            self._scope._native.enabled()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub fn note_bytes_read(&mut self, bytes: usize) {
        #[cfg(unix)]
        self._scope._native.note_bytes_read(bytes);
        #[cfg(not(unix))]
        let _ = bytes;
    }

    pub fn note_block_hashed(&mut self) {
        #[cfg(unix)]
        self._scope._native.note_block_hashed();
    }

    pub fn note_reverify_call(&mut self) {
        #[cfg(unix)]
        self._scope._native.note_reverify_call();
    }
}

/// Content-free counters for one named store operation.
///
/// Callers compute counts only after checking [`Self::enabled`], keeping the
/// disabled path to the same no-op branch as every other phase probe.
#[must_use]
pub struct StoreScope {
    _scope: Scope,
}

impl StoreScope {
    pub fn enabled(&self) -> bool {
        #[cfg(unix)]
        {
            self._scope._native.enabled()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub fn note_rows_read(&mut self, rows: usize) {
        #[cfg(unix)]
        self._scope._native.note_rows_read(rows);
        #[cfg(not(unix))]
        let _ = rows;
    }

    pub fn note_payload_bytes(&mut self, bytes: usize) {
        #[cfg(unix)]
        self._scope._native.note_payload_bytes(bytes);
        #[cfg(not(unix))]
        let _ = bytes;
    }

    pub fn note_event_decoded(&mut self) {
        #[cfg(unix)]
        self._scope._native.note_event_decoded();
    }
}

/// Starts one named store sub-scope. The accepted phases are frozen enum
/// variants, so no SQL, path, identifier, or payload content enters a trace.
pub fn store_scope(phase: Phase) -> StoreScope {
    store_scope_for(phase, StoreReadCaller::Unattributed)
}

/// Starts a named store sub-scope attributed to one frozen page consumer.
pub fn store_scope_for(phase: Phase, caller: StoreReadCaller) -> StoreScope {
    debug_assert!(matches!(
        phase,
        Phase::StoreOther
            | Phase::StoreOwnerLockWait
            | Phase::StoreConnectionLockWait
            | Phase::StoreQueryReplay
            | Phase::StoreQueryReducer
            | Phase::StoreQueryPoint
            | Phase::StoreEventDecode
            | Phase::StoreProviderView
            | Phase::StoreReceiptAttempt
    ));
    #[cfg(not(unix))]
    let _ = (phase, caller);
    StoreScope {
        _scope: Scope {
            #[cfg(unix)]
            _native: native::Scope::new_with_store_caller(phase, caller),
        },
    }
}

/// Starts a synchronous store-lock wait interval.
pub fn store_wait_scope(phase: Phase) -> WaitScope {
    debug_assert!(matches!(
        phase,
        Phase::StoreOwnerLockWait | Phase::StoreConnectionLockWait
    ));
    #[cfg(not(unix))]
    let _ = phase;
    WaitScope {
        #[cfg(unix)]
        _native: native::WaitScope::new(phase),
    }
}

/// Starts a content-free CAS hash scope. Only ordinary read-path verification
/// and write-side re-verification are accepted so their costs stay separable.
pub fn cas_hash_scope(reverify: bool) -> CasHashScope {
    CasHashScope {
        _scope: scope(if reverify {
            Phase::CasReverify
        } else {
            Phase::CasReadHash
        }),
    }
}

pub fn scope(phase: Phase) -> Scope {
    #[cfg(not(unix))]
    let _ = phase;
    Scope {
        #[cfg(unix)]
        _native: native::Scope::new(phase),
    }
}

/// Measure a synchronous operation. The closure cannot hold a scope over await.
pub fn sync<T>(phase: Phase, operation: impl FnOnce() -> T) -> T {
    let _scope = scope(phase);
    operation()
}

/// Measure an asynchronous operation, charging CPU only during its polls.
pub fn measure<F: Future>(phase: Phase, future: F) -> impl Future<Output = F::Output> {
    #[cfg(unix)]
    {
        native::Measure::new(phase, future, true)
    }
    #[cfg(not(unix))]
    {
        let _ = phase;
        future
    }
}

/// Attribute active control/handler work without calling its idle time RPC or
/// request assembly. Nested specific scopes retain their own exclusive CPU.
pub fn measure_active<F: Future>(phase: Phase, future: F) -> impl Future<Output = F::Output> {
    #[cfg(unix)]
    {
        native::Measure::new(phase, future, false)
    }
    #[cfg(not(unix))]
    {
        let _ = phase;
        future
    }
}

/// Declare before all executable state so its drop flushes after shutdown.
pub struct ExitGuard;

impl Drop for ExitGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        native::flush();
    }
}

#[cfg(unix)]
#[path = "phase_trace/native.rs"]
mod native;
