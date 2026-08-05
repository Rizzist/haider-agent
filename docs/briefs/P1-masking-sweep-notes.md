# P1 — masking sweep — notes

Lane P1, branch `p1-masking-sweep` from v0.0.70. Owner directive: emails
and sensitive identity strings auto-hidden EVERYWHERE (streamer-friendly)
— first char + `*` per part with a reveal affordance — extending the ONE
mask authority U2 shipped inside `/usage` (`format::mask_identity` +
per-visit `r` reveal) to every other surface that renders account
identity. Extend, don't fork: no second dialect, U2's tests untouched.

## Honest inventory (every surface that renders an identity string)

Grepped for identity/email/label rendering across `haider-tui`; the
daemon side confirmed what each string IS (`accounts.rs`: API-key logins
mint `"{provider} api key"`, OAuth bundles carry `display_identity` = the
signed-in email; `device_discovery.rs`: `account_label` = the store's JWT
email / account id).

| # | Surface | What renders | Verdict |
|---|---|---|---|
| 1 | `/usage` identity line (`render.rs`) | email/handle | ALREADY MASKED (U2) — untouched |
| 2 | `/accounts` rows (`render.rs` `render_accounts`) | `row.identity` — email for OAuth, key fragment for API | **LEAKED → masked + `r` reveal** |
| 3 | "found on this device" rows (`push_device_candidates_section`, shared by `/accounts` + `/providers`) | `candidate.account_label` — the store's signed-in email | **LEAKED → masked + per-screen reveal** |
| 4 | Device-import receipt (`live.rs` `DeviceImported`) | `descriptor.identity` in `device.message` | **LEAKED → masked-always at construction** |
| 5 | OAuth completion receipt (`app.rs` `oauth_add_completed`) | `descriptor.identity` in `accounts.message` | **LEAKED → masked-always at construction** |
| 6 | Login card Done stage (`render.rs` `login_lines`) | the daemon's `LoggedIn` identity | **LEAKED → masked-always at render** |
| 7 | Launcher header `account <label>` (`render.rs`) | `identity.account` — the ALIAS, only writer `adopt_identity(descriptor.alias)` | NOT an identity — unmasked BY DESIGN (below) |
| 8 | `/providers` active-account line | alias + auth flavor only | nothing to mask |
| 9 | Rotation note (`projection.rs`) | `rotation.to` alias + provider | nothing to mask |
| 10 | Session header / status bar | provider · model · device · `NO alias` pin | renders no account identity |
| 11 | Launcher roster rows / demo-select path (`runtime.rs`) | model/device/ago; the fabricated descriptor's identity is never rendered | nothing to mask |
| 12 | `/usage` alias chips `[alias]` | aliases | UNMASKED — U2 shipped precedent, the alias law below |

## One authority

`format::mask_identity` is THE mask — no second implementation, no
re-export needed (every surface lives in `haider-tui`). Doc extended to
name all surfaces. Shape unchanged (U2 owner addendum): first char of
local part + first char of domain survive, `*` for the rest
(length-preserving), final `.tld` readable; non-emails mask as one part;
the full local part never survives.

## Reveal semantics per surface (chosen honestly, per the directive)

- **`/accounts`** — `AccountsState::revealed` pin; `r` toggles (the
  login/OAuth/custom cards' total modality consumes keys first, so a
  typed alias/key `r` can never toggle it); masked-by-default restored on
  BOTH lanes: `exit_accounts` (esc) AND the one door `enter_accounts` —
  the U2 survivor lesson (⌃C walks `back_to_launcher` and bypasses the
  exit) baked in from the start. Key map names `r reveals`.
- **`/providers`** — its OWN `ProvidersState::revealed` pin with the same
  enter/exit resets: the shared device section takes the pin of the
  screen that HOSTS it (`model.screen` match; any other host renders
  masked-always). A reveal on `/accounts` never travels to `/providers`
  — per-surface pins, law-tested. Hint names `r reveals` only while a
  candidate actually carries an `account_label`.
- **Receipts (import + OAuth)** — masked-ALWAYS at construction: they
  are transient chrome with no key loop of their own; the durable,
  revealable surface is the account row the chained refresh lands.
- **Login card Done** — masked-ALWAYS at render: the card's keys belong
  to the alias/key fields (adding `r` would collide with typing), and it
  closes to `/accounts` where the reveal lives.
- **Launcher header** — the `account` segment renders the ALIAS. The
  daemon's alias grammar (`[a-z0-9][a-z0-9._-]{0,63}` — `@` impossible)
  means it can NEVER be an email, and U2 shipped `/usage`'s alias chips
  unmasked beside masked identities: masking the alias here would be a
  second dialect, not more safety. Law-tested instead: the raw identity
  never rides the launcher (`launcher_header_carries_the_alias_never_the_identity`).

## Laws (pinned in `p1_masking_sweep_tests.rs` + extended surface suites)

1. **Masked-by-default per surface** — `/accounts` rows (email AND key
   fragment), device labels on BOTH screens; raw never renders on open.
2. **Never-leaks-full-local-part** — every assert pairs the masked-form
   check with a `!contains(raw)` check; receipts and the login card get
   the same pair.
3. **Reveal+reset** — `r` per visit; esc lane AND the ⌃C Sub-Escape-Lane
   both restored by the enter-door reset; `/providers` re-visit resets
   its own pin.
4. **One authority / no second dialect** — all surfaces assert the SAME
   masked literals (`y**@w***.com`, `s*******`, `p*****@e******.invalid`)
   produced by the one helper; U2's helper tests still own its shape.
5. **Alias law** — launcher header carries the alias, never the identity.

Extended (not forked): `w5d_accounts_tests` (hierarchy/expired/pending
rows now assert the masked forms + no-raw), `d2_device_discovery_tests`
(masked label + masked receipt), `w3c3_login_tests` (Done stage masked +
no-raw). `u2_usage_screen_tests` untouched — still green.

## Ledger

- Test count: 1901 → 1905 (`xtask test-count --update`; +4 P1 laws, the
  extended suites grew asserts inside existing tests).
- Mutation campaign: 6 executed, 6 kills — see
  `P1-masking-sweep-mutation-notes.md` (mask-verbatim executed on TWO new
  surfaces + the login card, both reset doors, the receipt seam).
- `cargo fmt` clean; `cargo test -p haider-tui` green (81 binaries);
  ladder 16/16 (14 demo + 2 live rows).
