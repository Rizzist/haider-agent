//! Boot-time Loom registry seeding: two owner-specified default agent
//! types every profile starts with. Seed-once-if-absent — a profile that
//! has (or once had and revised) a seeded id is never overwritten, so user
//! edits always outlive the seed (the provider-registry merge law's
//! stricter sibling; registration itself is digest-idempotent but a
//! revised record would otherwise be clobbered back on every boot).

use haider_core::SqliteStoreHandle;
use haider_protocol::error::HaiderError;
use haider_protocol::loom::LoomAgentType;

/// The seeded defaults: generic enough to reach for daily, narrow enough
/// that delegating to one saves the parent's context. Colors are the
/// typed-agent accents the TUI paints (never the default gold).
fn seed_agent_types() -> Vec<LoomAgentType> {
    vec![
        LoomAgentType {
            id: "scout".into(),
            name: "Scout".into(),
            job: "Read-only reconnaissance. Locate the files, symbols, and \
                  seams relevant to the brief; read only what qualifies; \
                  return a terse map of file:line findings with one-line \
                  conclusions. Never edit, never run mutating commands, \
                  never speculate — report absence of evidence as absence."
                .into(),
            in_type: "Brief".into(),
            out_type: "Map".into(),
            clis: Vec::new(),
            apis: Vec::new(),
            denials: Vec::new(),
            skills: Vec::new(),
            scripts: Vec::new(),
            color: "#7aa2f7".into(),
            glyph: "⌖".into(),
            rev: 0,
        },
        LoomAgentType {
            id: "reviewer".into(),
            name: "Reviewer".into(),
            job: "Adversarial verification of one change or claim. Read the \
                  diff and its tests; actively try to refute correctness; \
                  report numbered findings with file:line and a concrete \
                  failure scenario; end with VERDICT: SHIP or VERDICT: FIX. \
                  Never edit the change under review."
                .into(),
            in_type: "Change".into(),
            out_type: "Verdict".into(),
            clis: Vec::new(),
            apis: Vec::new(),
            denials: Vec::new(),
            skills: Vec::new(),
            scripts: Vec::new(),
            color: "#bb9af7".into(),
            glyph: "⚖".into(),
            rev: 0,
        },
    ]
}

/// Seeds absent defaults; present ids (any rev) are left exactly as found.
pub(crate) async fn seed_loom_registry(store: &SqliteStoreHandle) -> Result<(), HaiderError> {
    for record in seed_agent_types() {
        if store.loom_agent_type(record.id.clone()).await?.is_none() {
            match store
                .loom_register_agent_type_with_install_cas(
                    record,
                    haider_protocol::loom::LoomRevisionExpectation {
                        rev: 0,
                        digest: None,
                    },
                )
                .await?
            {
                haider_core::LoomRegistryMutation::Applied { .. } => {}
                haider_core::LoomRegistryMutation::Conflict(_) => {
                    // Startup has no concurrent client door. A conflict here
                    // can only mean another startup owner violated the
                    // profile lock, so never overwrite the now-current row.
                }
            }
        }
    }
    Ok(())
}
