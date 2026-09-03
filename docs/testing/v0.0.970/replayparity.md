# replayparity — durable terminal byte parity (lane-970-replayparity)

## Disposition

The v0.0.969 mismatch is fixed by making the journal the single source of the
terminal envelope. New writers stamp `terminal_kind` and, where applicable,
`error_code` before serialization and commit. Live JSONL, session replay, and
`haider run --replay` then serialize the same retained `RawEnvelope`; the live
JSONL shape is unchanged, while the socket observes the announced additive
durable fields. The implementation and scoped verification support SHIP,
subject to the independent verifier result recorded below.

## Root cause and design

The old JSONL adapter decorated a cloned terminal envelope after the daemon had
committed a smaller `run_state` payload. Live therefore included
`terminal_kind`, while both replay doors correctly returned the smaller journal
row. Typed projections happened to agree, masking the raw mismatch.

The repair is at the journal boundary:

- `durable_run_terminal_v1` is the shared terminal classifier for `success`,
  `failure`, `budget`, `cancellation`, `timeout`, and `provider_error`.
- Both store append seams collect exact-run durable facts, preserve the first
  blocking cause, stamp the terminal fields before serialization, and publish
  the committed envelope. Direct fork-boundary cancellation rows are also
  typed before insertion.
- Deadline precedence uses an explicit durable deadline fact or the committed
  cancellation-intent time compared with the accepted absolute deadline. It
  does not infer timeout from a later terminal commit timestamp; an interrupt
  accepted before the deadline remains cancellation even if settlement is
  later.
- The client and live JSONL adapter consume the retained fields. A forced local
  outcome yields to a differently classified retained terminal, so a durable
  race is decided by the journal.
- Replay does not rebuild a retained terminal. Its compatibility classifier is
  used only when a pre-v0.0.970 row has no `terminal_kind`, and never rewrites
  the journal. Any retained string kind—known or future—and its optional string
  `error_code` pass through unchanged. Legacy `run_failed` classification
  requires true global-sequence adjacency, not adjacency in a run-filtered
  vector.
- Replay integrity now requires exactly one structurally typed terminal and
  validates the state/error shape of current known kinds while preserving an
  additive unknown future kind.

## Literal byte pin and normalization policy

The pin at `crates/haider-cli/tests/cli_tests.rs:1304-1487` retains the original
live JSONL line bytes, extracts literal object slices from the replay document's
`events` array, and compares the vectors without parsing and reserializing the
objects. It covers every run-correlated durable row in a text turn and a tool
turn, plus provider error and ordinary cancellation. The missing-credential
test adds a generic failure case, and the timeout test adds the caller-deadline
case. Store/protocol/client tests cover budget classification, blocked
cancellation, permission conflicts, effect uncertainty, and cause precedence.

There are **zero normalized fields inside `RawEnvelope`**. The contract declares
all of these committed: schema version, event id, sequence/cursor coordinates,
session/run/device ids, authority epoch, worker generation, commit timestamp,
render targets, the complete payload, key order, and JSON encoding.

Three objects/rows are outside the comparison; these are exclusions, not field
normalizations:

1. The initial live `accepted` object is not a `RawEnvelope`.
2. Replay document fields such as `schema`, `integrity`, `equivalence`, and
   `provider_requests` are derived container metadata, not event fields.
3. Live session-stream rows with no `run_id` or a different `run_id` are not in
   the run replay projection. Selecting rows with the source run id implements
   the contract's run boundary; selected envelope bytes are left untouched.

The old oneshot golden and SIGKILL harness normalizations that removed terminal
fields were deleted. Both now observe/compare the retained shape directly.

## Contract and schema ledger

`docs/event-schema-changelog.md` now announces the additive retained terminal
fields and states both governing laws: durable event shapes are additive-only
and announced, and derived/presentation-only fields never sit inside durable
payloads. `docs/jsonl-run-contract-v1.md` and
`docs/automation-contract-v1.md` now say that live and replay serialize one
retained terminal envelope.

The requested AHRB inventory is recorded with schema notes:
`session_state`, `usage`, `effect`, `node_committed`,
`headless_run_configured`, `session_renamed`, and
`process_signal_recorded`. `node_renamed` is explicitly recorded as an opaque
AHRB-observed kind because neither the v0.0.969/v0.0.970 sources nor searchable
tag history contain a typed producer/decoder; replay must preserve it without
inventing a schema. The changelog completeness test now inventories
`SessionConfigEventPayload` too.

## Verification

All commands used the lane's mandated environment; every Cargo invocation was
preceded by `df -m /` and remained well above the 700 MiB stop floor.

- Final affected packages:
  `cargo test -p haider-protocol -p haider-store -p haider-client -p
  haider-cli --locked` — pass, every unit/integration/doc-test suite, after the
  final edits.
- Focused raw parity:
  `live_jsonl_and_durable_replay_are_byte_identical_for_text_tool_error_and_cancel`,
  `anthropic_missing_credential_exits_65_without_network_access`, and
  `run_jsonl_timeout_has_one_distinct_timeout_terminal` — pass.
- Focused legacy/source-of-truth pins: success/failure/cancellation upcast,
  global adjacency, first blocker, unknown retained kind, and known retained
  terminal without reclassification — pass.
- `cargo clippy -p haider-protocol -p haider-store -p haider-client -p
  haider-cli --all-targets --locked -- -D warnings` — pass.
- `(cd scripts/qa-gate && bash run.sh test)` — 64/64 pass. There is no root
  `run.sh`; the maintained requested entry point is
  `scripts/qa-gate/run.sh`.
- `cargo run -p xtask --locked -- test-count` — 4,440 tests, baseline 4,440.
  The baseline advanced monotonically from 4,430 for ten new tests; no test
  attribute was removed, ignored, or platform-gated.
- Changed Rust files pass `rustfmt --check`; `git diff --check` passes.
  `cargo fmt --all -- --check` reaches only a pre-existing unrelated formatting
  delta in `crates/haider-tui/tests/tpsfix_widget_tests.rs:349` and `:363`.
  This lane did not edit that parallel-owned file.
- Final development binaries exceed registry #64's 10 MiB floor:
  `haider` 103,201,440 bytes and `haiderd` 185,286,128 bytes.

### SIGKILL matrix

`python3 scripts/qa-gate/turnperf_sigkill_matrix.py --bin-dir target/debug
--output /tmp/replayparity-sigkill-final.json` passed 47/47 discovered journal
and provider boundaries for text and tool turns, with zero failures. It ran the
final binaries:

- `haider` SHA-256
  `d2c8adff339a2baa118ac61910b627e8b38de69cb8398a9d0fdab3de74575304`
- `haiderd` SHA-256
  `3457b67556d1da0681d55e1388eab324f02926549c29470958d3cdf0973016e5`

The 47 terminal results were 8 success and 39 failure. Of the failure cases,
all 11 probed-then-abandoned recovery cases carried exactly
`terminal_kind:"failure"` and `error_code:"input_required"`; the remaining 28
carried the expected internal failure classification. The matrix now compares
the event objects without deleting terminal fields.

### Frozen v0.0.969 goldens

Both focused tests and the final full package suite passed without blessing or
fixture changes:

- `oneshot_run_golden.jsonl` SHA-256
  `d025fb4a5637af7a1a89db873cd98fcda4c4712fef609df2da53296025495b55`
- `turnhygiene/provider_request_no_budget.json` SHA-256
  `9456adc6a592b126eda1e227e6c949fb198c40ac3dec7c6e88b0ef169f20e4cc`

## Citation audit

- The brief's behavioral references to the terminal enum and terminal contract
  are correct, but their old line numbers drifted. The current terminal enum is
  `crates/haider-client/src/headless.rs:482`; the normative terminal section is
  `docs/jsonl-run-contract-v1.md:87-115`.
- The old live-only terminal writer citation is obsolete by design. The shared
  classifier is now `crates/haider-protocol/src/headless.rs:175-249`, store
  stamping begins at `crates/haider-store/src/event_store.rs:21010`, and the
  legacy-only replay upcast begins at `crates/haider-cli/src/run.rs:1150`.
- The AHRB `error/cancel` shorthand maps to the actual contract values
  `failure` and `cancellation`.
- `node_renamed` was not found in the current tree or tagged history; treating
  it as an existing typed producer would have been wrong, so its ledger entry
  is intentionally opaque.
- `LANE-COMMON.md` names base `8952219`; this worktree's current uncommitted
  parent is `0cb2cfb`. No rebase was performed.

## CI error-registry walk

- #20/#21/#54: test count only increased (4,430 to 4,440); zero test attributes
  were removed and no acceptance test was weakened or ignored.
- #64: both final binaries are greater than 10 MiB (sizes and hashes above).
- #94: no arbitrary product sleep/deadline was added. Timeout classification
  uses the already accepted absolute request deadline and the durable
  cancellation-intent/deadline fact; the regression test explicitly pins an
  interrupt-before-deadline settling later as cancellation.
- #95: no new open-connection wait was introduced.
- During implementation the raw pin initially selected session-wide live rows;
  it was corrected to select the source run while retaining original line
  bytes. The SIGKILL proof then exposed later effect facts replacing the first
  blocking cause; the store/compat reducers now preserve the first exact-run
  cause. A filtered-vector adjacency assumption was replaced with global `seq`
  adjacency. These failures are now named mutation pins.
- The first QA invocation tried the user-spelled root `bash run.sh test` and
  failed because this repository has no root script; rerunning the command from
  `scripts/qa-gate` passed 64/64.
- Scoped Clippy found four collapsible-match warnings in the new reducers; they
  were corrected and the final scoped all-target run is clean.
- Full workspace formatting reports only the pre-existing parallel-owned
  `tpsfix_widget_tests.rs` delta noted above; changed-file formatting and diff
  integrity pass.
- `crates/haider-daemon/src/oauth.rs` and `oauth_tests.rs` are byte-untouched.
  The supplied `LANE-COMMON.md`, `LANE-BRIEF-replayparity.md`, `turnperf/`, and
  `turnperf2/` evidence remains unmodified and untracked. All lane changes are
  intentionally uncommitted for the orchestrator.

## Independent verification

Two independent post-report re-audits returned SHIP:

- The source/design verifier confirmed literal zero-normalization parity,
  before-commit stamping, retained known/future terminal authority, isolated
  legacy upcasting with global adjacency, the final hashes/matrix/goldens, and
  untouched OAuth/evidence files.
- The contract verifier independently confirmed all requested terminal paths,
  both append seams and fork boundaries, deadline/blocker/permission/effect
  mutation pins, the complete AHRB schema inventory, final gate evidence, and
  the 4,440-test monotonic ledger.

## Verdict

SHIP
