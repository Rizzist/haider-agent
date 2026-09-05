# Lane economydiet — cut the fixed per-turn overhead without losing the action contract (v0.0.970, gpt-6-astra)
Worktree lane-970-economydiet (from origin/wave-970). ASK (benchmarking agent, AHRB economy pillar): haider spends ~11.7k FIXED tokens per
turn vs pi's 2.4k; the tool-result envelope is 7.2x pi's for the same result. Metrics: `per_turn_fixed_overhead_tokens` (exact common block
prefix across every primary request, system/developer side and tool side measured independently), `context_token_curve_slope`,
`wasted_tool_call_count` (harness-bench SPEC-v3-economy.md, read-only at ~/Documents/CODING/harness-bench). The read-only investigation
(971-benchmark-rootcause.log §3.2/§6) found haider advertises ~23 root tools vs opencode's ~11 and moved tool semantics into an ~11-12 KB
system manual. CONSTRAINT: actbias (LANDED) deliberately ADDED the inspect->edit->verify contract, a worked example and native tool
descriptions (+865 bytes) — the diet must KEEP those; cut elsewhere. modelcat (LANDED) added `list_models`; reuse that discovery pattern
for tools. CLAIM-AUDIT FIRST: measure the current fixed overhead on this tree with the AHRB economy CLI (read-only use of harness-bench)
or an equivalent exact-prefix count over a 5-turn fixture, split system-side vs tool-side, and the tool-result envelope bytes per result.
Deliver: 1. Tool-result envelope to the MODEL = output + non-zero exit (+ truncation marker when applicable); digests, effect ids, run ids,
limits and receipts move to the durable journal/events only (the AHRB effects/truncation contract stays intact on the journal side —
`/effects[n]` and the `[haider:truncated …]` line are unchanged where the contract demands them). 2. Tiered tool exposure: a core set
(~6: read, glob/search, edit, write, exec, a task/todo primitive) advertised by default for coding turns; every other tool reachable
through one discovery/describe call (`list_tools`/`describe_tool`, modelled on list_models) and then advertised for the rest of the
session; computer/mobile/monitor/peer/SSH/workflow/plan tools gated behind that or explicit config — lockdown allow-lists unchanged.
3. System prompt trim: remove redundancy in the manual now that native descriptions exist; keep the actbias contract + example verbatim;
report old -> new bytes for policy, manual, tool schemas, and the combined stable prefix, and re-pin deliberately. 4. Prefix-cache
safety: the stable prefix must stay byte-stable across turns (fingerprint order system -> system+tools -> +history); prove it with the
existing cache-fingerprint pins.
Acceptance (measured, before/after, same fixture): per_turn_fixed_overhead_tokens down by at least half; tool-result envelope bytes per
result down by at least half; wasted_tool_call_count not up; the full QA gate green; warm/one-shot turn wall neutral (ABBA within MAD).
Tests: envelope shape pins (model-facing vs journal-facing), tiered exposure pins (default set, discovery, promotion, lockdown), prompt
byte pins with rationale, cache-prefix stability pins, all JSONL/fixture goldens regenerated via tooling and reviewed. Merge forward BEFORE
your verdict. Full gate + clippy --tests + test-count. docs/testing/v0.0.970/economydiet.md with the numbers table. Commit, no trailer,
no push. MANDATORY VERIFIER line. LAST line SHIP or NO_SHIP.
