
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
## Verifier-value tracking (added 2026-09-04, owner directive)
New lanes run on gpt-6-astra. We are measuring whether the pre-finish verifier stage still earns its cost on this model. Before your final
message, run the verifier subagents as usual, then report a line exactly of this shape:
`VERIFIER: findings=<n> real=<n> noise=<n> — <one clause per real finding: what it was and what you changed>`
"real" = a finding that changed code, a test, or the verdict; "noise" = a finding you rejected with a reason. If findings=0, say so. The
landing gate (full workspace test) is independent of you and will be compared against this line.

## Merge forward BEFORE your verdict (added 2026-09-05)
Before running your final gate, `git fetch origin wave-970 && git merge --no-commit origin/wave-970` (the local ref is current if fetch
fails) and resolve conflicts preserving both sides. Expect these three to drift whenever the prompt/tool surface changes and handle them
without being asked: regenerate crates/haider-cli/tests/fixtures/turnhygiene/provider_request_no_budget.json (and any other JSONL/fixture
golden) through the repo's tooling — never hand-merge a golden; re-pin the instruct-pipe byte count in
crates/haider-daemon/src/permissions_core_tests.rs to the real merged value and say old -> new; recount test-baseline.txt with the
test-count tool. Then run the full gate on the MERGED tree. You cannot commit the merge (git dir outside your sandbox) — leave it resolved
in the working tree and say so; the orchestrator records it.
