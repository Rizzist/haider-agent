//! `/hooks` (H4) — screen state for the daemon's hook discovery + trust
//! surface, and the per-session log of journaled hook-engine facts.
//!
//! Two authorities live here, both DAEMON TRUTH:
//!
//! * [`HooksScreenState`] holds the last `hooks.list` snapshot (workspace +
//!   profile discovery for the active session's cwd) plus the screen chrome
//!   (cursor, confirmation card, in-flight receipt). Trust and revoke ride
//!   receipted commands and install NOTHING locally — the rows move only
//!   when the daemon's next list snapshot answers, the same discipline
//!   branches earned (`crate::branch`: no live branch before daemon truth).
//! * [`HookFactsLog`] records the additive hook-engine journal facts
//!   (`hook_notice` / `hook_fired` / `hook_subscription`) the session's
//!   committed stream carries. They are `render.ui == false` envelopes, so
//!   the transcript never paints them; like the branch registry they are
//!   recorded for `Apply` AND `Skip` and surfaced only on their own screen
//!   (the `/tree`-shows-`NodeCommitted` precedent). The decision-hook chip
//!   derives from the recorded facts, never from display state.

use std::collections::{HashMap, VecDeque};

use haider_protocol::envelope::RawEnvelope;
use haider_protocol::hook::{
    HookDecisionKind, HookEventPayload, HookRuntimeKind, HookSubscriptionState,
};
use haider_protocol::ids::RunId;
use haider_rpc::HookSummaryWire;

/// Journal facts retained per session — the STORE bound. The screen shows
/// at most [`FIRING_ROWS_MAX`] of these (newest first); the store keeps a
/// deeper tail so re-entering the screen after a burst still has history.
pub const HOOK_FACTS_CAP: usize = 48;

/// Firing rows the screen's lower half may paint — the RENDER bound.
pub const FIRING_ROWS_MAX: usize = 8;

/// One discovered hook, projected from the wire summary (`hooks.list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRow {
    pub name: String,
    /// The digest-pinned trust identity (blake3 hex, H3).
    pub digest: String,
    /// The defining file's path (workspace hooks.json or the profile's).
    pub source: String,
    /// `exec` / `subscribe` / `decision` as the wire spells it.
    pub kind: String,
    /// The matcher's event kind.
    pub event: String,
    pub trusted: bool,
    pub decision: bool,
    pub timeout_ms: u64,
}

impl HookRow {
    #[must_use]
    pub fn from_wire(wire: HookSummaryWire) -> Self {
        Self {
            name: wire.name,
            digest: wire.digest,
            source: wire.source,
            kind: wire.kind,
            event: wire.event,
            trusted: wire.trusted,
            decision: wire.decision,
            timeout_ms: wire.timeout_ms,
        }
    }
}

/// The rendered trust state of one row. `RevokedByEdit` is DERIVED from two
/// daemon snapshots — the daemon reported this hook trusted under one
/// digest, and now reports the SAME name+source untrusted under another —
/// never from a local guess: an edit revokes (H3 law), and the glyph says
/// which kind of untrusted this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustGlyph {
    Trusted,
    Untrusted,
    RevokedByEdit,
}

impl TrustGlyph {
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Trusted => "✓",
            Self::Untrusted => "○",
            Self::RevokedByEdit => "✗",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
            Self::RevokedByEdit => "revoked by edit",
        }
    }
}

/// The open trust/revoke confirmation card. VALUE-CARRYING like every hit:
/// the digest was captured when the card opened, so a list refresh under
/// the card can never retarget the receipt at a different hook (the daemon
/// validates the digest either way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustConfirm {
    pub digest: String,
    pub name: String,
    /// `true` asks `hooks.trust`; `false` asks `hooks.revoke`.
    pub grant: bool,
}

/// The `/hooks` screen state. OPTIMISM FORBIDDEN (the accounts-screen law):
/// a trust receipt retires the in-flight marker and chains a fresh
/// `hooks.list` — the trust column moves only when daemon truth answers.
#[derive(Debug, Default)]
pub struct HooksScreenState {
    /// `None` while the live read is in flight; `Some` is the daemon's (or
    /// the demo's honestly empty) listing.
    pub rows: Option<Vec<HookRow>>,
    /// The profile trust policy the listing reported.
    pub policy: Option<String>,
    /// Last action/refusal line, shown under the header.
    pub message: Option<String>,
    /// Keyboard highlight over the rows (owner menu law).
    pub cursor: usize,
    /// The open confirmation card, if any (esc cancels the CARD — the
    /// session-scoped esc law).
    pub confirm: Option<TrustConfirm>,
    /// The digest of the receipted command in flight. While `Some`, further
    /// trust actions are refused (one at a time) and the row wears `…`.
    pub pending: Option<String>,
    /// DAEMON-TRUTH memory: the digest each hook (name + source file) wore
    /// when the daemon last reported it TRUSTED — written from list
    /// snapshots and trust receipts only. An untrusted row whose identity
    /// is here under a DIFFERENT digest renders ✗ revoked-by-edit.
    baseline: HashMap<String, String>,
}

/// The baseline identity of one hook: name + defining file. The digest
/// deliberately stays OUT of the key — it is the value being tracked.
fn identity_key(name: &str, source: &str) -> String {
    format!("{name}\u{0}{source}")
}

/// The 8-hex short form of a digest, for rows and messages.
#[must_use]
pub fn short_digest(digest: &str) -> &str {
    digest.get(..8).unwrap_or(digest)
}

impl HooksScreenState {
    /// Enter the screen in LIVE mode: the listing is a daemon read in
    /// flight. The trust baseline survives re-entry on purpose — it is
    /// remembered daemon truth, and forgetting it would demote ✗ back to ○.
    pub fn open_live(&mut self) {
        self.rows = None;
        self.policy = None;
        self.message = None;
        self.cursor = 0;
        self.confirm = None;
    }

    /// Enter the screen in DEMO mode: a sim-honest EMPTY listing — the demo
    /// has no hook engine, and fabricating rows would invent trust state.
    pub fn open_demo(&mut self) {
        self.rows = Some(Vec::new());
        self.policy = None;
        self.message = Some("· demo — no hook engine; live mode lists hooks.json truth".to_owned());
        self.cursor = 0;
        self.confirm = None;
    }

    /// Apply one `hooks.list` snapshot — the ONLY writer of the rows. Every
    /// trusted row pins its identity's digest into the baseline (daemon
    /// truth), so a later edit renders ✗ rather than pretending the hook
    /// was never trusted.
    pub fn apply_snapshot(&mut self, policy: String, hooks: Vec<HookSummaryWire>) {
        for hook in &hooks {
            if hook.trusted {
                self.baseline
                    .insert(identity_key(&hook.name, &hook.source), hook.digest.clone());
            }
        }
        let rows: Vec<HookRow> = hooks.into_iter().map(HookRow::from_wire).collect();
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        self.rows = Some(rows);
        self.policy = Some(policy);
    }

    /// The daemon refused the listing — say so instead of "fetching…"
    /// forever.
    pub fn list_failed(&mut self, message: &str) {
        self.message = Some(format!("· hooks list failed — {message}"));
    }

    /// A trust/revoke receipt landed. Retires the in-flight marker and
    /// updates the BASELINE (an explicit revoke forgets the pin, so the row
    /// renders ○ untrusted, not ✗). The rows themselves do NOT move here —
    /// the caller chains a fresh `hooks.list` and daemon truth moves them.
    pub fn note_receipt(&mut self, digest: &str, trusted: bool) {
        self.pending = None;
        if let Some(row) = self
            .rows
            .as_ref()
            .and_then(|rows| rows.iter().find(|row| row.digest == digest))
        {
            let key = identity_key(&row.name, &row.source);
            if trusted {
                self.baseline.insert(key, digest.to_owned());
            } else {
                self.baseline.remove(&key);
            }
        }
        self.message = Some(if trusted {
            format!("✓ trusted {}", short_digest(digest))
        } else {
            format!("○ trust revoked {}", short_digest(digest))
        });
    }

    /// A trust/revoke command failed — the daemon's typed reason lands on
    /// this screen, and the in-flight gate releases.
    pub fn trust_failed(&mut self, digest: &str, message: &str) {
        self.pending = None;
        self.message = Some(format!("· {} — {message}", short_digest(digest)));
    }

    /// The rendered trust state of one row (see [`TrustGlyph`]).
    #[must_use]
    pub fn glyph(&self, row: &HookRow) -> TrustGlyph {
        if row.trusted {
            return TrustGlyph::Trusted;
        }
        match self.baseline.get(&identity_key(&row.name, &row.source)) {
            Some(pinned) if pinned != &row.digest => TrustGlyph::RevokedByEdit,
            _ => TrustGlyph::Untrusted,
        }
    }
}

/// One recorded hook-engine journal fact, in display vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookFactEntry {
    Fired {
        hook: String,
        kind: HookRuntimeKind,
        exit_code: Option<i32>,
        timed_out: bool,
        /// The decision-hook outcome: the proposed answer and whether the
        /// menu CAS actually applied it (a proposal that lost is journaled
        /// with `decision_applied == false` and never counts as authority).
        decision: Option<(HookDecisionKind, bool)>,
        observed_seq: u64,
    },
    Notice {
        hook: Option<String>,
        reason: String,
    },
    Subscription {
        hook: String,
        state: HookSubscriptionState,
        restart_attempt: u32,
    },
}

impl HookFactEntry {
    /// The firing row's text — one line, newest-first list.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            Self::Fired {
                hook,
                kind,
                exit_code,
                timed_out,
                decision,
                observed_seq,
            } => {
                let kind = match kind {
                    HookRuntimeKind::Exec => "exec",
                    HookRuntimeKind::Subscribe => "subscribe",
                    HookRuntimeKind::Decision => "decision",
                };
                let outcome = if *timed_out {
                    "timed out".to_owned()
                } else {
                    match decision {
                        Some((answer, applied)) => {
                            let answer = match answer {
                                HookDecisionKind::Allow => "allow",
                                HookDecisionKind::Deny => "deny",
                            };
                            if *applied {
                                format!("→ {answer} (applied)")
                            } else {
                                format!("→ {answer} (not applied)")
                            }
                        }
                        None => match exit_code {
                            Some(code) => format!("exit {code}"),
                            None => "exited".to_owned(),
                        },
                    }
                };
                format!("⚡ {hook} · {kind} · {outcome} · seq {observed_seq}")
            }
            Self::Notice { hook, reason } => match hook {
                Some(hook) => format!("· {hook} — {reason}"),
                None => format!("· {reason}"),
            },
            Self::Subscription {
                hook,
                state,
                restart_attempt,
            } => {
                let state = match state {
                    HookSubscriptionState::Started => "started",
                    HookSubscriptionState::Exited => "exited",
                    HookSubscriptionState::RestartScheduled => "restart scheduled",
                    HookSubscriptionState::Stopped => "stopped",
                };
                format!("↺ {hook} · subscribe {state} · attempt {restart_attempt}")
            }
        }
    }
}

/// Per-session record of journaled hook-engine facts, plus the derived
/// decision-hook chip. Checked in/out with the session (the A→B→A law) —
/// `AppModel` and `SessionState` each hold one and the session checkout
/// swaps it whole, exactly like `BranchState`.
#[derive(Debug, Default)]
pub struct HookFactsLog {
    /// Newest first, bounded by [`HOOK_FACTS_CAP`].
    entries: VecDeque<HookFactEntry>,
    /// The latest run any admitted envelope named — the "this run" the
    /// decision chip compares against. Command state, never display state.
    current_run: Option<RunId>,
    /// The run in which a decision hook's answer was APPLIED by the menu
    /// CAS (`decision_applied == true` in the journaled fact).
    decision_run: Option<RunId>,
}

impl HookFactsLog {
    /// Record what one ADMITTED envelope carries — called for `Apply` AND
    /// `Skip` (hook facts are `render.ui == false`; the display gate keeps
    /// holding for every transcript surface — the branch-registry
    /// exemption's twin). Non-hook envelopes only advance the run cursor.
    pub fn note_envelope(&mut self, envelope: &RawEnvelope) {
        if let Some(run) = &envelope.run_id
            && self.current_run.as_ref() != Some(run)
        {
            self.current_run = Some(run.clone());
        }
        if !HookEventPayload::is_engine_fact(&envelope.payload) {
            return;
        }
        // A future fact kind this build cannot decode is tolerated like any
        // unknown payload (forward-compat law) — never fatal, never guessed.
        let Ok(fact) = HookEventPayload::from_payload_value(envelope.payload.clone()) else {
            return;
        };
        let entry = match fact {
            HookEventPayload::HookNotice(notice) => HookFactEntry::Notice {
                hook: notice.hook,
                reason: notice.reason,
            },
            HookEventPayload::HookFired(fired) => {
                // THE CHIP FOLLOWS THE JOURNALED FACT: only an APPLIED
                // decision counts — a proposal that lost to another surface
                // is never represented as authority (the wire's own law).
                if fired.decision_applied {
                    self.decision_run = envelope.run_id.clone();
                }
                HookFactEntry::Fired {
                    hook: fired.hook,
                    kind: fired.kind,
                    exit_code: fired.exit_code,
                    timed_out: fired.timed_out,
                    decision: fired
                        .proposed_decision
                        .map(|answer| (answer, fired.decision_applied)),
                    observed_seq: fired.observed_seq,
                }
            }
            HookEventPayload::HookSubscription(subscription) => HookFactEntry::Subscription {
                hook: subscription.hook,
                state: subscription.state,
                restart_attempt: subscription.restart_attempt,
            },
            // `is_engine_fact` filtered to the three kinds above; anything
            // else is a future engine fact — tolerated, not displayed.
            _ => return,
        };
        self.entries.push_front(entry);
        while self.entries.len() > HOOK_FACTS_CAP {
            self.entries.pop_back();
        }
    }

    /// Recorded facts, newest first.
    pub fn recent(&self) -> impl Iterator<Item = &HookFactEntry> {
        self.entries.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// TRUE while the CURRENT run's permission was answered by a decision
    /// hook (journaled fact → chip). A new run's first envelope moves
    /// `current_run` and the chip drops — automation visibility is scoped
    /// to the run it actually acted in.
    #[must_use]
    pub fn decision_chip(&self) -> bool {
        self.decision_run.is_some() && self.decision_run == self.current_run
    }
}
