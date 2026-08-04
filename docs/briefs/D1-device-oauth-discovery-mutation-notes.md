# D1 device-oauth discovery + import — mutation notes

Every kill below was EXECUTED on 2026-08-04: the production mutation was
applied, the single named observer was run with
`cargo test -p haider-daemon --lib -- --exact <observer> --test-threads=1`
(output showed `running 1 test` and the test FAILED at runtime, in the
re-spawned isolated child where applicable), then the mutation was reverted
and the full gates were re-run green. A compile failure is never the claimed
evidence.

Scope note: refresh-now was CUT by owner amendment before this wave finished,
so the brief's two refresh laws (`refresh_now_rides_the_serialized_lease…`,
`refresh_now_expired_terminal_names_relogin_typed`) have no surface to
mutate. The only refresh-now artifact the dead lane produced — a feature-bit
doc comment in `frame.rs` — was excised; the golden-transcript law asserts no
D1 frame carries a refresh action and that `account.refresh` decodes as an
Unknown method.

## K1 — token-bytes-leak (EXECUTED, reverted)

- Mutation: `device_discovery.rs::discover_codex` — project the store's
  access token into the public candidate:
  `let account_label = Some(String::from_utf8_lossy(&parsed.tokens.access_token.0).into_owned());`
- Observer: `device_discovery::tests::discovery_reports_metadata_never_token_bytes`
- Observed RUNTIME failure: child panic
  `discovery response leaked token material eyJhbGciOiJSUzI1NiIs…` — the
  serialized `account.device_candidates` bytes contained the structurally
  real fixture JWT.

## K2 — skip-silently broken (EXECUTED, reverted)

- Mutation: `device_discovery.rs::discover_kimi` — on a parse failure,
  return a "malformed store" candidate instead of `None`
  (`match serde_json::from_slice { … Err(_) => return Some(candidate(… "store is malformed" …)) }`).
- Observer: `device_discovery::tests::absent_or_malformed_stores_are_skipped_silently`
- Observed RUNTIME failure: child panic
  `absent/malformed stores must be skipped silently, got [DeviceCandidate { … kimi-oauth … }]`
  — the wrong-shape kimi store became visible instead of indistinguishable
  from an absent one.

## K3 — bounded read removed (EXECUTED, reverted)

- Mutation: `device_discovery.rs::read_bounded` — read with
  `file.take(u64::MAX)` and return `Some(bytes)` unconditionally (byte cap
  and size check both gone).
- Observer: `device_discovery::tests::absent_or_malformed_stores_are_skipped_silently`
- Observed RUNTIME failure: child panic — the 300 KiB (over the 256 KiB
  `DISCOVERY_FILE_LIMIT`) gemini store parsed and surfaced a candidate:
  `…, got [DeviceCandidate { … provider: "gemini", … }]`.

## K4 — receipt dropped (EXECUTED, reverted)

- Mutation: `accounts.rs::handle_device_import` — erase the candidate from
  the durable receipt identity: `let receipt_candidate = None;`
- Observer: `accounts::accounts_tests::import_device_is_receipted_and_lands_a_working_account`
- Observed RUNTIME failure: child assertion
  `left: {"source":"codex","alias":"openai-oauth","provider":"openai-oauth"}`
  vs `right: {…,"candidate":"dc1_…"}` — the durable receipt no longer named
  the imported device candidate.

## K5 — unsupported-guessed (EXECUTED, reverted)

- Mutation: `device_discovery.rs::discover_gemini` — claim the store is
  importable: `import_supported = true`, `unsupported_reason = None`.
- Observer: `accounts::accounts_tests::unsupported_candidate_is_honest_not_guessed`
- Observed RUNTIME failure: child assertion
  `assertion failed: !gemini.wire.import_supported` — the guessed candidate
  broke the honesty law before any parser could be misapplied; the same test
  also pins that the import path refuses with the reported reason and
  commits/receipts nothing.

## K6 — disabled switch ignored (EXECUTED, reverted)

- Mutation: `device_discovery.rs::discover_device_candidates` — neuter both
  switches: `if false && (disabled || discovery_disabled_by_env())`.
- Observer: `device_discovery::tests::discovery_profile_switch_disables_and_stays_honest`
- Observed RUNTIME failure: child assertion
  `assertion failed: discover_device_candidates(true).is_empty()` — a
  disabled profile still scanned the device.

## Path-outside-home traversal — not applicable, with reasoning

No wire input ever names a path. `account.import_device` carries only the
opaque candidate id (`dc1_` + 64 hex, shape-checked by
`valid_device_candidate_id` at the RPC edge before the actor is consulted),
and the daemon re-runs discovery to resolve it — the paths probed are always
recomputed from `HOME` plus daemon-local env overrides, which are trusted
process configuration, not client input. A client therefore has no channel
through which to steer discovery or import at a path outside the profile's
own store locations, and there is no traversal surface to mutate. The
adjacent laws are held instead by the id-shape check, the unavailable-id
refusal (`device credential candidate is unavailable`, exercised by the
`unsupported_candidate_is_honest_not_guessed` observer), and K3's byte-bound
kill.
