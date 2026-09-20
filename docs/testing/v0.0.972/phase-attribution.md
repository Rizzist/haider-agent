# Phase attribution v4

`python3 scripts/qa-gate/turn_wall_harness.py --bin-dir <candidate>` and
`... --one-shot` enable buffered phase probes by default. Use `--no-phases`
for paired overhead controls. `--trace` is the separate, older stderr timing
port; it is not necessary for phase maps. Build both `haider` and `haiderd`
from the candidate. Uninstrumented released binaries produce a visible
`missing_process_traces` result, never fabricated phase measurements.

The benchmark owns a disposable `HAIDER_PHASE_TRACE_DIR`. Each executable
flushes one content-free JSONL file on exit. Warm records are read after the
owned daemon stops, then selected by exact client/daemon PID and command
CLOCK_MONOTONIC boundaries. Prompt text, paths, tool arguments and identifiers
other than PIDs are never recorded. The report retains raw selected records.
Trace schema v2 added content-free CAS byte, block-hash and re-verification-call
counters. Schema v3 adds the store-access split plus row, materialized-byte and
decoded-event counters. Schema v4 adds a frozen, content-free `store_caller`
tag to replay/reducer page-query and decode records, allowing the deep harness
to report pages, rows, bytes, wall and CPU by consumer family. The reader
remains compatible with schema v1/v2/v3 traces.
No credential or signing material is needed: the existing fake HTTP provider,
throwaway profile, real RPC and real tool fixtures remain authoritative.

## Accounting contract

`phase_attribution.total`, `.phases` and `.residual` use integer nanoseconds.
For each individual sample, phase sums plus residual equal the independently
measured total exactly. `phase_summary` pools these integers; divide by its
count for additive means. Independently computed medians do not add up and
are intentionally not used for a reconciliation table.

CPU uses POSIX CLOCK_THREAD_CPUTIME_ID (seconds and nanoseconds). It does not
read Mach ticks. Async scopes sample only each active poll, on that poll's
thread, and synchronous nested scopes subtract their inclusive CPU from the
parent. Interleaved work while a future is pending is not charged to it. The
independent warm process-tree total must use the corrected native sampler in
`turnperf_support.py`; cold totals remain getrusage seconds. Warm daemon-reaped CPU is reported as a separate counter-only row, without
inventing a wall interval or calling every child a tool. It remains in residual
when no independent counter is available. CPU of a probe
straddling a sample boundary is also left in residual, never prorated.
A negative CPU residual is an error; it is not clamped or rescaled.

Wall intervals overlap, particularly RPC waits and daemon/store work. The
report gives one explicit partition: active intervals precede waiting
intervals, then `wall_priority_low_to_high` resolves overlap. This is an
accounting convention, **not a causal critical path or claimed optimization
saving**. Keep raw intervals when studying concurrency. Uncovered intervals
remain residual. A zero wall allocation for a CPU-bearing phase can occur
when concurrent higher-priority work occupies all its wall interval.
"Active" means the poll or synchronous scope has not returned; its elapsed
wall time can include blocking syscalls and thread descheduling. It does not
mean that the CPU was executing throughout that wall interval.

## Probe coverage and limitations

| Phase | Actual boundary / limitation |
| --- | --- |
| spawn | Parent daemon-launch call; excludes executable loader/pre-main work |
| dynamic_link | Unavailable before Rust main; null in the cold map; part of residual |
| runtime_init | Client runtime builder and daemon runtime builder; worker bootstrap not executed inside those calls stays residual |
| store_open | Locked store connection, migrations, CAS/views and startup projections |
| directory_prep | Client profile resolution, store root/lease acquisition, platform runtime directory preparation |
| socket_handshake | Client connect/Welcome; overlaps cold daemon initialization wait |
| capability_catalog | Registered catalog and turn pack construction; cache hits may have no build probe |
| first_request | Cold rollup of observed request constituents between first client RPC and client teardown; startup mutations and later output stay residual |
| submit / rpc | Client begin_request/request active polls and suspension intervals, plus active server handle_frame polls; other transport tasks remain residual |
| store_journal | Store single-batch/grouped append/commit paths and StoreOwner::with_store_write profile mutations; other operations can appear in store_access or residual |
| projection_digest | SQL run-head projection updates, durable run lookup, initial budget usage, journal rendering and prompt cache metadata |
| provider_assembly | Built-in prepare_turn call including selected adapter wire serialization; other request setup stays residual |
| stream_decode | OpenAI SSE/framing/typed decoder push entry points; not Anthropic or the provider network wait |
| cas_read_hash / cas_reverify | CAS integrity hashing on actual reads / full hashing performed only to establish that a write target already contains the addressed bytes; v2 rows also count bytes read, blocks hashed and re-verification calls |
| store_query_replay / store_query_reducer / store_query_point | Full journal replay pages, payload-kind-filtered reducer pages, and hot metadata/checkpoint/head queries. Replay/reducer rows count materialized envelopes and true-weight bytes. |
| store_event_decode | Row iteration, envelope materialization, storage-class decode, validation and page canonicalization for one replay/reducer page; v3 counters report decoded rows/bytes/events without recording content, and v4 attributes them to a frozen consumer family. |
| store_provider_view | Provider-view CAS/index verification, reads and request-attempt persistence; v3 counters report referenced/written blocks and bytes. Nested CAS phases keep hash work distinct. |
| store_receipt_attempt | Turn receipt replay/ordinal queries and the fused acceptance transaction. |
| store_owner_lock_wait / store_connection_lock_wait / store_other | Profile-owner mutex acquisition / SQLite-connection mutex acquisition / the remaining generic store operation body. Lock intervals are emitted as zero-CPU waits so overlapping active holders retain wall attribution. These close the named `store_access` partition instead of silently dropping uncategorized calls. |
| tool_dispatch | Real general-tool dispatcher active polls/waits; external child CPU is separate |
| daemon_reaped_children | Corrected native before/after counter delta; CPU only, no wall allocation |
| client_control / turn_control | Active CLI dispatch / core drive_turn polls, excluding nested specific scopes; waiting time is not recorded for these broad control buckets |
| lockdown_bind_activate | Synchronous durable per-run lockdown binding plus active-session activation inside turn setup; excludes provider resolution and any later background-task fence |
| turn_setup / store_access | Active worker start_turn polls and generic StoreOwner::with_store operations, excluding nested specific scopes; supplementary buckets expose remaining setup/store work |
| completion_render | Headless output per-event adaptation and final output; excludes waiting for the next event; not resident TUI paint |
| teardown | Client owned-daemon teardown and daemon finalization; runtime drop/trace flush remains residual |

Absent probes have null values and `unobserved` coverage, not invented zero
cost. The total map is deliberately `partial_attribution`: pre-main/loader,
uninstrumented tasks and child processes are explicit unresolved work. This
infrastructure does not by itself justify selecting any analyst proposal.
The workloads are fresh-profile one-shot and resident-daemon/fresh-client,
new-session single/tool turns; neither measures long-history continuation.

## Cost and verification

Disabled probes perform a cached opt-in branch; no clocks, allocations,
locks or records after the initial environment lookup. Enabled active scopes
read one wall/thread-CPU pair at entry/exit, maintain a thread-local nesting
stack and append to a mutex-protected vector. Waiting intervals add one
record per resumed poll. No logging syscall occurs per scope. Each process
caps retained records at 100,000; overflow is counted and rejected by the
reader. The header declares the expected record count, so truncation at a
complete JSON row is also rejected. Trace flush is in the cold lifecycle total; the resident daemon's
final flush is outside the warm sample window. Retained trace memory can
raise diagnostic RSS and must not be described as released steady-state RSS.

Quantify cost with interleaved on/off harness runs on the same artifact,
correctness fixtures and sample counts. Record wall/CPU deltas and MAD,
separately for cold, warm single and warm tool. Non-quiet runs are diagnostic
only. Obtain the owner's scheduled quiet window before publishing final
numbers; no overhead claim is established by this document.
