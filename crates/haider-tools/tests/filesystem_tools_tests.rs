#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectOutcome, EffectPhase, FileFreshness};
use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_tools::{
    CasSink, ChangeLedger, EffectBroker, FsEdit, FsRead, FsSearch, FsWrite, JournalSink,
    PermissionPolicy, ResultBounds, ToolError, ToolResult, TurnAttribution,
};
#[cfg(unix)]
use haider_tools::{ChangeLedgerSink, FsWriteRecord};
use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::time::Duration;

// Every JournalSink double in this module is constructed as one value and
// moved exactly once into one broker. Arc fields expose read-only observation
// or synchronization state; they are implementation details of that sole sink
// value and are never used to box a second sink over the same journal.

#[derive(Debug, Default)]
struct SharedJournalStorage {
    payloads: Mutex<Vec<EventPayload>>,
    sink_taken: AtomicBool,
}

impl SharedJournalStorage {
    fn claim_sole_sink(&self, claimed: &mut bool) {
        if !*claimed {
            debug_assert!(
                !self.sink_taken.swap(true, Ordering::SeqCst),
                "test journal storage was boxed behind more than one sink value"
            );
            *claimed = true;
        }
    }
}

#[derive(Debug, Default)]
struct RecordingJournal {
    payloads: Vec<EventPayload>,
}

#[async_trait::async_trait]
impl JournalSink for RecordingJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.payloads.push(payload);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct SharedRecordingJournal {
    storage: Arc<SharedJournalStorage>,
    claimed: bool,
}

#[derive(Debug, Clone)]
struct JournalObserver {
    storage: Arc<SharedJournalStorage>,
}

impl JournalObserver {
    fn effect_phases(&self) -> Vec<EffectPhase> {
        self.storage
            .payloads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|payload| match payload {
                EventPayload::Effect(phase) => Some(phase.clone()),
                _ => None,
            })
            .collect()
    }
}

impl SharedRecordingJournal {
    fn observer(&self) -> JournalObserver {
        JournalObserver {
            storage: Arc::clone(&self.storage),
        }
    }
}

#[async_trait::async_trait]
impl JournalSink for SharedRecordingJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.storage.claim_sole_sink(&mut self.claimed);
        self.storage
            .payloads
            .lock()
            .map_err(|_| ToolError::journal("shared recording journal lock is poisoned"))?
            .push(payload);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct TerminalGateJournal {
    storage: Arc<SharedJournalStorage>,
    claimed: bool,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    terminal_attempts: Arc<AtomicUsize>,
    terminal_completions: Arc<AtomicUsize>,
}

impl TerminalGateJournal {
    fn observer(&self) -> JournalObserver {
        JournalObserver {
            storage: Arc::clone(&self.storage),
        }
    }
}

#[async_trait::async_trait]
impl JournalSink for TerminalGateJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.storage.claim_sole_sink(&mut self.claimed);
        let is_terminal = matches!(payload, EventPayload::Effect(EffectPhase::Outcome { .. }));
        if is_terminal && self.terminal_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            // The legal cancellation gate is before the append records
            // anything; this double never commits and then yields.
            self.reached.notify_one();
            self.release.notified().await;
        }
        self.storage
            .payloads
            .lock()
            .map_err(|_| ToolError::journal("terminal gate journal lock is poisoned"))?
            .push(payload);
        if is_terminal {
            self.terminal_completions.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct FailFirstTerminalJournal {
    storage: Arc<SharedJournalStorage>,
    claimed: bool,
    terminal_attempts: Arc<AtomicUsize>,
}

#[cfg(unix)]
impl FailFirstTerminalJournal {
    fn observer(&self) -> JournalObserver {
        JournalObserver {
            storage: Arc::clone(&self.storage),
        }
    }
}

#[cfg(unix)]
#[async_trait::async_trait]
impl JournalSink for FailFirstTerminalJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.storage.claim_sole_sink(&mut self.claimed);
        if matches!(payload, EventPayload::Effect(EffectPhase::Outcome { .. }))
            && self.terminal_attempts.fetch_add(1, Ordering::SeqCst) == 0
        {
            // Transactional sink law: fail before recording any durable phase.
            return Err(ToolError::journal(
                "injected first terminal outcome append failure",
            ));
        }
        self.storage
            .payloads
            .lock()
            .map_err(|_| ToolError::journal("terminal recording journal lock is poisoned"))?
            .push(payload);
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct RejectOutcomeJournal;

#[cfg(unix)]
#[async_trait::async_trait]
impl JournalSink for RejectOutcomeJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if matches!(payload, EventPayload::Effect(EffectPhase::Outcome { .. })) {
            return Err(ToolError::journal("outcome append unavailable"));
        }
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct DispatchBarrierJournal {
    barrier: Arc<tokio::sync::Barrier>,
}

#[cfg(unix)]
#[async_trait::async_trait]
impl JournalSink for DispatchBarrierJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if matches!(
            payload,
            EventPayload::Effect(EffectPhase::Dispatched { .. })
        ) {
            // This synchronization point precedes the conceptual commit.
            self.barrier.wait().await;
        }
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct DispatchGateJournal {
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[cfg(unix)]
#[async_trait::async_trait]
impl JournalSink for DispatchGateJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if matches!(
            payload,
            EventPayload::Effect(EffectPhase::Dispatched { .. })
        ) {
            // This synchronization point precedes the conceptual commit.
            self.reached.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Debug, Clone)]
struct GatedLedger {
    inner: ChangeLedger,
    reached: Arc<Barrier>,
    release: Arc<Barrier>,
}

#[cfg(unix)]
impl ChangeLedgerSink for GatedLedger {
    fn record_fs_write(
        &self,
        session: SessionId,
        turn: RunId,
        record: FsWriteRecord,
    ) -> ToolResult<()> {
        self.reached.wait();
        self.release.wait();
        self.inner.record_fs_write(session, turn, record)
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct RejectLedger;

#[cfg(unix)]
impl ChangeLedgerSink for RejectLedger {
    fn record_fs_write(
        &self,
        _session: SessionId,
        _turn: RunId,
        _record: FsWriteRecord,
    ) -> ToolResult<()> {
        Err(ToolError::ledger("injected change ledger append failure"))
    }
}

#[cfg(unix)]
#[derive(Debug, Clone)]
struct GatedRejectLedger {
    reached: Arc<Barrier>,
    release: Arc<Barrier>,
}

#[cfg(unix)]
impl ChangeLedgerSink for GatedRejectLedger {
    fn record_fs_write(
        &self,
        _session: SessionId,
        _turn: RunId,
        _record: FsWriteRecord,
    ) -> ToolResult<()> {
        self.reached.wait();
        self.release.wait();
        Err(ToolError::ledger("injected gated ledger append failure"))
    }
}

#[derive(Debug, Default)]
struct RecordingCas {
    writes: Vec<Vec<u8>>,
}

#[async_trait::async_trait]
impl CasSink for RecordingCas {
    async fn put(&mut self, bytes: &[u8]) -> ToolResult<ArtifactRef> {
        self.writes.push(bytes.to_vec());
        Ok(ArtifactRef::new(format!(
            "blake3:{}",
            blake3::hash(bytes).to_hex()
        )))
    }

    async fn put_file(&mut self, path: &Path) -> ToolResult<ArtifactRef> {
        let bytes = std::fs::read(path)
            .map_err(|error| ToolError::cas(format!("read recording CAS source: {error}")))?;
        self.put(&bytes).await
    }
}

fn allow(class: EffectClass) -> PermissionPolicy {
    let mut policy = PermissionPolicy::default();
    policy.allow(class);
    policy
}

fn broker_at<J>(journal: J, workspace_root: &Path) -> EffectBroker
where
    J: JournalSink + 'static,
{
    broker_generation(journal, workspace_root, 1)
}

fn broker_generation<J>(journal: J, workspace_root: &Path, generation: u64) -> EffectBroker
where
    J: JournalSink + 'static,
{
    EffectBroker::new_at(
        Box::new(journal),
        workspace_root,
        SessionId::new("session"),
        generation,
        1_700_000_000_000,
    )
    .expect("create broker")
}

fn terminal_phases(phases: &[EffectPhase]) -> Vec<&EffectPhase> {
    phases
        .iter()
        .filter(|phase| matches!(phase, EffectPhase::Outcome { .. }))
        .collect()
}

fn restore_file_freshness(broker: &mut EffectBroker, workspace_root: &Path, path: &Path) {
    let canonical = fs::canonicalize(path).expect("canonical test file");
    let relative = canonical
        .strip_prefix(fs::canonicalize(workspace_root).expect("canonical test workspace"))
        .expect("file under test workspace")
        .to_string_lossy()
        .into_owned();
    let bytes = fs::read(&canonical).expect("read test freshness bytes");
    broker
        .restore_freshness([FileFreshness {
            path: relative,
            digest: format!("blake3:{}", blake3::hash(&bytes).to_hex()),
        }])
        .expect("restore test freshness");
}

#[cfg(unix)]
#[tokio::test]
async fn fs_write_creates_and_overwrites_with_ledgered_four_phase_effects() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("written.txt");
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let ledger = ChangeLedger::new();
    let mut broker = broker_at(RecordingJournal::default(), directory.path());
    let policy = allow(EffectClass::FsWrite);

    broker
        .fs_write(
            &FsWrite::new("written.txt", "first\n"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect("create file");
    broker
        .fs_write(
            &FsWrite::new(&path, "second\n"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect("overwrite file");

    assert_eq!(
        fs::read_to_string(&path).expect("read written file"),
        "second\n"
    );
    assert_eq!(
        ledger
            .changes_for(&attribution.session, &attribution.turn)
            .expect("write ledger")
            .writes
            .len(),
        2
    );
    let phases = broker.journal_snapshot();
    assert_eq!(phases.len(), 8);
    for effect in phases.chunks_exact(4) {
        assert!(matches!(effect[0], EffectPhase::Intent(_)));
        assert!(matches!(effect[1], EffectPhase::Authorized { .. }));
        assert!(matches!(effect[2], EffectPhase::Dispatched { .. }));
        assert!(matches!(
            effect[3],
            EffectPhase::Outcome {
                outcome: EffectOutcome::Ok,
                ..
            }
        ));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn preimage_mismatch_returns_typed_conflict_and_failed_outcome() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("conflict.txt");
    fs::write(&path, "current").expect("seed file");
    let mut broker = broker_at(RecordingJournal::default(), directory.path());
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let ledger = ChangeLedger::new();
    restore_file_freshness(&mut broker, directory.path(), &path);

    let error = broker
        .fs_edit(
            &FsEdit::new(&path, "stale", "replacement"),
            &allow(EffectClass::FsWrite),
            &attribution,
            &ledger,
        )
        .await
        .expect_err("preimage mismatch");

    let ToolError::EditAnchor(conflict) = error else {
        panic!("expected typed edit anchor mismatch");
    };
    assert_eq!(
        conflict.path,
        fs::canonicalize(&path).expect("canonical conflict path")
    );
    assert_eq!(conflict.matches, 0);
    assert_eq!(fs::read_to_string(&path).expect("read file"), "current");
    assert!(!ledger.has_fs_writes(&attribution.session, &attribution.turn));
    assert!(matches!(
        broker.journal_snapshot().last(),
        Some(EffectPhase::Outcome {
            outcome: EffectOutcome::Failed { .. },
            ..
        })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn ambiguous_preimage_returns_typed_conflict_without_writing() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("ambiguous.txt");
    fs::write(&path, "same\nsame\n").expect("seed file");
    let mut broker = broker_at(RecordingJournal::default(), directory.path());
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let ledger = ChangeLedger::new();
    restore_file_freshness(&mut broker, directory.path(), &path);

    let error = broker
        .fs_edit(
            &FsEdit::new(&path, "same", "changed"),
            &allow(EffectClass::FsWrite),
            &attribution,
            &ledger,
        )
        .await
        .expect_err("ambiguous preimage");

    let ToolError::EditAnchor(conflict) = error else {
        panic!("expected typed edit anchor mismatch");
    };
    assert_eq!(conflict.matches, 2);
    assert_eq!(
        fs::read_to_string(&path).expect("unchanged file"),
        "same\nsame\n"
    );
    assert!(!ledger.has_fs_writes(&attribution.session, &attribution.turn));
}

#[cfg(unix)]
#[tokio::test]
async fn genuinely_unique_preimage_still_applies() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("unique.txt");
    fs::write(&path, "zaaz").expect("seed file");
    let mut broker = broker_at(RecordingJournal::default(), directory.path());
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let ledger = ChangeLedger::new();
    restore_file_freshness(&mut broker, directory.path(), &path);

    broker
        .fs_edit(
            &FsEdit::new(&path, "aa", "changed"),
            &allow(EffectClass::FsWrite),
            &attribution,
            &ledger,
        )
        .await
        .expect("unique preimage applies");

    assert_eq!(
        fs::read_to_string(&path).expect("patched file"),
        "zchangedz"
    );
    assert!(ledger.has_fs_writes(&attribution.session, &attribution.turn));
    assert!(matches!(
        broker.journal_snapshot().last(),
        Some(EffectPhase::Outcome {
            outcome: EffectOutcome::Ok,
            ..
        })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn ledger_attributes_successful_writes_to_the_exact_turn() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let first_path = directory.path().join("first.txt");
    let second_path = directory.path().join("second.txt");
    fs::write(&first_path, "a").expect("seed first");
    fs::write(&second_path, "x").expect("seed second");
    let session = SessionId::new("session-ledger");
    let first_turn = TurnAttribution::new(session.clone(), RunId::new("turn-1"));
    let second_turn = TurnAttribution::new(session.clone(), RunId::new("turn-2"));
    let policy = allow(EffectClass::FsWrite);
    let mut broker = broker_at(RecordingJournal::default(), directory.path());
    let ledger = ChangeLedger::new();
    restore_file_freshness(&mut broker, directory.path(), &first_path);
    restore_file_freshness(&mut broker, directory.path(), &second_path);

    broker
        .fs_edit(
            &FsEdit::new(&first_path, "a", "b"),
            &policy,
            &first_turn,
            &ledger,
        )
        .await
        .expect("first patch");
    broker
        .fs_edit(
            &FsEdit::new(&second_path, "x", "y"),
            &policy,
            &second_turn,
            &ledger,
        )
        .await
        .expect("second patch");

    let first = ledger
        .changes_for(&session, &first_turn.turn)
        .expect("first turn changes");
    let canonical_first = fs::canonicalize(&first_path).expect("canonical first");
    let canonical_second = fs::canonicalize(&second_path).expect("canonical second");
    assert_eq!(first.paths, vec![canonical_first.clone()]);
    assert_eq!(first.writes.len(), 1);
    assert!(ledger.path_touched(&session, &first_turn.turn, &canonical_first));
    assert!(!ledger.path_touched(&session, &first_turn.turn, &canonical_second));
    assert_eq!(
        first.writes[0].bytes_hash,
        format!("blake3:{}", blake3::hash(b"b").to_hex())
    );
    assert_eq!(
        ledger
            .changes_for(&session, &second_turn.turn)
            .expect("second turn changes")
            .paths,
        vec![canonical_second]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_patches_cannot_both_apply_the_same_stale_preimage() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("concurrent.txt");
    let original = format!("before\n{}", "padding\n".repeat(256 * 1024));
    fs::write(&path, &original).expect("seed file");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut first_broker = broker_generation(
        DispatchBarrierJournal {
            barrier: barrier.clone(),
        },
        directory.path(),
        1,
    );
    let mut second_broker =
        broker_generation(DispatchBarrierJournal { barrier }, directory.path(), 2);
    let policy = allow(EffectClass::FsWrite);
    let first_attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn-1"));
    let second_attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn-2"));
    let first_ledger = ChangeLedger::new();
    let second_ledger = ChangeLedger::new();
    let first_edit = FsEdit::new(&path, "before", "first");
    let second_edit = FsEdit::new(&path, "before", "second");
    restore_file_freshness(&mut first_broker, directory.path(), &path);
    restore_file_freshness(&mut second_broker, directory.path(), &path);

    let (first, second) = tokio::join!(
        first_broker.fs_edit(&first_edit, &policy, &first_attribution, &first_ledger,),
        second_broker.fs_edit(&second_edit, &policy, &second_attribution, &second_ledger,)
    );

    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let failed = if let Err(error) = first {
        error
    } else {
        second.expect_err("exactly one edit must conflict")
    };
    assert!(matches!(failed, ToolError::StaleRead { .. }));
    assert_eq!(
        usize::from(
            first_ledger.has_fs_writes(&first_attribution.session, &first_attribution.turn)
        ) + usize::from(
            second_ledger.has_fs_writes(&second_attribution.session, &second_attribution.turn)
        ),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn applied_write_is_ledgered_before_a_failed_outcome_append() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("outcome-failure.txt");
    fs::write(&path, "before").expect("seed file");
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(RejectOutcomeJournal, directory.path());
    let ledger = ChangeLedger::new();
    restore_file_freshness(&mut broker, directory.path(), &path);

    let error = broker
        .fs_edit(
            &FsEdit::new(&path, "before", "after"),
            &allow(EffectClass::FsWrite),
            &attribution,
            &ledger,
        )
        .await
        .expect_err("outcome append fails after apply");

    assert!(matches!(error, ToolError::Journal { .. }));
    let written = fs::read(&path).expect("read applied bytes");
    assert_eq!(written, b"after");
    let changes = ledger
        .changes_for(&attribution.session, &attribution.turn)
        .expect("applied write remains ledgered");
    assert_eq!(changes.writes.len(), 1);
    assert_eq!(
        changes.writes[0].bytes_hash,
        format!("blake3:{}", blake3::hash(&written).to_hex())
    );
    assert_eq!(broker.journal_snapshot().len(), 3);
    assert!(matches!(
        broker.journal_snapshot().last(),
        Some(EffectPhase::Dispatched { .. })
    ));
}

#[test]
fn cancelling_before_the_blocking_worker_starts_is_clean() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("build constrained runtime");
    runtime.block_on(async {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("cancel-before-worker.txt");
        fs::write(&path, "before").expect("seed file");
        let (occupied_sender, occupied_receiver) = tokio::sync::oneshot::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let blocking_occupant = tokio::task::spawn_blocking(move || {
            let _ = occupied_sender.send(());
            release_receiver.recv().expect("release blocking pool");
        });
        occupied_receiver.await.expect("blocking pool is occupied");

        let journal = SharedRecordingJournal::default();
        let observed_journal = journal.observer();
        let ledger = ChangeLedger::new();
        let observed_ledger = ledger.clone();
        let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
        let observed_attribution = attribution.clone();
        let mut broker = broker_at(journal, directory.path());
        restore_file_freshness(&mut broker, directory.path(), &path);
        let policy = allow(EffectClass::FsWrite);
        let edit = FsEdit::new(&path, "before", "after");
        let mut apply = Box::pin(broker.fs_edit(&edit, &policy, &attribution, &ledger));

        tokio::select! {
            biased;
            result = &mut apply => panic!("queued worker unexpectedly completed: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        drop(apply);
        let effect = match broker.journal_snapshot().first() {
            Some(EffectPhase::Intent(intent)) => intent.effect.clone(),
            phase => panic!("expected first intent phase, got {phase:?}"),
        };
        release_sender.send(()).expect("release blocking pool");
        blocking_occupant.await.expect("blocking occupant exits");
        let report = broker
            .close()
            .await
            .expect("cancelled queued worker closes cleanly");

        assert_eq!(fs::read_to_string(&path).expect("read file"), "before");
        assert!(
            !observed_ledger
                .has_fs_writes(&observed_attribution.session, &observed_attribution.turn)
        );
        assert_eq!(report.reconciled_effects, vec![effect]);
        let phases = observed_journal.effect_phases();
        let outcomes = terminal_phases(&phases);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            outcomes.as_slice(),
            [EffectPhase::Outcome {
                outcome: EffectOutcome::Unknown,
                ..
            }]
        ));
    });
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_apply_cannot_leave_a_write_without_ledger_evidence() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("cancelled.txt");
    fs::write(&path, "before").expect("seed file");
    let reached = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let observed_ledger = ChangeLedger::new();
    let ledger = GatedLedger {
        inner: observed_ledger.clone(),
        reached: Arc::clone(&reached),
        release: Arc::clone(&release),
    };
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let observed_attribution = attribution.clone();
    let mut broker = broker_at(RecordingJournal::default(), directory.path());
    restore_file_freshness(&mut broker, directory.path(), &path);
    let policy = allow(EffectClass::FsWrite);
    let edit = FsEdit::new(&path, "before", "after");
    let mut apply = Box::pin(broker.fs_edit(&edit, &policy, &attribution, &ledger));
    let worker_reached = tokio::task::spawn_blocking(move || reached.wait());

    tokio::select! {
        result = &mut apply => panic!("apply completed before ledger gate: {result:?}"),
        result = worker_reached => {
            result.expect("wait for post-rename ledger gate");
        }
    }
    assert_eq!(
        fs::read_to_string(directory.path().join("cancelled.txt")).expect("read renamed target"),
        "after"
    );
    assert!(
        !observed_ledger.has_fs_writes(&observed_attribution.session, &observed_attribution.turn)
    );

    drop(apply);
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .expect("release ledger append");
    broker.close().await.expect("successful finalizer drains");
    assert!(
        observed_ledger.has_fs_writes(&observed_attribution.session, &observed_attribution.turn)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_apply_during_ledger_failure_still_journals_failed_outcome() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("cancelled-ledger-failure.txt");
    fs::write(&path, "before").expect("seed file");
    let reached = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let ledger = GatedRejectLedger {
        reached: Arc::clone(&reached),
        release: Arc::clone(&release),
    };
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let journal = SharedRecordingJournal::default();
    let observed_journal = journal.observer();
    let mut broker = broker_at(journal, directory.path());
    restore_file_freshness(&mut broker, directory.path(), &path);
    let policy = allow(EffectClass::FsWrite);
    let edit = FsEdit::new(&path, "before", "after");
    let mut apply = Box::pin(broker.fs_edit(&edit, &policy, &attribution, &ledger));
    let worker_reached = tokio::task::spawn_blocking(move || reached.wait());

    tokio::select! {
        result = &mut apply => panic!("apply completed before ledger failure gate: {result:?}"),
        result = worker_reached => {
            result.expect("wait for post-rename ledger failure gate");
        }
    }
    assert_eq!(
        fs::read_to_string(directory.path().join("cancelled-ledger-failure.txt"))
            .expect("read renamed target"),
        "after"
    );

    drop(apply);
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .expect("release failing ledger append");
    let close_error = broker
        .close()
        .await
        .expect_err("cancelled caller's ledger failure surfaces at close");
    assert!(
        close_error
            .to_string()
            .contains("injected gated ledger append failure")
    );

    let phases = observed_journal.effect_phases();
    let outcomes = terminal_phases(&phases);
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(
        outcomes.as_slice(),
        [EffectPhase::Outcome {
            outcome: EffectOutcome::Failed { error },
            ..
        }] if error.contains("change ledger failed")
            && error.contains("injected gated ledger append failure")
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn caller_cancellation_cannot_sever_a_dispatched_terminal_append() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("cancel-terminal-claim.txt");
    fs::write(&path, "before").expect("seed file");
    let ledger_reached = Arc::new(Barrier::new(2));
    let ledger_release = Arc::new(Barrier::new(2));
    let ledger = GatedLedger {
        inner: ChangeLedger::new(),
        reached: Arc::clone(&ledger_reached),
        release: Arc::clone(&ledger_release),
    };
    let journal = TerminalGateJournal::default();
    let observed_journal = journal.observer();
    let terminal_reached = Arc::clone(&journal.reached);
    let terminal_release = Arc::clone(&journal.release);
    let terminal_attempts = Arc::clone(&journal.terminal_attempts);
    let terminal_completions = Arc::clone(&journal.terminal_completions);
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(journal, directory.path());
    restore_file_freshness(&mut broker, directory.path(), &path);
    let policy = allow(EffectClass::FsWrite);
    let edit = FsEdit::new(&path, "before", "after");
    let mut apply = Box::pin(broker.fs_edit(&edit, &policy, &attribution, &ledger));
    let worker_reached = tokio::task::spawn_blocking(move || ledger_reached.wait());

    tokio::select! {
        result = &mut apply => panic!("apply completed before ledger gate: {result:?}"),
        result = worker_reached => {
            result.expect("wait for post-rename ledger gate");
        }
    }
    drop(apply);
    let intent = match broker.journal_snapshot().first() {
        Some(EffectPhase::Intent(intent)) => intent.clone(),
        phase => panic!("expected first intent phase, got {phase:?}"),
    };

    let mut abandoned = Box::pin(broker.journal_unknown(&intent));
    tokio::select! {
        result = &mut abandoned => panic!("terminal claim completed before cancellation: {result:?}"),
        () = terminal_reached.notified() => {}
    }
    assert_eq!(terminal_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(terminal_completions.load(Ordering::SeqCst), 0);
    drop(abandoned);

    tokio::task::spawn_blocking(move || ledger_release.wait())
        .await
        .expect("release successful ledger append");
    terminal_release.notify_one();
    let report = broker
        .close()
        .await
        .expect("owned append and finalizer drain");

    assert_eq!(fs::read_to_string(&path).expect("read file"), "after");
    assert!(report.reconciled_effects.is_empty());
    assert_eq!(terminal_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(terminal_completions.load(Ordering::SeqCst), 1);
    let phases = observed_journal.effect_phases();
    let outcomes = terminal_phases(&phases);
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(
        outcomes.as_slice(),
        [EffectPhase::Outcome {
            outcome: EffectOutcome::Unknown,
            ..
        }]
    ));
}

#[tokio::test]
async fn cancelled_terminal_caller_leaves_owned_append_for_close_to_drain() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("swept-read.txt");
    fs::write(&path, "contents").expect("seed file");
    let journal = TerminalGateJournal::default();
    let observed_journal = journal.observer();
    let terminal_reached = Arc::clone(&journal.reached);
    let terminal_release = Arc::clone(&journal.release);
    let terminal_attempts = Arc::clone(&journal.terminal_attempts);
    let terminal_completions = Arc::clone(&journal.terminal_completions);
    let mut broker = broker_at(journal, directory.path());
    let intent = broker
        .normalize(&FsRead::new(&path))
        .await
        .expect("normalize read");
    let policy = allow(EffectClass::FsRead);
    broker
        .authorize(&intent, &policy)
        .await
        .expect("authorize read");
    broker
        .journal_dispatched(&intent)
        .await
        .expect("dispatch read");

    let mut abandoned = Box::pin(broker.journal_unknown(&intent));
    tokio::select! {
        result = &mut abandoned => panic!("terminal claim completed before cancellation: {result:?}"),
        () = terminal_reached.notified() => {}
    }
    assert_eq!(terminal_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(terminal_completions.load(Ordering::SeqCst), 0);
    drop(abandoned);
    terminal_release.notify_one();

    let report = broker.close().await.expect("close drains owned append");

    assert!(report.reconciled_effects.is_empty());
    assert_eq!(terminal_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(terminal_completions.load(Ordering::SeqCst), 1);
    let phases = observed_journal.effect_phases();
    let outcomes = terminal_phases(&phases);
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(
        outcomes.as_slice(),
        [EffectPhase::Outcome {
            outcome: EffectOutcome::Unknown,
            ..
        }]
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn finalizer_and_unknown_race_forces_the_loser_before_the_winner_append() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("terminal-claim-race.txt");
    fs::write(&path, "before").expect("seed file");
    let ledger_reached = Arc::new(Barrier::new(2));
    let ledger_release = Arc::new(Barrier::new(2));
    let journal = TerminalGateJournal::default();
    let observed_journal = journal.observer();
    let terminal_reached = Arc::clone(&journal.reached);
    let terminal_release = Arc::clone(&journal.release);
    let terminal_attempts = Arc::clone(&journal.terminal_attempts);
    let terminal_completions = Arc::clone(&journal.terminal_completions);
    let ledger = GatedLedger {
        inner: ChangeLedger::new(),
        reached: Arc::clone(&ledger_reached),
        release: Arc::clone(&ledger_release),
    };
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(journal, directory.path());
    restore_file_freshness(&mut broker, directory.path(), &path);
    let policy = allow(EffectClass::FsWrite);
    let edit = FsEdit::new(&path, "before", "after");
    let mut apply = Box::pin(broker.fs_edit(&edit, &policy, &attribution, &ledger));
    let worker_reached = tokio::task::spawn_blocking(move || ledger_reached.wait());

    tokio::select! {
        result = &mut apply => panic!("apply completed before ledger gate: {result:?}"),
        result = worker_reached => {
            result.expect("wait for post-rename ledger gate");
        }
    }
    drop(apply);
    let phases = broker.journal_snapshot();
    let intent = match phases.first() {
        Some(EffectPhase::Intent(intent)) => intent.clone(),
        phase => panic!("expected first intent phase, got {phase:?}"),
    };

    tokio::task::spawn_blocking(move || ledger_release.wait())
        .await
        .expect("release successful ledger append");
    terminal_reached.notified().await;
    tokio::time::timeout(Duration::from_millis(100), broker.journal_unknown(&intent))
        .await
        .expect("claim loser returns before the winner append is released")
        .expect("claim loser is a no-op");
    assert_eq!(terminal_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(terminal_completions.load(Ordering::SeqCst), 0);
    assert_eq!(terminal_phases(&observed_journal.effect_phases()).len(), 0);

    terminal_release.notify_one();
    broker
        .close()
        .await
        .expect("racing finalizer closes cleanly");

    let phases = observed_journal.effect_phases();
    assert_eq!(terminal_phases(&phases).len(), 1);
    assert_eq!(terminal_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(terminal_completions.load(Ordering::SeqCst), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn close_waits_for_a_cancelled_callers_failing_finalizer() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("close-drain.txt");
    fs::write(&path, "before").expect("seed file");
    let reached = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let ledger = GatedRejectLedger {
        reached: Arc::clone(&reached),
        release: Arc::clone(&release),
    };
    let journal = SharedRecordingJournal::default();
    let observed_journal = journal.observer();
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(journal, directory.path());
    restore_file_freshness(&mut broker, directory.path(), &path);
    let policy = allow(EffectClass::FsWrite);
    let edit = FsEdit::new(&path, "before", "after");
    let mut apply = Box::pin(broker.fs_edit(&edit, &policy, &attribution, &ledger));
    let worker_reached = tokio::task::spawn_blocking(move || reached.wait());

    tokio::select! {
        result = &mut apply => panic!("apply completed before ledger gate: {result:?}"),
        result = worker_reached => {
            result.expect("wait for ledger gate");
        }
    }
    drop(apply);
    let mut close = Box::pin(broker.close());
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut close)
            .await
            .is_err(),
        "close must wait for the registered finalizer"
    );
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .expect("release failing ledger");
    let error = close
        .await
        .expect_err("cancelled caller's ledger error surfaces at close");
    assert!(
        error
            .to_string()
            .contains("injected gated ledger append failure")
    );

    let phases = observed_journal.effect_phases();
    let outcomes = terminal_phases(&phases);
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(
        outcomes.as_slice(),
        [EffectPhase::Outcome {
            outcome: EffectOutcome::Failed { error },
            ..
        }] if error.contains("injected gated ledger append failure")
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn mixed_close_error_keeps_successful_reconciliations_visible() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let read_path = directory.path().join("pending-read.txt");
    let write_path = directory.path().join("failing-write.txt");
    fs::write(&read_path, "contents").expect("seed read file");
    fs::write(&write_path, "before").expect("seed write file");
    let journal = SharedRecordingJournal::default();
    let observed_journal = journal.observer();
    let mut broker = broker_at(journal, directory.path());
    restore_file_freshness(&mut broker, directory.path(), &write_path);

    let read_intent = broker
        .normalize(&FsRead::new(&read_path))
        .await
        .expect("normalize pending read");
    broker
        .authorize(&read_intent, &allow(EffectClass::FsRead))
        .await
        .expect("authorize pending read");
    broker
        .journal_dispatched(&read_intent)
        .await
        .expect("dispatch pending read");

    let reached = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let ledger = GatedRejectLedger {
        reached: Arc::clone(&reached),
        release: Arc::clone(&release),
    };
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let write_edit = FsEdit::new(&write_path, "before", "after");
    let write_policy = allow(EffectClass::FsWrite);
    let mut apply = Box::pin(broker.fs_edit(&write_edit, &write_policy, &attribution, &ledger));
    let worker_reached = tokio::task::spawn_blocking(move || reached.wait());

    tokio::select! {
        result = &mut apply => panic!("apply completed before ledger failure gate: {result:?}"),
        result = worker_reached => {
            result.expect("wait for ledger failure gate");
        }
    }
    drop(apply);
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .expect("release failing ledger");

    let close_error = broker
        .close()
        .await
        .expect_err("mixed close retains its error");
    assert_eq!(
        close_error.report.reconciled_effects,
        vec![read_intent.effect.clone()]
    );
    assert!(close_error.errors.iter().any(|error| {
        error
            .to_string()
            .contains("injected gated ledger append failure")
    }));

    let phases = observed_journal.effect_phases();
    let outcomes = terminal_phases(&phases);
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().any(|phase| {
        matches!(
            phase,
            EffectPhase::Outcome {
                effect,
                outcome: EffectOutcome::Unknown,
                ..
            } if effect == &read_intent.effect
        )
    }));
}

#[cfg(unix)]
#[tokio::test]
async fn failed_terminal_append_is_keyed_and_never_appends_a_fallback() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("terminal-append-failure.txt");
    fs::write(&path, "before").expect("seed file");
    let journal = FailFirstTerminalJournal::default();
    let observed_journal = journal.observer();
    let terminal_attempts = Arc::clone(&journal.terminal_attempts);
    let ledger = ChangeLedger::new();
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(journal, directory.path());
    restore_file_freshness(&mut broker, directory.path(), &path);

    let apply_error = broker
        .fs_edit(
            &FsEdit::new(&path, "before", "after"),
            &allow(EffectClass::FsWrite),
            &attribution,
            &ledger,
        )
        .await
        .expect_err("first terminal append fails");
    assert!(
        apply_error
            .to_string()
            .contains("injected first terminal outcome append failure")
    );
    let effect = match broker.journal_snapshot().first() {
        Some(EffectPhase::Intent(intent)) => intent.effect.clone(),
        phase => panic!("expected first intent phase, got {phase:?}"),
    };
    let close_error = broker
        .close()
        .await
        .expect_err("terminal append failure surfaces at close");
    assert!(
        close_error
            .to_string()
            .contains("injected first terminal outcome append failure")
    );
    assert!(close_error.to_string().contains(effect.as_str()));

    assert_eq!(fs::read_to_string(&path).expect("read file"), "after");
    assert_eq!(terminal_attempts.load(Ordering::SeqCst), 1);
    let phases = observed_journal.effect_phases();
    let outcomes = terminal_phases(&phases);
    assert_eq!(outcomes.len(), 0);
    assert_eq!(phases.len(), 3);
    assert!(matches!(
        phases.last(),
        Some(EffectPhase::Dispatched { .. })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn ledger_append_failure_becomes_a_failed_effect_outcome() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("ledger-failure.txt");
    fs::write(&path, "before").expect("seed file");
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(RecordingJournal::default(), directory.path());
    restore_file_freshness(&mut broker, directory.path(), &path);

    let error = broker
        .fs_edit(
            &FsEdit::new(&path, "before", "after"),
            &allow(EffectClass::FsWrite),
            &attribution,
            &RejectLedger,
        )
        .await
        .expect_err("ledger failure is returned");

    assert!(matches!(error, ToolError::Ledger { .. }));
    assert_eq!(
        fs::read_to_string(&path).expect("read applied file"),
        "after"
    );
    assert!(matches!(
        broker.journal_snapshot().last(),
        Some(EffectPhase::Outcome {
            outcome: EffectOutcome::Failed { error, },
            ..
        }) if error.contains("change ledger failed")
    ));
}

#[tokio::test]
async fn workspace_traversal_is_rejected_before_authorization_or_read() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let workspace = parent.path().join("workspace");
    fs::create_dir(&workspace).expect("create workspace");
    fs::write(parent.path().join("outside.txt"), "secret").expect("seed outside");
    let mut broker = broker_at(RecordingJournal::default(), &workspace);
    let mut cas = RecordingCas::default();

    let error = broker
        .fs_read(
            &FsRead::new("../outside.txt"),
            &allow(EffectClass::FsRead),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect_err("traversal must not leave workspace");

    assert!(matches!(error, ToolError::WorkspaceBoundary { .. }));
    assert!(broker.journal_snapshot().is_empty());
    assert!(cas.writes.is_empty());
}

/// MUTATION CHECK: remove `require_under_root` from write/patch resolution.
/// Expected failure: an outside file is created or replaced. Verified by
/// revert in W4a1.
#[tokio::test]
async fn mutating_paths_reject_parent_and_absolute_workspace_escapes() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let workspace = parent.path().join("workspace");
    fs::create_dir(&workspace).expect("create workspace");
    let outside = parent.path().join("outside.txt");
    fs::write(&outside, "outside-before").expect("seed outside");
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let policy = allow(EffectClass::FsWrite);
    let ledger = ChangeLedger::new();
    let mut broker = broker_at(RecordingJournal::default(), &workspace);

    let traversal = broker
        .fs_write(
            &FsWrite::new("../created-outside.txt", "escaped"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect_err("parent traversal must be rejected");
    let absolute = broker
        .fs_edit(
            &FsEdit::new(&outside, "outside-before", "outside-after"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect_err("absolute outside patch must be rejected");

    assert!(matches!(traversal, ToolError::WorkspaceBoundary { .. }));
    assert!(matches!(absolute, ToolError::WorkspaceBoundary { .. }));
    assert!(!parent.path().join("created-outside.txt").exists());
    assert_eq!(
        fs::read_to_string(&outside).expect("read outside"),
        "outside-before"
    );
    assert!(broker.journal_snapshot().is_empty());
    assert!(!ledger.has_fs_writes(&attribution.session, &attribution.turn));
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_symlink_escape_is_rejected_before_authorization_or_read() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("temporary parent");
    let workspace = parent.path().join("workspace");
    fs::create_dir(&workspace).expect("create workspace");
    let outside = parent.path().join("outside.txt");
    fs::write(&outside, "secret").expect("seed outside");
    symlink(&outside, workspace.join("escape.txt")).expect("create escape symlink");
    let mut broker = broker_at(RecordingJournal::default(), &workspace);
    let mut cas = RecordingCas::default();

    let error = broker
        .fs_read(
            &FsRead::new("escape.txt"),
            &allow(EffectClass::FsRead),
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect_err("symlink must not leave workspace");

    assert!(matches!(error, ToolError::WorkspaceBoundary { .. }));
    assert!(broker.journal_snapshot().is_empty());
}

/// MUTATION CHECK: canonicalize only the lexical path or permit a missing
/// leaf beneath an uncanonicalized parent. Expected failure: one of the
/// outside targets changes. Verified by revert in W4a1.
#[cfg(unix)]
#[tokio::test]
async fn mutating_paths_reject_leaf_and_parent_symlink_escapes() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("temporary parent");
    let workspace = parent.path().join("workspace");
    let outside_dir = parent.path().join("outside");
    fs::create_dir(&workspace).expect("create workspace");
    fs::create_dir(&outside_dir).expect("create outside");
    let outside_file = outside_dir.join("outside.txt");
    fs::write(&outside_file, "outside-before").expect("seed outside");
    symlink(&outside_file, workspace.join("leaf-link")).expect("leaf symlink");
    symlink(&outside_dir, workspace.join("parent-link")).expect("parent symlink");
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let policy = allow(EffectClass::FsWrite);
    let ledger = ChangeLedger::new();
    let mut broker = broker_at(RecordingJournal::default(), &workspace);

    let leaf = broker
        .fs_write(
            &FsWrite::new("leaf-link", "escaped"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect_err("leaf symlink escape");
    let parent_link = broker
        .fs_write(
            &FsWrite::new("parent-link/new.txt", "escaped"),
            &policy,
            &attribution,
            &ledger,
        )
        .await
        .expect_err("parent symlink escape");

    assert!(matches!(leaf, ToolError::WorkspaceBoundary { .. }));
    assert!(matches!(parent_link, ToolError::WorkspaceBoundary { .. }));
    assert_eq!(
        fs::read_to_string(&outside_file).expect("read outside"),
        "outside-before"
    );
    assert!(!outside_dir.join("new.txt").exists());
    assert!(broker.journal_snapshot().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn component_swapped_to_outside_symlink_after_authorization_is_refused() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("temporary parent");
    let workspace = parent.path().join("workspace");
    let component = workspace.join("component");
    let parked = workspace.join("parked");
    let outside = parent.path().join("outside");
    fs::create_dir_all(&component).expect("create workspace component");
    fs::create_dir(&outside).expect("create outside directory");
    fs::write(component.join("target.txt"), "before").expect("seed workspace target");
    fs::write(outside.join("target.txt"), "before").expect("seed outside target");

    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut broker = broker_at(
        DispatchGateJournal {
            reached: Arc::clone(&reached),
            release: Arc::clone(&release),
        },
        &workspace,
    );
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let ledger = ChangeLedger::new();
    let observed_ledger = ledger.clone();
    let edit = FsEdit::new(component.join("target.txt"), "before", "after");
    restore_file_freshness(&mut broker, &workspace, &component.join("target.txt"));
    let policy = allow(EffectClass::FsWrite);
    let task = tokio::spawn(async move {
        let result = broker.fs_edit(&edit, &policy, &attribution, &ledger).await;
        (result, broker)
    });

    reached.notified().await;
    fs::rename(&component, &parked).expect("park authorized component");
    symlink(&outside, &component).expect("swap component to outside symlink");
    release.notify_one();

    let (result, broker) = task.await.expect("patch task joins");
    assert!(matches!(result, Err(ToolError::PathChanged { .. })));
    assert_eq!(
        fs::read_to_string(parked.join("target.txt")).expect("read parked target"),
        "before"
    );
    assert_eq!(
        fs::read_to_string(outside.join("target.txt")).expect("read outside target"),
        "before"
    );
    assert!(!observed_ledger.has_fs_writes(&SessionId::new("session"), &RunId::new("turn")));
    assert!(matches!(
        broker.journal_snapshot().last(),
        Some(EffectPhase::Outcome {
            outcome: EffectOutcome::Failed { .. },
            ..
        })
    ));
}

#[tokio::test]
async fn canonical_path_aliases_share_a_digest_but_distinct_files_do_not() {
    let directory = tempfile::tempdir().expect("temporary directory");
    fs::create_dir(directory.path().join("nested")).expect("create nested");
    fs::write(directory.path().join("nested").join("same.txt"), "same").expect("seed same");
    fs::write(directory.path().join("other.txt"), "other").expect("seed other");
    let mut broker = broker_at(RecordingJournal::default(), directory.path());

    let relative = broker
        .normalize(&FsRead::new("nested/same.txt"))
        .await
        .expect("relative alias");
    let lexical_alias = broker
        .normalize(&FsRead::new("nested/../nested/same.txt"))
        .await
        .expect("lexical alias");
    let absolute = broker
        .normalize(&FsRead::new(
            directory.path().join("nested").join("same.txt"),
        ))
        .await
        .expect("absolute alias");
    let distinct = broker
        .normalize(&FsRead::new("other.txt"))
        .await
        .expect("distinct path");

    assert_eq!(relative.args_digest, lexical_alias.args_digest);
    assert_eq!(relative.args_digest, absolute.args_digest);
    assert_ne!(relative.args_digest, distinct.args_digest);
}

#[tokio::test]
async fn oversized_result_keeps_preview_and_freezes_full_payload_in_cas() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("large.txt");
    let full = "αβγ delta\n".repeat(16);
    fs::write(&path, &full).expect("seed file");
    let mut broker = broker_at(RecordingJournal::default(), directory.path());
    let mut cas = RecordingCas::default();

    let result = broker
        .fs_read(
            &FsRead::new(&path),
            &allow(EffectClass::FsRead),
            &mut cas,
            ResultBounds {
                max_preview_bytes: 9,
            },
        )
        .await
        .expect("bounded read");

    assert!(result.truncated);
    assert!(result.preview.len() <= 9);
    assert!(result.preview.is_char_boundary(result.preview.len()));
    assert!(result.artifact.is_some());
    assert_eq!(cas.writes, vec![full.into_bytes()]);
}

#[tokio::test]
async fn directory_read_and_search_are_sorted_bounded_read_effects() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = directory.path();
    fs::create_dir(root.join("nested")).expect("create nested");
    fs::write(root.join("zeta.txt"), "needle z\n").expect("seed zeta");
    fs::write(root.join("alpha.txt"), "nothing\nneedle a\n").expect("seed alpha");
    fs::write(root.join("nested").join("beta.txt"), "needle b\n").expect("seed beta");
    let policy = allow(EffectClass::FsRead);
    let mut cas = RecordingCas::default();
    let mut broker = broker_at(RecordingJournal::default(), directory.path());

    let listed = broker
        .fs_read(
            &FsRead::new(root),
            &policy,
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("list");
    assert_eq!(listed.preview, "alpha.txt\nnested/\nzeta.txt\n");

    let searched = broker
        .fs_search(
            &FsSearch::new(root, "needle"),
            &policy,
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("search");
    assert_eq!(
        searched.preview,
        "alpha.txt:2:needle a\nnested/beta.txt:1:needle b\nzeta.txt:1:needle z\n"
    );
    assert!(cas.writes.is_empty());
}

#[tokio::test]
async fn file_read_limit_without_offset_starts_at_the_first_numbered_line() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("lines.txt");
    fs::write(&path, "one\ntwo\nthree\n").expect("seed lines");
    let policy = allow(EffectClass::FsRead);
    let mut cas = RecordingCas::default();
    let mut broker = broker_at(RecordingJournal::default(), directory.path());

    let result = broker
        .fs_read(
            &FsRead::new(path).with_line_range(None, Some(2)),
            &policy,
            &mut cas,
            ResultBounds::default(),
        )
        .await
        .expect("limit-only read");

    assert_eq!(result.preview, "1: one\n2: two\n");
    assert!(cas.writes.is_empty());
}
