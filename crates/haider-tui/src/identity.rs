//! TUI identity (W3c3, report R11 cut 1) — two identities, never one.
//!
//! * [`haider_protocol::ids::SessionId`] is the PROTOCOL's opaque string
//!   session identity. It keys the session map, the attached/last-detached
//!   slots, hit targets, the persistence DTO, and every live RPC coordinate.
//!   The daemon mints its own; the demo mints stable
//!   [`demo_session_id`] strings. Nothing in the TUI may parse one.
//! * [`UiGeneration`] is a LOCAL, process-monotonic `u64` minted once per
//!   session row and never reused. It is what the demo driver's arms and
//!   token meters key on, and what asynchronous response guards (the answer
//!   outbox's `origin`, the auto-title callback) compare.
//!
//! Keeping them separate is R11's explicit instruction: "Do not repurpose a
//! session ID as the stale-timer epoch." Before W3c3 a single `u64` played
//! both roles, so the protocol's opaque string could not be adopted without
//! also inventing a numeric epoch. The split is exactly that invention —
//! the generation carries forward the OLD id's semantics (monotonic, never
//! reused, `0` = the no-session scratch surface), so every demo law that
//! read `session_identity()` keeps reading the same numbers.

use haider_protocol::ids::SessionId;

/// A local, process-monotonic UI generation. See the module charter.
///
/// `Copy` and cheap to hash on purpose: it is the key of the demo driver's
/// arm table and per-session token meters, both consulted on every beat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct UiGeneration(u64);

impl UiGeneration {
    /// The no-session SCRATCH surface — the old `session_identity()`
    /// sentinel `0`, preserved verbatim so the driver's "is this event for
    /// the surface on screen" gates keep their exact demo behavior.
    pub const SCRATCH: Self = Self(0);

    /// The first generation an allocator may hand out (the scratch sentinel
    /// is reserved, so real sessions start at 1 — the demo seeds hold 1-3
    /// exactly as their old numeric ids did).
    pub const FIRST: Self = Self(1);

    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// True for the no-session scratch surface.
    #[must_use]
    pub const fn is_scratch(self) -> bool {
        self.0 == Self::SCRATCH.0
    }
}

impl std::fmt::Display for UiGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// The DEMO's session-id prefix. The demo's identities are stable strings so
/// a persisted demo file round-trips them verbatim; the v1 store's numeric
/// ids upcast through [`demo_session_id`] and only through it.
pub const DEMO_SESSION_PREFIX: &str = "demo-session-";

/// The demo's stable session id for a generation (report R11 cut 1: "Demo
/// seeds use stable string IDs such as `demo-session-1`").
///
/// This is the ONLY place the demo's numeric generation becomes a session
/// id: the v1 → v2 demo-store upcaster calls it, the seeds call it, and
/// `new_session` calls it, so a v1 file's `id: 2` and a freshly seeded
/// session 2 are the same string by construction.
#[must_use]
pub fn demo_session_id(generation: UiGeneration) -> SessionId {
    SessionId::new(format!("{DEMO_SESSION_PREFIX}{}", generation.get()))
}
