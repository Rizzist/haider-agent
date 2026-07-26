#![allow(clippy::expect_used)]

use haider_protocol::EventPayload;
use haider_protocol::effect::{EffectClass, EffectOutcome, EffectPhase};
use haider_protocol::ids::{ArtifactRef, RunId, SessionId};
use haider_tools::{
    CasSink, ChangeLedger, ChangeLedgerSink, EffectBroker, FsList, FsPatch, FsRead, FsSearch,
    FsWriteRecord, JournalSink, PermissionPolicy, ResultBounds, ToolError, ToolResult,
    TurnAttribution,
};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

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

#[derive(Debug, Clone, Default)]
struct SharedRecordingJournal {
    payloads: Arc<Mutex<Vec<EventPayload>>>,
}

impl SharedRecordingJournal {
    fn effect_phases(&self) -> Vec<EffectPhase> {
        self.payloads
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

#[async_trait::async_trait]
impl JournalSink for SharedRecordingJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        self.payloads
            .lock()
            .map_err(|_| ToolError::journal("shared recording journal lock is poisoned"))?
            .push(payload);
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct TerminalGateJournal {
    payloads: Arc<Mutex<Vec<EventPayload>>>,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl TerminalGateJournal {
    fn effect_phases(&self) -> Vec<EffectPhase> {
        self.payloads
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

#[async_trait::async_trait]
impl JournalSink for TerminalGateJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if matches!(payload, EventPayload::Effect(EffectPhase::Outcome { .. })) {
            self.reached.notify_one();
            self.release.notified().await;
        }
        self.payloads
            .lock()
            .map_err(|_| ToolError::journal("terminal gate journal lock is poisoned"))?
            .push(payload);
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
struct FailFirstTerminalJournal {
    payloads: Arc<Mutex<Vec<EventPayload>>>,
    terminal_attempts: Arc<AtomicUsize>,
}

impl FailFirstTerminalJournal {
    fn effect_phases(&self) -> Vec<EffectPhase> {
        self.payloads
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

#[async_trait::async_trait]
impl JournalSink for FailFirstTerminalJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if matches!(payload, EventPayload::Effect(EffectPhase::Outcome { .. }))
            && self.terminal_attempts.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Err(ToolError::journal(
                "injected first terminal outcome append failure",
            ));
        }
        self.payloads
            .lock()
            .map_err(|_| ToolError::journal("terminal recording journal lock is poisoned"))?
            .push(payload);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct RejectOutcomeJournal;

#[async_trait::async_trait]
impl JournalSink for RejectOutcomeJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if matches!(payload, EventPayload::Effect(EffectPhase::Outcome { .. })) {
            return Err(ToolError::journal("outcome append unavailable"));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct DispatchBarrierJournal {
    barrier: Arc<tokio::sync::Barrier>,
}

#[async_trait::async_trait]
impl JournalSink for DispatchBarrierJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if matches!(
            payload,
            EventPayload::Effect(EffectPhase::Dispatched { .. })
        ) {
            self.barrier.wait().await;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct DispatchGateJournal {
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl JournalSink for DispatchGateJournal {
    async fn append(&mut self, payload: EventPayload) -> ToolResult<()> {
        if matches!(
            payload,
            EventPayload::Effect(EffectPhase::Dispatched { .. })
        ) {
            self.reached.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct GatedLedger {
    inner: ChangeLedger,
    reached: Arc<Barrier>,
    release: Arc<Barrier>,
}

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

#[derive(Debug, Clone, Copy)]
struct RejectLedger;

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

#[derive(Debug, Clone)]
struct GatedRejectLedger {
    reached: Arc<Barrier>,
    release: Arc<Barrier>,
}

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

#[derive(Debug, Clone)]
struct RacingLedger {
    inner: ChangeLedger,
    reached: Arc<tokio::sync::Notify>,
    race: Arc<Barrier>,
}

impl ChangeLedgerSink for RacingLedger {
    fn record_fs_write(
        &self,
        session: SessionId,
        turn: RunId,
        record: FsWriteRecord,
    ) -> ToolResult<()> {
        self.inner.record_fs_write(session, turn, record)?;
        self.reached.notify_one();
        self.race.wait();
        Ok(())
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
}

fn allow(class: EffectClass) -> PermissionPolicy {
    let mut policy = PermissionPolicy::default();
    policy.allow(class);
    policy
}

fn broker_at<J>(journal: J, workspace_root: &Path) -> EffectBroker<J>
where
    J: JournalSink,
{
    broker_generation(journal, workspace_root, 1)
}

fn broker_generation<J>(journal: J, workspace_root: &Path, generation: u64) -> EffectBroker<J>
where
    J: JournalSink,
{
    EffectBroker::new_at(
        journal,
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

#[tokio::test]
async fn preimage_mismatch_returns_typed_conflict_and_failed_outcome() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("conflict.txt");
    fs::write(&path, "current").expect("seed file");
    let mut broker = broker_at(RecordingJournal::default(), directory.path());
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let ledger = ChangeLedger::new();

    let error = broker
        .fs_patch(
            &FsPatch::new(&path, "stale", "replacement"),
            &allow(EffectClass::FsWrite),
            &attribution,
            &ledger,
        )
        .await
        .expect_err("preimage mismatch");

    let ToolError::Conflict(conflict) = error else {
        panic!("expected typed conflict");
    };
    assert_eq!(
        conflict.path,
        fs::canonicalize(&path).expect("canonical conflict path")
    );
    assert_eq!(conflict.expected_preimage, "stale");
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

    broker
        .fs_patch(
            &FsPatch::new(&first_path, "a", "b"),
            &policy,
            &first_turn,
            &ledger,
        )
        .await
        .expect("first patch");
    broker
        .fs_patch(
            &FsPatch::new(&second_path, "x", "y"),
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
    let first_patch = FsPatch::new(&path, "before", "first");
    let second_patch = FsPatch::new(&path, "before", "second");

    let (first, second) = tokio::join!(
        first_broker.fs_patch(&first_patch, &policy, &first_attribution, &first_ledger,),
        second_broker.fs_patch(&second_patch, &policy, &second_attribution, &second_ledger,)
    );

    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let failed = if let Err(error) = first {
        error
    } else {
        second.expect_err("exactly one patch must conflict")
    };
    assert!(matches!(failed, ToolError::Conflict(_)));
    assert_eq!(
        usize::from(
            first_ledger.has_fs_writes(&first_attribution.session, &first_attribution.turn)
        ) + usize::from(
            second_ledger.has_fs_writes(&second_attribution.session, &second_attribution.turn)
        ),
        1
    );
}

#[tokio::test]
async fn applied_write_is_ledgered_before_a_failed_outcome_append() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("outcome-failure.txt");
    fs::write(&path, "before").expect("seed file");
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(RejectOutcomeJournal, directory.path());
    let ledger = ChangeLedger::new();

    let error = broker
        .fs_patch(
            &FsPatch::new(&path, "before", "after"),
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
        let observed_journal = journal.clone();
        let ledger = ChangeLedger::new();
        let observed_ledger = ledger.clone();
        let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
        let observed_attribution = attribution.clone();
        let mut broker = broker_at(journal, directory.path());
        let policy = allow(EffectClass::FsWrite);
        let patch = FsPatch::new(&path, "before", "after");
        let mut apply = Box::pin(broker.fs_patch(&patch, &policy, &attribution, &ledger));

        tokio::select! {
            biased;
            result = &mut apply => panic!("queued worker unexpectedly completed: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        drop(apply);
        release_sender.send(()).expect("release blocking pool");
        blocking_occupant.await.expect("blocking occupant exits");
        broker
            .close()
            .await
            .expect("cancelled queued worker closes cleanly");

        assert_eq!(fs::read_to_string(&path).expect("read file"), "before");
        assert!(
            !observed_ledger
                .has_fs_writes(&observed_attribution.session, &observed_attribution.turn)
        );
        assert_eq!(terminal_phases(&observed_journal.effect_phases()).len(), 0);
    });
}

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
    let policy = allow(EffectClass::FsWrite);
    let patch = FsPatch::new(&path, "before", "after");
    let mut apply = Box::pin(broker.fs_patch(&patch, &policy, &attribution, &ledger));
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
    let observed_journal = journal.clone();
    let mut broker = broker_at(journal, directory.path());
    let policy = allow(EffectClass::FsWrite);
    let patch = FsPatch::new(&path, "before", "after");
    let mut apply = Box::pin(broker.fs_patch(&patch, &policy, &attribution, &ledger));
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

#[tokio::test]
async fn cancelling_after_worker_completion_produces_exactly_one_terminal_phase() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("cancel-after-worker.txt");
    fs::write(&path, "before").expect("seed file");
    let reached = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let journal = TerminalGateJournal {
        payloads: Arc::new(Mutex::new(Vec::new())),
        reached: Arc::clone(&reached),
        release: Arc::clone(&release),
    };
    let observed_journal = journal.clone();
    let ledger = ChangeLedger::new();
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(journal, directory.path());
    let policy = allow(EffectClass::FsWrite);
    let patch = FsPatch::new(&path, "before", "after");
    let mut apply = Box::pin(broker.fs_patch(&patch, &policy, &attribution, &ledger));

    tokio::select! {
        result = &mut apply => panic!("apply completed before terminal gate: {result:?}"),
        () = reached.notified() => {}
    }
    drop(apply);
    release.notify_one();
    broker.close().await.expect("finalizer drains cleanly");

    assert_eq!(fs::read_to_string(&path).expect("read file"), "after");
    let phases = observed_journal.effect_phases();
    let outcomes = terminal_phases(&phases);
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(
        outcomes.as_slice(),
        [EffectPhase::Outcome {
            outcome: EffectOutcome::Ok,
            ..
        }]
    ));
}

#[tokio::test]
async fn finalizer_and_unknown_race_can_claim_only_one_terminal_phase() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("terminal-claim-race.txt");
    fs::write(&path, "before").expect("seed file");
    let race = Arc::new(Barrier::new(3));
    let worker_reached = Arc::new(tokio::sync::Notify::new());
    let journal = SharedRecordingJournal::default();
    let observed_journal = journal.clone();
    let ledger = RacingLedger {
        inner: ChangeLedger::new(),
        reached: Arc::clone(&worker_reached),
        race: Arc::clone(&race),
    };
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(journal, directory.path());
    let policy = allow(EffectClass::FsWrite);
    let patch = FsPatch::new(&path, "before", "after");
    let mut apply = Box::pin(broker.fs_patch(&patch, &policy, &attribution, &ledger));

    tokio::select! {
        result = &mut apply => panic!("apply completed before race barrier: {result:?}"),
        () = worker_reached.notified() => {}
    }
    drop(apply);
    let phases = broker.journal_snapshot();
    let intent = match phases.first() {
        Some(EffectPhase::Intent(intent)) => intent.clone(),
        phase => panic!("expected first intent phase, got {phase:?}"),
    };
    let unknown_race = Arc::clone(&race);
    let release_race = Arc::clone(&race);
    let unknown = async {
        tokio::task::spawn_blocking(move || unknown_race.wait())
            .await
            .expect("unknown reaches race barrier");
        broker.journal_unknown(&intent).await
    };
    let release = tokio::task::spawn_blocking(move || release_race.wait());
    let (unknown_result, release_result) = tokio::join!(unknown, release);
    unknown_result.expect("claim loser is a no-op");
    release_result.expect("release race barrier");
    broker
        .close()
        .await
        .expect("racing finalizer closes cleanly");

    let phases = observed_journal.effect_phases();
    assert_eq!(terminal_phases(&phases).len(), 1);
}

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
    let observed_journal = journal.clone();
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(journal, directory.path());
    let policy = allow(EffectClass::FsWrite);
    let patch = FsPatch::new(&path, "before", "after");
    let mut apply = Box::pin(broker.fs_patch(&patch, &policy, &attribution, &ledger));
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

#[tokio::test]
async fn failed_terminal_append_escalates_to_unknown_and_surfaces_on_close() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("unknown-escalation.txt");
    fs::write(&path, "before").expect("seed file");
    let journal = FailFirstTerminalJournal::default();
    let observed_journal = journal.clone();
    let ledger = ChangeLedger::new();
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(journal, directory.path());

    let apply_error = broker
        .fs_patch(
            &FsPatch::new(&path, "before", "after"),
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
    let close_error = broker
        .close()
        .await
        .expect_err("terminal append failure surfaces at close");
    assert!(
        close_error
            .to_string()
            .contains("injected first terminal outcome append failure")
    );

    assert_eq!(fs::read_to_string(&path).expect("read file"), "after");
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
async fn ledger_append_failure_becomes_a_failed_effect_outcome() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("ledger-failure.txt");
    fs::write(&path, "before").expect("seed file");
    let attribution = TurnAttribution::new(SessionId::new("session"), RunId::new("turn"));
    let mut broker = broker_at(RecordingJournal::default(), directory.path());

    let error = broker
        .fs_patch(
            &FsPatch::new(&path, "before", "after"),
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
    let patch = FsPatch::new(component.join("target.txt"), "before", "after");
    let policy = allow(EffectClass::FsWrite);
    let task = tokio::spawn(async move {
        let result = broker
            .fs_patch(&patch, &policy, &attribution, &ledger)
            .await;
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
async fn list_and_search_are_sorted_bounded_read_effects() {
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
        .fs_list(
            &FsList::new(root),
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
