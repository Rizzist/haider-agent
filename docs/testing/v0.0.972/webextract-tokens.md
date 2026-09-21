# Web extraction token economics

This report measures the phase-3 fix candidate with the estimator
that Haider uses for provider-bound text:
`ceil(serialized JSON-string bytes / 4)`. It is a stable provider-neutral
estimate, not a model tokenizer result.

The fix candidate closes all three measured correctness gaps. Script-only
pages carry the documented marker, the table-heavy fixture is a real Markdown
table and narrowly beats legacy, and a live result above the 16 KiB model cap
pages through its standard `cap:<call_id>` alias. Across all four fixtures the
current reducer saves 22.1% versus legacy and 60.9% versus bounded passthrough.

## Method

The committed harness in
`crates/haider-core/src/webextract_token_harness_tests.rs` expands four
deterministic templates, serves them from a loopback HTTP server, calls the
production guarded fetcher, builds the daemon's exact `BoundedResult`, applies
the real actor-owned model projection, and accounts for both boundaries with
`haider_tools::estimated_text_tokens`.

The arms are:

1. **Current**: the landed extractor and reducer at this candidate.
2. **Legacy**: the same harness applied without committing to phase-0 commit
   `0305498c`, which is the last reducer before `haider-webextract` integration.
3. **Passthrough**: a disposable worktree at this candidate with
   `reduce_response_body` changed to return the UTF-8 response bytes directly.
   The production 96 KiB fetch-output cap and actor model-boundary caps remain
   enabled. That scratch change is not in the candidate.

Each arm fetched freshly generated fixtures from its own local server. The
templates expand to 159,612 bytes (bloated static HTML), 60,491 bytes (JS
shell), 52,307 bytes (table-heavy HTML), and 94,039 bytes (JSON API).

## Fetch-result tokens

Positive percentages are savings by the current reducer. A negative value is
a regression.

| Fixture | Current | Legacy | Bounded passthrough | Current vs legacy | Current vs passthrough |
|---|---:|---:|---:|---:|---:|
| Bloated static | 1,730 | 4,861 | 24,645 | 64.4% | 93.0% |
| JS shell | 32 | 13 | 15,141 | -146.2% | 99.8%[^js] |
| Table-heavy | 8,093 | 8,118 | 13,096 | 0.3% | 38.2% |
| JSON API | 21,534 | 27,311 | 27,311 | 21.2% | 21.2% |
| **Total** | **31,389** | **40,303** | **80,193** | **22.1%** | **60.9%** |

[^js]: The 19-token increase versus legacy is intentional honesty: the current
    result includes `[haider_js_shell: no readable static content; JavaScript
    rendering required]` instead of silently returning only the fetch header.

The brief estimated about 50–70% savings versus the then-current reducer and
about 90% versus untruncated passthrough. The bloated static fixture lands in
both bands. The aggregate remains below those illustrative bands; there is no
aggregate hard gate. JSON reduction is modest, while the corrected table path
beats legacy by 25 estimated tokens without losing table structure.

## Provider-bound model tokens

This second table accounts for the exact projection sent to the model. The
actor caps an otherwise untruncated `web_fetch` preview at 16 KiB and applies
the generic 8 KiB path when the producer already declared truncation. These
caps make the three arms converge, so they must not be confused with reducer
savings.

| Fixture | Current | Legacy | Bounded passthrough | Current vs legacy | Current vs passthrough |
|---|---:|---:|---:|---:|---:|
| Bloated static | 1,730 | 4,141 | 2,103 | 58.2% | 17.7% |
| JS shell | 32 | 13 | 4,108 | -146.2% | 99.2%[^js] |
| Table-heavy | 4,143 | 4,143 | 4,108 | 0.0% | -0.9% |
| JSON API | 4,334 | 4,752 | 4,752 | 8.8% | 8.8% |
| **Total** | **10,239** | **13,049** | **15,071** | **21.5%** | **32.1%** |

## What the landed producer cap saves

The passthrough arm retains the landed 96 KiB fetch-output cap. Only the
bloated static response crosses that cap.

| Fixture | Untruncated passthrough | Bounded passthrough | Tokens saved by producer cap | Saving |
|---|---:|---:|---:|---:|
| Bloated static | 39,968 | 24,645 | 15,323 | 38.3% |
| JS shell | 15,141 | 15,141 | 0 | 0.0% |
| Table-heavy | 13,096 | 13,096 | 0 | 0.0% |
| JSON API | 27,311 | 27,311 | 0 | 0.0% |
| **Total** | **95,516** | **80,193** | **15,323** | **16.0%** |

## Content and paging checks

The current extraction arm records two semantic checks beside the token
counts, and both are green:

- `js_shell_marker=true`: the script-only fixture degrades with the documented
  marker instead of a silently empty body.
- `table_preserved=true`: the selected table root emits its header, GFM
  separator, and all rows as a Markdown table.

A live daemon regression fetches a 63,000-byte loopback document in a real
session, verifies that the durable result has `cursor="cap:fetch-page"` and a
capture artifact, and reads the complete start/end-delimited result through
the actual `task_output` route. It then evicts the volatile alias and rebuilds
it from the durable fetch result, pinning restart lookup as well as live paging.

## Reproduction

Run all Cargo commands through the shared-machine slot and keep the target in
the worktree:

```sh
source /Users/rizzist/Developer/haiderharness/env.sh
export CARGO_TARGET_DIR="$PWD/target"
export HAIDER_WEBEXTRACT_TOKEN_ARM=current
export HAIDER_WEBEXTRACT_TOKEN_OUTPUT=/tmp/webextract-current.json
/Users/rizzist/Developer/haiderharness/runtime/build-slot.sh webextract-current -- \
  cargo test -p haider-core --lib --locked \
  webextract_token_harness_tests::measure_webextract_token_economics -- \
  --ignored --exact --nocapture
```

For the legacy arm, create a disposable detached worktree at `0305498c`, apply
this candidate without committing so that it supplies only the harness, set
the arm to `legacy`, and run the same command. For passthrough, use a disposable
worktree at this candidate, replace only `reduce_response_body` with raw
lossy-UTF-8 passthrough, set the arm to `passthrough`, and run the command. Keep
the production caps in both comparative arms and remove the scratch worktrees
afterward.

The recorded raw reports have these SHA-256 digests:

| Evidence | SHA-256 |
|---|---|
| `current.json` | `7f865e4cae2e4996876b32d7448099f77463c34179755eaceb5d94bfeea487a1` |
| `legacy.json` | `cc7a4614cc29d8240c8445209a24b89fc27b1436f0f9bda5e0c835e7311344ac` |
| `passthrough.json` | `b9c92bddb29c656806ecff049b168b4c7902ed0e9334b0050af35888f1a7eae9` |
