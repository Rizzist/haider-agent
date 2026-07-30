# W5e-3 — review of record #1 — SHIP

Implementer AND reviewer: Fable 5. Branch `w5-e3` @ `cd43d64`.
Authority: report §5.3 (dynamic argument slots) + the W5e-2 discovered
catalog.

## What changed

`PaletteItem::Arg` carried `&'static str`, which structurally could not hold
discovered data — that was the blocker, not the wiring. It now carries owned
strings (the enum drops `Copy` for `Clone`; ~6 call sites and 3 test files
follow), and a new `DynamicSlots` feeds three slots from daemon truth:

- **`/model`** — the ACTIVE provider's DISCOVERED models, in the provider's
  own order, with its declared default marked. Follows the session provider;
  a provider with no discovered models offers NOTHING.
- **`/provider`** — the registry with honest health (unavailable providers
  still listed, with their reason).
- **`/account`** — live aliases, auth label, `in use` marker.

Selecting an undiscovered model is REFUSED with a reason instead of being
applied. That is the load-bearing property: without it the picker would
happily accept a slug the provider never named, which is the hardcoded-model
problem wearing a different hat.

## Feature gating, applied BEFORE shipping

The W5e-1b post-mortem said the next wave would gate its new affordances
rather than wait for a field report. Done: with no catalog and no
`provider_models_v1`, `/model` names the stale daemon and the remedy instead
of claiming "no models". Pinned by
`model_without_the_feature_names_the_stale_daemon`.

## Mutations (executed post-commit)

| # | Mutation | Result |
|---|---|---|
| M1 | `dynamic_slots` early-returns `Default` (picker stops being fed by discovery) | KILLED — 4 tests |
| M2 | `/model` falls back to the first model when the request is not found | KILLED |

M1's first attempt used `let _ = (providers, models, accounts);` and failed to
COMPILE — recorded because a compile failure is not a kill, so it was redone
as an early return that compiles and then killed properly.

## Gate

clippy `--workspace --all-targets -D warnings` clean. Ledger 1072 → 1078.
TUI suite 497 green; full per-crate gate green.

## Known gate flakes (named, load-only, tracked)

The full-gate run flagged two, both of which pass repeatedly in isolation and
under deliberate concurrent load. `gate.sh` now persists per-crate logs, so
for the first time they are NAMED rather than "something flaked":

- `haider-client::fatal_protocol_error_frame_fails_pending_requests` —
  8 passed / 1 failed in the gate; 7 consecutive clean runs after (3 isolated,
  4 with two other crates' suites running). `read_frames` returns empty only
  on EOF, so the fixture is not racing the request write; under CPU
  starvation the CLIENT's own timeout beats the fake daemon's fatal frame and
  the request resolves to a different error variant than
  `Fatal(overloaded)`. Fixture-timing sensitivity, not a product defect, and
  not a regression from this branch's `RpcClient::welcome` addition (which is
  inert on this path).
- `haider-daemond::worker_aware_drain_terminalizes_durable_queued_turns_before_store_close`
  — third sighting, always under full-gate contention, never isolated.

Neither blocks a release, but a flaky gate erodes the signal every later wave
depends on. Fix as W5f hygiene: give both fixtures timeouts that CPU
starvation cannot beat.

## Not in this cut

- Session-create carrying the chosen provider/model/account end to end: the
  slots choose, and `/model`//`/provider` apply to the session identity, but
  the launcher's `session.create` still uses the resolved profile's defaults.
  That is W5f's first task, alongside the live turn.
- The editable alias field on the login/OAuth cards (§5.3's suffix proposal
  on `revision_conflict`) — deferred with W5f.

## Verdict

**SHIP.**
