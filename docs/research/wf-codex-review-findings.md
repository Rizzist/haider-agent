# W-F — codex (gpt-5.6-sol xhigh) review findings on shipped W-B/W-E

Independent adversarial review, 2026-08-09, owner-directed. All line
anchors verified against the code by the coordinator (Fable) before
acceptance. Confirmed-real findings become W-F fixes; each fix gets a
regression law.

## HIGH (confirmed real — verified in code)

1. **Redirect-to-localhost SSRF** — webfetch.rs origin policy allows
   loopback HTTP (`else if !ip.is_loopback()` refuses plain-HTTP only for
   NON-loopback, webfetch.rs:187-189), and the broker `Network{host}`
   approval (worker.rs:4814) is per the ORIGINAL host. Per-hop
   re-validation re-runs the FENCE but not the APPROVAL, so an approved
   fetch of `https://attacker.example/` can 302 to
   `http://127.0.0.1:<port>/` and reach a local service unapproved.
   FIX: forbid a redirect whose target is loopback/private/link-local
   when the chain STARTED from a public host (a downgrade fence) — this
   closes public→loopback while keeping loopback→loopback (the test mock
   servers) working. (Do NOT globally ban loopback: webfetch_tests fetch
   from 127.0.0.1 mock servers.)

2. **Quadratic HTML-reducer DoS** — webfetch.rs:388: a closing
   DROP_CONTENT tag does `drop_stack.iter().rposition(...)`. Input of
   `<script>`×N (pushes N) then `</style>`×N (each an O(N) scan finding
   nothing) is O(N²); within the 4-MiB source cap N≈500k → CPU
   exhaustion. FIX: bound drop_stack depth (e.g. ≤64; ignore deeper
   opens) or restructure so a closing tag is O(1)/O(log). Law: a crafted
   4-MiB input reduces in bounded time/ops.

3. **UTF-8 panic in entity decode** — webfetch.rs:534:
   `rest[..rest.len().min(12)]` slices at byte 12 which can split a
   multi-byte codepoint (`&aaaaaaaaaaé;`) → panic on non-char-boundary.
   FIX: char-boundary-safe scan (char_indices / find ';' without a raw
   byte slice). Law: the crafted input reduces without panic.

4. **alpha/search unbounded read** — web_search.rs:127: `response.bytes()`
   buffers the whole body before the 32-KiB cap / status handling.
   Endpoint is chatgpt.com (Bearer-authed, semi-trusted) so practical
   risk is lower, but bound the read for defense-in-depth. FIX: bounded
   streaming read (reuse the read_body_bounded pattern).

## MEDIUM (confirmed real)

5. **Public-IP classifier gaps** — origin.rs:263 / openai.rs:3064 omit
   `198.18.0.0/15` (RFC 2544 benchmarking) and `240.0.0.0/4` (reserved,
   Class E); also check `192.0.0.0/24`, `192.0.2.0/24`/`198.51.100.0/24`
   /`203.0.113.0/24` (TEST-NET), `100.64.0.0/10` (CGNAT), `::/128`,
   IPv4-mapped IPv6, `64:ff9b::/96` (NAT64). FIX: extend the blocked
   set; law sweeps the added ranges both directions.

6. **Slowloris (no whole-body deadline)** — webfetch.rs:269: the 30s
   timeout is PER-CHUNK and resets each chunk; 1 byte / 29s holds the
   fetch open indefinitely. FIX: an overall fetch deadline (wall clock
   across all hops+chunks). Law: a slow-drip mock is aborted by the
   deadline.

## LOW / minor

7. **W-E phase-totality** — style.rs:354: the `len + SHIMMER_TAIL` (11)
   period doesn't divide the wrapping u8 clock (256), so phase 255→0
   jumps centre 2→0 every ~154 animated seconds (one-frame cosmetic
   hiccup). FIX (optional): drive the sweep off `phase % period` computed
   from a period that divides evenly, or accept as cosmetic + document.
   NOTE the theoretical `len==usize::MAX-2` zero-divisor is NOT reachable
   (verb is "thinking", 8 chars) — do not spend effort there beyond a
   debug_assert.

8. **W-E render allocs** — render.rs:246: 8 one-char `String`s per
   thinking repaint (~240/s at 30fps). FIX (optional): static/&str
   glyphs or a reused buffer.

9. **web_fetch off-by-one truncated flag** — webfetch.rs:291: a body
   exactly 4 MiB is marked truncated before checking EOF. Minor
   correctness. FIX: only mark truncated when more bytes remain.

10. **Same-turn 404/410 probing** — worker.rs:3448/4953: a gone
    alpha/search endpoint changes only the NEXT-turn capability; same
    turn keeps advertising web_search until the provider-request ceiling.
    Low (bounded). FIX (optional): latch degraded within the turn too.

## Not W-F

- W-E is otherwise shippable; W-B's core (opaque replay, per-pair
  advertisement) reviewed clean. These findings are hardening of shipped
  code, not a redesign.
