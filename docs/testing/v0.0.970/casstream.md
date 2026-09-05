# v0.0.970 — CAS streaming lane

Branch `lane-970-casstream`, originally at `7694ef9c`. The final source tree
includes `origin/wave-970` at `372a2639fd5ae971d6ecc29f61f7cbe1ad5a0551`
(toolshape merge), plus this lane. Work is uncommitted; the orchestrator must
record the Git merge metadata and commit.

## Behavior and durability

- Negotiated `artifact_put_binary_v1` adds high-bit-tagged, length-prefixed
  Begin/Chunk/Finish frames on the existing local IPC connection, independently
  of JSON/MessagePack selection. Each chunk is at most 64 KiB; the decoder
  rejects an excessive length before allocation. Total admission is 512 MiB.
  The legacy JSON method retains its 33 MiB cap and old-peer fallback.
- Each binary frame has a bounded correlation ID and receives an ordinary
  response. Chunk acknowledgments provide bounded backpressure; the main
  connection select loop continues serving Pings while CAS disk jobs run.
  The client's existing request timeout covers lock admission, source reads,
  all acknowledgments and final publication together, without per-chunk resets.
- `CasUpload` hashes and writes unpublished staging. Only exact-length,
  matching-digest Finish publishes an immutable object, with file and target
  directory durability before success. Drop removes incomplete staging. A
  process crash can leave an unreferenced staging/orphan file; it cannot publish
  a partial object or a journal reference to missing, unsynced text.
- Large journal text (inclusive 64 KiB threshold) uses a private versioned
  MessagePack record and CAS digest internally. Every event-store read hydrates
  the original `RawEnvelope`; no text-reference field or new client-visible
  event shape exists. Unknown nested JSON strings, Unicode, segmented replies,
  replay/export/hook readers and provider-resume opaque text retain their
  values. Hook page budgets use the original logical envelope byte count.
  Small records and legacy JSON/MessagePack reads retain their existing formats.
- `open_verified` returns a seekable file after bounded-buffer digest
  verification and rewinds the same handle. `open_cas_reader` verifies outside
  the SQLite lock. PDF admission checks metadata against its 32 MiB cap before
  collecting parser input through a bounded reader. The PDF parser still
  requires contiguous bytes for admitted PDFs. Larger blob consumers can use
  the reader without collecting the whole artifact.

The active-upload Ping pin runs after a chunk acknowledgment. Servicing Pings
while a disk job is deliberately held pending is verified by inspection of the
separate job/select branches, rather than claimed as a stall-injection test.

## Reconnect and the 968 resume seam

Binary transport retries use the existing headless `reconnect_before_session`
seam and the original attachment snapshot. They restart at byte zero; chunk
ACKs are not durable resume offsets. A lost final response is safe because a
repeated complete digest deduplicates. The subsequent session/submit retry
retains the same durable command identity and sequence cursor. The separate
968 provider request-attempt/prefix-resume mechanism is preserved, including
opaque resume text hydrated from CAS; this lane adds no competing run cursor.

## Named pins

| Claim | Tests |
|---|---|
| framing/feature gate/limits | `binary_artifact_roundtrips_at_every_split_in_both_encodings`, `binary_artifact_rejects_unnegotiated_oversized_and_invalid_bodies` |
| partial frame and hygiene | `binary_artifact_partial_frame_does_not_deliver_and_debug_redacts`, `casstream_partial_frame_abort_reconnect_digest_and_length_integrity` |
| exact digest, length, abort | `streamed_cas_put_checks_digest_and_declared_length_before_publication`, `streamed_cas_overlong_chunk_poisons_complete_prefix`, `streamed_cas_partial_drop_aborts_and_reconnect_retry_deduplicates` |
| concurrent durability | `streamed_cas_dedup_syncs_shard_while_original_publisher_is_paused`, `streamed_cas_publication_racing_legacy_put_and_file_put_still_syncs_shard` |
| client reconnect + durable submit | `headless_binary_upload_disconnect_retries_snapshot_then_resumes_durable_submit`; existing legacy `headless_attach_uploads_then_submits_with_durable_identity` |
| retryable errors | `binary_cas_error_preserves_json_upload_retryability` |
| streamed read | `streamed_cas_read_large_blob_is_seekable_and_detects_corruption_before_read` |
| transparent schema/replay | `text_cas_unknown_nested_payload_is_transparent_across_reopen_and_all_replay_paths`, `text_cas_segmented_reply_and_repeated_json_text_share_one_digest`, `text_cas_threshold_is_inclusive_and_small_rows_remain_legacy_compatible` |
| text integrity/transaction abort | `text_cas_corruption_missing_object_and_wrong_length_fail_closed`, `text_cas_sql_abort_never_publishes_partial_envelopes` |
| hook budgets/968 resume | `text_cas_hook_metadata_budget_counts_hydrated_bytes_without_reading_objects`, `text_cas_preserves_provider_resume_opaque_text_and_mixed_reply_fields` |

## Measurement protocol

The legacy implementation cannot accept one 264 MiB artifact: its default
wire ceiling is 48 MiB and its independent decoded artifact cap is 33 MiB.
Increasing the wire ceiling alone does not remove the decoded cap. Evidence
therefore separates actual 264 MiB JSON attempts (rejected) from accepted
264 MiB binary puts. No speedup is claimed between failure and success.
The additional matched successful workload is eight distinct 33 MiB objects
(264 MiB total) over each transport on the same build, with fresh processes
and stores. This isolates the additive transport paths without modifying old
limits to manufacture a successful baseline.

`scripts/qa-gate/casstream_bench.py` records N>=3, load1<10 at each accepted
sample, raw wall times and exact per-process peak RSS from getrusage/wait4.
The raw IPC client is Python, so its RSS is explicitly harness RSS, not a Rust
CLI measurement. Daemon RSS is the actual product process. Fixture generation
and hashing precede timing; upload wall includes file read, encoding, IPC and
durable CAS acknowledgment. Post-write on-disk BLAKE3 validation is untimed.

The final merged run completed **12/12 valid trials, N=3 per mode**, with
maximum observed load1 **6.8628**. All accepted objects were independently
rehashed from disk after acknowledgment. All child daemons exited 0. All
three actual single-264-MiB JSON construction/send attempts received fatal
`invalid_frame`, and their rejected objects were absent.

| Workload | Outcome | Wall median seconds (min–max) | Daemon peak RSS median MiB | Python client peak RSS median MiB |
|---|---|---:|---:|---:|
| JSON single 264 MiB | Rejected, 3/3 | 0.348 (0.308–0.381) | 45.44 | 642.84 |
| JSON 8 × 33 MiB | Accepted, 3/3 | 16.945 (13.905–21.102) | 90.05 | 412.33 |
| Binary 8 × 33 MiB | Accepted, 3/3 | 11.401 (10.433–14.645) | 46.63 | 27.00 |
| Binary single 264 MiB | Accepted, 3/3 | 11.367 (10.904–16.866) | 46.45 | 27.14 |

On the matched successful 8 × 33 MiB workload, median wall time decreased
**32.7%** and daemon peak RSS decreased **48.2%**. Python harness peak RSS
decreased 93.5%; this is not a measurement of Rust CLI memory. Wall MAD is
3.040 s for JSON-eight, 0.968 s for binary-eight and 0.462 s for binary-single.
The valid slower trials are retained; no outlier is discarded by wall time.
There is no failure-to-success wall comparison for the single 264 MiB case.

Raw samples and full median/range/MAD statistics are in
`casstream-evidence/measurements-merged.json` and
`casstream-evidence/measurements-merged-summary.json`, with stdout and
per-case daemon logs alongside them. The measured merged `haiderd` is
**199,582,736 bytes**, above the 10 MiB integrity floor, with SHA-256
`9bb55101dd8e34cb7ef069fd66f78a1fbd43f67644aa47d9a7f1879368221d09`.
The current binary hash matches the measurement record. Host details are
recorded in the raw evidence and `host.json` (macOS arm64).

The harness uses unoptimized dev binaries with debug info disabled. These
are same-build transport/allocation diagnostics, not release-build latency
promises. Reproduction requires Python `blake3` and the prebuilt daemon:

```sh
python3 scripts/qa-gate/casstream_bench.py \
  --daemon target/debug/haiderd \
  --output docs/testing/v0.0.970/casstream-evidence/new-measurements.json \
  --runs 3 --max-load 10
```

The executed invocation used `PYTHONPATH=/private/tmp/casstream-python` to
load the benchmark-only Python dependency. The script refuses an existing
output path, preserving prior evidence.

The first merged launch was refused at load1 12.82 before collecting a sample
(`measurements-merged-load-blocked.stdout.log`). A subsequent attempt started
below 10, recorded one valid rejected legacy-single attempt, then exceeded the
load ceiling during the legacy-eight case (load max 10.50; 28.25 s). That
incomplete series is preserved as `measurements-merged-overload-1.*`; it is
excluded from the final balanced series because of the predeclared load rule.
The accepted overloaded write still passed on-disk digest verification and
its daemon exited cleanly.

## Citation audit and lane boundaries

`969-common.md` was located at
`/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/969-common.md`; its older wave-969 base
wording is superseded by the supplied 970 common rules. The casstream change itself does not edit oauth, worker, provider transport
or supervisor code. The mandatory upstream source merge imports toolshape
changes in worker/actor/tool surfaces unchanged from its commit, as recorded
in the merge-forward file list.

The supplied round-1/round-2 evidence describes prior source snapshots.
Applicable constructs were re-found, not trusted by line number: CAS
publication/directory sync and the provider-view barrier remain in
`haider-store/src/cas.rs`; the stream receive loop remains in
`haider-daemon/src/connection.rs`; provider-view durability is preserved.
D6.6's claim that ordinary whole-store reads hold the StoreOwner lock remains
correct, with drifted line numbers; the new reader and staging interfaces
explicitly release it before blob work. X1.8's namespace durability rule is
preserved. No latency savings from those proposal tables are reported as
measurements by this lane.

## Merge-forward gate

Required `git fetch origin wave-970` was attempted and denied because
`FETCH_HEAD` is outside the writable sandbox. The fallback
`git merge --no-commit origin/wave-970` was also attempted and denied at
`ORIG_HEAD.lock`. Initially HEAD and local fallback both named `7694ef9c`.
While the first benchmark ran, the shared remote-tracking ref advanced to
`372a2639` (toolshape). Its source diff was applied with `git apply` after a
successful `--check`, excluding the test counter so xtask could regenerate it.

There were no overlapping conflicts: `frame.rs` retains both upstream's
addition and this lane's progress response; 63 other upstream paths are
byte-for-byte identical to `372a2639` (including new files). The proof is
`casstream-evidence/merge-forward.log`. New upstream files remain untracked
because the sandbox cannot update the Git index; they are present in the
working tree, not deleted. No golden was hand-merged. All gates and measurements
are rerun on this union source tree. The orchestrator must record the source
merge and commit; no permission escalation was attempted.

## Pre-merge gate details (superseded by the merged rerun)

- `cargo test --offline -p haider-rpc -p haider-client --all-targets`: PASS;
  307 reported passes including a subprocess helper result.
- `cargo test --offline -p haider-store --all-targets`: PASS; 293 reported
  passes and no ignored tests after the concurrent durability fixes.
- `cargo clippy --offline -p haider-rpc -p haider-store -p haider-core
  -p haider-client -p haider-daemon --all-targets -- -D warnings`: PASS.
- `cargo fmt --all -- --check` and `git diff --check`: PASS.
- `UPDATE_FIXTURES=1 cargo test --offline -p haider-cli --test
  turnhygiene_pin_tests
  provider_request_body_is_budget_independent_and_matches_the_golden_ledger`:
  PASS; the generated golden has no diff. No golden was hand-edited.
- `cargo run --offline -p xtask -- test-count --update`: **4,836 → 4,856**.
- Instruct-pipe runtime pin passed on macOS: **13,552 → 13,552 bytes**;
  the tool surface was unchanged, so no source pin adjustment was needed.
- Welcome feature-count pin updated **113 → 114**, with exact set membership
  retaining all prior features and adding the binary capability.

The first daemon/core test build failed on a test-only reference to a constant
not re-exported by haider-client; it now uses ClientConfig's actual default
request timeout. The next run passed all core suites and 1,050 daemon tests,
with only the old feature-count assertion failing; the count was updated and
the complete affected gate rerun. Both diagnostic logs are retained.

Pre-existing ignored tests remain unchanged: core's
`measure_cold_fold_after_several_compactions` (manual timing probe), and daemon's
`live_smoke_packaged_default_model_validates_a_real_key`,
`validator_ping_uses_real_adapter_and_stores_no_secret_in_errors` (live account
credentials), and
`host_loopback_gate_authenticates_and_advertises_capabilities` (host-bind gate).
No test was newly ignored, weakened or gated to pass this lane.

All Cargo invocations used the common environment law; daemon tests also used
`HAIDER_TEST_SIBLINGS_PREBUILT=1` after building haider and haiderd. The first
verified haiderd was 199,155,792 bytes, above the 10 MiB integrity floor. Disk
space was checked before each build and remained above 700 MiB.

## Independent verifier ledger

1. Concurrent CAS dedup could acknowledge before another publisher synced the
   target shard. Fixed streaming and legacy individual put/read success paths;
   deterministic paused-publisher tests pin independent directory sync.
2. Binary upload discarded CAS error retryability. Fixed message/retryability
   mapping and added parity test. Streamed write failures also reach the same
   store-health latch as ordinary puts.
3. The initial benchmark only compared an aggregate 8 x 33 MiB workload and
   probed the rejected 264 MiB prefix. Expanded evidence to actual 264 MiB
   JSON construction/send attempts, with explicit failure-versus-success
   labeling and a separate matched successful workload.

## CI error registry walk (classes 1–98)

The numbered classes are walked using the existing 968 registry audit mapping
in `docs/testing/v0.0.968/retainfix.md`. Executed checks are listed separately
from source inspection; Linux/Windows execution is not claimed on this macOS
host. Platform-independent codec/store and duplex tests run here; their other
platform behavior is by inspection.

| Classes | Result / evidence |
|---|---|
| 1–16 | Additive binary response and streaming APIs compile; old wire goldens and client suites pass. Final Clippy is recorded separately. Casstream adds no dependency; upstream toolshape's manifest/lockfile changes are imported unchanged. Binary Debug redacts content. |
| 17–19 | Disk jobs run outside store locks and the receive task's await path; owned staging drops on abort. No production unwrap/expect or unsafe block added. Formatting/diff gates recorded. |
| 20–24 | Test counter is regenerated with xtask; required stack/environment used. Private versioned journal format preserves old reads and adds no public event schema migration. No provider catalog/authority changes. |
| 25–30 | Actual daemon peak RSS and separately labeled Python client peak; atomic private CAS publication and same-handle verified reads. Windows IPC behavior is by inspection. Request timeout remains one existing total budget. |
| 31–40 | No casstream release, Android, dependency or runner changes. New modules are declared; typed errors preserve legacy retryability; public APIs do not depend on OS-specific types. |
| 41–48 | Hermetic short temporary profiles; actual IPC smoke and measured cases. No descriptor sweep, unsafe code, path walker or runtime-root rewrite. Test counter covers new modules. |
| 49–58 | Binary progress acknowledges staging only; final reply follows CAS durability. JSON fallback and platform byte goldens retained. Inclusive text threshold has explicit boundary tests. |
| 59–63 | No roster/UI changes. Keepalive receive loop remains active around CAS jobs. Named behavior pins and public reader return types compile. No platform archive helper added. |
| 64–68 | Prebuilt haider/haiderd used with sibling flag; haiderd exceeds 10 MiB. No raw-errno assertions or STT edits. Failed CAS writes preserve errors and store-health accounting. |
| 69–78 | No executable casing, workflow dispatch, route or release edits. Discovery disabled; dedicated profiles and owned process teardown. Hydration preserves all client event projections. Diff, formatting and merge-ref checks recorded. |
| 79–89 | No process-cancel/supervisor/OS-exit changes. Staging ownership is explicit on disconnect; deterministic paused-publisher tests do not substitute virtual time for real processes. Non-macOS behavior is by inspection. |
| 90–93 | No sparse-file or line-ending-sensitive source assertion. Performance fixture data is generated and hashed; getrusage/wait4 captures process peak RSS, with load validity in every sample. |
| 94 | Existing client request budget wraps admission + source reads + all ACKs + Finish; no reset per chunk. Benchmark diagnostic timeout is derived from 512 MiB / 4 MiB/s = 128 seconds. |
| 95 | Client heartbeat remains separate during file reads/ACK waits; daemon selects reads/liveness while the one bounded CAS job awaits blocking IO. Active-upload Ping exercised, deliberately stalled-job case by inspection. |
| 96–98 | Provider terminal reserve and route attribution unchanged; 968 resume opaque text and hydrated replay ordering pinned. CAS objects precede journal references; logical hook byte budgets retained. |

## Final merged gate

The gate uses explicit packages `haider-rpc`, `haider-store`, `haider-core`,
`haider-client`, `haider-daemon`, `haider-protocol`, `haider-provider`,
`haider-tools`, `haider-daemond`, `haider-tui` and `haider-cli`: this lane's five
crates plus six affected by the imported upstream change. It does not run
workspace-wide Clippy.

- Fresh sibling binaries: `cargo build --offline -p haider-cli -p
  haider-daemond --bins`: PASS (`final-binaries.log`).
- `cargo test --offline` with all eleven `-p` arguments and
  `--all-targets -- --test-threads=4`: PASS, exit 0; 291 result blocks,
  **5,119 reported passes, 0 failures, 12 pre-existing ignores**
  (`merged-tests.log`, `merged-test-summary.json`). Reported passes include
  subprocess/helper results; this is distinct from xtask's source baseline.
- `cargo clippy --offline` with the same eleven packages and
  `--all-targets -- -D warnings`: PASS, exit 0 (`merged-clippy.log`).
- `UPDATE_FIXTURES=1 cargo test --offline -p haider-cli --test
  turnhygiene_pin_tests`: PASS, all nine tests; no regenerated golden diff
  (`merged-golden-regeneration.log`).
- Instruct-pipe merged runtime pin: PASS, **13,552 → 13,552 bytes**. No
  source repin is required because the real value is unchanged.
- `cargo run --offline -p xtask -- test-count --update`: **4,836 original
  → 4,866 upstream → 4,886 merged** (20 lane tests; `test-count.log`).
- `cargo fmt --all -- --check` and `git diff --check`: PASS.

All log paths above are under `casstream-evidence/`. The gate uses the common
environment law and prebuilt-siblings flag stated above. The expanded gate's
12 pre-existing ignores are listed with their reasons in
`merged-preexisting-ignores.txt`: four from the original five-crate gate,
seven provider live-service/fixture-promotion checks and one manual macOS
screenshot. No lane change adds an ignore or platform gate. Existing UI timing
thresholds that require optimized builds are not claimed as release latency
validation by this dev-profile run.

The earlier successful gates are archived under
`casstream-evidence/premerge/`; the earlier 12-sample measurement is retained
in `measurements.json` with its own binary hash and is not substituted for the
merged measurement. `casstream-source-sha256.json` fingerprints the final lane
source, tests and harness; `merge-forward.log` proves the imported source.
The local fallback `origin/wave-970` was checked again after the merged gate
and still named `372a2639fd5ae971d6ecc29f61f7cbe1ad5a0551`.

## Verdict

The independent verifier returned **SHIP** after checking the merged source,
all gate results, source/binary hashes, all 12 valid measurements and this
report. There are no unresolved findings. Its ledger is **3 findings,
3 real, 0 noise**: concurrent dedup durability, CAS error retryability and
actual 264 MiB rejection measurements, resolved as described above.

Work remains uncommitted. The orchestrator records the Git merge and commit;
the supplied lane briefs and turnperf/turnperf2 evidence are not to be committed.

SHIP
