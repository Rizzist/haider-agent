# v0.0.970 act-bias audit and verification

## Claim audit

- Claim (a) is correct, with drifted line numbers. Before this lane,
  `SystemPromptBuilder::shared_immutable_base` in
  `crates/haider-daemon/src/worker.rs` had identity, authority, context, and
  grant-scope prose, but no implementation trigger, inspect -> edit -> verify
  contract, or worked editing example. The current construct begins at line
  2177, not at the line range quoted by the benchmark. OpenCode v1.18.9's
  `packages/opencode/src/session/prompt/default.txt` does carry both a worked
  search/read/edit example and explicit implement/test/lint/typecheck direction.
- Claim (b) is correct in substance, with drifted line numbers. The filesystem
  manifests kept prose, but `provider_definition` (now near line 13501) replaced
  every provider-visible description with an empty string and a structural stub
  schema. The shared manual (now near line 13481) was therefore the only
  provider-visible prose. "Bare" means prose-free/minimal, not schema-free: the
  stub still retains types, enums, required fields, properties, and items.
- The cited 11,246-byte instruct-pipe pin is historically correct for v0.0.969,
  but not the merged wave-970 starting point. Monitor and `list_models` inventory
  additions had already moved the pin 11,246 -> 11,812 -> 12,122 before this
  lane. The test records both the historical number and the actual pre-actbias
  baseline so the lane delta is not misattributed.

## Change

- Advanced the shared prompt contract to `haider-system-v4` and added a terse
  implementation rule: inspect only until the target is known, edit, then run
  the smallest relevant build/test; do not remain in planning.
- Added one native-tool worked example: `fs_search` -> `fs_read` -> `fs_edit` ->
  `process_exec` verification.
- Restored concise provider-native what/when descriptions for exactly the
  action-critical filesystem surface: `fs_glob`, `fs_search`, `fs_write`,
  `fs_edit`, and `fs_path`. Detailed signatures and bounds remain in the shared
  manual; unrelated tools remain description-free.
- Added exact contract/example, description-scope/content, inventory, and byte
  regression pins. Regenerated the provider-request and normalized run goldens
  deliberately for the v4 cache boundary and selected descriptions.

## Prompt byte accounting

| Surface | Before | After | Lane delta |
| --- | ---: | ---: | ---: |
| Shared immutable policy, empty tool set | 359 | 725 | +366 |
| Instruct pipe (stub wire + shared tool manual) | 12,122 | 12,621 | +499 |
| Stable provider prefix (policy + instruct pipe) | 12,481 | 13,346 | +865 |
| Full manifest comparison, macOS | 18,056 | 18,205 | +149 |

The instruct-pipe increase is exactly the 499 bytes of the five restored native
descriptions. Relative to the historical v0.0.969 pin, the final value is
11,246 -> 12,621 (+1,375): +876 was already present from merged tool inventory,
and +499 belongs to actbias. The updated release assertion pins 12,621 and also
asserts that subtracting native description bytes returns the 12,122 wave
baseline; its comment preserves the 11,246 history and rationale.

## Verification

All Cargo commands used:

`RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`

Daemon-driven tests additionally used `HAIDER_TEST_SIBLINGS_PREBUILT=1`. A
`df -m /` check immediately preceded every build/test command; all checks stayed
above the 700 MiB stop floor. The prebuilt `target/debug/haiderd` measured
197,290,544 bytes, above the 10 MiB minimum.

- Focused prompt contract/example, exact v4 prompt, selective schema
  descriptions, and instruct-pipe byte tests: pass.
- Turn-hygiene provider request plus text/tool JSONL goldens: pass after explicit
  `UPDATE_FIXTURES=1` regeneration and again without the update flag.
- One-shot JSONL golden: pass after explicit
  `HAIDER_ONESHOT_GOLDEN_UPDATE=1` regeneration and again without the flag.
- `cargo test --workspace`: pass.
- `cargo clippy --workspace -- -D warnings`: pass. `--all-targets` was not used,
  per the shared-machine rule.
- `cargo fmt --all -- --check`: pass.
- `git diff --check`: pass.
- `cargo run -p xtask -- test-count --update`: baseline 4,748 -> 4,750.
- `cargo run -p xtask -- test-count`: 4,750/4,750, pass.
- `cargo run -p xtask -- check`: pass; nine pre-existing LOC soft-cap warnings
  remain warnings.

No OAuth or parallel-lane-owned source was changed. `LANE-COMMON.md`,
`LANE-BRIEF-actbias.md`, `turnperf/`, and `turnperf2/` remain untracked and are
excluded from the commit.
