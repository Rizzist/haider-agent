# Lane runperm — a headless coding run must be able to write and execute (v0.0.970, gpt-5.6 xhigh, P0)
Worktree lane-970-runperm (from origin/wave-970). EVIDENCE (independent read-only investigation, full log
/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/971-benchmark-rootcause.log): an external benchmark scored haider 0.0.969 at 0/20 vs opencode
4/20 on long agentic tasks. Root cause #1: `haider run` initializes allow_writes=false, allow_exec=false, auto_allow=false
(crates/haider-cli/src/run.rs:100-108,221-229); headless sessions are created in Autonomous interaction mode where filesystem
writes/edits and process execution default to Ask, and Autonomous converts an unresolved Ask into a DENIAL because no human can answer.
So a headless run cannot write a file or run a test unless the user passes explicit flags — the success ceiling was 0 by construction.
opencode's default build agent is `* = allow`. CLAIM-AUDIT these lines first and report corrections.
OWNER DIRECTIVE (2026-09-04, decisive): "it should never ask. should always be auto." In AUTONOMOUS mode there is no such thing as an
unresolved Ask — Ask is an INTERACTIVE-ONLY concept. Implement that as the rule, not as a flag default.
Deliver: 1. In Autonomous interaction mode every permission decision resolves to ALLOW. Remove the Ask->deny conversion entirely: an Ask
policy encountered in Autonomous mode is a resolution to allow, and the tool proceeds. This covers filesystem write/edit and process
execution, and every other Ask-shaped gate reachable in a headless run — audit them all and list them in the evidence doc. 2. Two things
still say no, and ONLY these two: (a) an EXPLICIT user deny (config/flag/policy rule the user wrote) — honoured, and reported to the model
in typed, model-readable terms naming the rule; (b) lockdown mode's allow-list, which is an opt-in security posture. Everything else
allows. Keep an explicit `--read-only` for users who want a no-write run; when it is on, denials are typed and model-readable
("write denied: run is --read-only") and the run's terminal cause names it. A denial must NEVER be silent. 3. THE DEFAULT INVOCATION IS THE ONE THAT WORKS: `haider run -p "<task>"` with NO flags must be able to write files and run
processes. allow_writes/allow_exec/auto_allow initialization in run.rs must reflect that; the existing explicit flags stay as no-ops or
as overrides, never as the thing that unlocks basic function. A third-party benchmarking haider as shipped, with no flags, must get a
write-capable agent. `haider run --help` and the docs state the default plainly. 4. An audit of every other non-interactive entry point
(subagents, workflows, hooks) for the same silent-denial shape; fix or state why unaffected.
Tests: headless run writes a file end to end with no flags; `--read-only` refuses with the typed message and a named terminal cause; a
denial is never silent; outside-workspace write still gated; existing permission/lockdown pins green. `cargo test --workspace`, clippy
-D warnings, ENV LAW, test-count update. docs/testing/v0.0.970/runperm.md. Commit on the lane branch, no trailer, no push. LAST line SHIP or NO_SHIP.
