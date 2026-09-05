# Lane ceilingdecl — declare the turn ceiling and surface partial progress at every internal cap (v0.0.970, gpt-6-astra, small)
Worktree lane-970-ceilingdecl (from origin/wave-970). ASK (benchmarking agent, AHRB fidelity pillar): `declared_turn_ceiling` must be an
adapter-manifest declaration (optional `[fidelity]` block also declares typed `internal_cap_exit_codes` and a `workspace_path` template);
`internal_cap_detected` is true only with typed evidence; `workspace_state` is mutated|untouched; and a run must never exit 70 with an
untouched workspace without surfacing partial progress. turnbudget (LANDED) already made the cap a 32-request soft tranche + 64 hard cap
with a typed note, durable resumable continuation and TUI/journal visibility — CLAIM-AUDIT what it left: the exit code and terminal
cause at the hard cap, whether the run result names `end_reason`, whether partial-progress facts (files touched, tool calls made,
continuation handle) are in the result when the cap hits, and whether any adapter manifest declares the ceiling.
Deliver: 1. Run result / `--output json` terminal block carries `end_reason` (typed: e.g. `harness_internal_ceiling`), the ceiling values
(soft, hard, used), the continuation handle, and `workspace_state` (mutated|untouched, computed from a pre/post tree receipt of the
workspace) plus a partial-progress summary (files written, tool calls, last request ordinal). 2. Exit-code semantics documented and
typed: the cap exit code is declared, distinct, and stable; document it in the adapter `[fidelity]` block. 3. Adapter manifest
declaration of `declared_turn_ceiling` (= the hard cap, with the soft tranche noted), `internal_cap_exit_codes`, and the `workspace_path`
template; product copy under bench/adapters/haider-agent/ if it exists, else the exact TOML for the owner to paste into harness-bench.
Tests: cap hit -> result block pins (end_reason, ceilings, handle, workspace_state both values, progress summary); exit code pin; replay
of a capped run shows the same block; manifest parse test if the product copy exists. Merge forward BEFORE your verdict. Full gate +
clippy --tests + test-count. docs/testing/v0.0.970/ceilingdecl.md. Commit, no trailer, no push. MANDATORY VERIFIER line. LAST line SHIP or NO_SHIP.
