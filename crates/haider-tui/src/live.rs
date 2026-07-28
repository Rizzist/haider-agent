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
    },
    Cancel {
        command_id: CommandId,
        session: SessionId,
        worker_generation: u64,
        run_id: RunId,
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
}

impl LiveCommand {
    /// The durable idempotency key, for the mutations that have one.
    #[must_use]
    pub const fn command_id(&self) -> Option<&CommandId> {
        match self {
            Self::Create { command_id, .. }
            | Self::Submit { command_id, .. }
            | Self::Cancel { command_id, .. }
            | Self::Answer { command_id, .. } => Some(command_id),
            Self::LoginApi { command_id, .. } => Some(command_id),
            Self::List { .. }
            | Self::Attach { .. }
            | Self::Detach { .. }
            // A stage carries no durable identity BY DESIGN (see above).
            | Self::Stage { .. } => None,
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
    Answered {
        command_id: CommandId,
    },
    /// `vault.stage` answered with an opaque single-use reference.
    Staged {
        vault_reference: String,
        provider: String,
        alias: Option<String>,
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
    login_attempt: Option<u64>,
    /// Attempts with a `Stage` command in flight, in ISSUE order — the
    /// RPC socket answers a connection's commands in order, so each
    /// `Staged` reply pops the front: that is the attempt it answers.
    /// (Belt and braces: the `Staged` gate also requires the popped
    /// attempt to BE the live one, so even an ordering violation cannot
    /// cross-bind a cancelled attempt's vault reference to a new card.)
    staged_attempts: std::collections::VecDeque<u64>,
    /// When the card's non-durable `vault.stage` was issued. A Stage
    /// swallowed by a disconnect or a wedged daemon would otherwise park
    /// the card at "validating…" with `accepts_input() == false` forever
    /// (review W3c3 D2-5).
    login_started: Option<std::time::Instant>,
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
            staged_attempts: std::collections::VecDeque::new(),
            login_started: None,
            outbox: Vec::new(),
            menus: HashMap::new(),
            generations: HashMap::new(),
            active_run: HashMap::new(),
            instance: instance.into(),
            next_command: 0,
            connected: true,
            pending_first_turn: HashMap::new(),
            creating: HashMap::new(),
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
        vec![LiveCommand::List { cursor: None }]
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
                    commands.push(self.submit(model, &session, text));
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
            LiveReply::Answered { command_id } | LiveReply::Cancelled { command_id } => {
                self.retire(&command_id);
                Vec::new()
            }
            LiveReply::Staged {
                vault_reference,
                provider,
                alias,
            } => {
                // TUI6.3 fix 1(c): correlate the reply to the attempt it
                // answers — the front of the issue-ordered queue — and
                // require that attempt to be the LIVE one on both sides
                // (driver binding AND the open card). The r3 probe showed
                // abort→re-/login letting an old and a new Staged reuse
                // one login command id: a credential could commit after
                // the UI said cancelled. A ghost reply is dropped whole —
                // no mint, no enqueue, no flash (the user saw the cancel).
                let answered = self.staged_attempts.pop_front();
                let live = answered.is_some()
                    && answered == self.login_attempt
                    && model.login.as_ref().map(|card| card.attempt) == answered;
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
                })]
            }
            LiveReply::LoggedIn {
                command_id,
                identity,
            } => {
                self.retire(&command_id);
                // TUI6.3 fix 1(c): the result lands only on the card that
                // ASKED — the command must be the live login command and
                // the open card must be the live attempt. A stale
                // LoggedIn (its attempt retired, or a newer card open)
                // touches nothing: the r3 probe had an old result marking
                // a NEW provider/alias card successful.
                let owns = self.login_command.as_ref() == Some(&command_id)
                    && self.login_attempt.is_some()
                    && model.login.as_ref().map(|card| card.attempt) == self.login_attempt;
                if owns {
                    self.close_login(&command_id);
                    self.login_attempt = None;
                    model.login_result(Ok(identity));
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
            LiveReply::Failed {
                command_id,
                code,
                message,
                retryable,
            } => {
                // A rejected TURN must release the optimistic mid-turn UI,
                // or the session sits in "running" forever with no envelope
                // ever coming to clear it — and Esc, which now cancels only
                // a run the stream named, has nothing to cancel (P1-4).
                if let Some(id) = &command_id {
                    let was_submit = self.outbox.iter().any(|pending| {
                        &pending.command_id == id
                            && matches!(pending.command, LiveCommand::Submit { .. })
                    });
                    if was_submit {
                        model.turn_active = false;
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
                model.flash = Some(format!("· reconnecting — {reason}"));
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
        if self.login_started.is_none() && self.login_command.is_none() {
            return;
        }
        self.login_started = None;
        self.login_attempt = None;
        // Staging is connection-scoped: no reply will ever arrive for
        // these, so the correlation queue dies with the transaction.
        self.staged_attempts.clear();
        if let Some(id) = self.login_command.take() {
            self.retire(&id);
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
        self.login_started
            .map(|started| started + LOGIN_STAGE_TIMEOUT)
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
        }
        Vec::new()
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
    fn resume(&mut self, model: &mut AppModel) -> Vec<LiveCommand> {
        let mut commands = vec![LiveCommand::List { cursor: None }];
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
                    max_tokens: model.identity.context_window,
                    first_text: text,
                })]
            }
            AppRequest::SubmitText { text, .. } => match model.active_session.clone() {
                Some(session) => vec![self.submit(model, &session, text)],
                None => Vec::new(),
            },
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
                // later reply must correlate to it or die.
                self.login_attempt = Some(attempt);
                self.staged_attempts.push_back(attempt);
                // The card is now UNANSWERABLE-BY-ITSELF until a response
                // lands: arm the deadline that covers BOTH transactions.
                self.login_started = Some(self.now);
                vec![LiveCommand::Stage {
                    stage_id: format!("{}-stage-{}", self.instance, self.next_command),
                    secret,
                    provider,
                    alias,
                }]
            }
            AppRequest::LoginRetired { attempt } => {
                // TUI6.3 fix 1(b): the card closed — retire the attempt.
                // Whatever the transaction had in flight (a stage awaiting
                // its reference, the login command awaiting commit, the
                // deadline) is invalidated; the reply gates then drop the
                // ghosts silently. The staged_attempts queue keeps its
                // entries — each in-flight `Staged` reply still POPS its
                // tag on arrival and fails the liveness gate.
                if self.login_attempt == Some(attempt) {
                    self.login_attempt = None;
                    self.login_started = None;
                    if let Some(id) = self.login_command.take() {
                        self.retire(&id);
                    }
                }
                Vec::new()
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
            AppRequest::Interrupt => {
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
                })]
            }
            // Runtime-owned effects: `live_pass` hands these BACK to the
            // shell (they need the terminal or the process), so reaching
            // here at all would be a routing bug, not a discard.
            AppRequest::CopySelection | AppRequest::CopyText(_) | AppRequest::Quit => Vec::new(),
            // DEMO-ONLY VOCABULARY. The reducer refuses every one of these
            // upstream in live mode (`AppModel::refuse_demo_only`), so this
            // arm is unreachable by design — but it must never be a SILENT
            // discard. Returning an empty vector quietly is exactly how
            // `/compact` fabricated `turn_active = true` and then wedged
            // the session forever (W3c3.1 r2, P1-A): the request vanished
            // and no surface said so. If a future reducer path forgets its
            // gate, the user sees a flash instead of a dead UI, and the
            // pinned test sees a failure instead of silence.
            AppRequest::Compact
            | AppRequest::Talk
            | AppRequest::ChipSubmit { .. }
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

    fn submit(&mut self, model: &AppModel, session: &SessionId, text: String) -> LiveCommand {
        let command_id = self.mint();
        let worker_generation = self.generations.get(session).copied().unwrap_or_default();
        // `/queue turn` holds mid-turn input to the end of the turn;
        // `/queue steer` (the default) delivers it at the next safe
        // boundary. The MODE is the user's standing choice, so it rides
        // every submit — the daemon, not the client, decides when.
        let mode = if model.queue_mode {
            DeliveryMode::Queue
        } else {
            DeliveryMode::Steer
        };
        self.enqueue(LiveCommand::Submit {
            command_id,
            session: session.clone(),
            worker_generation,
            text,
            mode,
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
        AppRequest::Compact => "/compact",
        AppRequest::Talk => "push-to-talk",
        AppRequest::ChipSubmit { .. } => "steering a subagent",
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
