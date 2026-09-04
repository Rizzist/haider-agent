# Lane toolrepair — a malformed tool call must be recoverable, and edits must be emittable by weak models (v0.0.970, gpt-5.6 xhigh)
Worktree lane-970-toolrepair (from origin/wave-970). EVIDENCE (/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/971-benchmark-rootcause.log):
3 of 20 benchmark runs died on malformed tool JSON. In haider a malformed tool call is TERMINAL — the call is durably closed as failed and
the whole run becomes a provider failure (crates/haider-core/src/actor.rs, see the investigation's cited lines); opencode lowercases a
mismatched tool name where it can and otherwise converts the failure into an `invalid` tool result the model can correct
(OPENCODE packages/opencode/src/session/llm.ts:283-298). Separately, haider's edit surface is harder to emit: `fs_edit(path,
edits:[{old,new,replace_all?}])` requiring a fresh prior read and exactly one anchor, versus opencode's flat
`edit(filePath, oldString, newString, replaceAll?)` with nine exact/fuzzy/whitespace/context-aware replacers. CLAIM-AUDIT first.
Deliver: 1. One malformed tool-argument frame becomes a durable typed `invalid_tool_call` result naming what was wrong, and the run gets
ONE repair continuation instead of terminating; a second consecutive malformed frame still terminates. 2. A tolerant tool-name match
(case/underscore) with the correction reported in the result. 3. Flat `edit(file_path, old_string, new_string, replace_all?)` and
`write(file_path, content)` aliases advertised alongside the existing transactional tools, reusing the SAME internals and the same
safety rules (fresh-read requirement, anchor uniqueness) — aliases change the SCHEMA SHAPE, never the guarantees. 4. When an anchor fails
to match, the error must say why and show the nearest candidate rather than a bare failure.
Tests: malformed frame -> typed invalid result + one repair continuation, second one terminates; tool-name repair; alias round-trip
producing identical effects to the existing tools; anchor-miss message includes the near match; existing filesystem/tool pins green.
`cargo test --workspace`, clippy -D warnings, ENV LAW, test-count update. docs/testing/v0.0.970/toolrepair.md. Commit on the lane branch,
no trailer, no push. LAST line SHIP or NO_SHIP.
