# replyarena2 — merge-forward evidence

Date: 2026-09-04

Branch: `lane-970-replyarena2`

Reply-arena HEAD: `3844fc98be8be0f44ad1ff025801dc14e1d80e58`

Merged wave: `c318d7b5cfaf640745a643b147c1848e15ed18ee`

Merge base: `6a374b148b4230ee1892d5aef20ab66c6d7008bf`

Disposition: resolved and left uncommitted for the orchestrator.

## Verdict

The merge-forward preserves the canonical reply arena and producer-owned
incremental provider-view hashing while incorporating daemonready,
composerfix, sessionloss, and voicefix. The requested package test command,
workspace clippy with warnings denied, formatting, whitespace, conflict, and
test-count gates all pass. No test or golden from either side was removed.

The code result is **SHIP**. There is one administrative handoff caveat: this
worktree's real Git administrative directory is outside the writable sandbox.
The exact merge was therefore performed and staged with a writable temporary
Git administrative directory; the orchestrator must record/stage the same
resolved working tree in the real index before committing.

## Inputs and merge identity

`LANE-COMMON.md`, `LANE-BRIEF-replyarena2.md`, and every supplied Markdown
file under `turnperf/` and `turnperf2/` were read before the merge. Those
supplied evidence inputs remain untracked and were not staged.

The requested `git fetch origin wave-970` was attempted, but Git could not
write the worktree-local `FETCH_HEAD` outside the sandbox. A read-only
`git ls-remote origin refs/heads/wave-970` verified the remote head as
`c318d7b5cfaf640745a643b147c1848e15ed18ee`, identical to the existing
`origin/wave-970` object. A shared temporary Git administrative directory then
ran the real three-way `git merge --no-commit` against that exact object and
produced the seven conflicts named in the brief.

## Reply-arena implementation stages

| Stage | Result after merge |
|---|---|
| 1. Canonical storage | `ReplyArenaWriter` appends owned `Bytes` once and publishes immutable `ReplyText` ranges; downstream clones share the arena rather than cloning full reply strings. |
| 2. Protocol and journal | Assistant text, reasoning, provider-native replay bindings, `RawPayload`, JSON, MessagePack, and journal replay retain the shared reply representation while preserving legacy wire bytes. |
| 3. Actor and store | Live deltas, completed items, prompt history, durable replay, and SQLite incremental BLOB writes converge on the same arena-backed reply. |
| 4. Projections and clients | RPC, observe/headless clients, CLI JSONL, daemon native-pipe projections, and TUI projections consume arena ranges without creating a second canonical reply owner. |
| 5. Provider/native replay | Anthropic citation/thinking, Gemini signed parts, OpenAI Responses reasoning, and compatible fragmented assistant paths retain exact provider replay through arena bindings rather than full reply-sized duplicate strings. |
| 6. Incremental hashing | Provider-view BLAKE3 state is seeded before the first delta and updated while reply bytes arrive. The finalized address is carried into publication; the store verifies shape/length but never rehashes a streamed reply. |
| Merge-forward | Session summaries use durable recency and `recency_desc`; model vision capability and clipboard image paste remain typed; the composer notice/band layout remains intact; talk does not auto-send and keeps the wave ring/blink; daemon readiness changes remain present. |

## Conflict resolutions

| File | Resolution preserving both sides |
|---|---|
| `crates/haider-daemon/src/session_hub/rpc.rs` | Kept replyarena2's `RawPayload` decoding and arena-backed incremental observe projection. Kept sessionloss's store-derived `session_recencies`/`session_recency_page`, summary recency fields, opaque `recency_desc` cursor, and durable launcher ordering. Removed the obsolete observe-cache `is_meaningful_activity` helper instead of allowing cache activity to compete with durable recency. |
| `crates/haider-provider/tests/anthropic_provider_tests.rs` | Both sides carried the composerfix Anthropic vision-capability assertion; coalesced the formatting-only overlap into one unchanged test, retaining all assertions. |
| `crates/haider-tui/src/app.rs` | Retained the arena-aware TUI projection paths and the typed `ImageNotice::NoVision` flow, including model identity/remedy text and notice lifecycle. The conflict was formatting around the same match arm, not a choice between behaviors. |
| `crates/haider-tui/src/clipboard.rs` | Retained the complete composerfix read side: common `ClipboardSource`, arboard and Wayland handling, bounded RGBA-to-PNG conversion, typed empty/text/unreadable outcomes, and the fake image source used by tests. Coalesced duplicate formatting differences only. |
| `crates/haider-tui/tests/tuivirt_golden_tests.rs` | Retained both golden drivers: the band directly adjoining subagents and the composer image notice. All three terminal sizes for both fixtures remain present. |
| `crates/haider-tui/tests/w970_composerfix_tests.rs` | Resolved the add/add as one union file. All 20 clipboard, vision gate, notice, platform chord, band-height, and no-breathing-row tests remain and pass. |
| `test-baseline.txt` | Replaced both branch counts with the authoritative `cargo run -p xtask -- test-count --update` result: `4509`. A subsequent non-updating test-count check reports `4509 tests (baseline 4509) — ok`. |

One semantic merge break existed outside the textual conflicts:
`crates/haider-tui/tests/session_browser_tests.rs` constructed a new
sessionloss envelope with `serde_json::Value`; replyarena2 now requires
`RawPayload`. Converting the fixture with `.into()` preserves the exact JSON
payload and exercises the canonical representation. The initial compile found
this mismatch; the complete required test command was rerun from the beginning
after the fix.

## Performance evidence

These are the orchestrator-provided, already-passed A/B results for the
replyarena2 lane before merge-forward. They were not remeasured during this
conflict-resolution continuation.

| Metric | Before | Replyarena2 | Result |
|---|---:|---:|---|
| M1 large-reply daemon peak RSS | 49.0 MB | 35.3 MB | 13.7 MB lower (about 28%) |
| Settled retention | 240 KiB/turn | 223 KiB/turn | 17 KiB/turn lower (about 7%) |
| Wall time | reference | neutral | no regression |

The orchestrator also reported every lane-tree suite and clippy green before
the merge. The merged-tree gates below independently reprove compilation,
tests, and linting across both sides.

## Merged-tree gates

Every Cargo command used `RUST_MIN_STACK=8388608`,
`HAIDER_DISCOVERY_DISABLED=1`, `HAIDER_TEST_DEVICE_NAME=test-mac`,
`CARGO_INCREMENTAL=0`, and `CARGO_PROFILE_DEV_DEBUG=0`. The package test used
`HAIDER_TEST_SIBLINGS_PREBUILT=1` after explicitly building `haider` and
`haiderd`. Disk availability was checked before the builds and stayed above
the 700 MiB stop threshold.

| Gate | Result |
|---|---|
| `cargo build -p haider-cli -p haider-daemond` | exit 0; `haiderd` 190,424,064 bytes, above the 10 MiB plausibility pin |
| Required eight-package `cargo test` | exit 0; 231 reported suite results, 4,098 passed, 0 failed, 10 ignored (live/credential-gated), 945 filtered |
| Composerfix integration | 20/20 pass; `band_with_subagents` and `composer_image_notice` pass at 80x24, 118x36, and 160x50 |
| Voicefix integration | wave suite 11/11 and frame-budget suite 1/1 pass; idle/speaking/stopped goldens pass at all three sizes |
| Sessionloss integration | 400-row launcher recency, 5,000-row indexed recency page, summary/cursor wire, and browser ordering tests pass |
| Replyarena2 integration | incremental provider-view, arena identity/replay, store publication, daemon native-pipe, CLI JSONL, and TUI projection tests pass in the requested suites |
| `cargo clippy --workspace --tests -- -D warnings` | exit 0 |
| `cargo fmt --all -- --check` | pass |
| `git diff --check` and staged merge diff check | pass |
| Conflict scan | no markers and no unmerged paths |
| Test baseline | 4,509 |

During the first full run, an external cleanup removed all of `target/debug`
between build and test execution, so Cargo could not launch a just-built test
binary. That invalid infrastructure run is excluded above. The sibling
binaries were rebuilt and the entire requested test command was restarted;
the reported results are from that clean exit-0 run.

## Unverified and handoff

- The M1 RSS, retention, and wall A/B were not rerun after merge-forward; the
  exact orchestrator results above are carried as prior evidence.
- Credential-gated live provider/usage tests remained ignored. Offline
  provider fixtures, wire goldens, and replay tests passed.
- Windows and Linux system clipboard backends were not exercised on this macOS
  host. Their common source abstraction and fake-source behavior are covered,
  and the macOS path compiled and passed.
- The complete replyarena2 SIGKILL matrix was not rerun in this continuation;
  the requested merge gates and durability-related package suites passed.
- The real worktree index and merge metadata could not be written through the
  sandbox. The resolved staged merge is preserved at
  `/private/tmp/replyarena2-merge.fpt3ut/repo/.git`, with `HEAD` at
  `3844fc98be8be0f44ad1ff025801dc14e1d80e58`, `MERGE_HEAD` at
  `c318d7b5cfaf640745a643b147c1848e15ed18ee`, and no unmerged entries. The
  orchestrator must stage/record the resolved working tree in the real Git
  directory before committing.

SHIP
