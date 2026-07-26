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
//!   drops a finalizer: it drains all registered work and surfaces unobserved
//!   errors. Process death is the W3 crash-recovery seam: durable `Dispatched`
//!   phases without a terminal phase are reconciled at startup by journaling
//!   [`EffectOutcome::Unknown`] (see [`EffectBroker::journal_unknown`]).
//! - **Digest-bound approvals.** `args_digest` is BLAKE3 over canonical
//!   (recursively key-sorted) argument JSON — never a tool name. A persistent
//!   "always allow" stores class + exact digest, so a mutated operation gets
//!   a new digest and re-asks.
//! - **Intent-bound authorization.** Lifecycle state retains the complete
//!   journaled intent. Authorization and later transitions accept that intent,
//!   and reject a reused effect id whose digest or other fields differ.
//! - **Deny wins** over every form of approval, and `Dispatched`/`Outcome`
//!   can only follow an `Allow` authorization (enforced by `require_state`).
//! - The journal sink never leaves the broker. Tests inspect a broker-owned,
//!   read-only snapshot of successfully appended phases instead of recovering
//!   or mutating the sink.

use crate::{ToolError, ToolResult};
use async_trait::async_trait;
use haider_protocol::EventPayload;
use haider_protocol::effect::{
    AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase,
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
#[async_trait]
pub trait JournalSink: Send {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()>;
}

/// An operation that can be reduced to the protocol's permission key.
pub trait EffectOperation {
    fn effect_class(&self) -> EffectClass;
    fn summary(&self) -> String;
    fn arguments(&self) -> ToolResult<Value>;

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

#[derive(Debug, Clone)]
struct DenyRule {
    class: EffectClass,
    reason: String,
}

/// Permission policy keyed by normalized effect class.
///
/// Classes absent from all three lists default to `Ask`. Overlaps are resolved
/// conservatively: deny, exact always-allow, allow, ask.
#[derive(Debug, Clone, Default)]
pub struct PermissionPolicy {
    allowlist: Vec<EffectClass>,
    asklist: Vec<EffectClass>,
    denylist: Vec<DenyRule>,
    always_allow: Vec<AlwaysAllowRule>,
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
        if let Some(rule) = self.denylist.iter().find(|rule| rule.class == intent.class) {
            return PolicyDecision::Deny {
                reason: rule.reason.clone(),
            };
        }
        if self
            .always_allow
            .iter()
            .any(|rule| rule.class == intent.class && rule.args_digest == intent.args_digest)
            || self.allowlist.contains(&intent.class)
        {
            return PolicyDecision::Allow;
        }
        PolicyDecision::Ask
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleState {
    Intent,
    AuthorizedAllow,
    AuthorizedBlocked,
    Dispatched,
    Terminalizing,
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
    /// Terminal states are retained, never evicted. `Terminalizing` is the
    /// atomic ownership claim: all later terminal writers become no-ops.
    lifecycles: HashMap<EffectId, LifecycleRecord>,
    journaled_phases: Vec<EffectPhase>,
    terminal_errors: Vec<ToolError>,
}

/// Shared ownership lets a broker-owned finalizer durably close a dispatched
/// effect after its caller has been cancelled, while preserving one lifecycle
/// lock and a read-only phase snapshot.
struct BrokerJournal<J> {
    sink: Arc<tokio::sync::Mutex<J>>,
    state: Arc<Mutex<BrokerJournalState>>,
}

impl<J> BrokerJournal<J>
where
    J: JournalSink,
{
    fn new(sink: J) -> Self {
        Self {
            sink: Arc::new(tokio::sync::Mutex::new(sink)),
            state: Arc::new(Mutex::new(BrokerJournalState {
                lifecycles: HashMap::new(),
                journaled_phases: Vec::new(),
                terminal_errors: Vec::new(),
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
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .journaled_phases
            .push(phase);
        Ok(())
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
    /// `Terminalizing` and `Outcome` are successful no-op states for competing
    /// writers. Earlier lifecycle states remain errors because they have not
    /// crossed the durable dispatch boundary.
    fn claim_terminal(&self, intent: &EffectIntent) -> ToolResult<Option<TerminalClaim<J>>> {
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
                }))
            }
            LifecycleState::Terminalizing | LifecycleState::Outcome => Ok(None),
            state => Err(ToolError::Lifecycle {
                message: format!(
                    "effect {} is {state:?}, expected {:?}",
                    intent.effect,
                    LifecycleState::Dispatched
                ),
            }),
        }
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

    fn record_terminal_error(&self, error: ToolError) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .terminal_errors
            .push(error);
    }

    fn take_terminal_errors(&self) -> Vec<ToolError> {
        std::mem::take(
            &mut self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .terminal_errors,
        )
    }

    async fn journal_outcome(
        &self,
        intent: &EffectIntent,
        outcome: EffectOutcome,
    ) -> ToolResult<()> {
        let Some(claim) = self.claim_terminal(intent)? else {
            return Ok(());
        };
        claim.append(outcome).await
    }

    async fn finish<T>(&self, intent: &EffectIntent, result: ToolResult<T>) -> ToolResult<T> {
        let outcome = match &result {
            Ok(_) => EffectOutcome::Ok,
            Err(error) => EffectOutcome::Failed {
                error: error.to_string(),
            },
        };
        let Some(claim) = self.claim_terminal(intent)? else {
            return result;
        };
        match claim.append(outcome).await {
            Ok(()) => result,
            Err(error) => Err(error),
        }
    }
}

impl<J> Clone for BrokerJournal<J> {
    fn clone(&self) -> Self {
        Self {
            sink: Arc::clone(&self.sink),
            state: Arc::clone(&self.state),
        }
    }
}

/// Unique, non-cloneable authority to append one terminal phase.
struct TerminalClaim<J> {
    journal: BrokerJournal<J>,
    effect: EffectId,
}

impl<J> TerminalClaim<J>
where
    J: JournalSink,
{
    async fn append(self, outcome: EffectOutcome) -> ToolResult<()> {
        let phase = EffectPhase::Outcome {
            effect: self.effect.clone(),
            outcome,
        };
        match self.journal.append_phase(phase).await {
            Ok(()) => {
                self.journal.mark_outcome(&self.effect);
                Ok(())
            }
            Err(error) => {
                // Retain the original append failure even when the one allowed
                // Unknown escalation succeeds, so close() can surface it.
                self.journal.record_terminal_error(error.clone());
                let escalation = EffectPhase::Outcome {
                    effect: self.effect.clone(),
                    outcome: EffectOutcome::Unknown,
                };
                match self.journal.append_phase(escalation).await {
                    Ok(()) => self.journal.mark_outcome(&self.effect),
                    Err(escalation_error) => {
                        self.journal.record_terminal_error(escalation_error);
                    }
                }
                Err(error)
            }
        }
    }
}

pub(crate) struct EffectFinish<J> {
    journal: BrokerJournal<J>,
    intent: EffectIntent,
}

impl<J> EffectFinish<J>
where
    J: JournalSink,
{
    pub(crate) async fn finish<T>(self, result: ToolResult<T>) -> ToolResult<T> {
        self.journal.finish(&self.intent, result).await
    }
}

/// Normalizes, authorizes, and journals effectful operations.
pub struct EffectBroker<J> {
    journal: BrokerJournal<J>,
    workspace_root: PathBuf,
    workspace_dir: OwnedFd,
    session_id: SessionId,
    worker_generation: u64,
    started_at_ms: u64,
    next_effect: u64,
    next_menu: u64,
    pending_permissions: HashMap<MenuId, PendingPermission>,
    one_shot_rules: Vec<OneShotRule>,
    next_finalizer: u64,
    finalizers: tokio::task::JoinSet<(u64, Option<ToolError>)>,
    observed_finalizers: HashSet<u64>,
}

impl<J> EffectBroker<J>
where
    J: JournalSink,
{
    /// Creates a broker rooted at `workspace_root`.
    ///
    /// `worker_generation` is the durable actor generation. Together with the
    /// session, broker start time, and local counters it prevents identity
    /// reuse across same-millisecond restarts.
    pub fn new(
        journal: J,
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
    pub fn new_at(
        journal: J,
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
            one_shot_rules: Vec::new(),
            next_finalizer: 0,
            finalizers: tokio::task::JoinSet::new(),
            observed_finalizers: HashSet::new(),
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

    /// Drains every broker-owned apply finalizer before orderly shutdown.
    ///
    /// Operation errors already returned by `fs_patch` are considered
    /// observed. Errors from a cancelled caller, task failure, or terminal
    /// append are returned here. Process death is instead recovered by the W3
    /// dispatched-without-terminal startup reconciliation seam.
    pub async fn close(mut self) -> ToolResult<()> {
        let mut errors = Vec::new();
        while let Some(finalizer) = self.finalizers.join_next().await {
            match finalizer {
                Ok((id, Some(error))) if !self.observed_finalizers.contains(&id) => {
                    errors.push(error);
                }
                Ok(_) => {}
                Err(error) => errors.push(ToolError::Runtime {
                    message: format!("effect finalizer task failed: {error}"),
                }),
            }
        }
        errors.extend(self.journal.take_terminal_errors());
        surface_close_errors(errors)
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
        let state = if verdict == AuthorizationVerdict::Allow {
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
        match decision {
            DecisionKind::AllowOnce => self.one_shot_rules.push(OneShotRule {
                class: pending.class,
                args_digest: pending.args_digest,
                decision: PolicyDecision::Allow,
            }),
            DecisionKind::AllowAlways => {
                let rule = AlwaysAllowRule {
                    class: pending.class,
                    args_digest: pending.args_digest,
                };
                if !policy.always_allow.contains(&rule) {
                    policy.always_allow.push(rule);
                }
            }
            DecisionKind::RejectOnce => self.one_shot_rules.push(OneShotRule {
                class: pending.class,
                args_digest: pending.args_digest,
                decision: PolicyDecision::Deny {
                    reason: "rejected once by user".into(),
                },
            }),
            DecisionKind::RejectAlways => {
                policy.deny(pending.class, "rejected always by user");
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

    pub async fn journal_outcome(
        &mut self,
        intent: &EffectIntent,
        outcome: EffectOutcome,
    ) -> ToolResult<()> {
        self.journal.journal_outcome(intent, outcome).await
    }

    /// Reconciles the dispatch/outcome crash window explicitly.
    pub async fn journal_unknown(&mut self, intent: &EffectIntent) -> ToolResult<()> {
        self.journal_outcome(intent, EffectOutcome::Unknown).await
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
            AuthorizationVerdict::Allow => {
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
        self.journal.finish(intent, result).await
    }

    pub(crate) fn effect_finish(&self, intent: &EffectIntent) -> EffectFinish<J> {
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
        self.observed_finalizers.insert(id);
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
            body: vec![
                format!("Effect class: {:?}", intent.class),
                format!("Exact argument rule: {}", intent.args_digest),
            ],
            options: vec![
                permission_option("allow_once", "Allow once", DecisionKind::AllowOnce),
                permission_option(
                    "allow_always",
                    "Always allow this operation",
                    DecisionKind::AllowAlways,
                ),
                permission_option("reject_once", "Reject once", DecisionKind::RejectOnce),
                permission_option(
                    "reject_always",
                    "Always reject this class",
                    DecisionKind::RejectAlways,
                ),
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

fn surface_close_errors(errors: Vec<ToolError>) -> ToolResult<()> {
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
        Ok(())
    } else if unique.len() == 1 {
        Err(unique.remove(0))
    } else {
        Err(ToolError::Runtime {
            message: format!(
                "effect broker close observed multiple errors: {}",
                unique
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
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
