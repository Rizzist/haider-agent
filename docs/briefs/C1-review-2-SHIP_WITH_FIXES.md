codex
## Findings

1. **Medium — wrong-shape captures can still replace named fixtures.** [anthropic_live_tests.rs:315](/Users/rizzist/haider-run/haider-c1/crates/haider-provider/tests/anthropic_live_tests.rs:315) applies the same generic text check to `Text` and `ImageText`, while [anthropic_live_tests.rs:442](/Users/rizzist/haider-run/haider-c1/crates/haider-provider/tests/anthropic_live_tests.rs:442) considers any successful response with positive input/output usage “usage-heavy.” The current [text_only.sse:2](/Users/rizzist/haider-run/haider-c1/crates/haider-provider/tests/fixtures/anthropic/text_only.sse:2) satisfies both gates. Because promotion generates its golden from the same replay and the offline test checks only replay consistency, such mislabeled fixtures would be written and accepted. Add request-shape validation for `image_in`, a meaningful large-input invariant for `usage_heavy`, and negative gate tests.

Everything else traced clean:

- `reqwest::retry::never()` installs the blanket `Never` classifier, covering every class handled by reqwest’s retry policy. The constructor consumes the inspected config directly.
- `stream_sse_source` arms a fresh timeout around each `next_chunk()` call. Expiry emits one retryable `Transport`, returns, drops the response, and closes the channel; the paused-clock test verifies termination.
- Successful promotion preserves exactly the existing seven unique shapes, writes `provisional: false`, and produces a manifest accepted by the count-independent offline replay test.
- Thinking starts emit nothing for both empty and nonempty content.
- All SSE files have exactly one terminal newline; `git diff --check` passes.
- `src/anthropic_tests.rs` honors the no-inline-tests intent: test bodies are isolated in a dedicated `*_tests.rs` file, matching the existing workspace pattern.
- No regressions found in the r1-clean SSE, error, credential, CLI, actor, or dependency paths.

Validation: workspace tests passed, 207 passed and 3 expected ignored; formatting and the 210-test baseline passed. Clippy could not reacquire `target/debug/.cargo-lock` under the read-only sandbox.

VERDICT: SHIP_WITH_FIXES
hook: Stop
hook: Stop Completed
tokens used
131,743
