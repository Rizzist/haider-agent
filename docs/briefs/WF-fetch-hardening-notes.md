# W-F fetch-hardening notes

Fix-first pass over `docs/research/wf-codex-review-findings.md` (independent
gpt-5.6-sol xhigh adversarial review of shipped W-B/W-E, coordinator-verified).
Every HIGH (H1–H4), the confirmed MEDIUM findings (M5 classifier, M6 slowloris,
M9 truncation off-by-one), and the two LOW W-E items fixed on branch
`wf-fetch-hardening` (off `main` at v0.0.81), each with a regression law that
FAILS without the fix and passes with it. No real network in any test —
loopback mock servers and scripted inputs only. Test ledger 2143 → 2149.
Commits are per-finding so an interruption preserves progress.

## Commits

| Commit | Scope |
|---|---|
| `e05c7b7` | webfetch.rs + webfetch_tests.rs — H1 SSRF redirect downgrade fence |
| `4d98c42` | webfetch.rs + webfetch_tests.rs — H2 bounded HTML-reducer drop stack |
| `9291c1d` | webfetch.rs + webfetch_tests.rs — H3 codepoint-safe entity scan |
| `7963251` | web_search.rs — H4 bounded streaming search-body read |
| `2175a70` | openai.rs + openai_tests.rs — M5 public-IP classifier ranges |
| `51d4a58` | webfetch.rs + tests/webfetch_tests.rs + lib.rs — M6 deadline, M9 truncation |
| `c93af87` | render.rs + style.rs — W-E low items (static glyphs, shimmer doc) |

## Per-finding report

### HIGH

- **H1 — redirect-to-localhost SSRF.**
  `haider-provider/src/webfetch.rs`. The per-hop origin fence allows plain HTTP
  to LOOPBACK (the mock-server allowance) and re-validates the FENCE per hop,
  but the broker's `Network{host}` approval is per the ORIGINAL host — so an
  approved public fetch of `https://attacker.example/` could 302 to
  `http://127.0.0.1:<port>/` and reach a local service, each hop passing the
  fence in isolation. Fix: a DOWNGRADE fence in the engine. The fetch loop
  records whether hop 0 resolved PUBLIC (`chain_started_public`); on every later
  hop `validate_fetch_target(url, resolver, forbid_public_downgrade)` REFUSES
  (InvalidRequest) any target that resolves to a non-public address once the
  chain started public. This closes public→loopback/private/link-local at the
  engine regardless of what host the broker approved, while keeping
  loopback→loopback (the test mock path) and public→public untouched. The broker
  approval semantics are deliberately unchanged — the authoritative closure is
  the engine fence, which is where the SSRF actually reaches the socket.
  Law: `webfetch_tests::public_chain_refuses_a_downgrade_redirect_to_non_public`.
  Executed mutation kill (see mutation notes).

- **H2 — quadratic HTML-reducer DoS.**
  `webfetch.rs::reduce_html_to_text`. A closing DROP_CONTENT tag scans the drop
  stack (`drop_stack.iter().rposition(...)`); `<script>`×N then `</style>`×N is
  O(N²) (each close scans the N-deep stack, matching nothing), and within the
  4 MiB source cap N≈200k → CPU exhaustion. Fix: cap the stack at
  `MAX_DROP_STACK_DEPTH = 64`; opens past the cap are ignored (content is still
  dropped while the stack is non-empty), so every close is O(1). A reduction of
  hostile input, not a fidelity contract — legitimate documents never nest
  drop-chrome this deep.
  Law: `webfetch_tests::html_reducer_is_bounded_on_adversarial_nested_drop_tags`
  (reduce runs on its own thread against a 5s budget, so a regressed quadratic
  build FAILS at the budget instead of hanging the suite).
  Executed mutation kill (see mutation notes).

- **H3 — UTF-8 panic in entity decode.**
  `webfetch.rs::decode_entities`. `rest[..rest.len().min(12)].find(';')` slices
  at byte 12, which can fall INSIDE a multibyte codepoint (`&aaaaaaaaaaé;`, where
  `é` spans bytes 11–12) → panic on a non-char-boundary. Fix: a char-boundary
  scan — `rest.char_indices().take_while(|(i,_)| *i < 12).find(|(_,c)| *c == ';')`
  — that never slices mid-codepoint (`;` is ASCII, so its index equals its byte
  offset, preserving the original 12-byte window semantics).
  Law: `webfetch_tests::entity_decode_does_not_panic_on_multibyte_boundary`.
  Executed mutation kill (see mutation notes).

- **H4 — alpha/search unbounded read.**
  `haider-daemon/src/web_search.rs::ReqwestWebSearchHttp::post_json` used
  `response.bytes()`, buffering the WHOLE body from the unofficial
  `chatgpt.com/alpha/search` endpoint before the 32 KiB downstream result cap
  ever applied. Fix: `read_body_capped` streams under
  `SEARCH_RESPONSE_CAP_BYTES = 1 MiB` (webfetch `read_body_bounded` pattern) —
  generous enough that legitimate results still parse, bounded so a
  runaway/compromised response cannot exhaust memory. Endpoint is Bearer-authed
  and semi-trusted, so this is defense-in-depth.
  Law: `web_search::web_search_tests::production_transport_caps_the_response_body`
  (loopback server, oversized body clamped to exactly the cap).
  NOTE: this law lives in the file's pre-existing inline `#[cfg(test)] mod
  web_search_tests`, which `xtask test-count` does not scan (only `tests/` dirs
  and `*_tests.rs` files count), so it is NOT machine-counted — same as its three
  shipped siblings. Left co-located with them rather than split into a new file.
  Executed mutation kill (see mutation notes).

### MEDIUM

- **M5 — public-IP classifier gaps.**
  `haider-provider/src/openai.rs::blocked_ipv4/ipv6_credential_target` — the
  base classifier the web_fetch public fence (`blocked_public_web_target`), the
  fixed-origin fence, and the OpenAI-compatible fence all derive from. Added:
  `100.64.0.0/10` (CGNAT / RFC 6598), `198.18.0.0/15` (RFC 2544 benchmarking),
  `192.0.0.0/24` (IETF protocol assignments), `240.0.0.0/4` (reserved / Class E),
  and IPv6 `64:ff9b::/96` (NAT64 well-known prefix — e.g. `64:ff9b::7f00:1`
  embeds `127.0.0.1`). `::/128` and IPv4-mapped IPv6 were already covered.
  DELIBERATELY NOT ADDED: the TEST-NET ranges `192.0.2.0/24`, `198.51.100.0/24`,
  `203.0.113.0/24`. They are guaranteed-never-routed documentation ranges
  (RFC 5737), so blocking them adds no real SSRF protection, and the existing
  origin laws use them as SAFE PUBLIC stand-ins (e.g. the webfetch redirect
  re-validation law redirects to `198.51.100.7` to exercise the public-plain-HTTP
  rule without any risk of a real dial). Blocking them would silently change what
  those laws exercise. Law:
  `openai::tests::m5_classifier_blocks_added_special_use_ranges_both_directions`
  (each added range swept in-range=blocked / just-outside=allowed).

- **M6 — slowloris (no whole-body deadline).**
  `webfetch.rs`. The 30s chunk-idle timeout RESETS every chunk, so a
  1-byte-per-29s drip holds a fetch open indefinitely. Fix: an ABSOLUTE
  `WEB_FETCH_TOTAL_DEADLINE = 120s` computed once at fetch start and enforced
  across all hops and chunks via `within_fetch_deadline` / `timeout_at` — a
  timeout at/after the deadline is the typed overall-deadline Transport abort, a
  timeout inside it stays the per-op (open / chunk-idle) abort. Deadline-
  injectable seam `fetch_public_url_with_deadline` lets the law drive a short
  deadline. Law:
  `webfetch_tests::slow_drip_body_is_aborted_by_the_overall_deadline` (loopback
  server sends headers then holds; a 300ms injected deadline aborts it).

- **M9 — off-by-one truncated flag at the source cap.**
  `webfetch.rs::read_body_bounded` flagged `truncated` whenever a chunk reached
  `>= remaining`, so a body of EXACTLY 4 MiB was marked truncated before EOF was
  known. Fix: only flag when MORE bytes than the cap arrive (`chunk.len() >
  remaining`), and at an exact-cap fill do ONE honest extra read — more bytes ⇒
  truncated, clean EOF ⇒ not. Law:
  `webfetch_tests::source_cap_boundary_truncation_is_off_by_one_honest` (exactly
  4 MiB with clean EOF is NOT truncated; one byte past IS).

### LOW (W-E)

- **W7 — shimmer phase-totality.**
  `haider-tui/src/style.rs::shimmer_centre`. The shared u8 clock wraps at 256,
  which is not a multiple of `len + SHIMMER_TAIL`, so the sweep centre jumps once
  per wrap (~154 animated seconds) — a one-frame cosmetic hiccup. Documented as
  an accepted artifact (a clean fix would require a wider clock than the pure
  `(phase, len)` contract allows) plus a `debug_assert` pinning the unreachable
  `len + SHIMMER_TAIL` overflow invariant. Behaviour unchanged; W-E laws LE1–LE7
  stay green.

- **W8 — thinking-frame allocations.**
  `haider-tui/src/render.rs::thinking_line` allocated 8 one-char `String`s (plus
  a `format!` for the dot) per repaint (~240/s at 30fps). Fix: a static
  `VERB_GLYPHS: [&str; 8]` table (with a `debug_assert` lockstep to `VERB`) and a
  static dot-with-space slice — zero per-frame allocation. Rendered output is
  byte-identical, so all W-E laws stay green.

## Verification

- Per-crate suites green: `haider-provider` (full lib + integration, incl. all 7
  new provider laws), `haider-daemon --lib -- --test-threads=4` (342 passed,
  incl. H4), `haider-tui` shimmer/style/render/subagent suites (W-E laws LE1–LE7
  + app_render + subagent green after the render/style edits).
- All four HIGH fixes executed-kill-verified (commit-before-mutation → single
  `count==1` anchor → the ONE named test in isolation ["running 1 test"] →
  observed RUNTIME failure → `git checkout --` revert → green). See mutation
  notes.
- `cargo fmt --all -- --check` exit 0 at every commit; no leftover
  merge-conflict markers; ledger honestly 2149 (H4's law is in the uncounted
  inline module).
- Pre-existing W-B web laws (origin matrix, redirect re-validation, redirect cap,
  output cap, content-type gate, 4 MiB source cap) and W-E shimmer laws stay
  green.
