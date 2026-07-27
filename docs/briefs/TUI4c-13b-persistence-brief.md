# TUI4c part 2 — item 13b: across-restart persistence (PARKED, next round)

Foundation landed in part 1 (`3da820d`): the session map (`crate::session::SessionState`),
`head_ros` recorded at every roster claim, seeds materialized via `mock::seed_session_states`,
`/reset` already rebuilds seeds + resets the roster counter to `ROSTER_FIRST_CLAIM`.

## The sim's exact contract (verified against tui.js this round)

**Save** (tui.js:765-772, key `haider-tui-v1`): `{ sessions, themeName, vfs, launcherDir,
voice, voiceSession, accounts, nodes }`, on EVERY change to any of those (Rust port:
mark a `store_dirty` flag on session/theme/vfs/dir/voice mutation; debounce writes in the
run loop, plus write on quit and on detach). Startup decisions live under their OWN key
(`haider-tui-startup-v1`, tui.js:747-763) — harness-level, not session state; our port has
no startup gates yet, so skip until they exist.

**Load** (tui.js:699-754), guards IN ORDER — each was a sim bug fix, verify each by revert:
1. Proceed only if `sessions` is a non-empty array; ANY parse error → seeds (`catch {}`).
   SUPERSEDED BY TUI4.1 (review P1-1 + D3-3 — this line as written shipped too permissive):
   guard 1 is STRICT. A `version` discriminator (`DEMO_STORE_VERSION`) is checked FIRST,
   every DTO is `deny_unknown_fields`, `SessionDto`'s structural core carries NO
   `#[serde(default)]` (only `head`, whose backfill is guard 4 below), and a persisted
   session id 0 is rejected. A session missing its shape rejects the WHOLE file back to
   seeds — the sim's `s.branches.map(…)` throws before `setSessions`, so per-field
   defaulting was never the sim's contract.
2. Entry-id collision guard: sim scans `e(\d+)` and bumps its id counter +1000. Our
   equivalent: persist `card_seq` (menu ids `voice-card-N` collide otherwise) and restore
   `next_session_id` past the max persisted session id.
3. Roster counter restore: `next = max(3, every head.ros + 1, every chip.ros + 1)` —
   `claimName` must NEVER re-issue a used callsign after reload. `head_ros` exists on
   `SessionState`; ChipModel/ChipSeed still need a `ros: Option<u64>` recorded at
   `claim_name` sites in script.rs (branch_subagent, branch_auth).
4. Hydration backfill: `head: s.head || rosterAt(next++)`; per branch `menus: b.menus || []`;
   `chips: sweepClosedChips(b.chips)` (exists: `session::sweep_closed_chips`) then
   callsign backfill `rosterAt(c.ros ?? next++)` for chips missing one.
5. `rosterRef = next` AFTER the walk; singles guarded (`themeName` only if known,
   `vfs` merged over seed, `launcherDir`, `voice`).

**Deliberately NOT restored** (tui.js §6 of the audit): run states (every session loads
IDLE — our `turn_active: false`), activeId/screen (always boot → launcher), msgQueues,
menu RESOLVERS (persisted cards answered after reload get the sim's
"· stale menu dismissed — no live run attached (answered after reload)" note at 874-876 —
port this: an Answer whose origin id has no live arms lands that note), timers.

**/reset** (tui.js:1913-1943): `removeItem` + reseed + roster=3 — the Rust reset path
already reseeds; add the file delete.

## Port shape

- DEMO-scoped store: JSON at `<profile dir>/demo-tui-state.json` (name it `demo-*`; find
  the profile dir helper in haider-store or fall back to `~/.haider/`). Document loudly:
  **this is the DEMO store; the real daemon store replaces it at W3c** — name the module
  `demo_store.rs` so nobody mistakes it.
- Serde via DTOs, not derives on runtime types: `hon: &'static str` on
  ChipModel/RosterName cannot Deserialize — mirror structs with owned Strings, hydrate
  `hon` via `Box::leak` (bounded, demo-scoped, document) or a known-hon lookup.
  SessionProjection persists as its `entries()` + tokens + open menus (check what
  `apply_seed_row` can rebuild vs what needs direct entry serde).
- Corrupt/missing file → seeds, NEVER a crash (test with truncated JSON + wrong types).

## Tests the coordinator specified (still owed)

Two user sessions + a seeded one round-trip serialize → fresh model → hydrate: same
rendered launcher list, same re-entered transcripts, NO duplicate callsigns (claim after
reload continues past the max persisted ros — revert guard 3 to watch it fail); corrupt
file → seeds; `/reset` purges user sessions + deletes the file; interrupt→idle(i) not
overwritten by a stale beat post-restore (no arms survive a restart, so a persisted
idle(i) must survive hydration verbatim); `sweepClosedChips` on load (persist a closed
chip, hydrate, gone — revert guard 4 to watch it fail).

Gate as always: cargo test/clippy/fmt · xtask --update · dump_screens eyeball · full
probe ladder from scripts/tui-probes/ (note: the first post-build pty-probe-sub run can
flake cold at 118×36 — boot outpaces the 3.2 s pump; re-run warm).
