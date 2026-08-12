# Speed-optimization analysis

Date: 2026-08-12  
Branch baseline: `speed-opt`, v0.0.907  
Measurement environment: macOS sandbox, `CARGO_INCREMENTAL=0`,
`HAIDER_DISCOVERY_DISABLED=1`; release binaries built with
`cargo build --release -p haider-cli -p haider-daemond`.

This document records the Phase 1 measurements before optimization. Phase 2
results and verification are appended below as they are completed. Debug-build
timings are intentionally excluded.

## Executive findings

| Area | Measured current cost | Finding | Risk | Phase 1 verdict |
| --- | ---: | --- | --- | --- |
| Cold launch | exactly 40 x 25 ms (about 1.00 s) after successful attach to a newly spawned daemon; daemon initialization itself was 20.5-22.3 ms warm | The launcher waits for its own healthy daemon child as though it were a race loser. OSC 11 theme detection separately costs about 113-114 ms when the terminal does not answer. | Low for the PID-qualified wait fix; high for the existing pre-exec fd safety sweep, which remains untouched | Optimize the proven 1 s wait now; defer theme-policy changes |
| Journal append | median 17,027 individual append calls/s (58.7 us/event); batches of 100 reached 83,102 events/s | One SQLite transaction/commit occurs per `append` call, not per envelope in a caller-provided batch. Receipt and accepted-turn envelopes already share one transaction. | High: durability and ordering law | Defer additional fsync batching |
| Prompt compile | 0.387 ms at 100 events, 3.985 ms at 1k, 33.570 ms at 5k (p50) | Every turn rereads the durable journal. Tree ancestry assembly repeatedly scans the full envelope vector and becomes superlinear. | Medium-high: exact branch/agent/compaction projection semantics | Optimize now with an incremental exact-key cache and linear ancestry indexing |
| HTTP clients | approximately 1.28 us to construct versus 3-4 ns to clone in a 10k-construction CPU benchmark | Account-provider resolution constructs adapters per turn; those adapters construct new reqwest clients, discarding connection pools. Some special clients have origin/DNS security policies that are semantically load-bearing. | Medium: proxy/timeout/origin/DNS policy preservation | Optimize ordinary provider reuse now only where the full adapter configuration key is exact; retain policy-specialized clients |
| TUI transcript | 10k rows: 77.25 ms p50 / 78.66 ms p95 at tail; 64.33 / 64.91 ms mid-scroll | Every draw formats, parses, wraps, clones, and lays out the complete transcript before applying viewport scroll. Tail rendering is more than 2.3x the 33 ms frame budget. | Medium-high UI mechanics; no durability/protocol risk | Optimize now with cached entry layouts and viewport virtualization; plain renderer unchanged |

## Measurement method and detailed findings

### Cold launch

The release launcher path was inspected and temporarily instrumented at phase
boundaries. A fresh isolated profile cannot complete a host-style launch in
this sandbox because Unix socket binding fails with `EPERM`, but all work
before that bind and the client-side post-attach behavior were measurable.

Warm daemon-startup samples, cumulative from process entry:

| Boundary | Four warm samples (ms) |
| --- | --- |
| profile/store lock | 4.242, 4.632, 4.223, 4.905 |
| SQLite store open | 11.312, 10.657, 11.311, 12.005 |
| generation recovery | 11.570, 10.942, 11.613, 12.257 |
| effect recovery | 11.612, 10.984, 11.652, 12.297 |
| turn recovery | 11.633, 11.003, 11.672, 12.317 |
| account/provider initialization | 21.600, 21.989, 20.547, 22.246 |

The first cold-cache observation reached store open at 57.089 ms and account
initialization at 70.239 ms. No `sysinfo` dependency or launch-time sysinfo
scan exists in the workspace.

The reported one second is instead deterministic in the client launcher:
after a successful attach, any retained `Child` is polled 40 times with a
25 ms delay so that a daemon candidate which lost a concurrent store-lock
race can be reaped. The same loop also runs when that child is the authenticated
daemon currently serving the socket. Peer credentials already expose the
serving PID, so the proposed fix is to perform the loser grace only when the
child PID differs from the socket peer PID. This does not alter the known
fd-hygiene `pre_exec` close sweep or any daemon safety property.

OSC 11 theme detection was measured in a pseudo-terminal that deliberately did
not reply: 112.64-114.23 ms. That timeout is real but is an intentional terminal
query/input-handling policy and is materially smaller than the proven launcher
bug. It is deferred to avoid changing theme authority or consuming terminal
input differently without a separate UX contract.

### Journal write path

A release benchmark used the real SQLite `Store`, fresh profiles, generic
durable events, five repetitions, and 10,000 events per repetition.

| Shape | Results | Median throughput |
| --- | --- | ---: |
| one `append` call per event | 654.622, 602.837, 587.298, 527.744, 576.674 ms | 17,027 appends/s (58.7 us each) |
| batches of 100 | repeated 100-event calls | 83,102 events/s |
| one 10k-event batch | repeated whole batch | 126,800 events/s |

SQLite is configured for WAL with `synchronous=FULL`. Each `EventStore::append`
call opens one transaction, inserts all envelopes supplied by that call, and
commits once. It does not fsync once per envelope in an already-batched call.
Turn acceptance is especially important: its receipt and accepted envelopes
are inserted in the same transaction, and the session actor publishes or
acknowledges only after the awaited append completes. `flush` is a WAL
checkpoint, not the original durability boundary.

Additional batching is therefore deferred. The measured 17k acknowledged
append calls/s is not the current end-to-end bottleneck, while changing the
boundary would require a crash-shaped law proving both of the following:

- every acknowledged receipt/event is present after kill and reopen;
- an unacknowledged suffix is either wholly absent or wholly present, never a
  partial receipt/event group.

Any future batching design must group only callers that have not yet received
an acknowledgement, commit the whole group with `FULL` durability, and release
their waiters only after that commit. That invasive scheduler and crash harness
are not justified by the current measurement.

### Prompt compilation

The benchmark populated the real SQLite store with realistic five-event
completed turns, ran two warmups and ten recorded release samples, and compiled
the active provider projection.

| Events | Tree compiler p50 / p95 | Linear journal oracle p50 / p95 |
| ---: | ---: | ---: |
| 100 | 0.387 / 0.400 ms | 0.192 / 0.198 ms |
| 1,000 | 3.985 / 4.014 ms | 1.688 / 1.698 ms |
| 5,000 | 33.570 / 33.646 ms | 8.678 / 8.876 ms |

At 5k events, tree compilation is 3.87x the linear oracle and adds about
24.9 ms. Production compiles once per logical turn (not for each provider/tool
continuation), but each new turn rereads all journal pages and rebuilds the
tree. Ancestry compilation then filters the complete envelope vector once per
ancestry node, making the worst case O(ancestry x events). Compaction planning
also has a separate quadratic scan but is not on the ordinary provider-round
path.

The Phase 2 design keeps the durable journal authoritative and adds a
daemon-lifetime cache. Its exact projection identity is session plus
`(head sequence, compaction epoch, branch, agent, current run)`. It reads only
the suffix after a cached head and indexes envelopes by branch once for linear
ancestry assembly. Restart naturally starts with an empty cache. The required
law compares cached and fresh compilation after append, committed compaction,
and branch/agent switches.

### HTTP client construction and pooling

A release CPU microbenchmark constructed an OpenAI-style reqwest client 10,000
times. Construction medians were about 1.28 us/client (12.7-14.7 ms total),
while cloning an existing client cost 3-4 ns. The CPU saving per ordinary turn
is small; the important cost is losing reqwest's connection pool and therefore
paying avoidable DNS/connect/TLS work on a later request.

Production construction sites found in the provider layer:

- OpenAI and compatible adapters construct a client per adapter; the separate
  endpoint validator is deliberately one-shot.
- Anthropic constructs a client per adapter.
- Gemini constructs a client per adapter.
- catalog discovery constructs one client per discovery and deliberately sends
  `Connection: close`.
- web fetch constructs per-hop clients because the DNS pin and validated origin
  change; this is a security boundary, not a pooling candidate.

Daemon-side usage reporting, web search, and the credential broker already
construct once per long-lived owner. OAuth/JWKS verifier construction is a
separate policy path. Account-provider resolution currently rebuilds a provider
adapter per turn, which rebuilds the ordinary provider client. Phase 2 may
cache only an adapter whose complete model, tuning, credential generation, and
network-policy key is unchanged. DNS pinning, origin guards, timeout, proxy,
and credential refresh semantics must remain exact.

### TUI transcript

The release benchmark uses the real `SessionProjection`, renderer, ratatui
`TestBackend` at 118x36, five warmups, and sixty samples. With 10,000 transcript
rows:

| Position | p50 | p95 | max |
| --- | ---: | ---: | ---: |
| tail | 77.249 ms | 78.655 ms | 81.812 ms |
| middle | 64.327 ms | 64.910 ms | not recorded in the primary run |

A second run reproduced tail p50 77.536 ms / p95 82.521 ms and middle p50
63.906 ms / p95 66.007 ms. Existing release points were 8.41 ms p95 at 1k,
23.49 ms at 3k, and 55.05 ms at 5k. The live frame budget is 33 ms.

`render_session` and its subagent counterpart walk every projection entry,
format every block, parse and wrap markdown, clone the entire `Text`, ask the
paragraph for every logical line height, and only then apply scroll. Sticky
prompt anchoring also scans the complete anchor list. The proposed implementation
caches each entry's owned rendered lines and wrapped height under exact
projection revision, width, and theme keys; only dynamic entries are refreshed
for animation phase changes. A prefix-height index selects the viewport plus a
bounded overscan, while global scroll, jump, sticky prompt, selection, and copy
coordinates remain unchanged. The plain renderer is outside this seam and must
remain byte-identical.

## Phase 2 results

The optimize-now set was implemented without changing event order, wire
protocols, durable acknowledgement boundaries, or plain-renderer output. No
store durability code was changed.

### Before/after summary

| Area | Before | After | Result | Risk disposition |
| --- | ---: | ---: | --- | --- |
| Cold-launch winner wait | 1,000 ms (40 x 25 ms) | 0 ms when the authenticated peer PID is the retained child PID | Removes the proven one-second first-paint stall. The measured remaining daemon initialization plus unanswered OSC 11 ceiling is about 136 ms before scheduling/connect overhead. | Low; the fd-hygiene `pre_exec` sweep is untouched |
| Journal append | 58.7 us / acknowledged single-event call; 17,027/s | unchanged | Deferred: the existing receipt/event transaction and `FULL` commit remain the acknowledgement boundary. | High durability/ordering law |
| Prompt compile, 5k events | 33.570 ms p50 / 33.646 ms p95 | exact-head cache hit 0.0768 / 0.0934 ms; five-event advance 11.321 / 11.411 ms | 99.77% lower on a cache hit and 66.3% lower after a realistic completed-turn suffix. A forced fresh compile is 16.431 / 16.484 ms after linear ancestry indexing. | Medium-high, covered by fresh-versus-cached mutation laws |
| HTTP adapter construction | 1.282 us per new reqwest client; 10k adapters implied 10k pools | OpenAI 0.348 us, Anthropic 0.255 us, Gemini 1.279 us per adapter; one client build per shared policy/endpoint in the 10k run | OpenAI/Anthropic adapter CPU fell 73%/80%. Gemini's CPU is essentially flat, but all three retain one connection pool instead of 10k. | Medium; timeout, proxy, redirect, retry, origin and DNS guards preserved |
| TUI, 10k rows, tail | 77.249 ms p50 / 78.655 ms p95 | 0.101 ms p95 warm; 54.863 ms one-time cold fill | 99.87% lower warm p95 and comfortably below the 33 ms frame budget. | Medium UI mechanics; plain renderer untouched |
| TUI, 10k rows, middle | 64.327 ms p50 / 64.910 ms p95 | 0.106 ms p95 warm | 99.84% lower warm p95. | Same as above |

The cold-launch end-to-end figure is deliberately not invented: Unix-domain
socket bind is denied by this sandbox, so a fresh profile cannot reach a real
first paint here. The fixed branch removes an exact deterministic 1,000 ms,
and the remaining pre-bind components are the release measurements reported
above. It needs one host launch trace in the orchestrator battery.

### Cold launch implementation

After a ready attach, the client now compares the retained child's PID with
the kernel-authenticated socket peer PID. It skips the loser-reaping grace only
for the exact healthy winner; a different or unavailable peer PID keeps the
conservative 40-poll race-loser behavior. This leaves daemon election, child
detachment, timeout behavior, and descriptor closure unchanged.

Seams:

- `crates/haider-client/src/spawn.rs:61` defines the PID predicate and
  `crates/haider-client/src/spawn.rs:285` gates the loser grace.
- `crates/haider-client/tests/client_tests.rs:641` is the socket-independent
  winner/no-wait law, including conservative different/unknown-PID cases.

Verdict: **optimized now**. OSC 11 remains deferred because its 113-114 ms
timeout is a visible terminal/theme policy, not the measured one-second bug.

### Prompt compilation implementation and measurements

The daemon owns one `PromptHistoryCache`. Per session it samples the durable
head, reads only the missing journal suffix, detects a committed compaction
epoch in that suffix, and caches projections under the exact
`(head seq, compaction epoch, branch, agent, current run)` key. The global map
lock is not held over store or artifact I/O, and a concurrent older compile is
not allowed to replace a newer cached head. The durable journal and CAS remain
authoritative; restart simply begins with an empty cache.

Ancestry compilation now indexes the fixed agent's envelopes by owning branch
once and uses two binary searches for each node's sequence slice. This removes
the previous full-journal scan per ancestry node even on a forced fresh
compile.

The release after-run used the real SQLite store, realistic five-event
completed turns, two warmups, and ten recorded samples. `advance` appends one
five-event turn after priming the cache.

| Events | Phase 1 fresh p50 / p95 | Phase 2 forced-fresh p50 / p95 | Exact-head hit p50 / p95 | Five-event advance p50 / p95 |
| ---: | ---: | ---: | ---: | ---: |
| 100 | 0.387 / 0.400 ms | 0.983 / 1.297 ms | 0.0149 / 0.0284 ms | 0.392 / 0.424 ms |
| 1,000 | 3.985 / 4.014 ms | 4.002 / 5.266 ms | 0.0261 / 0.0283 ms | 2.356 / 2.399 ms |
| 5,000 | 33.570 / 33.646 ms | 16.431 / 16.484 ms | 0.0768 / 0.0934 ms | 11.321 / 11.411 ms |

The small-fixture forced-fresh difference is benchmark overhead/run variance,
so the cache-hit and append columns are the production-path result; the 5k
forced-fresh result independently demonstrates removal of the superlinear
ancestry scan. The after harness was recreated from the same recorded fixture
shape rather than retained as production code.

Seams:

- `crates/haider-core/src/prompt_history.rs:44`, `:221`, and `:357` define and
  operate the exact-key suffix cache; `:450` keeps the fresh compiler as the
  equivalence oracle; `:810` builds the linear ancestry projection.
- `crates/haider-core/src/lib.rs:56` exports the cache type.
- `crates/haider-daemon/src/session_hub/mod.rs:567`, `:970`, and `:2766` own
  and expose the daemon-lifetime cache; `crates/haider-daemon/src/worker.rs:3809`
  routes production turn compilation through it.
- `crates/haider-core/tests/prompt_history_tests.rs:149` proves append
  invalidation, `:480` primes before committed compaction and compares cached
  with fresh afterward, and `:1088`/`:1407` compare fresh and cached results
  across main/named branch and agent switches, including identical errors.

Verdict: **optimized now**. Further delta-compiling the final message vector is
deferred: the current exact cache removes unchanged-head work, while the
remaining mutation path is linear and changing partial-message assembly would
expand compaction/branch risk for a smaller measured gain.

### HTTP client reuse implementation and measurements

Provider adapter instances still own their credential handle and all request
settings. Only the policy-identical reqwest transport is shared:

- ordinary OpenAI and Anthropic-family transports use one process client;
- fixed first-party OAuth transports share a client together with their exact
  `FixedOriginGuard`;
- OpenAI-compatible transports are keyed by canonical base URL plus origin
  policy;
- Gemini transports are keyed by the exact model stream endpoint and retain
  the cached-contents endpoint in the same fixed-origin allowlist.

All builders retain `no_proxy`, redirects disabled, retry disabled, and the
same connect/response-open/chunk-idle timeout policy. Credentials are not in a
global cache and were not given process-lifetime ownership.

The release after-run constructed 10,000 adapters after 100 warmups and
included real `MemoryVault` credential resolution:

| Provider | Before transport construction | After mean adapter construction | Client builds observed |
| --- | ---: | ---: | ---: |
| OpenAI | 1.282 us | 0.348 us | 1 |
| Anthropic | 1.282 us | 0.255 us | 1 |
| Gemini | 1.282 us | 1.279 us | 1 for the exact model endpoint |

The network sandbox prevents a DNS/connect/TLS before/after, so the table does
not assign a made-up handshake number. The material behavior change is that
later turns reuse reqwest's real connection pool rather than necessarily
creating a new one.

Seams:

- `crates/haider-provider/src/openai.rs:75-191`, `:255`, `:331`, `:360`, and
  `:822` implement ordinary, guarded fixed-origin, and exact compatible pools.
- `crates/haider-provider/src/anthropic.rs:84-151`, `:281`, and `:364`
  implement ordinary and fixed OAuth pools.
- `crates/haider-provider/src/gemini.rs:47-128` and `:207` implement the exact
  endpoint pool and its regression counter.
- `crates/haider-provider/src/lib.rs:120`, `:137`, and `:148` expose hidden
  construction counters to integration laws.
- `crates/haider-provider/tests/openai_provider_tests.rs:23`,
  `anthropic_provider_tests.rs:27`, and `gemini_provider_tests.rs:26` prove
  that adapters with different credential handles do not build a second
  client under the same complete network-policy key.
- `crates/haider-provider/src/openai_tests.rs:1841` keeps the Azure header
  golden offline with the existing deterministic resolver; all request/header
  assertions are unchanged.

Verdict: **optimized now** for turn providers. Catalog discovery remains a
one-shot `Connection: close` operation; the endpoint validator remains
one-shot; web-fetch clients remain per-hop because the validated origin and
DNS pin change at each hop. Daemon usage reporting, web search, and the broker
were already long-lived. Those paths are intentionally not folded into these
pools.

### TUI transcript implementation and measurements

Each live session and subagent chip now owns a layout cache keyed by exact
projection revision, width, and theme. It stores owned rendered lines and
wrapped height per entry, refreshes only pending/in-progress animated tool rows
on a phase change, and recomputes changed entries when the projection mutates.
Session swaps explicitly clear the live layout cache.

The viewport selector uses cached global row starts to choose the contiguous
entry window intersecting the viewport plus two rows of overscan. Tail rows,
sticky-user anchors, node jumps, subagent metrics, and scroll offsets remain
in the original global coordinate space. The plain renderer was not changed.

Final release law (`118x36`, five warmups, sixty samples):

| Rows | Warm p95 |
| ---: | ---: |
| 1,000 | 0.244 ms |
| 3,000 | 0.116 ms |
| 5,000 | 0.101 ms |
| 10,000 tail | 0.101 ms |
| 10,000 middle | 0.106 ms |

The first 10k frame, which formats and measures all entries once, was
54.863 ms. Every later frame renders the bounded window. The non-monotonic
small values are normal timer/cache noise and, importantly, show that warm
cost no longer grows with transcript length.

Seams:

- `crates/haider-tui/src/projection.rs:200`, mutation bumps beginning at
  `:379`, and accessor `:1162` provide exact invalidation.
- `crates/haider-tui/src/app.rs:787`, `:2834`, and constructors at `:852`,
  `:907`, and `:3219` own caches; `:10269`, `:10466`, and `:10523` clear the
  live cache at projection identity changes.
- `crates/haider-tui/src/demo_store.rs:870` initializes hydrated chip caches.
- `crates/haider-tui/src/render.rs:29-228` contains cached layout and bounded
  selection; session rendering uses it at `:3429`/`:3503`, and subagent
  rendering at `:5333`/`:5375`.
- `crates/haider-tui/tests/w3c3_render_bench_tests.rs:89` enforces the release
  33 ms law through 10k tail and middle frames and bounds the cold fill.
- `docs/OPTIMIZATIONS.md:14` and `:17` mark the two cache/viewport ledger
  entries shipped without changing their visual-design contract.

Verdict: **optimized now**. The existing `u16` global scroll saturation above
65,535 wrapped rows is explicitly deferred as a separate compatibility seam;
changing it here would violate the requirement to keep scroll mechanics
byte-identical. The one-time cold layout fill can be revisited if attach-time
profiles show it dominating in real sessions.

## Verification

All commands used `HAIDER_DISCOVERY_DISABLED=1 CARGO_INCREMENTAL=0`. The final
`cargo fmt --all -- --check` and `git diff --check` pass. No golden changed or
required regeneration.

| Crate | Sandbox result | Notes |
| --- | ---: | --- |
| `haider-core` | 100 passed, 0 failed | Full suite; includes append, compaction, branch, and agent cache equivalence laws |
| `haider-store` | 70 passed, 0 failed | Full suite; store code is unchanged |
| `haider-daemon -- --test-threads=4` | 326 passed, 60 socket-blocked, 2 ignored | Every main-suite failure originates in a Unix-socket fixture bind returning `EPERM` |
| `haider-provider` | 199 passed, 10 socket-blocked, 6 ignored | Ten loopback fixture binds return `EPERM`; the pooling laws and offline Azure request golden pass. |
| `haider-tui` | 971 passed, 4 socket-blocked | Four link tests bind a fake Unix daemon and return `EPERM`; the final release performance law separately passes 1/1. A post-hardening session/projection subset passed 64/64. |
| `haider-cli` | 77 passed, 17 socket-blocked | The blocked autospawn/headless cases fail because the daemon cannot bind its Unix endpoint (`EPERM`) |
| `haider-client` | 20 passed, 35 socket-blocked | Direct fake-daemon binds and spawned-daemon endpoint setup are denied; the new socket-independent PID law passes |

There are no non-environment assertion failures in executable paths. The
orchestrator host battery must rerun the socket-dependent suites and a real
fresh-profile first-paint trace.

## Deferred work and reasons

- **Fsync/commit batching:** deferred. It is the only high durability-risk
  item, and the measured 17k individually acknowledged calls/s does not justify
  inserting a commit scheduler without a kill-between-append-and-ack recovery
  law. Existing receipt/event atomic transactions and ack-after-commit behavior
  are unchanged.
- **OSC 11 theme timeout:** deferred. It costs 113-114 ms on a silent terminal,
  but changing it affects terminal input consumption and theme authority.
- **Compaction-plan range validation:** its quadratic range comparison is not
  on the normal provider-round compile path and typical compaction counts are
  small; defer until a compaction-heavy profile triggers it.
- **Catalog, endpoint-probe, web-fetch, and OAuth/JWKS one-shot clients:**
  deferred or intentionally retained where lifecycle or DNS/origin pinning is
  semantically load-bearing. They were not the per-turn connection-pool loss.
- **TUI scroll coordinate widening:** deferred to a compatibility-focused
  change because this wave promised unchanged scrolling, selection, search,
  copy, and plain-renderer behavior.

No commit, tag, version bump, MCP change, or daemon fd-safety edit was made.
