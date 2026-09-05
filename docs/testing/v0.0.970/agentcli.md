# v0.0.970 agentcli

Public surface: `haider agent spawn|list|message|cancel|wait` and
`haider workflow run|status|list`. The normative JSON and exit contracts are
in [automation-contract-v1.md](../../automation-contract-v1.md#public-agent-and-workflow-commands-v00970);
[client-contract-v1.md](../../client-contract-v1.md#22-public-agentworkflow-cli-and-operator-authored-delegation)
documents the additive native protocol bridge.

Final validation passes: **15/15 T0, 0 FAIL, 0 SKIP, 0 ENV_BLOCKED**, with
15/15 status-owned daemon cleanup proofs and 105/105 natural zero-status TUI
exits. The 17-crate Rust gate and strict nine-crate Clippy pass. Source changes
remain uncommitted on `lane-970-agentcli` for the orchestrator.

## Implementation and scope

The existing `spawn_subagent` entry was a provider tool, not a standalone RPC.
An optional `HeadlessRunSpecV1.agent_spawn` pin, guarded by `agent_cli_v1`,
uses `headless.run.start` to durably accept the operator request. The parent
invokes the canonical broker/deferred-child path with zero provider requests.
Actual child identities, original `ChildResult`, and terminal sequence numbers
come from session journals. Follow-up message runs are explicitly identified
as child-journal reports, without reusing the original parent report.

The new CLI uses the current-thread runtime, bounded journal pages, continuous
attachment draining, and the existing long-lived Ping/Pong transport. Command
timeouts observe work; the daemon retains accepted-run and child cleanup
ownership. Workflow selectors and triggers go through native admission. Main
catalog templates with human confirmation cannot run as autonomous children.
A child's `request_input` without a default yields durable
`no_human_available`, closes its menu, and continues to its eventual result.

Minimal adjacent changes are the typed headless pin, actor direct-delegation
entry, daemon pin-to-harness binding, public-child interaction inheritance,
and restart reconciliation. Public interaction inheritance does not grant
model-created descendants standalone workflow admission. The parent still
passes its finalization guard. Protected OAuth sources are untouched. No
provider transport, budget ceiling, supervisor retention policy, or prompt/tool
catalog is replaced.

## Upstream and citation audit

Read the supplied lane common/brief, `969-common.md`, and both turnperf lens
packs before implementation. Historical absolute line citations in the lens
tables drifted; constructs were located in this worktree before use. Relevant
checks: CLI runtime selection in `crates/haider-cli/src/main.rs`, broker/tool
completion and finalization in `crates/haider-core/src/actor.rs`, child binding
in `crates/haider-daemon/src/delegation.rs`, harness wiring in `worker.rs`, and
recovery in `turn_recovery.rs`. The brief's assumption that `spawn_subagent`
was already an RPC was wrong; the existing tool remains the execution authority.
No performance estimate from the lens tables is claimed as a measurement.

The lane HEAD is `9270f40286d3181fd22c20600b4ae4f9586b8c1d`. Fetch and merge
in the original worktree were denied because its external Git metadata is
read-only. A writable temporary clone successfully fetched `origin wave-970`
and ran `git merge --no-commit origin/wave-970`: upstream is that same HEAD,
with “Already up to date.” No incoming content or conflicts required copying.
The source changes and this report remain uncommitted for the orchestrator.

## Verification

The public black-box module `crates/haider-cli/tests/agent_cli_tests.rs`
passes **11/11** against freshly built sibling `haider` and `haiderd`:

| Behavior | Named test |
| --- | --- |
| spawn/list/wait, exact IDs, one ChildResult, child=1 / parent=0 requests | `agent_spawn_list_wait_publish_durable_child_result_and_exact_identities` |
| message receipt and actual follow-up run report | `agent_message_to_idle_child_returns_new_run_delivery_receipt` |
| wait timeout preserves IDs and continues work; cancel and exit 130 | `agent_wait_timeout_observes_without_cancelling_then_cancel_is_terminal` |
| no_human_available resolves and provider continues | `agent_headless_input_is_rejected_and_provider_continues_to_child_result` |
| failed child / durable red report / exit 1 | `agent_wait_failed_child_returns_durable_red_report_and_exit_one` |
| workflow list/run/status and real child graph | `workflow_list_run_status_expose_actual_child_graph_activation` |
| no-spawn leaves absent daemon | `agent_and_workflow_no_spawn_leave_fresh_profile_without_daemon` |
| malformed/missing target JSON errors | `agent_missing_target_and_malformed_cli_are_typed_errors` |
| prompt alias, working directory, and literal `--help`/`--json` after `--` | `agent_spawn_prompt_flag_and_cwd_are_public_noninteractive_inputs` |
| provider default is resolved by the daemon | `agent_spawn_provider_only_resolves_model_through_daemon_session_authority` |
| provider without a default preserves native refusal | `agent_spawn_provider_without_published_default_retains_native_rejection` |

Core direct-spawn tests pass **4/4**, including red-report failure,
finalization refusal, and recovery-store failure cleanup. The daemon tests
pass all four added boundaries: partial establishment before acceptance,
accepted child before broker completion, abandoned parents with/without
accepted child turns, and human-gated catalog refusal. The restart tests also
assert that inherited public interaction does not bypass descendant workflow
slot requirements. Protocol/RPC tests pin legacy omission, new-pin round-trip,
and mandatory feature negotiation.

The T0 `t0.agent.spawn_result` check passes on the final rebuilt implementation:
`spawn_exit=0 wait_exit=0 child_state=done child_result_seq=33 terminal_seq=20`;
status-owned PID 72119 reports `stopped_cleanly` and `alive_after=false`.
Its sourced BudgetSum is 288,000ms:
`2 × (30,000 startup + 60,000 request + 10,000 observation + 2,000 terminal)
+ 60,000 cleanup status + 20,000 stop + 2,000 stop grace + 2,000 PID observation`.
The full inventory budget becomes 29,892,500ms. This check is untimed correctness
evidence, not a latency benchmark. See
[the retained row](agentcli-gate/final-input-mirror/t0.agent.spawn_result.row.json)
and [binary hashes](agentcli-gate/final-input-mirror/frozen-binaries.json).

The initial full T0 pack was 14/15 PASS, with 15/15 daemon cleanup proofs.
The only failed check was palette activation: its monitor oracle missed a real
card, and explicit `/update` failed clean process exit. The narrow monitor
oracle now requires actual controls plus content, with positive and
flash/stale/incomplete-card negative tests. Python QA self-tests pass 70/70.
A subsequent unchanged-budget palette run confirms monitors PASS, but retains
the update failure and a one-off rollback attach timeout. A separate fresh
six-command history sequence passes 6/6 including rollback; its cause is not
claimed from connection-retirement logs. All initial/focused evidence remains
in `agentcli-gate/`; final disposition follows below.

After the updater fix, the next whole palette run proves `/update` PASS with
natural PID 66956 status 0 and daemon 60715 cleanly stopped. All 51 TUI children
exit naturally, but undo/redo/rollback hit the original composer-readiness
deadline. Subsequent direct raw-byte audit corrects the initial diagnosis:
each failed PTY contains `message haider`, but the probe's BEL-only OSC regex
erases that text across an ST-terminated control span. See the retained
`final-fixed/raw-byte-audit.json`; the earlier claim of absent raw bytes was
wrong. A focused full repaint shows the exact composer at
2.657s and all six history actions pass. The shared readiness helper now
requires that exact composer in a new full frame inside the original 25s
absolute boot deadline. Three regression cases reject stale placeholder text,
session-only evidence, and extending the remaining budget. Python self-tests
pass 73/73 at this point; independent review accepts the correction. A fresh
full T0 run exercises every check sharing this helper.

That full run finishes 14/15 PASS, with all 15 daemon cleanup proofs and no
skip or environment-blocked row. Its sole failure is `/sessions`: the actual
composer contains `/sesions`. Direct raw inspection confirms that the requested
session attached successfully; the launcher-only inference from ANSI-stripped
output was also wrong. The native input mirror can mistake a delayed local
echo for a foreign edit before learning its own owner, overwriting a newer
keystroke. The failed report remains in `agentcli-gate/final-verified/`.
The OSC parser now recognizes both BEL and ST without consuming a later
painted frame; its three added regressions pass, and Python self-tests are
76/76 ([self-check log](agentcli-gate/python-selfcheck-osc.log)); the existing
shared probe tests also pass 2/2 ([probe log](agentcli-gate/probelib-tests.log)).
Corrected raw/text counts are retained in
`final-fixed/osc-corrected-audit.json`.

The input fix adds optional `caller_owner` to the existing
`SessionSurfaceWatching` acknowledgement, stamped with the daemon's actual
caller identity. The TUI learns it before releasing the existing adoption
barrier, and correlates that acknowledgement to the requested session and
connection epoch. Delayed own echoes then preserve newer text, cursor and
attachments; a foreign owner's matching text/revision cannot rename the
caller. Owner-less legacy responses keep their prior compatibility behavior.
This adds no wait, queued text history, typing delay or observation threshold.
The minimal adjacent changes are RPC response shape, daemon watch response,
client response destructuring, and TUI link/driver plus regressions. No
durable event kind changes. Four new regressions cover delayed first self-echo,
foreign collision, stale watch epoch, and additive wire compatibility; the
existing daemon watch test compares the ack's identity to its actual delta.
Independent review accepts this implementation; all six affected crate reruns
pass, including all four added regressions.

The complete gate ran all 17 workspace crates with scoped `-p` commands and
`--no-fail-fast`, then reran affected crates after each subsequent correction.
Final logs sum to **5,430 libtest passes, 0 failures, 13 pre-existing ignores**;
these include nested subprocess summaries and modules included by multiple
integration targets, rather than unique source tests. The final CLI suite is
748/748, including the public 11/11 module and all three updater tests in each
applicable target. See [per-crate totals](agentcli-gate/rust/final-test-totals.json)
and [post-update command exits](agentcli-gate/rust/postupdate-ledger.json).
The subsequent delimiter correction's complete CLI/lint rerun also exits 0;
see [its command ledger](agentcli-gate/rust/literal-flags-verified-ledger.json).
The final owner correction reruns daemond, CLI, RPC, client, daemon, and TUI in
full; every command exits 0, followed by strict nine-crate Clippy, recount,
formatting and unsafe gates. See [the final ledger](agentcli-gate/rust/input-mirror-ledger.json).
The final daemon compatibility rerun retains all prior byte/feature assertions
and checks the new feature's withholding boundary. The advertised feature
count is 116 → 117.

Every crate uses `cargo test -p CRATE --locked --no-fail-fast --
--test-threads=4`. Build-capable commands use `RUST_MIN_STACK=8388608`,
`HAIDER_DISCOVERY_DISABLED=1`, `HAIDER_TEST_DEVICE_NAME=test-mac`,
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_BUILD_JOBS=2`, and
the task-owned temporary `CARGO_TARGET_DIR`; real-daemon tests also use
`HAIDER_TEST_SIBLINGS_PREBUILT=1 TMPDIR=/tmp`. Disk headroom is checked before
each build with the 700 MiB stop floor. Both siblings were rebuilt before
the final tests; `haiderd` is 201,478,752 bytes, exceeding the 10 MiB guard.

Strict Clippy with `--tests --locked -- -D warnings` passes for CLI, client,
core, daemon, daemond, protocol, RPC, store, and TUI. Final formatting and whitespace
checks pass. The unsafe gate remains production=189/test=20. The authoritative
test-count tool updates **4966 → 4995**. The final upstream ref guard still
matches `9270f402`. No release-profile timing claim is made from debug PTY runs;
the inherited package version remains 0.0.969 for the orchestrator's release bump.

The broader QA exit finding required an adjacent fix in the existing explicit
update checker (`update/tui.rs`, `update/mod.rs`, `update/discovery.rs`). A first
detached-thread candidate was rejected during review: it let the CLI exit but
could leave curl running. The replacement retains blocking-worker ownership,
cancels and reaps the actual curl child when the TUI closes, and joins its
watcher. Existing download/install and background-check paths are unchanged.
The three stalled-loopback regressions pass: closure during a response,
response-size refusal, and cancellation before starting a request. The two
live-process cases require the same spawned PID's real SIGKILL wait receipt,
watcher join, and TCP closure within the existing absolute 2.5s TUI exit budget.
No permission-denied PID lookup is interpreted as process death. Independent
review accepts the replacement's cleanup ownership and authoritative wait proof.

`provider_request_no_budget.json` was regenerated using the existing
`UPDATE_FIXTURES=1` test mode and is byte-identical to upstream. All existing
JSONL goldens pass unchanged. The measured instruct-pipe pin remains
**13,552 → 13,552 bytes** (full prefix=19,736; registered=29; advertised=26;
native descriptions=690); the exact named pin test executed once and passed
with `--nocapture`.
Final author review also fixed help/JSON pre-scans reading beyond the CLI's
`--` option terminator. The real-daemon prompt test now exercises both literal
flag prompts to completion. Its first extension reused one finite fake script
for three runs and exhausted it; each independent case now has a fresh script,
profile, and explicit daemon cleanup. The failed attempt is retained without
changing the observation deadline or result assertion.
Executed on macOS arm64; Windows/Linux behavior is by inspection only. The
new updater process tests are Unix-only because the existing transport executes
`/usr/bin/curl`; every public agent/workflow black-box test runs without a
platform skip. No failing assertion or deadline was weakened to obtain green.

## Final real-daemon gate

The final full normal T0 pack passes **15/15**, retaining the original
discovery order, action oracles, typing cadence, deadlines and cleanup law.
The report schema validator exits 0; measurement eligibility is accepted with
no rejection reason. This remains correctness evidence, with no release
latency claim. The complete report is
[qa-gate-t0-Syeds-MacBook-Air.local-20260905T142423Z.json](agentcli-gate/final-input-mirror/qa-gate-t0-Syeds-MacBook-Air.local-20260905T142423Z.json).

`/sessions` now passes with natural PID 81200 exit 0 and exact typed-command
evidence (eight `/sessions`, zero `/sesions` occurrences). `/update` passes
with natural PID 83220 exit 0. All 105 recorded TUI children have real wait
status 0, balanced alternate screens and no panic; all 15 checks retain their
own positive no-orphan evidence. Palette daemon 79485 and parity daemon 83326
both report `stopped_cleanly` and `alive_after=false`. See
[the completion audit](agentcli-gate/final-input-mirror/completion-audit.json),
[typing proof](agentcli-gate/final-input-mirror/sessions-typing-proof.json), and
[raw exit receipts](agentcli-gate/final-input-mirror/pty-exit-trace.jsonl).

The final frozen/current binaries match SHA-256
`89a3dea016ea1e9cb3d5b7714e68852bd3a80aaf405affd87d02e5f464c9cef3`
(`haider`) and
`c25116657bbcb1b34078ba0dcfa55f134b67d39adabfb4213bff9886c50668dd`
(`haiderd`). All 49 hashes in
[the source manifest](agentcli-gate/source-sha256.json) match the final tree.
The supplied common/brief/lens packs are excluded from that manifest and have
not been committed. Earlier failed reports, rejected updater candidate evidence,
and corrected diagnoses remain retained in [the evidence index](agentcli-gate/README.md).

## Verifier value

Fourteen findings have changed code, tests, or verdict: scope(1), initial engine(3),
CLI(3), abandoned-parent recovery(1), monitor oracle(1), updater exit(1), and
updater ownership/watchdog review(2), plus composer readiness/parser(1) and
input-mirror ownership(1).
No finding has been rejected as noise. The author-found option-terminator fix
is not attributed to the independent verifier tally.

| Finding | Change |
| --- | --- |
| The requested spawn RPC was actually a provider tool. | Added the typed headless bridge over the canonical tool authority. |
| A crash during partial child establishment could lose progress. | Reconcile the durable row and accepted turn without duplicate children. |
| Inherited headless interaction could authorize descendant workflows. | Split public interaction from direct operator admission authority. |
| Direct delegation skipped the parent's finalization guard. | Retain the guard before parent completion. |
| Observation failures discarded known coordinates. | Preserve session/run/child identities in the error result. |
| Catalog IDs were not native workflow selectors. | Normalize to the native workflow reference. |
| Provider-only requests failed before default-model resolution. | Resolve the actual pair through session-create authority. |
| Abandoned parents could strand accepted or row-only children. | Cancel and terminalize both recovery boundaries. |
| The monitor palette oracle missed the actual card. | Require native controls/content and reject incomplete/stale cards. |
| Explicit update checks blocked TUI process exit. | Cancel in-flight release discovery on receiver closure. |
| A detached updater candidate could orphan curl. | Keep ownership through kill, real wait receipt, and watcher join. |
| The updater test used an unsourced watchdog. | Derive polling and shutdown checks from the existing 2.5s exit budget. |
| The attach oracle used stale paint history and mishandled OSC terminators. | Require a fresh exact composer and preserve text following BEL/ST control spans. |
| A delayed own input echo could erase newer typing. | Establish authoritative caller identity at the epoch-fenced watch adoption barrier. |

The independent final audit returns **SHIP** with **14 findings, 14 real, 0
noise**. It independently confirms the final 15/15 T0 report, all 15 daemon
cleanup proofs, 105 unique naturally exited TUI processes, matching frozen
binary hashes, all 49 source hashes, and the Rust gate totals. No blocker
remains. All lane changes and supplied evidence remain uncommitted.
