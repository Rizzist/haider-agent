//! Daemon owner of the computer-use presence indicator and its Stop control.
//!
//! One process-wide [`CuPresence`] drives the platform-neutral
//! [`PresenceMachine`] and routes its commands to one renderer per surface:
//!
//! * `Screen` — the desktop overlay helper process
//!   (`haiderd --cu-presence-overlay`), spawned on the first CU action and
//!   closed (stdin EOF) when presence retires;
//! * `Phone` — the connected Android APK, which draws its own accessibility
//!   overlay from the requests it serves; the daemon only tells it when the
//!   run's presence ended (`presence.end`);
//! * `Browser` — reserved for the webextract-2 browser adapter, which
//!   registers a renderer with [`CuPresence::register_renderer`].
//!
//! Stop, from any surface, flips the in-flight action's cancel token at once,
//! refuses every later CU action of that run, and submits the run's
//! receipt-backed `TurnCancelCommand` — the same cancellation authority the
//! TUI's ESC (`turn.cancel`) uses.

use haider_protocol::computer::ComputerAction;
use haider_protocol::mobile::MobileAction;
use haider_tools::presence::{
    CONCEAL_ACK_SEQ_START, CONCEAL_ACK_TIMEOUT, POINTER_ACK_TIMEOUT, PRESENCE_IDLE_TIMEOUT,
    PresenceCommand, PresenceEndReason, PresenceEvent, PresenceMachine, PresenceMark,
    PresencePoint, PresenceRefusal, PresenceSurface,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// Invoked (outside every presence lock) when the human stops a run.
pub(crate) type StopHook = Arc<dyn Fn() + Send + Sync>;
/// Receives events a renderer reports (Stop, Ack, errors).
pub(crate) type EventSink = Arc<dyn Fn(PresenceEvent) + Send + Sync>;
/// Creates a renderer for one surface, or `None` when that surface cannot
/// render here (no helper, headless, disconnected APK).
pub(crate) type RendererFactory =
    Arc<dyn Fn(EventSink) -> Option<Box<dyn PresenceRenderer>> + Send + Sync>;

/// One live rendering of a surface.
pub(crate) trait PresenceRenderer: Send {
    /// Delivers one command. `false` means the renderer is gone.
    fn send(&mut self, command: &PresenceCommand) -> bool;
    /// Whether [`PresenceEvent::Ack`] replies are produced for pointers.
    fn acknowledges_pointers(&self) -> bool {
        false
    }
    /// Whether this renderer's UI can appear in the model's screenshots, so
    /// it must be concealed around each capture (Linux notification, or a
    /// helper whose OS refused capture exclusion).
    fn capturable(&self) -> bool {
        false
    }
    /// Retires the renderer after its final `Hide`.
    fn close(&mut self) {}
}

struct LeaseHooks {
    stop: StopHook,
    in_flight: Option<StopHook>,
}

/// Shared capture ownership for one surface (verifier finding B2 on
/// 8c614f6c). Every model-facing capture holds the surface concealed; the
/// FIRST holder sends Conceal, the LAST one to retire sends Reveal, so
/// overlapping captures from different sessions never reveal each other's
/// capture, and a capture cancelled while awaiting its ack still retires.
struct CaptureConceal {
    holders: usize,
    /// A Conceal is in effect on the current renderer (Reveal owed).
    sent: bool,
    /// The sole Conceal whose acknowledgement may release this hold.
    outstanding_seq: Option<u64>,
    /// Whether the current Conceal has been acknowledged.
    acked: tokio::sync::watch::Sender<bool>,
}

impl Default for CaptureConceal {
    fn default() -> Self {
        Self {
            holders: 0,
            sent: false,
            outstanding_seq: None,
            acked: tokio::sync::watch::Sender::new(false),
        }
    }
}

struct State {
    machine: PresenceMachine<String>,
    hooks: HashMap<String, LeaseHooks>,
    renderers: BTreeMap<PresenceSurface, Box<dyn PresenceRenderer>>,
    acks: HashMap<u64, oneshot::Sender<()>>,
    captures: BTreeMap<PresenceSurface, CaptureConceal>,
    /// Outstanding Conceal sequence numbers and their surface.
    conceal_acks: HashMap<u64, PresenceSurface>,
}

/// Process-wide presence controller.
pub(crate) struct CuPresence {
    state: StdMutex<State>,
    factories: StdMutex<BTreeMap<PresenceSurface, RendererFactory>>,
    ticker_running: AtomicBool,
    tick_interval: Duration,
    /// Conceal acks use their own sequence space, disjoint from pointers.
    next_conceal_seq: std::sync::atomic::AtomicU64,
    /// Test seam: how long a run's Stop hook waits before submitting its turn
    /// cancellation (zero in production). Models a slow cancel commit so the
    /// Stop-refusal path is observable before the cancellation lands.
    stop_cancel_delay: StdMutex<Duration>,
    me: Weak<CuPresence>,
}

impl CuPresence {
    pub(crate) fn new(idle_timeout: Duration, tick_interval: Duration) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            state: StdMutex::new(State {
                machine: PresenceMachine::new(idle_timeout),
                hooks: HashMap::new(),
                renderers: BTreeMap::new(),
                acks: HashMap::new(),
                captures: BTreeMap::new(),
                conceal_acks: HashMap::new(),
            }),
            factories: StdMutex::new(BTreeMap::new()),
            ticker_running: AtomicBool::new(false),
            tick_interval,
            next_conceal_seq: std::sync::atomic::AtomicU64::new(CONCEAL_ACK_SEQ_START),
            stop_cancel_delay: StdMutex::new(Duration::ZERO),
            me: me.clone(),
        })
    }

    /// Delays every Stop hook's turn cancellation (tests only).
    #[cfg(test)]
    pub(crate) fn set_stop_cancel_delay(&self, delay: Duration) {
        *lock(&self.stop_cancel_delay) = delay;
    }

    /// Installs (or replaces) the renderer factory for `surface`. The
    /// webextract-2 browser adapter plugs in here for `Browser`.
    pub(crate) fn register_renderer(&self, surface: PresenceSurface, factory: RendererFactory) {
        lock(&self.factories).insert(surface, factory);
    }

    fn event_sink(&self, surface: PresenceSurface) -> EventSink {
        let me = self.me.clone();
        Arc::new(move |event| {
            if let Some(presence) = me.upgrade() {
                presence.on_event(surface, event);
            }
        })
    }

    /// Starts one CU action. On success returns an optional acknowledgement
    /// the caller should await (bounded) before posting pointer input.
    pub(crate) fn begin(
        &self,
        key: &str,
        surface: PresenceSurface,
        mark: PresenceMark,
        point: Option<PresencePoint>,
        stop: StopHook,
        in_flight: StopHook,
    ) -> Result<Option<oneshot::Receiver<()>>, PresenceRefusal> {
        let mut state = lock(&self.state);
        let (seq, commands) =
            state
                .machine
                .begin_action(key.to_owned(), surface, mark, point, Instant::now())?;
        state.hooks.insert(
            key.to_owned(),
            LeaseHooks {
                stop,
                in_flight: Some(in_flight),
            },
        );
        self.dispatch(&mut state, commands);
        let ack = if mark.posts_pointer_input()
            && state
                .renderers
                .get(&surface)
                .is_some_and(|renderer| renderer.acknowledges_pointers())
        {
            let (sender, receiver) = oneshot::channel();
            state.acks.insert(seq, sender);
            Some(receiver)
        } else {
            None
        };
        drop(state);
        self.ensure_ticker();
        Ok(ack)
    }

    /// The action begun for `key` finished; its idle window starts now.
    pub(crate) fn finish(&self, key: &str) {
        let mut state = lock(&self.state);
        state.machine.finish_action(&key.to_owned(), Instant::now());
        if let Some(hooks) = state.hooks.get_mut(key) {
            hooks.in_flight = None;
        }
    }

    /// The run behind `key` ended.
    pub(crate) fn end(&self, key: &str, reason: PresenceEndReason) {
        let mut state = lock(&self.state);
        state.hooks.remove(key);
        let commands = state.machine.end(&key.to_owned(), reason);
        self.dispatch(&mut state, commands);
    }

    /// The human pressed Stop on `surface`. Returns how many runs were
    /// stopped. Hooks run after the state lock is released.
    pub(crate) fn stop(&self, surface: PresenceSurface) -> usize {
        let mut state = lock(&self.state);
        let (keys, commands) = state.machine.stop(surface);
        let mut run_hooks = Vec::with_capacity(keys.len());
        for key in &keys {
            if let Some(hooks) = state.hooks.get(key) {
                // Flip the in-flight action's token BEFORE `dispatch` below
                // releases any pending pointer acknowledgement: an action
                // waiting on that ack must observe cancellation the moment
                // it resumes, never a window in which it could execute.
                if let Some(in_flight) = &hooks.in_flight {
                    in_flight();
                }
                run_hooks.push(Arc::clone(&hooks.stop));
            }
        }
        self.dispatch(&mut state, commands);
        drop(state);
        for stop in &run_hooks {
            stop();
        }
        tracing::info!(surface = ?surface, runs = keys.len(), "computer-use presence Stop pressed");
        keys.len()
    }

    /// Sends Conceal to `surface`'s renderer if it is capturable, and marks
    /// the Conceal as owed a Reveal. Returns false if a capturable renderer
    /// rejected it, so a recreated renderer is retired before any Show.
    /// Caller holds the state lock.
    fn send_conceal(&self, state: &mut State, surface: PresenceSurface) -> bool {
        let capturable = state
            .renderers
            .get(&surface)
            .is_some_and(|renderer| renderer.capturable());
        if !capturable {
            return true;
        }
        // Stay in the Conceal sequence range even at u64 rollover. A pointer
        // acknowledgement must never alias a Conceal acknowledgement.
        let seq = self
            .next_conceal_seq
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(if current == u64::MAX {
                    CONCEAL_ACK_SEQ_START
                } else {
                    current + 1
                })
            })
            .unwrap_or(CONCEAL_ACK_SEQ_START);
        let sent = state
            .renderers
            .get_mut(&surface)
            .is_some_and(|renderer| renderer.send(&PresenceCommand::Conceal { surface, seq }));
        if sent {
            state.conceal_acks.retain(|_, pending| *pending != surface);
            state.conceal_acks.insert(seq, surface);
            let capture = state.captures.entry(surface).or_default();
            capture.sent = true;
            capture.outstanding_seq = Some(seq);
            capture.acked.send_replace(false);
        }
        sent
    }

    /// Takes one capture hold on `surface`, synchronously (so a caller that
    /// is cancelled right afterwards still releases it). The first holder
    /// conceals. Returns a receiver to await the Conceal's ack, if one is
    /// outstanding.
    pub(crate) fn begin_capture(
        &self,
        surface: PresenceSurface,
    ) -> Option<tokio::sync::watch::Receiver<bool>> {
        let mut state = lock(&self.state);
        let first = {
            let capture = state.captures.entry(surface).or_default();
            capture.holders += 1;
            capture.holders == 1
        };
        if first && !self.send_conceal(&mut state, surface) {
            // A renderer that cannot accept Conceal must not remain on
            // screen while the capture proceeds without an ack.
            if let Some(mut renderer) = state.renderers.remove(&surface) {
                renderer.close();
            }
            state.conceal_acks.retain(|_, pending| *pending != surface);
        }
        let capture = state.captures.entry(surface).or_default();
        (capture.sent && !*capture.acked.borrow()).then(|| capture.acked.subscribe())
    }

    /// Releases one capture hold; the last holder reveals.
    pub(crate) fn end_capture(&self, surface: PresenceSurface) {
        let mut state = lock(&self.state);
        let reveal = {
            let Some(capture) = state.captures.get_mut(&surface) else {
                return;
            };
            capture.holders = capture.holders.saturating_sub(1);
            if capture.holders == 0 {
                capture.outstanding_seq = None;
            }
            capture.holders == 0 && std::mem::take(&mut capture.sent)
        };
        if reveal && let Some(renderer) = state.renderers.get_mut(&surface) {
            let _ = renderer.send(&PresenceCommand::Reveal { surface });
        }
    }

    /// Number of capture holds on `surface` (tests).
    #[cfg(test)]
    pub(crate) fn capture_holders(&self, surface: PresenceSurface) -> usize {
        lock(&self.state)
            .captures
            .get(&surface)
            .map_or(0, |capture| capture.holders)
    }

    pub(crate) fn is_stopped(&self, key: &str) -> bool {
        lock(&self.state).machine.is_stopped(&key.to_owned())
    }

    #[cfg(test)]
    pub(crate) fn is_shown(&self, surface: PresenceSurface) -> bool {
        lock(&self.state).machine.is_shown(surface)
    }

    fn on_event(&self, surface: PresenceSurface, event: PresenceEvent) {
        match event {
            PresenceEvent::Stop => {
                self.stop(surface);
            }
            PresenceEvent::Ack { seq } => {
                let mut state = lock(&self.state);
                if let Some(sender) = state.acks.remove(&seq) {
                    let _ = sender.send(());
                } else if state.conceal_acks.get(&seq) == Some(&surface)
                    && state
                        .captures
                        .get(&surface)
                        .is_some_and(|capture| capture.sent && capture.outstanding_seq == Some(seq))
                {
                    state.conceal_acks.remove(&seq);
                    if let Some(capture) = state.captures.get_mut(&surface) {
                        capture.outstanding_seq = None;
                        capture.acked.send_replace(true);
                    }
                }
            }
            PresenceEvent::Ready {
                platform,
                capture_excluded,
            } => {
                tracing::debug!(%platform, capture_excluded, surface = ?surface, "presence renderer ready");
            }
            PresenceEvent::Error { message } => {
                tracing::debug!(%message, surface = ?surface, "presence renderer reported");
            }
        }
    }

    fn tick(&self) -> bool {
        let mut state = lock(&self.state);
        let commands = state.machine.tick(Instant::now());
        self.dispatch(&mut state, commands);
        let live = state.machine.has_leases();
        if !live {
            state.hooks.clear();
        }
        live
    }

    fn ensure_ticker(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.ticker_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.me.clone();
        let interval = self.tick_interval;
        handle.spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(presence) = me.upgrade() else { return };
                if !presence.tick() {
                    presence.ticker_running.store(false, Ordering::Release);
                    // A begin() racing this exit re-arms through the swap.
                    if !lock(&presence.state).machine.has_leases()
                        || presence.ticker_running.swap(true, Ordering::AcqRel)
                    {
                        return;
                    }
                }
            }
        });
    }

    fn dispatch(&self, state: &mut State, commands: Vec<PresenceCommand>) {
        for command in commands {
            let surface = command.surface();
            let mut initialized = true;
            if let std::collections::btree_map::Entry::Vacant(slot) = state.renderers.entry(surface)
            {
                // Only a Show or a Pointer on a surface that should be
                // visible (its renderer died mid-session) creates one.
                if !matches!(
                    command,
                    PresenceCommand::Show { .. } | PresenceCommand::Pointer { .. }
                ) {
                    continue;
                }
                let factory = lock(&self.factories).get(&surface).cloned();
                let Some(mut renderer) =
                    factory.and_then(|factory| factory(self.event_sink(surface)))
                else {
                    continue;
                };
                let held = state
                    .captures
                    .get(&surface)
                    .is_some_and(|capture| capture.holders > 0);
                if !held
                    && matches!(command, PresenceCommand::Pointer { .. })
                    && !renderer.send(&PresenceCommand::Show {
                        surface,
                        label: surface.badge_label().to_owned(),
                    })
                {
                    continue;
                }
                slot.insert(renderer);
                // A renderer recreated during a capture must be concealed
                // BEFORE its first Show/Pointer, so it cannot flash in the
                // in-flight screenshot even for one frame.
                if held {
                    initialized = self.send_conceal(state, surface);
                    if initialized && matches!(command, PresenceCommand::Pointer { .. }) {
                        initialized = state.renderers.get_mut(&surface).is_some_and(|renderer| {
                            renderer.send(&PresenceCommand::Show {
                                surface,
                                label: surface.badge_label().to_owned(),
                            })
                        });
                    }
                }
            }
            let alive = initialized
                && state
                    .renderers
                    .get_mut(&surface)
                    .is_some_and(|renderer| renderer.send(&command));
            if !alive || matches!(command, PresenceCommand::Hide { .. }) {
                if let Some(mut renderer) = state.renderers.remove(&surface) {
                    renderer.close();
                }
                // Pending acknowledgements can no longer arrive; release any
                // capture waiting on this renderer's Conceal ack.
                state.acks.clear();
                state.conceal_acks.retain(|_, pending| *pending != surface);
                if let Some(capture) = state.captures.get_mut(&surface) {
                    capture.sent = false;
                    capture.outstanding_seq = None;
                    capture.acked.send_replace(true);
                }
            }
        }
    }
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The daemon-wide controller with the production renderers installed.
pub(crate) fn global() -> &'static Arc<CuPresence> {
    static GLOBAL: OnceLock<Arc<CuPresence>> = OnceLock::new();
    GLOBAL.get_or_init(|| {
        let presence = CuPresence::new(PRESENCE_IDLE_TIMEOUT, Duration::from_secs(1));
        presence.register_renderer(
            PresenceSurface::Screen,
            Arc::new(|sink| overlay::spawn(sink).map(|process| Box::new(process) as _)),
        );
        presence.register_renderer(
            PresenceSurface::Phone,
            Arc::new(|_sink| Some(Box::new(ApkPresenceRenderer) as _)),
        );
        presence
    })
}

/// The APK draws its overlay itself from the requests it serves; it only
/// needs to learn that the run's presence ended.
struct ApkPresenceRenderer;

impl PresenceRenderer for ApkPresenceRenderer {
    fn send(&mut self, command: &PresenceCommand) -> bool {
        if let PresenceCommand::Hide { reason, .. } = command {
            crate::mobile_transport::send_presence_end(*reason);
        }
        true
    }
}

/// One dispatcher's (run's) presence membership.
pub(crate) struct PresenceLease {
    key: String,
    presence: Arc<CuPresence>,
    stop: StopHook,
    ended: AtomicBool,
}

/// Finishes the action when dropped, including on cancellation.
pub(crate) struct ActionGuard<'a> {
    lease: &'a PresenceLease,
}

impl Drop for ActionGuard<'_> {
    fn drop(&mut self) {
        self.lease.presence.finish(&self.lease.key);
    }
}

impl PresenceLease {
    pub(crate) fn new(presence: Arc<CuPresence>, key: String, stop: StopHook) -> Self {
        Self {
            key,
            presence,
            stop,
            ended: AtomicBool::new(false),
        }
    }

    /// Lease on `presence` (the daemon-wide [`global`] in production) whose Stop cancels `run_id`
    /// through the hub's receipt-backed turn cancellation.
    pub(crate) fn for_run(
        presence: Arc<CuPresence>,
        hub: crate::session_hub::SessionHub,
        session_id: haider_protocol::ids::SessionId,
        run_id: haider_protocol::ids::RunId,
    ) -> Self {
        let key = format!("{session_id}/{run_id}");
        let delay = *lock(&presence.stop_cancel_delay);
        let stop = run_cancel_hook(hub, session_id, run_id, delay);
        Self::new(presence, key, stop)
    }

    async fn begin(
        &self,
        surface: PresenceSurface,
        mark: PresenceMark,
        point: Option<PresencePoint>,
        in_flight: StopHook,
    ) -> Result<ActionGuard<'_>, PresenceRefusal> {
        let ack = self.presence.begin(
            &self.key,
            surface,
            mark,
            point,
            Arc::clone(&self.stop),
            in_flight,
        )?;
        let guard = ActionGuard { lease: self };
        if let Some(ack) = ack {
            // Bounded: a slow or dead overlay never blocks control.
            let _ = tokio::time::timeout(POINTER_ACK_TIMEOUT, ack).await;
            // Stop may have landed while we waited (it also releases the
            // ack): re-check so a stopped run's click is never posted.
            if self.presence.is_stopped(&self.key) {
                return Err(PresenceRefusal::Stopped);
            }
        }
        Ok(guard)
    }

    /// Presence for one desktop `computer` action.
    pub(crate) async fn begin_computer(
        &self,
        action: &ComputerAction,
        point: Option<PresencePoint>,
        cancel: &haider_tools::ComputerCancelToken,
    ) -> Result<ActionGuard<'_>, PresenceRefusal> {
        let cancel = cancel.clone();
        self.begin(
            PresenceSurface::Screen,
            PresenceMark::for_computer_action(action),
            point,
            Arc::new(move || cancel.cancel()),
        )
        .await
    }

    /// Presence for one `mobile` action; `Ok(None)` for actions that do not
    /// operate the phone's screen (SMS reads, app listing).
    pub(crate) async fn begin_mobile(
        &self,
        action: &MobileAction,
        cancel: &haider_tools::MobileCancelToken,
    ) -> Result<Option<ActionGuard<'_>>, PresenceRefusal> {
        let Some(mark) = PresenceMark::for_mobile_action(action) else {
            return Ok(None);
        };
        let cancel = cancel.clone();
        self.begin(
            PresenceSurface::Phone,
            mark,
            None,
            Arc::new(move || cancel.cancel()),
        )
        .await
        .map(Some)
    }

    /// Conceals capturable presence UI (Linux notification popup, refused
    /// Windows exclusion) for the duration of one model-facing screenshot.
    ///
    /// The capture hold — and the guard that releases it — is taken
    /// synchronously, BEFORE the first await: if the caller is cancelled
    /// while waiting for the helper's ack (Esc, Stop), dropping this future
    /// drops the guard and the indicator is revealed (unless another capture
    /// still holds the surface).
    pub(crate) async fn conceal_for_capture(&self) -> RevealGuard {
        let ack = self.presence.begin_capture(PresenceSurface::Screen);
        let guard = RevealGuard {
            presence: Arc::clone(&self.presence),
            surface: PresenceSurface::Screen,
        };
        if let Some(mut ack) = ack {
            let _ = tokio::time::timeout(CONCEAL_ACK_TIMEOUT, ack.wait_for(|acked| *acked)).await;
        }
        guard
    }

    /// Whether the human already stopped this run. Checked right after
    /// authorization so a stopped run never even dispatches another action.
    pub(crate) fn is_stopped(&self) -> bool {
        self.presence.is_stopped(&self.key)
    }

    /// The run ended; retire its presence.
    pub(crate) fn end(&self, cancelled: bool) {
        if self.ended.swap(true, Ordering::AcqRel) {
            return;
        }
        let reason = if cancelled {
            PresenceEndReason::Cancelled
        } else {
            PresenceEndReason::RunEnded
        };
        self.presence.end(&self.key, reason);
    }
}

/// One capture hold; releasing the last hold reveals the surface.
pub(crate) struct RevealGuard {
    presence: Arc<CuPresence>,
    surface: PresenceSurface,
}

impl Drop for RevealGuard {
    fn drop(&mut self) {
        self.presence.end_capture(self.surface);
    }
}

impl Drop for PresenceLease {
    fn drop(&mut self) {
        self.end(false);
    }
}

/// Stop → the run's receipt-backed `TurnCancelCommand`, the same authority
/// as the client's `turn.cancel`.
fn run_cancel_hook(
    hub: crate::session_hub::SessionHub,
    session_id: haider_protocol::ids::SessionId,
    run_id: haider_protocol::ids::RunId,
    delay: Duration,
) -> StopHook {
    let runtime = tokio::runtime::Handle::try_current().ok();
    Arc::new(move || {
        let Some(runtime) = runtime.as_ref() else {
            return;
        };
        let hub = hub.clone();
        let session_id = session_id.clone();
        let run_id = run_id.clone();
        runtime.spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let request_json = serde_json::json!({
                "session_id": session_id,
                "run_id": run_id,
                "reason": "cu-presence-stop",
            })
            .to_string();
            let command = haider_core::TurnCancelCommand {
                command_id: format!("cu-presence-stop-{run_id}"),
                request_digest: crate::delegation::digest_bytes(request_json.as_bytes()),
                request_json,
                session_id: session_id.clone(),
                worker_generation: hub.worker_generation(),
                run_id: run_id.clone(),
                cancelling_event_id: haider_protocol::ids::EventId::new(format!(
                    "cu-presence-stop-cancelling-{run_id}"
                )),
                device_id: hub.device_id(),
            };
            match hub.cancel_internal_turn(command).await {
                Ok(_) => tracing::info!(%session_id, %run_id, "presence Stop cancelled the run"),
                Err(error) => {
                    tracing::info!(%session_id, %run_id, ?error, "presence Stop found no active run to cancel");
                }
            }
        });
    })
}

/// Desktop overlay helper process management.
mod overlay {
    use super::{EventSink, PresenceRenderer};
    use haider_tools::presence::{PresenceCommand, PresenceEvent, presence_helper_command};
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::time::Duration;

    const READY_UNKNOWN: u8 = 0;
    const READY_EXCLUDED: u8 = 1;
    const READY_CAPTURABLE: u8 = 2;

    /// What the helper's `Ready` said about capture exclusion.
    ///
    /// Until `Ready` arrives the helper's UI is treated as CAPTURABLE: the
    /// helper is spawned by the run's first action, usually a screenshot,
    /// and its popup/windows may already be on screen before `Ready` is read.
    /// The conceal is queued on stdin after `Show`, so the helper handles it
    /// in order; an excluded helper simply acks it.
    #[derive(Debug)]
    pub(super) struct ReadyState(AtomicU8);

    impl Default for ReadyState {
        fn default() -> Self {
            Self(AtomicU8::new(READY_UNKNOWN))
        }
    }

    impl ReadyState {
        pub(super) fn record(&self, event: &PresenceEvent) {
            if let PresenceEvent::Ready {
                capture_excluded, ..
            } = event
            {
                let state = if *capture_excluded {
                    READY_EXCLUDED
                } else {
                    READY_CAPTURABLE
                };
                self.0.store(state, Ordering::Release);
            }
        }

        pub(super) fn capturable(&self) -> bool {
            self.0.load(Ordering::Acquire) != READY_EXCLUDED
        }
    }

    pub(super) struct OverlayProcess {
        child: Option<Child>,
        stdin: Option<ChildStdin>,
        ready: Arc<ReadyState>,
    }

    pub(super) fn spawn(sink: EventSink) -> Option<OverlayProcess> {
        let (program, args) = presence_helper_command()?;
        spawn_program(&program, &args, sink)
    }

    pub(super) fn spawn_program(
        program: &std::path::Path,
        args: &[String],
        sink: EventSink,
    ) -> Option<OverlayProcess> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(?error, helper = %program.display(), "computer-use presence overlay could not start");
                return None;
            }
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take()?;
        let ready = Arc::new(ReadyState::default());
        let reader_ready = Arc::clone(&ready);
        let reader = std::thread::Builder::new()
            .name("cu-presence-events".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    match serde_json::from_str::<PresenceEvent>(line.trim()) {
                        Ok(event) => {
                            reader_ready.record(&event);
                            sink(event);
                        }
                        Err(error) => tracing::debug!(?error, "unparseable presence overlay event"),
                    }
                }
            });
        if reader.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        Some(OverlayProcess {
            child: Some(child),
            stdin,
            ready,
        })
    }

    impl PresenceRenderer for OverlayProcess {
        fn send(&mut self, command: &PresenceCommand) -> bool {
            let Some(stdin) = self.stdin.as_mut() else {
                return false;
            };
            let Ok(line) = serde_json::to_string(command) else {
                return true;
            };
            writeln!(stdin, "{line}")
                .and_then(|()| stdin.flush())
                .is_ok()
        }

        fn acknowledges_pointers(&self) -> bool {
            true
        }

        fn capturable(&self) -> bool {
            self.ready.capturable()
        }

        fn close(&mut self) {
            // EOF on stdin is the helper's exit signal.
            self.stdin.take();
            if let Some(mut child) = self.child.take() {
                let _ = std::thread::Builder::new()
                    .name("cu-presence-reap".into())
                    .spawn(move || {
                        for _ in 0..40 {
                            if matches!(child.try_wait(), Ok(Some(_))) {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                    });
            }
        }
    }

    impl Drop for OverlayProcess {
        fn drop(&mut self) {
            self.close();
        }
    }
}

#[cfg(test)]
#[path = "cu_presence_tests.rs"]
mod tests;
