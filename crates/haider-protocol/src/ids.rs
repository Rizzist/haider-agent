//! Opaque identifiers. Every id is a string newtype: display identity (callsigns)
//! never appears here — the wire keys everything by opaque id (§5.1 dignity rule).

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(
    #[doc = "A session — a global, directory-agnostic object."]
    SessionId
);
string_id!(
    #[doc = "A branch (named ref) within a session's history tree."]
    BranchId
);
string_id!(
    #[doc = "One run (turn) on a branch."]
    RunId
);
string_id!(
    #[doc = "An agent — head or subagent; recursive."]
    AgentId
);
string_id!(
    #[doc = "An enrolled device (daemon identity)."]
    DeviceId
);
string_id!(
    #[doc = "A single committed event."]
    EventId
);
string_id!(
    #[doc = "A menu (typed interaction card), answerable by id from any surface."]
    MenuId
);
string_id!(
    #[doc = "A delegation lease for a placed child."]
    LeaseId
);
string_id!(
    #[doc = "A side-effect record (intent through outcome)."]
    EffectId
);
string_id!(
    #[doc = "A history-tree node."]
    NodeId
);
string_id!(
    #[doc = "BLAKE3 content address into the CAS."]
    ArtifactRef
);
string_id!(
    #[doc = "Content hash of a sealed workspace tree (verification binds to this)."]
    WorkspaceRevision
);
string_id!(
    #[doc = "A stored credential alias (never the secret)."]
    CredentialAlias
);
string_id!(
    #[doc = "One turn item (message/reasoning/tool-call/…) in the item lifecycle."]
    ItemId
);
