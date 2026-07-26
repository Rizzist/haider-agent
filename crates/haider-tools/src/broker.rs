//! Effect normalization, permission evaluation, and lifecycle journaling.
//!
//! Owned invariants:
//! - **Four-phase effect law.** Every effect journals `Intent` → `Authorized`
//!   → `Dispatched` → `Outcome`, in that order; each phase is appended through
//!   the [`JournalSink`] before the next step runs, and a failed append halts
//!   the effect. The crash window between dispatch and result is
//!   representable as [`EffectOutcome::Unknown`] (see
//!   [`EffectBroker::journal_unknown`]).
//! - **Digest-bound approvals.** `args_digest` is BLAKE3 over canonical
//!   (recursively key-sorted) argument JSON — never a tool name. A persistent
//!   "always allow" stores class + exact digest, so a mutated operation gets
//!   a new digest and re-asks.
//! - **Deny wins** over every form of approval, and `Dispatched`/`Outcome`
//!   can only follow an `Allow` authorization (enforced by `require_state`).
//! - While a broker owns its sink, effect phases enter the journal only
//!   through the broker — there is no mutable accessor to append around the
//!   lifecycle checks.

use crate::{ToolError, ToolResult};
use async_trait::async_trait;
use haider_protocol::EventPayload;
use haider_protocol::effect::{
    AuthorizationVerdict, EffectClass, EffectIntent, EffectOutcome, EffectPhase,
};
use haider_protocol::ids::{EffectId, MenuId, WorkspaceRevision};
use haider_protocol::menu::{DecisionKind, Menu, MenuAnswer, MenuKind, MenuOption, MenuScope};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

// Process-wide counters so distinct brokers in one harness process can never
// mint colliding effect or menu ids.
static NEXT_EFFECT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MENU_ID: AtomicU64 = AtomicU64::new(1);

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
    Outcome,
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

/// Normalizes, authorizes, and journals effectful operations.
pub struct EffectBroker<J> {
    journal: J,
    /// Terminal states are retained, never evicted: a duplicate transition on
    /// a finished effect must fail loudly instead of re-journaling a phase.
    lifecycles: HashMap<EffectId, LifecycleState>,
    pending_permissions: HashMap<MenuId, PendingPermission>,
    one_shot_rules: Vec<OneShotRule>,
}

impl<J> EffectBroker<J>
where
    J: JournalSink,
{
    pub fn new(journal: J) -> Self {
        Self {
            journal,
            lifecycles: HashMap::new(),
            pending_permissions: HashMap::new(),
            one_shot_rules: Vec::new(),
        }
    }

    pub fn journal(&self) -> &J {
        &self.journal
    }

    /// Consumes the broker to reclaim its sink (e.g. at session teardown).
    /// Only shared and consuming access exist — see the module header.
    pub fn into_journal(self) -> J {
        self.journal
    }

    /// Produces and journals an intent whose digest is the BLAKE3 hash of
    /// recursively key-sorted JSON arguments.
    pub async fn normalize<O>(&mut self, operation: &O) -> ToolResult<EffectIntent>
    where
        O: EffectOperation,
    {
        let canonical = canonical_json(operation.arguments()?)?;
        let args_digest = format!("blake3:{}", blake3::hash(&canonical).to_hex());
        let sequence = NEXT_EFFECT_ID.fetch_add(1, Ordering::Relaxed);
        let intent = EffectIntent {
            effect: EffectId::new(format!("effect-{sequence}")),
            class: operation.effect_class(),
            summary: operation.summary(),
            args_digest,
            workspace_revision: operation.workspace_revision(),
        };
        self.append_phase(EffectPhase::Intent(intent.clone()))
            .await?;
        self.lifecycles
            .insert(intent.effect.clone(), LifecycleState::Intent);
        Ok(intent)
    }

    /// Evaluates and journals the authorization verdict for one intent.
    pub async fn authorize(
        &mut self,
        intent: &EffectIntent,
        policy: &PermissionPolicy,
    ) -> ToolResult<AuthorizationVerdict> {
        self.require_state(&intent.effect, LifecycleState::Intent)?;
        let decision = match policy.decision(intent) {
            PolicyDecision::Ask => self
                .take_one_shot_decision(intent)
                .unwrap_or(PolicyDecision::Ask),
            decision => decision,
        };
        let mut pending_permission = None;
        let verdict = match decision {
            PolicyDecision::Allow => AuthorizationVerdict::Allow,
            PolicyDecision::Ask => {
                let menu = self.permission_menu_for(intent);
                let menu_id = menu.id.clone();
                pending_permission = Some((
                    menu_id.clone(),
                    PendingPermission {
                        menu,
                        class: intent.class.clone(),
                        args_digest: intent.args_digest.clone(),
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
        self.lifecycles.insert(intent.effect.clone(), state);
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

    pub async fn journal_dispatched(&mut self, effect: &EffectId) -> ToolResult<()> {
        self.require_state(effect, LifecycleState::AuthorizedAllow)?;
        self.append_phase(EffectPhase::Dispatched {
            effect: effect.clone(),
        })
        .await?;
        self.lifecycles
            .insert(effect.clone(), LifecycleState::Dispatched);
        Ok(())
    }

    pub async fn journal_outcome(
        &mut self,
        effect: &EffectId,
        outcome: EffectOutcome,
    ) -> ToolResult<()> {
        self.require_state(effect, LifecycleState::Dispatched)?;
        self.append_phase(EffectPhase::Outcome {
            effect: effect.clone(),
            outcome,
        })
        .await?;
        self.lifecycles
            .insert(effect.clone(), LifecycleState::Outcome);
        Ok(())
    }

    /// Reconciles the dispatch/outcome crash window explicitly.
    pub async fn journal_unknown(&mut self, effect: &EffectId) -> ToolResult<()> {
        self.journal_outcome(effect, EffectOutcome::Unknown).await
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
                self.journal_dispatched(&intent.effect).await?;
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
        match result {
            Ok(value) => {
                self.journal_outcome(&intent.effect, EffectOutcome::Ok)
                    .await?;
                Ok(value)
            }
            Err(error) => {
                self.journal_outcome(
                    &intent.effect,
                    EffectOutcome::Failed {
                        error: error.to_string(),
                    },
                )
                .await?;
                Err(error)
            }
        }
    }

    async fn append_phase(&mut self, phase: EffectPhase) -> ToolResult<()> {
        self.journal.append(EventPayload::Effect(phase)).await
    }

    fn require_state(&self, effect: &EffectId, expected: LifecycleState) -> ToolResult<()> {
        let actual = self.lifecycles.get(effect).copied();
        if actual == Some(expected) {
            return Ok(());
        }
        Err(ToolError::Lifecycle {
            message: format!("effect {effect} is {actual:?}, expected {expected:?}"),
        })
    }

    fn permission_menu_for(&self, intent: &EffectIntent) -> Menu {
        let sequence = NEXT_MENU_ID.fetch_add(1, Ordering::Relaxed);
        Menu {
            id: MenuId::new(format!("permission-{sequence}")),
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

fn permission_option(key: &str, label: &str, decision: DecisionKind) -> MenuOption {
    MenuOption {
        key: key.into(),
        label: label.into(),
        detail: None,
        decision: Some(decision),
    }
}

fn selected_decision(menu: &Menu, answer: &MenuAnswer) -> ToolResult<DecisionKind> {
    let option = answer
        .option_key
        .as_ref()
        .and_then(|key| menu.options.iter().find(|option| &option.key == key))
        .or_else(|| {
            usize::try_from(answer.option_index)
                .ok()
                .and_then(|index| menu.options.get(index))
        })
        .ok_or_else(|| ToolError::InvalidArgument {
            message: format!("invalid option for permission menu {}", menu.id),
        })?;
    option.decision.ok_or_else(|| ToolError::InvalidArgument {
        message: format!("permission menu option `{}` has no decision", option.key),
    })
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
