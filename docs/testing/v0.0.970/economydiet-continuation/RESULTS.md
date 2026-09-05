Continuation of economydiet commit `55982a52019ef8d2f9e330ee61dea93e9f8aa3e7`
onto the already-started merge of wave-970
`9270f40286d3181fd22c20600b4ae4f9586b8c1d`. Only working files were edited;
Git metadata, the index, and commits were not changed. The orchestrator owns
staging and committing the resolved tree.

| File | Resolution and verification |
| --- | --- |
| `crates/haider-daemon/src/permissions_core_tests.rs` | Retained v5's actual eight-tool catalog measurement, native semantics/schema pins, zero-manual assertion and half-baseline floor. Restored incoming native-description/count diagnostics. Runtime reports 30 registered, 8 advertised, 606 policy bytes, 1,490 native-description bytes, and 5,670 pipe bytes. Historical pipe: **13,552 → 5,670**; merge-specific pin: **5,670 → 5,670**, so no fabricated re-pin. Incoming old 26-tool/full-manual arithmetic does not apply to the intentional default tier. |
| `crates/haider-cli/tests/fixtures/oneshot_run_golden.jsonl` | Regenerated using `HAIDER_ONESHOT_GOLDEN_UPDATE=1`; all 28 records reviewed. Against wave, only line 23 changes system version v4 → v5. |
| `crates/haider-cli/tests/fixtures/turnhygiene/run_jsonl_text_turn.jsonl` | Regenerated using `UPDATE_FIXTURES=1`; all 28 records reviewed. Against wave, only line 23 changes system version v4 → v5. |
| `crates/haider-cli/tests/fixtures/turnhygiene/run_jsonl_tool_turn.jsonl` | Regenerated using `UPDATE_FIXTURES=1`; all 59 records reviewed. Against wave, only line 54 changes system version v4 → v5. Durable receipt contents remain intact. |
| `crates/haider-cli/tests/fixtures/turnhygiene/provider_request_no_budget.json` | Regenerated through its test; byte-identical to committed economydiet. Against wave, only system messages and the eight-tool catalog change, as intended. |
| `crates/haider-daemon/src/connection_tests.rs` | Preserved incoming custom-provider feature and **115 → 116** pin. The test reports exactly 116 and passes; no additional drift or `left:` failure occurred. |
| `test-baseline.txt` | Preserved orchestrator's **4,969 → 4,991** update. Fresh `cargo run -q -p xtask -- test-count` reports 4,991 with baseline 4,991, exit 0. |
| `scripts/qa-gate/CI_REGISTRY_WALK_QAGATE3.md` | Preserved the orchestrator's resolved registry walk without editing. |

No golden was hand-merged. The generation commands and exit codes are retained
in `golden-*.json`, `golden-*.log` and `golden-*.exit`. The comparison covers
all 116 records against both parents, with no difference from committed
economydiet. Every changed line against their common base `368f093c` is also
recorded in `golden-review.json` and the accompanying `.diff` files: ceilingdecl
adds the durable workspace-before pair and its resulting sequence, event,
item and workspace-revision offsets; v5 changes the system-version field.
Journalview's narrative, provider correlation and provider-round terminals
remain intact. Customprov's additive handshake/discovery implementation and
ceilingdecl's cap/receipt behavior remain in the merged tree and full gate.

Fresh AHRB captures used the same unmodified adapter, quick economy task,
eight primary requests, 17 tool results and reference tokenizer. The adapter
SHA-256 remains `38f9a9c622ae69523231e0dfd99a7f97d9c6a0b8c350e6f14d21425e6504554d`.
The before side uses the original frozen upstream baseline; the after side
uses binaries freshly built from this merged working tree, copied to
`/private/tmp/economydiet-continuation-bin` and hashed in `binaries.json`.
The merged daemon is 201,508,944 bytes, exceeding the 10 MiB requirement.

| Measured metric | Before | Merged after | Reduction |
| --- | ---: | ---: | ---: |
| AHRB fixed overhead, reference tokens | 14,222 | 6,031 | 57.59% |
| System-side reference tokens | 5,481 | 605 | 88.96% |
| Tool-side reference tokens | 8,741 | 5,426 | 37.92% |
| Canonical stable-prefix bytes | 16,698 | 7,062 | 57.71% |
| Model envelope overhead, bytes/result | 1,099.35 | 84.12 | 92.35% |
| Task-proven wasted tool calls | 0 | 0 | unchanged |

The merged fixed overhead is below the 7,111-token acceptance ceiling.
`economydiet_measure.py` independently cross-checked every request's reference
token count, the exact common prefix and the context slope against AHRB.
System and tool blocks remain byte-identical across all eight requests.
Both official AHRB completion labels remain `terminal-without-effect`, the
previously documented adapter limitation; the independent captured-call,
model-readback and external-filesystem join passes all seven effect checks
on both sides. No claim about real-model success is made.

The required test command was exactly
`cargo test -q --workspace --no-fail-fast`, under the complete ENV LAW and
`HAIDER_TEST_SIBLINGS_PREBUILT=1` after fresh sibling prebuild. It exited 0:
**5,397 top-level tests plus 12 nested subprocess probes, zero failures,
13 existing ignores** (5,409 passes including nested probes). No tests were
weakened, ignored or platform-gated. `workspace-totals.json` separates nested
filtered reexecutions from top-level result blocks. Formatting and the
source/test-baseline whitespace check also pass. Runtime verification is
macOS arm64; Linux and Windows are by inspection.

The lane-common/brief and turnperf/turnperf2 inputs were read and left
unchanged by this continuation. Historical lens line citations drifted;
the relevant constructs were located again: compatible provider-view
assembly is `openai.rs:1938`, generic request serialization `openai.rs:1817`,
request writer allocation/growth `lib.rs:876/897`, and request-attempt commit
`actor.rs:4432`. Round 2's correction supersedes Round 1's device-sync cost
interpretation; no historical latency estimate is represented as a fresh
merged measurement.

The second required gate was exactly
`cargo clippy --workspace --tests -- -D warnings`, under the same ENV LAW.
It exited **0**, completing in 2m53s. Its exact invocation and environment
are in `workspace-clippy.json`; the log and exit file independently record
success. The independent verifier reviewed both final gates, all regenerated
goldens, the measured prefixes, pins and recount and returned unconditional
SHIP with no findings.

VERIFIER: findings=0 real=0 noise=0 — no findings
SHIP
