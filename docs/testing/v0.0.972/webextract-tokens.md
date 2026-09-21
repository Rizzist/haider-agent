# Web extraction token economics

This report measures the landed phase 0/1 extraction stack with the estimator
that Haider uses for provider-bound text:
`ceil(serialized JSON-string bytes / 4)`. It is a stable provider-neutral
estimate, not a model tokenizer result.

The measurement verdict is **HOLD** against the complete phase-3 brief. The
bloated static page meets both estimates, but the four-fixture aggregate saves
only 19.0% versus the legacy reducer and 59.3% versus bounded passthrough. The
JS-shell result has no honest shell marker, the selected subtree does not
preserve the table as a Markdown table, and a live `cap:<id>` retrieval of a
model-capped fetch fails.

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
| JS shell | 13 | 13 | 15,141 | 0.0% | 99.9%[^js] |
| Table-heavy | 9,352 | 8,118 | 13,096 | -15.2% | 28.6% |
| JSON API | 21,534 | 27,311 | 27,311 | 21.2% | 21.2% |
| **Total** | **32,629** | **40,303** | **80,193** | **19.0%** | **59.3%** |

[^js]: The current JS-shell payload is only the 47-byte fetch header. The
    extractor returned no readable body and emitted neither `haider_js_shell`
    nor another `js-shell` marker. This is not a usable 99.9% optimization.

The brief estimated about 50–70% savings versus the then-current reducer and
about 90% versus untruncated passthrough. The bloated static fixture lands in
both bands. The aggregate misses both bands; only the comparison with
passthrough is directionally close. JSON reduction is modest, and the
table-heavy result is larger than legacy.

## Provider-bound model tokens

This second table accounts for the exact projection sent to the model. The
actor caps an otherwise untruncated `web_fetch` preview at 16 KiB and applies
the generic 8 KiB path when the producer already declared truncation. These
caps make the three arms converge, so they must not be confused with reducer
savings.

| Fixture | Current | Legacy | Bounded passthrough | Current vs legacy | Current vs passthrough |
|---|---:|---:|---:|---:|---:|
| Bloated static | 1,730 | 4,141 | 2,103 | 58.2% | 17.7% |
| JS shell | 13 | 13 | 4,108 | 0.0% | 99.7%[^js] |
| Table-heavy | 4,479 | 4,143 | 4,108 | -8.1% | -9.0% |
| JSON API | 4,334 | 4,752 | 4,752 | 8.8% | 8.8% |
| **Total** | **10,556** | **13,049** | **15,071** | **19.1%** | **30.0%** |

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
counts:

- `js_shell_marker=false`: the empty JS shell does not degrade with the marker
  required by the brief.
- `table_preserved=false`: the current main-content selection emits rows but
  not the fixture's Markdown header and separator. Its 9,352-token result is
  therefore both larger than legacy and not structurally preserved.

A measurement-only live daemon probe fetched a 63,000-byte loopback document
in a real session. The durable `ToolResult` had a 63,049-byte preview,
`truncated=false`, `cursor=null`, and `artifact=null`. Paging
`cap:fetch-page` through the existing task-output capture path failed with
`unknown capture in this session`. The model projection is capped transiently,
but the fetch never registers the promised conversation-local capture alias.
The probe was removed after recording evidence because phase 3 is measurement
only. Paged retrieval is therefore **not verified working**; it is a blocking
contract gap.

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
| `current.json` | `65933a7b35df9e5674b6a0ef54d7570b507af01b8c89ba6a31a4f3e7f31589ae` |
| `legacy.json` | `cc7a4614cc29d8240c8445209a24b89fc27b1436f0f9bda5e0c835e7311344ac` |
| `passthrough.json` | `b9c92bddb29c656806ecff049b168b4c7902ed0e9334b0050af35888f1a7eae9` |
| `paging-live.json` | `3abe605ac9562c8bdbf4f4c1b03b1402b970dd89961920cefdad66bb4cfa967b` |
