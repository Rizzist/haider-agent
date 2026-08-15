//! Effect normalization, permission evaluation, and lifecycle journaling.
//!
//! Owned invariants:
//! - **Four-phase effect law.** Every completed effect journals `Intent` →
//!   `Authorized` → `Dispatched` → `Outcome`, in that order; each phase is
//!   appended through the [`JournalSink`] before the next step runs, and a
//!   failed append halts the effect. A single atomic terminal claim makes
//!   competing writers no-ops, so at most one `Outcome` can land per effect.
//! - **Owned terminalization.** Filesystem apply finalizers are registered with
//!   the broker. Orderly shutdown calls [`EffectBroker::close`], which never
//!   drops a finalizer: it drains all registered work, reconciles every
//!   remaining `Dispatched` effect to [`EffectOutcome::Unknown`], and surfaces
//!   unobserved errors. Dropping a terminal claim before its broker-owned
//!   append starts re-arms the effect; once started, caller cancellation cannot
//!   sever append from settlement. Process death is the W3 crash-recovery
//!   seam: durable `Dispatched` phases without a terminal phase are reconciled
//!   at startup by journaling `Unknown` (see [`EffectBroker::journal_unknown`]).
//! - **Sole journal handle contract.** A [`JournalSink`] value supplied to a
//!   broker must be the sole handle to its underlying journal. The open trait
//!   cannot make two sink values over shared storage unrepresentable; creating
//!   them is a protocol violation. Production construction consumes the store
//!   handle by value. The cross-process defense is the profile store's
//!   single-opener exclusion (advisory profile lock: one open store per
//!   profile) plus `worker_generation` identity stamped per store-open —
//!   envelopes carry which generation wrote them; commit-time stale-generation
//!   REJECTION is not implemented today and would be an additional guard.
//! - **Digest-bound one-shot approvals.** `args_digest` is BLAKE3 over
//!   canonical (recursively key-sorted) argument JSON — never a tool name.
//!   Approve-once binds class + exact digest. Session grants are reconstructed
//!   by the daemon from the durable menu answer. Filesystem grants remain
//!   class-scoped; process grants are exact command-shape scoped and a
//!   class-wide `ProcessExec` grant is rejected.
//! - **Intent-bound authorization.** Lifecycle state retains the complete
//!   journaled intent. Authorization and later transitions accept that intent,
//!   and reject a reused effect id whose digest or other fields differ.
//! - **Deny wins** over every form of approval, and `Dispatched`/`Outcome`
//!   can only follow an `Allow` authorization (enforced by `require_state`).
//! - The journal sink never leaves the broker. Tests inspect a broker-owned,
//!   read-only snapshot of successfully appended phases instead of recovering
//!   or mutating the sink.
//! - **Crash-consistent file freshness.** A terminal outcome may carry one
//!   workspace-relative file digest. The shared broker state reduces it only
//!   after the outcome append succeeds; daemon reconstruction replays those
//!   same session-scoped outcomes before the next turn creates its dispatcher.

use crate::process::ProcessRegistry;
use crate::{ToolError, ToolResult};
use async_trait::async_trait;
use haider_protocol::EventPayload;
use haider_protocol::effect::{
    AuthorizationSource, AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome,
    EffectPhase, FileFreshness,
};
use haider_protocol::ids::{EffectId, MenuId, SessionId, WorkspaceRevision};
use haider_protocol::menu::{DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope};
use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Port through which the actor will durably envelope effect payloads.
///
/// The broker deliberately supplies a typed [`EventPayload::Effect`], while
/// the actor remains responsible for session identity, fencing, sequence, and
/// durable-before-visible publication.
///
/// A sink value must be the sole handle to its underlying journal. Boxing this
/// open trait does not enforce that law: constructing two values over the same
/// journal is a protocol violation. Production adapters must consume their
/// store handle by value. The underlying profile store enforces single-opener
/// exclusion (advisory profile lock) and stamps `worker_generation` identity
/// per open; it does not reject stale-generation commits at append time.
///
/// Each append is transactional. `Err` guarantees that no part of `payload`
/// became durable. More strongly, an append future dropped or unwound before
/// returning MUST NOT have committed any part of `payload`: durability must be
/// the final act before the future returns `Ready`. The production SQLite
/// transaction naturally provides this boundary; test doubles must gate or
/// fail before recording and must not commit and then yield.
#[async_trait]
pub trait JournalSink: Send {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()>;
}

/// Effects reconciled during an orderly broker shutdown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectBrokerCloseReport {
    pub reconciled_effects: Vec<EffectId>,
    pub leaked_processes: Vec<String>,
}

/// An orderly close that observed errors after retaining its successful work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectBrokerCloseError {
    pub report: EffectBrokerCloseReport,
    pub errors: Vec<ToolError>,
}

impl std::fmt::Display for EffectBrokerCloseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.errors.len() == 1 {
            return write!(formatter, "{}", self.errors[0]);
        }
        write!(
            formatter,
            "effect broker close observed multiple errors: {}",
            self.errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )
    }
}

impl std::error::Error for EffectBrokerCloseError {}

/// An operation that can be reduced to the protocol's permission key.
pub trait EffectOperation {
    fn effect_class(&self) -> EffectClass;
    fn summary(&self) -> String;
    fn arguments(&self) -> ToolResult<Value>;

    /// Human-readable, bounded facts shown before an `Ask` operation may
    /// dispatch. Implementations should identify the target and the proposed
    /// mutation; authorization never relies on this display text.
    fn approval_preview(&self) -> Vec<String> {
        vec![self.summary()]
    }

    /// Returns the arguments used for authorization after resolving any
    /// workspace-relative values. Non-filesystem operations use their regular
    /// arguments; filesystem operations override this to bind canonical paths.
    fn canonical_arguments(&self, _workspace_root: &Path) -> ToolResult<Value> {
        self.arguments()
    }

    fn workspace_revision(&self) -> Option<WorkspaceRevision> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlwaysAllowRule {
    pub class: EffectClass,
    pub args_digest: String,
}

/// Durable scope of an approve-for-session grant.
///
/// A command shape is the canonical process argument digest: exact shell
/// program bytes + canonical cwd + sorted environment allowlist. The call id
/// is deliberately absent. This is narrower than executable-name or parsed
/// argv scopes and cannot collapse shell quoting/whitespace with different
/// semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionGrantScope {
    Class,
    CommandShape { args_digest: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionGrant {
    pub class: EffectClass,
    pub scope: SessionGrantScope,
}

impl SessionGrant {
    pub fn for_effect(class: EffectClass, args_digest: impl Into<String>) -> Self {
        let scope = if class == EffectClass::ProcessExec {
            SessionGrantScope::CommandShape {
                args_digest: args_digest.into(),
            }
        } else {
            SessionGrantScope::Class
        };
        Self { class, scope }
    }

    fn matches(&self, intent: &EffectIntent) -> bool {
        if self.class != intent.class {
            return false;
        }
        match &self.scope {
            SessionGrantScope::Class => true,
            SessionGrantScope::CommandShape { args_digest } => args_digest == &intent.args_digest,
        }
    }
}

#[derive(Debug, Clone)]
struct DenyRule {
    class: EffectClass,
    reason: String,
}

/// Permission policy keyed by normalized effect class.
///
/// Classes absent from all three lists default to `Ask`. Overlaps are resolved
/// conservatively: deny, exact always-allow, session/class allow, ask.
#[derive(Debug, Clone, Default)]
pub struct PermissionPolicy {
    allowlist: Vec<EffectClass>,
    asklist: Vec<EffectClass>,
    denylist: Vec<DenyRule>,
    always_allow: Vec<AlwaysAllowRule>,
    session_allow: Vec<SessionGrant>,
}

impl PermissionPolicy {
    pub fn new(
        allowlist: Vec<EffectClass>,
        asklist: Vec<EffectClass>,
        denylist: Vec<EffectClass>,
    ) -> Self {
        let mut policy = Self {
            allowlist,
            asklist,
            ..Self::default()
        };
        for class in denylist {
            policy.deny(class, "denied by policy");
        }
        policy
    }

    /// Reclassifies `class`, keeping it in at most one of the three lists.
    /// Digest-bound always-allow rules for the class are left intact; deny
    /// still beats them at decision time.
    pub fn set(&mut self, class: EffectClass, decision: PolicyDecision) {
        self.remove_class(&class);
        match decision {
            PolicyDecision::Allow => self.allowlist.push(class),
            PolicyDecision::Ask => self.asklist.push(class),
            PolicyDecision::Deny { reason } => self.denylist.push(DenyRule { class, reason }),
        }
    }

    pub fn allow(&mut self, class: EffectClass) {
        self.set(class, PolicyDecision::Allow);
    }

    pub fn ask(&mut self, class: EffectClass) {
        self.set(class, PolicyDecision::Ask);
    }

    pub fn deny(&mut self, class: EffectClass, reason: impl Into<String>) {
        self.set(
            class,
            PolicyDecision::Deny {
                reason: reason.into(),
            },
        );
    }

    pub fn always_allow(&mut self, intent: &EffectIntent) {
        let rule = AlwaysAllowRule {
            class: intent.class.clone(),
            args_digest: intent.args_digest.clone(),
        };
        if !self.always_allow.contains(&rule) {
            self.always_allow.push(rule);
        }
    }

    pub fn always_allow_rules(&self) -> &[AlwaysAllowRule] {
        &self.always_allow
    }

    /// Adds a durable-menu-derived class grant. `ProcessExec` is deliberately
    /// refused here: shell session grants must use [`SessionGrantScope::CommandShape`].
    pub fn allow_for_session(&mut self, class: EffectClass) -> ToolResult<()> {
        self.allow_session_grant(SessionGrant {
            class,
            scope: SessionGrantScope::Class,
        })
    }

    /// Adds a reconstructed session grant after validating that process
    /// execution can never become class-wide.
    pub fn allow_session_grant(&mut self, grant: SessionGrant) -> ToolResult<()> {
        if grant.class == EffectClass::ProcessExec && grant.scope == SessionGrantScope::Class {
            return Err(ToolError::invalid_argument(
                "class-wide ProcessExec session grants are forbidden",
            ));
        }
        if !self.session_allow.contains(&grant) {
            self.session_allow.push(grant);
        }
        Ok(())
    }

    pub fn session_allowlist(&self) -> &[SessionGrant] {
        &self.session_allow
    }

    pub fn allowlist(&self) -> &[EffectClass] {
        &self.allowlist
    }

    pub fn asklist(&self) -> &[EffectClass] {
        &self.asklist
    }

    pub fn denylist(&self) -> impl Iterator<Item = &EffectClass> {
        self.denylist.iter().map(|rule| &rule.class)
    }

    fn remove_class(&mut self, class: &EffectClass) {
        self.allowlist.retain(|listed| listed != class);
        self.asklist.retain(|listed| listed != class);
        self.denylist.retain(|listed| &listed.class != class);
    }

    fn decision(&self, intent: &EffectIntent) -> PolicyDecision {
        if let Some(rule) = self
            .denylist
            .iter()
            .find(|rule| class_rule_matches(&rule.class, &intent.class))
        {
            return PolicyDecision::Deny {
                reason: rule.reason.clone(),
            };
        }
        if self
            .always_allow
            .iter()
            .any(|rule| rule.class == intent.class && rule.args_digest == intent.args_digest)
            || self.session_allow.iter().any(|grant| grant.matches(intent))
            || self
                .allowlist
                .iter()
                .any(|listed| class_rule_matches(listed, &intent.class))
        {
            return PolicyDecision::Allow;
        }
        PolicyDecision::Ask
    }
}

/// Whether one policy-list class entry covers an intent's class. Classes
/// match exactly, with ONE deliberate widening (W-B): a `Network` rule with
/// an EMPTY host is the class-FAMILY rule — daemon defaults cannot know the
/// host a model will fetch before it asks, while menu-minted grants always
/// carry the real host and stay exact.
fn class_rule_matches(rule: &EffectClass, intent: &EffectClass) -> bool {
    if rule == intent {
        return true;
    }
    matches!(
        (rule, intent),
        (EffectClass::Network { host }, EffectClass::Network { .. }) if host.is_empty()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleState {
    Intent,
    AuthorizedAllow,
    AuthorizedBlocked,
    Dispatched,
    Terminalizing,
    TerminalJournalFailed,
    Outcome,
}

#[derive(Debug, Clone)]
struct LifecycleRecord {
    intent: EffectIntent,
    state: LifecycleState,
}

#[derive(Debug, Clone)]
struct PendingPermission {
    menu: Menu,
    class: EffectClass,
    args_digest: String,
}

#[derive(Debug, Clone)]
struct OneShotRule {
    class: EffectClass,
    args_digest: String,
    decision: PolicyDecision,
}

struct BrokerJournalState {
    /// Terminal states are retained, never evicted. `Terminalizing` is an
    /// atomic ownership claim; dropping its authority before dispatch re-arms
    /// `Dispatched`.
    lifecycles: HashMap<EffectId, LifecycleRecord>,
    journaled_phases: Vec<EffectPhase>,
    terminal_errors: HashMap<EffectId, ToolError>,
    freshness: HashMap<PathBuf, String>,
}

/// Private intra-broker sharing lets a registered finalizer close a dispatched
/// effect after its caller is cancelled. No sink handle crosses the public
/// broker boundary.
struct BrokerJournal {
    sink: Arc<tokio::sync::Mutex<Box<dyn JournalSink>>>,
    state: Arc<Mutex<BrokerJournalState>>,
}

impl BrokerJournal {
    fn new(sink: Box<dyn JournalSink>) -> Self {
        Self {
            sink: Arc::new(tokio::sync::Mutex::new(sink)),
            state: Arc::new(Mutex::new(BrokerJournalState {
                lifecycles: HashMap::new(),
                journaled_phases: Vec::new(),
                terminal_errors: HashMap::new(),
                freshness: HashMap::new(),
            })),
        }
    }

    fn journal_snapshot(&self) -> Vec<EffectPhase> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .journaled_phases
            .clone()
    }

    async fn append_phase(&self, phase: EffectPhase) -> ToolResult<()> {
        self.sink
            .lock()
            .await
            .append(EventPayload::Effect(phase.clone()))
            .await?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let EffectPhase::Outcome {
            freshness: Some(freshness),
            ..
        } = &phase
        {
            state
                .freshness
                .insert(PathBuf::from(&freshness.path), freshness.digest.clone());
        }
        state.journaled_phases.push(phase);
        Ok(())
    }

    fn restore_freshness(&self, freshness: impl IntoIterator<Item = FileFreshness>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for record in freshness {
            state
                .freshness
                .insert(PathBuf::from(record.path), record.digest);
        }
    }

    fn freshness_digest(&self, path: &Path) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .freshness
            .get(path)
            .cloned()
    }

    fn insert_intent(&self, intent: EffectIntent) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lifecycles
            .insert(
                intent.effect.clone(),
                LifecycleRecord {
                    intent,
                    state: LifecycleState::Intent,
                },
            );
    }

    fn require_state(&self, effect: &EffectId, expected: LifecycleState) -> ToolResult<()> {
        let actual = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lifecycles
            .get(effect)
            .map(|record| record.state);
        if actual == Some(expected) {
            return Ok(());
        }
        Err(ToolError::Lifecycle {
            message: format!("effect {effect} is {actual:?}, expected {expected:?}"),
        })
    }

    fn require_recorded_intent(&self, intent: &EffectIntent) -> ToolResult<EffectIntent> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = state.lifecycles.get(&intent.effect) else {
            return Err(ToolError::Lifecycle {
                message: format!("effect {} has no journaled intent", intent.effect),
            });
        };
        if record.intent != *intent {
            return Err(ToolError::Lifecycle {
                message: format!(
                    "effect {} does not match its journaled intent (expected digest {}, received {})",
                    intent.effect, record.intent.args_digest, intent.args_digest
                ),
            });
        }
        Ok(record.intent.clone())
    }

    fn set_state(&self, effect: &EffectId, state: LifecycleState) -> ToolResult<()> {
        let mut journal = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = journal
            .lifecycles
            .get_mut(effect)
            .ok_or_else(|| ToolError::Lifecycle {
                message: format!("effect {effect} has no journaled intent"),
            })?;
        record.state = state;
        Ok(())
    }

    /// Atomically transfers terminalization ownership exactly once.
    ///
    /// `Terminalizing`, `TerminalJournalFailed`, and `Outcome` are successful
    /// no-op states for competing writers. Earlier lifecycle states remain
    /// errors because they have not crossed the durable dispatch boundary.
    fn claim_terminal(&self, intent: &EffectIntent) -> ToolResult<Option<TerminalClaim>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = state.lifecycles.get_mut(&intent.effect) else {
            return Err(ToolError::Lifecycle {
                message: format!("effect {} has no journaled intent", intent.effect),
            });
        };
        if record.intent != *intent {
            return Err(ToolError::Lifecycle {
                message: format!(
                    "effect {} does not match its journaled intent (expected digest {}, received {})",
                    intent.effect, record.intent.args_digest, intent.args_digest
                ),
            });
        }
        match record.state {
            LifecycleState::Dispatched => {
                record.state = LifecycleState::Terminalizing;
                Ok(Some(TerminalClaim {
                    journal: self.clone(),
                    effect: intent.effect.clone(),
                    dispatched: false,
                    settled: false,
                }))
            }
            LifecycleState::Terminalizing
            | LifecycleState::TerminalJournalFailed
            | LifecycleState::Outcome => Ok(None),
            state => Err(ToolError::Lifecycle {
                message: format!(
                    "effect {} is {state:?}, expected {:?}",
                    intent.effect,
                    LifecycleState::Dispatched
                ),
            }),
        }
    }

    fn close_sweep_candidates(&self) -> Vec<EffectIntent> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut candidates = state
            .lifecycles
            .values_mut()
            .filter_map(|record| {
                if record.state == LifecycleState::Terminalizing {
                    record.state = LifecycleState::Dispatched;
                }
                (record.state == LifecycleState::Dispatched).then(|| record.intent.clone())
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.effect.as_str().cmp(right.effect.as_str()));
        candidates
    }

    fn mark_outcome(&self, effect: &EffectId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = state.lifecycles.get_mut(effect) {
            debug_assert_eq!(record.state, LifecycleState::Terminalizing);
            record.state = LifecycleState::Outcome;
        } else {
            debug_assert!(false, "terminal claim retains its lifecycle record");
        }
    }

    fn mark_terminal_journal_failed(&self, effect: &EffectId, error: ToolError) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = state.lifecycles.get_mut(effect) {
            debug_assert_eq!(record.state, LifecycleState::Terminalizing);
            record.state = LifecycleState::TerminalJournalFailed;
            state.terminal_errors.insert(effect.clone(), error);
        } else {
            debug_assert!(false, "terminal claim retains its lifecycle record");
        }
    }

    fn abandon_terminal(&self, effect: &EffectId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = state.lifecycles.get_mut(effect)
            && record.state == LifecycleState::Terminalizing
        {
            record.state = LifecycleState::Dispatched;
        }
    }

    fn take_terminal_errors(&self) -> Vec<(EffectId, ToolError)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut errors = state.terminal_errors.drain().collect::<Vec<_>>();
        errors.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
        errors
    }

    /// Completes an effect from inside a task already owned by the broker's
    /// finalizer set.
    async fn finish_in_finalizer<T>(
        &self,
        intent: &EffectIntent,
        result: ToolResult<T>,
        freshness: Option<FileFreshness>,
    ) -> ToolResult<T> {
        let outcome = match &result {
            Ok(_) => EffectOutcome::Ok,
            Err(error) => EffectOutcome::Failed {
                error: error.to_string(),
            },
        };
        let Some(claim) = self.claim_terminal(intent)? else {
            return result;
        };
        match claim.append(outcome, freshness).await {
            Ok(()) => result,
            Err(error) => Err(error),
        }
    }
}

impl Clone for BrokerJournal {
    fn clone(&self) -> Self {
        Self {
            sink: Arc::clone(&self.sink),
            state: Arc::clone(&self.state),
        }
    }
}

/// Unique, non-cloneable authority to append one terminal phase.
struct TerminalClaim {
    journal: BrokerJournal,
    effect: EffectId,
    dispatched: bool,
    settled: bool,
}

impl TerminalClaim {
    /// Appends and settles inside a broker-owned finalizer task.
    ///
    /// Before the first poll, dropping this future is a pre-dispatch
    /// cancellation and re-arms the claim. Once polled, the sink append,
    /// lifecycle transition, and settlement remain one owned task unit.
    async fn append(
        mut self,
        outcome: EffectOutcome,
        freshness: Option<FileFreshness>,
    ) -> ToolResult<()> {
        self.dispatched = true;
        let phase = EffectPhase::Outcome {
            effect: self.effect.clone(),
            outcome,
            freshness,
        };
        match self.journal.append_phase(phase).await {
            Ok(()) => {
                self.journal.mark_outcome(&self.effect);
                self.settled = true;
                Ok(())
            }
            Err(error) => {
                self.journal
                    .mark_terminal_journal_failed(&self.effect, error.clone());
                self.settled = true;
                Err(error)
            }
        }
    }
}

impl Drop for TerminalClaim {
    fn drop(&mut self) {
        if !self.dispatched && !self.settled {
            self.journal.abandon_terminal(&self.effect);
        }
    }
}

pub(crate) struct EffectFinish {
    journal: BrokerJournal,
    intent: EffectIntent,
}

pub(crate) struct FinalizerObserver {
    id: u64,
    observed: Arc<Mutex<HashSet<u64>>>,
}

impl FinalizerObserver {
    pub(crate) fn observe(self) {
        self.observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.id);
    }
}

impl EffectFinish {
    pub(crate) async fn finish<T>(self, result: ToolResult<T>) -> ToolResult<T> {
        self.journal
            .finish_in_finalizer(&self.intent, result, None)
            .await
    }

    pub(crate) async fn finish_with_freshness<T>(
        self,
        result: ToolResult<T>,
        freshness: Option<FileFreshness>,
    ) -> ToolResult<T> {
        self.journal
            .finish_in_finalizer(&self.intent, result, freshness)
            .await
    }

    pub(crate) async fn finish_outcome(self, outcome: EffectOutcome) -> ToolResult<()> {
        let Some(claim) = self.journal.claim_terminal(&self.intent)? else {
            return Ok(());
        };
        claim.append(outcome, None).await
    }
}

/// Normalizes, authorizes, and journals effectful operations.
pub struct EffectBroker {
    journal: BrokerJournal,
    workspace_root: PathBuf,
    workspace_dir: OwnedFd,
    session_id: SessionId,
    worker_generation: u64,
    started_at_ms: u64,
    next_effect: u64,
    next_menu: u64,
    pending_permissions: HashMap<MenuId, PendingPermission>,
    approval_previews: HashMap<EffectId, Vec<String>>,
    one_shot_rules: Vec<OneShotRule>,
    next_finalizer: u64,
    finalizers: tokio::task::JoinSet<(u64, Option<ToolError>)>,
    observed_finalizers: Arc<Mutex<HashSet<u64>>>,
    pub(crate) processes: ProcessRegistry,
}

impl EffectBroker {
    /// Creates a broker rooted at `workspace_root`.
    ///
    /// Construction consumes `journal`; production adapters must themselves
    /// be constructed by consuming the sole underlying store handle by value.
    ///
    /// `worker_generation` is the durable actor generation. Together with the
    /// session, broker start time, and local counters it prevents identity
    /// reuse across same-millisecond restarts.
    pub fn new(
        journal: Box<dyn JournalSink>,
        workspace_root: impl AsRef<Path>,
        session_id: SessionId,
        worker_generation: u64,
    ) -> ToolResult<Self> {
        Self::new_at(
            journal,
            workspace_root,
            session_id,
            worker_generation,
            unix_time_ms(),
        )
    }

    /// Deterministic constructor for restart tests and actor integration.
    ///
    /// As with [`Self::new`], `journal` must own the sole underlying store
    /// handle and is consumed by this constructor.
    pub fn new_at(
        journal: Box<dyn JournalSink>,
        workspace_root: impl AsRef<Path>,
        session_id: SessionId,
        worker_generation: u64,
        started_at_ms: u64,
    ) -> ToolResult<Self> {
        let requested_root = workspace_root.as_ref();
        let workspace_root = fs::canonicalize(requested_root)
            .map_err(|error| ToolError::io("canonicalize workspace root", requested_root, error))?;
        if !workspace_root.is_dir() {
            return Err(ToolError::invalid_argument(format!(
                "workspace root is not a directory: {}",
                workspace_root.display()
            )));
        }
        let workspace_dir = rustix::fs::openat(
            rustix::fs::CWD,
            &workspace_root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| ToolError::io("open workspace root", &workspace_root, error))?;
        Ok(Self {
            journal: BrokerJournal::new(journal),
            workspace_root,
            workspace_dir,
            session_id,
            worker_generation,
            started_at_ms,
            next_effect: 0,
            next_menu: 0,
            pending_permissions: HashMap::new(),
            approval_previews: HashMap::new(),
            one_shot_rules: Vec::new(),
            next_finalizer: 0,
            finalizers: tokio::task::JoinSet::new(),
            observed_finalizers: Arc::new(Mutex::new(HashSet::new())),
            processes: ProcessRegistry::default(),
        })
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn duplicate_workspace_dir(&self) -> ToolResult<OwnedFd> {
        rustix::io::dup(&self.workspace_dir)
            .map_err(|error| ToolError::io("duplicate workspace root", &self.workspace_root, error))
    }

    /// Read-only copy of every effect phase whose durable append succeeded.
    ///
    /// The sink itself is intentionally neither borrowed nor recoverable.
    pub fn journal_snapshot(&self) -> Vec<EffectPhase> {
        self.journal.journal_snapshot()
    }

    /// Restores session-scoped freshness reduced from this session's durable
    /// terminal effect outcomes. Paths are workspace-relative and are checked
    /// before entering the broker-owned map.
    pub fn restore_freshness(
        &mut self,
        freshness: impl IntoIterator<Item = FileFreshness>,
    ) -> ToolResult<()> {
        let mut checked = Vec::new();
        for record in freshness {
            let relative = PathBuf::from(&record.path);
            if relative.as_os_str().is_empty()
                || relative.is_absolute()
                || relative.components().any(|component| {
                    !matches!(
                        component,
                        std::path::Component::Normal(_) | std::path::Component::CurDir
                    )
                })
            {
                return Err(ToolError::WorkspaceBoundary {
                    workspace_root: self.workspace_root.clone(),
                    requested_path: relative,
                    resolved_path: None,
                });
            }
            checked.push(record);
        }
        self.journal.restore_freshness(checked);
        Ok(())
    }

    pub(crate) fn freshness_digest(&self, relative_path: &Path) -> Option<String> {
        self.journal.freshness_digest(relative_path)
    }

    /// Drains finalizers, then reconciles unterminated dispatches to `Unknown`.
    ///
    /// Operation errors already returned by filesystem mutations are considered
    /// observed. Errors from a cancelled caller, task failure, or terminal
    /// append are returned here, with terminal append errors keyed by effect
    /// and the complete reconciliation report retained on
    /// [`EffectBrokerCloseError`]. A successful report names every effect
    /// reconciled by the close sweep.
    /// Process death is instead recovered by the W3 dispatched-without-terminal
    /// startup reconciliation seam.
    pub async fn close(mut self) -> Result<EffectBrokerCloseReport, EffectBrokerCloseError> {
        self.processes.cancel_all().await;
        let mut errors = Vec::new();
        self.drain_finalizers(&mut errors).await;

        let mut reconciled_effects = Vec::new();
        for intent in self.journal.close_sweep_candidates() {
            let claim = match self.journal.claim_terminal(&intent) {
                Ok(claim) => claim,
                Err(error) => {
                    errors.push(error);
                    continue;
                }
            };
            let Some(claim) = claim else {
                continue;
            };
            let (id, result) = self.register_terminal_append(claim, EffectOutcome::Unknown, None);
            match result.await {
                Ok(Ok(())) => {
                    self.observe_finalizer(id);
                    reconciled_effects.push(intent.effect);
                }
                Ok(Err(_)) => {
                    self.observe_finalizer(id);
                }
                Err(_) => {
                    // The JoinSet retains the task failure with its full
                    // diagnostic; the second drain records it below.
                }
            }
        }
        self.drain_finalizers(&mut errors).await;

        let terminal_errors = self.journal.take_terminal_errors();
        errors.retain(|error| {
            !terminal_errors
                .iter()
                .any(|(_, terminal_error)| terminal_error == error)
        });
        let leaked_processes = self.processes.leaked_call_ids().await;
        errors.extend(leaked_processes.iter().map(|call_id| ToolError::Runtime {
            message: format!(
                "process `{call_id}` leaked after process-group termination escalation"
            ),
        }));
        finish_close(
            EffectBrokerCloseReport {
                reconciled_effects,
                leaked_processes,
            },
            errors,
            terminal_errors,
        )
    }

    /// Produces and journals an intent whose digest is the BLAKE3 hash of
    /// recursively key-sorted JSON arguments.
    pub async fn normalize<O>(&mut self, operation: &O) -> ToolResult<EffectIntent>
    where
        O: EffectOperation,
    {
        let canonical = canonical_json(operation.canonical_arguments(&self.workspace_root)?)?;
        let args_digest = format!("blake3:{}", blake3::hash(&canonical).to_hex());
        self.next_effect += 1;
        let intent = EffectIntent {
            effect: EffectId::new(format!(
                "effect-{}-{}-{}-{}",
                self.session_id.as_str(),
                self.worker_generation,
                self.started_at_ms,
                self.next_effect
            )),
            class: operation.effect_class(),
            summary: operation.summary(),
            args_digest,
            workspace_revision: operation.workspace_revision(),
        };
        self.append_phase(EffectPhase::Intent(intent.clone()))
            .await?;
        self.journal.insert_intent(intent.clone());
        self.approval_previews
            .insert(intent.effect.clone(), operation.approval_preview());
        Ok(intent)
    }

    /// Evaluates and journals the authorization verdict for one intent.
    pub async fn authorize(
        &mut self,
        intent: &EffectIntent,
        policy: &PermissionPolicy,
    ) -> ToolResult<AuthorizationVerdict> {
        self.require_state(&intent.effect, LifecycleState::Intent)?;
        let recorded = self.require_recorded_intent(intent)?;
        let decision = match policy.decision(&recorded) {
            PolicyDecision::Ask => self
                .take_one_shot_decision(&recorded)
                .unwrap_or(PolicyDecision::Ask),
            decision => decision,
        };
        let mut pending_permission = None;
        let verdict = match decision {
            PolicyDecision::Allow => AuthorizationVerdict::Allow,
            PolicyDecision::Ask => {
                let menu = self.permission_menu_for(&recorded);
                let menu_id = menu.id.clone();
                pending_permission = Some((
                    menu_id.clone(),
                    PendingPermission {
                        menu,
                        class: recorded.class.clone(),
                        args_digest: recorded.args_digest.clone(),
                    },
                ));
                AuthorizationVerdict::Ask { menu: menu_id }
            }
            PolicyDecision::Deny { reason } => AuthorizationVerdict::Deny { reason },
        };
        self.append_phase(EffectPhase::Authorized {
            effect: intent.effect.clone(),
            verdict: verdict.clone(),
        })
        .await?;
        // Register the menu only after its `Ask` verdict is durably journaled,
        // so no answerable menu exists for an unjournaled authorization.
        if let Some((menu_id, pending)) = pending_permission {
            self.pending_permissions.insert(menu_id, pending);
        }
        let state = if matches!(
            verdict,
            AuthorizationVerdict::Allow | AuthorizationVerdict::PreAuthorized { .. }
        ) {
            LifecycleState::AuthorizedAllow
        } else {
            LifecycleState::AuthorizedBlocked
        };
        self.set_state(&intent.effect, state)?;
        Ok(verdict)
    }

    pub fn permission_menu(&self, menu: &MenuId) -> Option<&Menu> {
        self.pending_permissions
            .get(menu)
            .map(|permission| &permission.menu)
    }

    /// Resolves a permission menu into a one-shot or persistent policy rule.
    ///
    /// The operation is retried as a fresh effect after resolution. This keeps
    /// every effect's journal at exactly one `Authorized` phase.
    pub fn resolve_permission(
        &mut self,
        answer: &MenuAnswer,
        policy: &mut PermissionPolicy,
    ) -> ToolResult<()> {
        let pending = self
            .pending_permissions
            .get(&answer.menu)
            .cloned()
            .ok_or_else(|| ToolError::InvalidArgument {
                message: format!("unknown permission menu {}", answer.menu),
            })?;
        let decision = selected_decision(&pending.menu, answer)?;
        self.pending_permissions.remove(&answer.menu);
        self.apply_permission_decision(pending.class, pending.args_digest, decision, policy)?;
        Ok(())
    }

    /// Restores a permission decision from the exact durable menu that the
    /// daemon's first-committed-wins CAS resolved. This is used when a daemon
    /// restarts while approval is pending; callers must supply the committed
    /// menu and answer read from the journal, never raw client input.
    pub fn restore_permission(
        &mut self,
        menu: &Menu,
        answer: &MenuAnswer,
        class: EffectClass,
        args_digest: String,
        policy: &mut PermissionPolicy,
    ) -> ToolResult<()> {
        if answer.menu != menu.id {
            return Err(ToolError::InvalidMenuAnswer {
                menu: answer.menu.clone(),
                message: format!(
                    "answer targets {}, but durable permission menu is {}",
                    answer.menu, menu.id
                ),
            });
        }
        if !matches!(&menu.kind, MenuKind::Permission { .. }) {
            return Err(ToolError::InvalidArgument {
                message: format!("menu {} is not a permission menu", menu.id),
            });
        }
        let decision = selected_decision(menu, answer)?;
        self.apply_permission_decision(class, args_digest, decision, policy)?;
        Ok(())
    }

    fn apply_permission_decision(
        &mut self,
        class: EffectClass,
        args_digest: String,
        decision: DecisionKind,
        policy: &mut PermissionPolicy,
    ) -> ToolResult<()> {
        match decision {
            DecisionKind::AllowOnce => self.one_shot_rules.push(OneShotRule {
                class,
                args_digest,
                decision: PolicyDecision::Allow,
            }),
            DecisionKind::AllowAlways => {
                policy.allow_session_grant(SessionGrant::for_effect(class, args_digest))?
            }
            DecisionKind::RejectOnce => self.one_shot_rules.push(OneShotRule {
                class,
                args_digest,
                decision: PolicyDecision::Deny {
                    reason: "rejected once by user".into(),
                },
            }),
            DecisionKind::RejectAlways => {
                policy.deny(class, "rejected always by user");
            }
        }
        Ok(())
    }

    pub async fn journal_dispatched(&mut self, intent: &EffectIntent) -> ToolResult<()> {
        self.require_recorded_intent(intent)?;
        self.require_state(&intent.effect, LifecycleState::AuthorizedAllow)?;
        self.append_phase(EffectPhase::Dispatched {
            effect: intent.effect.clone(),
        })
        .await?;
        self.set_state(&intent.effect, LifecycleState::Dispatched)?;
        Ok(())
    }

    /// Journals a user-initiated authorization and dispatches without applying
    /// model-effect permission policy. This is reserved for direct composer
    /// actions such as `!cmd`.
    pub(crate) async fn begin_user_typed<O>(&mut self, operation: &O) -> ToolResult<EffectIntent>
    where
        O: EffectOperation,
    {
        let intent = self.normalize(operation).await?;
        self.append_phase(EffectPhase::Authorized {
            effect: intent.effect.clone(),
            verdict: AuthorizationVerdict::PreAuthorized {
                source: AuthorizationSource::UserTyped,
            },
        })
        .await?;
        self.set_state(&intent.effect, LifecycleState::AuthorizedAllow)?;
        self.journal_dispatched(&intent).await?;
        Ok(intent)
    }

    pub async fn journal_outcome(
        &mut self,
        intent: &EffectIntent,
        outcome: EffectOutcome,
    ) -> ToolResult<()> {
        self.dispatch_terminal(intent, outcome, None).await
    }

    /// Reconciles a dispatch/outcome recovery window explicitly.
    ///
    /// `Unknown` is reserved for orderly-shutdown and startup reconciliation;
    /// terminal transport errors retain `Dispatched` durably and are reported.
    pub async fn journal_unknown(&mut self, intent: &EffectIntent) -> ToolResult<()> {
        self.journal_outcome(intent, EffectOutcome::Unknown).await
    }

    /// Begins the brokered `web_fetch` effect (W-B, LW7): intent (URL in the
    /// summary and canonical arguments) → authorization under the
    /// `Network { host }` class → `Dispatched`. The daemon dispatcher owns
    /// the guarded HTTP work between this and [`EffectBroker::journal_outcome`]
    /// — the `spawn_subagent` begin/finish precedent.
    pub async fn begin_web_fetch(
        &mut self,
        operation: &crate::WebFetch,
        policy: &PermissionPolicy,
    ) -> ToolResult<EffectIntent> {
        self.begin(operation, policy).await
    }

    /// Begins the short `AgentSpawn` effect. The caller must establish the
    /// child session and durable delegation link, then immediately call
    /// [`Self::finish_agent_spawn`]; child execution is not part of this
    /// effect's lifetime.
    pub async fn begin_agent_spawn(
        &mut self,
        operation: &crate::SpawnSubagent,
        policy: &PermissionPolicy,
    ) -> ToolResult<EffectIntent> {
        self.begin(operation, policy).await
    }

    /// Terminalizes the short spawn effect after durable establishment.
    pub async fn finish_agent_spawn(
        &mut self,
        intent: &EffectIntent,
        result: ToolResult<()>,
    ) -> ToolResult<()> {
        self.finish(intent, result).await
    }

    /// Runs intent → authorize for one operation and journals `Dispatched`
    /// iff the verdict is `Allow`. `Ask` and `Deny` become typed errors, so a
    /// tool built on `begin` cannot reach its side effect unauthorized.
    pub(crate) async fn begin<O>(
        &mut self,
        operation: &O,
        policy: &PermissionPolicy,
    ) -> ToolResult<EffectIntent>
    where
        O: EffectOperation,
    {
        let intent = self.normalize(operation).await?;
        match self.authorize(&intent, policy).await? {
            AuthorizationVerdict::Allow | AuthorizationVerdict::PreAuthorized { .. } => {
                self.journal_dispatched(&intent).await?;
                Ok(intent)
            }
            AuthorizationVerdict::Ask { menu } => Err(ToolError::AuthorizationRequired { menu }),
            AuthorizationVerdict::Deny { reason } => Err(ToolError::PermissionDenied { reason }),
        }
    }

    /// Journals the terminal `Outcome` matching `result` (`Ok` → `Ok`, `Err`
    /// → `Failed`) and passes the result through unchanged.
    pub(crate) async fn finish<T>(
        &mut self,
        intent: &EffectIntent,
        result: ToolResult<T>,
    ) -> ToolResult<T> {
        let outcome = match &result {
            Ok(_) => EffectOutcome::Ok,
            Err(error) => EffectOutcome::Failed {
                error: error.to_string(),
            },
        };
        match self.dispatch_terminal(intent, outcome, None).await {
            Ok(()) => result,
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn finish_with_freshness<T>(
        &mut self,
        intent: &EffectIntent,
        result: ToolResult<T>,
        freshness: Option<FileFreshness>,
    ) -> ToolResult<T> {
        let outcome = match &result {
            Ok(_) => EffectOutcome::Ok,
            Err(error) => EffectOutcome::Failed {
                error: error.to_string(),
            },
        };
        match self.dispatch_terminal(intent, outcome, freshness).await {
            Ok(()) => result,
            Err(error) => Err(error),
        }
    }

    pub(crate) fn effect_finish(&self, intent: &EffectIntent) -> EffectFinish {
        EffectFinish {
            journal: self.journal.clone(),
            intent: intent.clone(),
        }
    }

    pub(crate) fn register_finalizer<F>(&mut self, finalizer: F) -> u64
    where
        F: Future<Output = Option<ToolError>> + Send + 'static,
    {
        self.next_finalizer += 1;
        let id = self.next_finalizer;
        self.finalizers.spawn(async move {
            let error = finalizer.await;
            (id, error)
        });
        id
    }

    pub(crate) fn observe_finalizer(&mut self, id: u64) {
        self.observed_finalizers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id);
    }

    pub(crate) fn finalizer_observer(&self, id: u64) -> FinalizerObserver {
        FinalizerObserver {
            id,
            observed: Arc::clone(&self.observed_finalizers),
        }
    }

    fn register_terminal_append(
        &mut self,
        claim: TerminalClaim,
        outcome: EffectOutcome,
        freshness: Option<FileFreshness>,
    ) -> (u64, tokio::sync::oneshot::Receiver<ToolResult<()>>) {
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        let id = self.register_finalizer(async move {
            let result = claim.append(outcome, freshness).await;
            let error = result.as_ref().err().cloned();
            let _ = result_sender.send(result);
            error
        });
        (id, result_receiver)
    }

    async fn dispatch_terminal(
        &mut self,
        intent: &EffectIntent,
        outcome: EffectOutcome,
        freshness: Option<FileFreshness>,
    ) -> ToolResult<()> {
        let Some(claim) = self.journal.claim_terminal(intent)? else {
            return Ok(());
        };
        let (id, result) = self.register_terminal_append(claim, outcome, freshness);
        match result.await {
            Ok(result) => {
                self.observe_finalizer(id);
                result
            }
            Err(error) => Err(ToolError::Runtime {
                message: format!("terminal outcome finalizer failed: {error}"),
            }),
        }
    }

    async fn drain_finalizers(&mut self, errors: &mut Vec<ToolError>) {
        while let Some(finalizer) = self.finalizers.join_next().await {
            match finalizer {
                Ok((id, Some(error)))
                    if !self
                        .observed_finalizers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains(&id) =>
                {
                    errors.push(error);
                }
                Ok(_) => {}
                Err(error) => errors.push(ToolError::Runtime {
                    message: format!("effect finalizer task failed: {error}"),
                }),
            }
        }
    }

    async fn append_phase(&self, phase: EffectPhase) -> ToolResult<()> {
        self.journal.append_phase(phase).await
    }

    fn require_state(&self, effect: &EffectId, expected: LifecycleState) -> ToolResult<()> {
        self.journal.require_state(effect, expected)
    }

    fn require_recorded_intent(&self, intent: &EffectIntent) -> ToolResult<EffectIntent> {
        self.journal.require_recorded_intent(intent)
    }

    fn set_state(&self, effect: &EffectId, state: LifecycleState) -> ToolResult<()> {
        self.journal.set_state(effect, state)
    }

    fn permission_menu_for(&mut self, intent: &EffectIntent) -> Menu {
        self.next_menu += 1;
        let mut body = self
            .approval_previews
            .get(&intent.effect)
            .cloned()
            .unwrap_or_else(|| vec![intent.summary.clone()]);
        body.push(format!("Effect class: {:?}", intent.class));
        if intent.class == EffectClass::ProcessExec {
            body.push(
                "Approve for this session allows only this exact command shape \
                 (shell program, canonical cwd, and environment allowlist)."
                    .into(),
            );
        } else {
            body.push(format!(
                "Approve for this session allows the {:?} class until the session ends.",
                intent.class
            ));
        }
        Menu {
            id: MenuId::new(format!(
                "permission-{}-{}-{}-{}",
                self.session_id.as_str(),
                self.worker_generation,
                self.started_at_ms,
                self.next_menu
            )),
            kind: MenuKind::Permission {
                effect_summary: intent.summary.clone(),
            },
            title: format!("Allow {}?", intent.summary),
            body,
            options: vec![
                permission_option("approve_once", "Approve once", DecisionKind::AllowOnce),
                permission_option(
                    "approve_for_session",
                    "Approve for this session",
                    DecisionKind::AllowAlways,
                ),
                permission_option("deny", "Deny", DecisionKind::RejectOnce),
            ],
            blocking: true,
            scope: MenuScope::Session,
            origin: "effect_broker".into(),
            ttl_ms: None,
            timeout_option: None,
        }
    }

    /// Consumes (single use) a stored menu decision for this exact
    /// class + digest. One-shot rules are consulted only when the base policy
    /// would ask, so a policy-level allow or deny always wins.
    fn take_one_shot_decision(&mut self, intent: &EffectIntent) -> Option<PolicyDecision> {
        let index = self.one_shot_rules.iter().position(|rule| {
            rule.class == intent.class && rule.args_digest == intent.args_digest
        })?;
        Some(self.one_shot_rules.remove(index).decision)
    }
}

fn finish_close(
    report: EffectBrokerCloseReport,
    mut errors: Vec<ToolError>,
    terminal_errors: Vec<(EffectId, ToolError)>,
) -> Result<EffectBrokerCloseReport, EffectBrokerCloseError> {
    errors.extend(
        terminal_errors
            .into_iter()
            .map(|(effect, error)| ToolError::Runtime {
                message: format!("effect {effect} terminal journal append failed: {error}"),
            }),
    );
    let mut unique = Vec::new();
    for error in errors {
        if !unique
            .iter()
            .any(|existing: &ToolError| existing.to_string() == error.to_string())
        {
            unique.push(error);
        }
    }
    if unique.is_empty() {
        Ok(report)
    } else {
        Err(EffectBrokerCloseError {
            report,
            errors: unique,
        })
    }
}

fn permission_option(key: &str, label: &str, decision: DecisionKind) -> MenuOption {
    MenuOption {
        key: key.into(),
        label: label.into(),
        detail: None,
        decision: Some(decision),
    }
}

fn selected_decision(menu: &Menu, answer: &MenuAnswer) -> ToolResult<DecisionKind> {
    let option = match &answer.option_key {
        Some(key) => menu
            .options
            .iter()
            .find(|option| &option.key == key)
            .ok_or_else(|| ToolError::InvalidMenuAnswer {
                menu: menu.id.clone(),
                message: format!("unknown option key `{key}`"),
            })?,
        None => usize::try_from(answer.option_index)
            .ok()
            .and_then(|index| menu.options.get(index))
            .ok_or_else(|| ToolError::InvalidMenuAnswer {
                menu: menu.id.clone(),
                message: format!("option index {} is out of range", answer.option_index),
            })?,
    };
    option.decision.ok_or_else(|| ToolError::InvalidMenuAnswer {
        menu: menu.id.clone(),
        message: format!("option `{}` has no decision", option.key),
    })
}

fn unix_time_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn canonical_json(value: Value) -> ToolResult<Vec<u8>> {
    serde_json::to_vec(&canonicalize(value)).map_err(|error| ToolError::InvalidArgument {
        message: format!("effect arguments are not canonicalizable JSON: {error}"),
    })
}

/// Sorts object keys recursively. Array order is semantic and preserved.
fn canonicalize(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.into_iter().collect();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonicalize(value));
            }
            Value::Object(canonical)
        }
        scalar => scalar,
    }
}
