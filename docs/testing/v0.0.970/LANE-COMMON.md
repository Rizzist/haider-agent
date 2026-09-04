
---
# COMMON RULES FOR EVERY 970 LANE

**Workflow: investigate -> decide -> implement -> verify loop until an independent
verifier returns SHIP. Use subagents: 1-3 research, 1-3 verifiers.**

**Base:** `wave-970` @ `8952219`, which is the v0.0.969 release currently in
its final CI gate. The 967 release will be MERGED FORWARD into wave-970 by the
orchestrator; you never rebase.

**Citations drift.** Every `file:line` in this brief was taken from analysis on
`d75a8ea (v0.0.968 = main)`; the tree has moved twelve merges since (notably `worker.rs`, `main.rs`,
`session_recover.rs`, `oauth.rs`, `hooks_tests.rs`). **Audit every citation before
relying on it and report correct / drifted / wrong.** Grep for the construct, do not
trust the number.

**Four other 968 lanes run concurrently** in sibling worktrees. Their territories:
- `deleg`  — delegation waits (InputRequired hang + cancellation-tail waits)
- `retain` — worker-supervisor retirement + ObserveDigestCache bound (worker manager, session hub)
- `wfcont` — workflow continuation across autonomous hops (worker rebind vs actor refresh)
- `maxcost`— `--max-cost` binding before the next provider request (budget path in worker)
- `resume` — resume-on-reconnect for provider streams (provider transport + run state)
Stay inside your territory. If your fix genuinely needs a change in another lane's
area, make the SMALLEST possible change there, name it explicitly in your report,
and expect the orchestrator to reconcile.

**Verification bar.** Named tests for every behaviour you claim, including
mutation checks where the brief asks. Nothing may be weakened, `#[ignore]`d, or
platform-gated to reach green. Windows/Linux behaviour you cannot execute here
must be labelled "by inspection". Every deadline you add is DERIVED from the
budgets it wraps, with the arithmetic in the comment (registry #94). Any wait on
external state while a negotiated connection is open keeps the keepalive serviced
(registry #95).

**Resources.** Scope builds with `-p <crate>`; do NOT run
`cargo clippy --workspace --all-targets` (five lanes share this machine). Check
`df -m /` before every build; stop with `ENVIRONMENT-BLOCKED` under 700 MiB.
ENV LAW: `RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`.
Prebuild siblings and set `HAIDER_TEST_SIBLINGS_PREBUILT=1` for daemon tests.
Built `haiderd` must exceed 10 MiB (registry #64).

**Do not touch:** `crates/haider-daemon/src/oauth.rs`, `oauth_tests.rs` (gate-fixed, stable). Parallel-owned files are named per brief — if your fix needs one, STOP and report NO_SHIP with the exact need.

**Deliverable:** the verdict(s) the brief asks for with evidence; the change;
the tests; the affected crates green; the CI error registry walk appended.
Leave the work UNCOMMITTED — the orchestrator owns committing.
**End with SHIP or NO_SHIP on its own line, as the LAST line of output.**
