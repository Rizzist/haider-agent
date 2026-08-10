# W-F fetch-hardening mutation notes

Executed kills for the `wf-codex-review-findings` HIGH set, branch
`wf-fetch-hardening`, 2026-08-10. All four (H1–H4) were EXECUTED end-to-end:
commit-before-mutation (each fix is its own commit; tree clean at `c93af87`
when the kills ran) → a single `python3` anchor asserting `count==1` → the ONE
named test in isolation ("running 1 test") → observed RUNTIME failure (never a
compile error) → `git checkout --` revert → re-run green.

## EXECUTED kills

### H1 — SSRF redirect downgrade fence

- Fix commit: `e05c7b7` (tree clean at `c93af87`).
- Anchor (`crates/haider-provider/src/webfetch.rs`, `count==1`):
  `if forbid_public_downgrade && !public {` →
  `if false && forbid_public_downgrade && !public {`
  (defeats the downgrade fence; a public chain may again be redirected onto
  loopback — the original SSRF).
- Test in isolation:
  `cargo test -p haider-provider --lib public_chain_refuses_a_downgrade_redirect_to_non_public`
  → `running 1 test`.
- Observed RUNTIME failure:
  `test webfetch_tests::public_chain_refuses_a_downgrade_redirect_to_non_public ... FAILED`,
  panic at `webfetch_tests.rs:135` on the `expect_err("public->loopback
  downgrade must be refused")` — with the fence disabled, validating
  `http://127.0.0.1:8080/service` from a public chain now SUCCEEDS, so the
  `expect_err` panics. `FAILED. 0 passed; 1 failed`.
- Revert `git checkout -- crates/haider-provider/src/webfetch.rs`; re-ran →
  `ok. 1 passed`.

### H2 — quadratic HTML-reducer bound

- Fix commit: `4d98c42` (tree clean at `c93af87`).
- Anchor (`webfetch.rs`, `count==1`):
  `drop_stack.len() < MAX_DROP_STACK_DEPTH` →
  `drop_stack.len() < MAX_DROP_STACK_DEPTH.saturating_mul(1_000_000)`
  (effectively removes the depth cap → the O(N²) close scan is restored, and the
  const stays referenced so the mutation compiles).
- Test in isolation:
  `cargo test -p haider-provider --lib html_reducer_is_bounded_on_adversarial_nested_drop_tags`
  → `running 1 test`.
- Observed RUNTIME failure (at the 5s budget, not a hang):
  `... FAILED`, panic at `webfetch_tests.rs:187`: **"adversarial reduce must
  finish within the bound — an O(N^2) impl would not: Timeout"** — the reduce
  thread does not deliver within the 5s `recv_timeout`, so the `expect` fires.
  `FAILED. 0 passed; 1 failed ... finished in 5.03s`.
- Revert `git checkout -- crates/haider-provider/src/webfetch.rs`; re-ran →
  `ok. 1 passed ... finished in 0.42s` (bounded build finishes fast).

### H3 — codepoint-safe entity terminator scan

- Fix commit: `9291c1d` (tree clean at `c93af87`).
- Anchor (`webfetch.rs`, `count==1`): the char-boundary scan
  ```
  let Some(end) = rest
      .char_indices()
      .take_while(|(index, _)| *index < 12)
      .find(|(_, character)| *character == ';')
      .map(|(index, _)| index)
  else {
  ```
  → the original panicking byte slice
  `let Some(end) = rest[..rest.len().min(12)].find(';') else {`.
- Test in isolation:
  `cargo test -p haider-provider --lib entity_decode_does_not_panic_on_multibyte_boundary`
  → `running 1 test`.
- Observed RUNTIME failure:
  `... FAILED`, panic at `webfetch.rs:670`: **"byte index 12 is not a char
  boundary; it is inside 'é' (bytes 11..13) of `&aaaaaaaaaaé;`"** — exactly the
  hostile input the fix guards. `FAILED. 0 passed; 1 failed`.
- Revert `git checkout -- crates/haider-provider/src/webfetch.rs`; re-ran →
  `ok. 1 passed`.

### H4 — bounded search-body read

- Fix commit: `7963251` (tree clean at `c93af87`).
- Anchor (`crates/haider-daemon/src/web_search.rs`, `count==1`):
  `if chunk.len() >= remaining {` → `if chunk.len() >= usize::MAX {`
  (the clamp/break never triggers → the whole oversized body is buffered,
  defeating the cap, and `remaining` stays referenced so it compiles).
- Test in isolation:
  `cargo test -p haider-daemon --lib production_transport_caps_the_response_body`
  → `running 1 test`.
- Observed RUNTIME failure:
  `... FAILED`, `assertion left == right failed: an oversized body is clamped to
  exactly the cap, never buffered whole` — **left: 1114112** (the whole
  1 MiB + 64 KiB body) vs **right: 1048576** (the 1 MiB cap). `FAILED. 0 passed;
  1 failed`.
- Revert `git checkout -- crates/haider-daemon/src/web_search.rs`; re-ran →
  `ok. 1 passed`.

## MEDIUM / LOW (reasoned production-mutation → observing-law pairs)

| # | Production mutation | Observing law | Expected RUNTIME failure |
|---|---|---|---|
| M5 | Drop one added range from `blocked_ipv4/ipv6_credential_target` (e.g. the `240.0.0.0/4` or `64:ff9b::/96` arm). | `openai::tests::m5_classifier_blocks_added_special_use_ranges_both_directions` | The dropped range's in-range representative asserts `blocked_credential_target(..)` and now returns false → the `must be blocked` assert fails. |
| M6 | Revert the body read to the per-chunk `timeout(CHUNK_IDLE_TIMEOUT, …)` (no absolute deadline). | `webfetch_tests::slow_drip_body_is_aborted_by_the_overall_deadline` | The 300ms deadline never fires; the drip holds until the 30s idle timeout, so `started.elapsed() < 10s` fails (and the message no longer contains "overall"). |
| M9 | Restore `if chunk.len() >= remaining { … return (body, true) }` (flag at the boundary, no extra read). | `webfetch_tests::source_cap_boundary_truncation_is_off_by_one_honest` | An exactly-4-MiB body with clean EOF is flagged truncated → `assert!(!at.truncated)` fails. |
| W7 | Change the shimmer period so `shimmer_centre` no longer advances one glyph per tick. | `we_thinking_shimmer_tests::le2_the_sweep_travels_and_wraps` (unchanged; the doc/debug_assert edit keeps it green as coverage) | The `0,1,…,len-1,(rest)` sequence assertion fails. |
| W8 | Feed the shimmer loop a glyph table that mis-spells `VERB`. | `debug_assert_eq!(VERB_GLYPHS.concat(), VERB, …)` in `render.rs` (debug/test builds) + `we_thinking_shimmer_tests::le2` | The debug_assert fires ("the glyph table must spell the verb"); the rendered crest columns diverge from 0→1→2. |

## Review of record (coordinator, Fable)

Verified the H1 downgrade-fence design in-code (webfetch.rs:115-122,256-260):
the loop records `chain_started_public` on the first validated hop and,
once public, `validate_fetch_target(forbid_public_downgrade=true)` refuses
any non-public target — closing public→loopback SSRF at the ENGINE (the
authoritative socket-reaching layer), independent of the broker's per-host
approval, while loopback→loopback (chain_started_public=Some(false)) stays
allowed so the loopback mock-server laws work. Correct placement.

The lane executed all four HIGH kills with exact predicted failures (H1
expect_err, H2 5.03s timeout vs budget, H3 "byte index 12 is not a char
boundary; inside 'é'", H4 whole-body 1114112 vs cap 1048576). Spot-checked
against the notes — consistent. M5's deliberate TEST-NET exclusion (those
ranges are safe public stand-ins used by existing origin laws) is correct
reasoning, not a gap. Campaign ACCEPTED.
