//! Effects — permissions key off normalized effects (what an operation
//! touches), never tool names (§9). The effect record is a four-phase life:
//! intent → authorized → dispatched → outcome. A crash between dispatch and
//! outcome yields `Unknown`, surfacing `effect_outcome_unknown` (§3 honesty).

use crate::ids::{EffectId, MenuId, WorkspaceRevision};
use serde::{Deserialize, Serialize};

/// Durable per-session identity of the bytes last observed or written for one
/// workspace-relative file. A redacted read uses a keyed digest so the
/// journal cannot serve as an offline guessing oracle; public projections
/// (headless run output) omit redacted freshness claims entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFreshness {
    pub path: String,
    pub digest: String,
}

/// Daemon-stamped provenance for one effect that changed workspace state.
///
/// Producers supply `mutation_digest`; the store assigns the monotonic
/// revision and its subject digest at the same commit point as the terminal
/// effect fact. All fields are nested under an optional outcome field so
/// pre-revision journals retain their exact wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceMutation {
    pub effect_id: EffectId,
    pub mutation_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_revision: Option<WorkspaceRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_digest: Option<String>,
    /// The mutated path or content would be redacted in an agent view. Its
    /// exact digests remain owner-local: agent-visible results and public
    /// projections withhold `mutation_digest`/`subject_digest`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted_content: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum EffectClass {
    /// Daemon/provider transport bookkeeping; never agent activity.
    ProviderTransport,
    /// Operator checkpoint restore (undo, redo, rollback), not agent editing.
    CheckpointRestore,
    FsRead,
    FsWrite,
    ProcessExec,
    /// Execute a command on a saved remote SSH target. This remains distinct
    /// from local process authority and defaults to an explicit Ask.
    RemoteExecution,
    Network {
        host: String,
    },
    GitOp,
    AgentSpawn,
    /// Deliver untrusted input to another peer at a turn boundary.
    PeerMessage,
    CredentialAccess,
    /// Reserved (§9.3) — the computer-use pack lands post-0.1.
    GuiAct,
    /// Read pixels or the pointer location without actuating the desktop.
    ScreenObserve,
    /// Actuate the pointer, keyboard, scroll wheel, or an interactive wait.
    ScreenControl,
    /// Read pixels or structured accessibility state from an activated mobile capability.
    MobileObserve,
    /// Actuate an explicitly activated mobile capability.
    MobileControl,
    /// Read SMS data from an explicitly activated mobile capability.
    ReadSms,
    /// Spend one provider-issued rate-limit reset credit. This effect is
    /// deliberately distinct from ordinary network access: it consumes a
    /// limited account resource and can never be auto-approved.
    UsageResetApply,
}

/// Journaled BEFORE dispatch — the durable pre-image of every side effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "JournalEffectIntent", into = "JournalEffectIntent")]
pub struct EffectIntent {
    pub effect: EffectId,
    pub class: EffectClass,
    /// One-line human summary ("patch src/lib.rs", "run cargo test").
    pub summary: String,
    /// Digest of normalized arguments — approvals bind to this; a mutated
    /// operation gets a new digest, invalidating prior approvals.
    pub args_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_revision: Option<WorkspaceRevision>,
}

/// Keep authority typed in memory while using only released class values on
/// disk. The 0.0.972 intent decoder ignores this additive origin field.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EffectOrigin {
    CheckpointRestore,
    ProviderTransport,
    UsageResetApply,
    // A later writer may add an origin while retaining a released carrier.
    // Older readers keep that carrier's meaning instead of bricking replay.
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize)]
struct JournalEffectIntent {
    effect: EffectId,
    class: EffectClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effect_origin: Option<EffectOrigin>,
    summary: String,
    args_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_revision: Option<WorkspaceRevision>,
}

fn released_class(class: EffectClass) -> (EffectClass, Option<EffectOrigin>) {
    match class {
        EffectClass::CheckpointRestore => {
            (EffectClass::FsWrite, Some(EffectOrigin::CheckpointRestore))
        }
        EffectClass::ProviderTransport => (
            EffectClass::Network {
                host: "provider-request".into(),
            },
            Some(EffectOrigin::ProviderTransport),
        ),
        EffectClass::UsageResetApply => (
            EffectClass::CredentialAccess,
            Some(EffectOrigin::UsageResetApply),
        ),
        // Keep this exhaustive: adding an internal class must require a
        // deliberate released-decoder-compatible write representation.
        class @ (EffectClass::FsRead
        | EffectClass::FsWrite
        | EffectClass::ProcessExec
        | EffectClass::RemoteExecution
        | EffectClass::Network { .. }
        | EffectClass::GitOp
        | EffectClass::AgentSpawn
        | EffectClass::PeerMessage
        | EffectClass::CredentialAccess
        | EffectClass::GuiAct
        | EffectClass::ScreenObserve
        | EffectClass::ScreenControl
        | EffectClass::MobileObserve
        | EffectClass::MobileControl
        | EffectClass::ReadSms) => (class, None),
    }
}

fn restore_class(
    class: EffectClass,
    effect_origin: Option<EffectOrigin>,
) -> Result<EffectClass, &'static str> {
    // Prefer explicit provenance, never a tool name or human summary.
    // Reject a mismatched carrier rather than guessing mutation authority.
    Ok(match (effect_origin, class) {
        (Some(EffectOrigin::CheckpointRestore), EffectClass::FsWrite) => {
            EffectClass::CheckpointRestore
        }
        (Some(EffectOrigin::ProviderTransport), EffectClass::Network { host })
            if host == "provider-request" =>
        {
            EffectClass::ProviderTransport
        }
        (Some(EffectOrigin::UsageResetApply), EffectClass::CredentialAccess) => {
            EffectClass::UsageResetApply
        }
        (None | Some(EffectOrigin::Unknown), class) => class,
        (Some(_), _) => return Err("effect origin does not match its released class carrier"),
    })
}

impl From<EffectIntent> for JournalEffectIntent {
    fn from(intent: EffectIntent) -> Self {
        let (class, effect_origin) = released_class(intent.class);
        Self {
            effect: intent.effect,
            class,
            effect_origin,
            summary: intent.summary,
            args_digest: intent.args_digest,
            workspace_revision: intent.workspace_revision,
        }
    }
}

impl TryFrom<JournalEffectIntent> for EffectIntent {
    type Error = &'static str;

    fn try_from(intent: JournalEffectIntent) -> Result<Self, Self::Error> {
        let class = restore_class(intent.class, intent.effect_origin)?;
        Ok(Self {
            effect: intent.effect,
            class,
            summary: intent.summary,
            args_digest: intent.args_digest,
            workspace_revision: intent.workspace_revision,
        })
    }
}

/// Direct inventory fields use the same authority mapping as journal intents.
/// The additive origin lives beside the released class tag in these fields.
pub(crate) mod compatible_class {
    use super::{EffectClass, EffectOrigin, released_class, restore_class};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    struct Carrier {
        #[serde(flatten)]
        class: EffectClass,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect_origin: Option<EffectOrigin>,
    }

    pub(crate) fn serialize<S: Serializer>(
        class: &EffectClass,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let (class, effect_origin) = released_class(class.clone());
        if effect_origin.is_none() {
            return class.serialize(serializer);
        }
        Carrier {
            class,
            effect_origin,
        }
        .serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<EffectClass, D::Error> {
        let value = Carrier::deserialize(deserializer)?;
        restore_class(value.class, value.effect_origin).map_err(serde::de::Error::custom)
    }
}

pub(crate) mod compatible_classes {
    use super::EffectClass;
    use serde::ser::SerializeSeq;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    struct ClassRef<'a>(&'a EffectClass);

    impl Serialize for ClassRef<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            super::compatible_class::serialize(self.0, serializer)
        }
    }

    #[derive(Deserialize)]
    struct ClassValue(#[serde(with = "super::compatible_class")] EffectClass);

    pub(crate) fn serialize<S: Serializer>(
        classes: &[EffectClass],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(classes.len()))?;
        for class in classes {
            sequence.serialize_element(&ClassRef(class))?;
        }
        sequence.end()
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<EffectClass>, D::Error> {
        Ok(Vec::<ClassValue>::deserialize(deserializer)?
            .into_iter()
            .map(|value| value.0)
            .collect())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum AuthorizationVerdict {
    Allow,
    /// The user directly initiated this effect (for example with `!cmd`).
    /// It is durably distinguished from a model effect allowed by policy.
    PreAuthorized {
        source: AuthorizationSource,
    },
    /// Blocked on a permission menu.
    Ask {
        menu: MenuId,
    },
    Deny {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationSource {
    UserTyped,
}

/// Effect lifecycle events as journaled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum EffectPhase {
    Intent(EffectIntent),
    Authorized {
        effect: EffectId,
        verdict: AuthorizationVerdict,
    },
    Dispatched {
        effect: EffectId,
    },
    Outcome {
        effect: EffectId,
        outcome: EffectOutcome,
        /// Atomically advances the session's file freshness state with the
        /// terminal effect. `None` preserves the pre-C1 wire shape exactly.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        freshness: Option<FileFreshness>,
        /// Present only when the effect actually changed workspace state.
        /// The store stamps the revision and subject atomically on commit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_mutation: Option<WorkspaceMutation>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum EffectOutcome {
    Ok,
    /// The effect crossed the dispatch boundary and was deliberately stopped.
    /// Cancellation is a terminal outcome, never a failure.
    Cancelled,
    /// Cancellation won, but userspace could not prove process-group death.
    /// The note is durable and orderly broker close separately reports the
    /// leaked registry entry.
    CancelledEscalated {
        note: String,
    },
    Failed {
        error: String,
    },
    /// Crash window between dispatch and result — reconciled via Recovery menu.
    Unknown,
}
