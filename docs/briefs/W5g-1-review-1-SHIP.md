# W5g-1 — review of record #1 — SHIP

Reviewer: Fable 5. Branch `w5-g1`, reviewed at commit 324926f (frozen ref).
Implementer split per brief: codex lane (gpt-5.6 xhigh) took provider/rpc/
daemon; Fable took the TUI half.

## What the patch does

The locked v0.1.0 scope demands "per-model context windows … real limits,
never guessed." Discovery already received real `context_window` values
from the codex catalog and dropped them; the meter divided real token
counts by a hardcoded 200k seed. Now:

- `DiscoveredModel.context_window: Option<u64>` — parsed from the codex
  shape (`0` → absent), ALWAYS `None` for Anthropic (its `/v1/models`
  declares no windows — the absence is honest, not a bug).
- Additive tolerant wire: `ModelDetailWire { name, context_window }` +
  `#[serde(default)] model_details` on `ProviderSummaryWire`. Old peers
  omit → empty. `models` untouched for compat.
- Daemon summaries now derive `models` FROM `model_details`, so the 1:1
  name alignment is structural, not asserted.
- TUI: `ProvidersState::declared_window` + `AppModel::
  refresh_context_window` — a declared window always wins; none declared
  keeps the current figure (seed defaults stay honest fallbacks). Wired at
  `adopt_identity`, `/model`, `/provider`, and BOTH catalog-arrival arms:
  the pin protects the user's provider/model choice, not a stale number,
  so a late catalog corrects even a pinned identity's meter.

## Review notes

- The derive-models-from-details refactor in `provider_summary` is the
  strongest line in the patch: a whole class of misalignment bugs became
  unrepresentable. Kept.
- The codex lane could not run the loopback-binding suites in its sandbox
  (43 daemon OAuth + 1 provider redirect, PermissionDenied) — re-run and
  green in the local full gate (gate15).
- `haider-cli` seeds `identity.context_window` from `profile.
  default_max_tokens` at startup; the refresh overrides it once a catalog
  lands. Display-only either way (session budget is decoupled via
  `SESSION_OUTPUT_CAP`). Non-blocking.

## Mutations (reviewer-chosen, EXECUTED post-commit)

| # | Mutation | Result |
|---|---|---|
| M1 | `refresh_context_window` body emptied | KILLED (3 runtime failures, exactly the window-carrying tests) |
| M2 | refresh dropped from `ProviderModelsRefreshed` arm only | KILLED (exactly the pinned-identity test) |
| M3 | codex parse arm hardcodes `None` | KILLED (`codex_declared_context_window_is_preserved`) |
| M4 | `serde(default)` removed from `model_details` | KILLED (old-daemon tolerance test) |
| M5 | registry skips the `pickable` filter | KILLED twice — new alignment test AND the pre-existing pickable test (defense in depth from the derive refactor) |

## Gate

Workspace clippy `-D warnings` clean; targeted suites green; full
per-crate gate `gate15.out`; ledger 1113 → 1124.

## Verdict

**SHIP** (merge to main, ships as v0.0.24).
