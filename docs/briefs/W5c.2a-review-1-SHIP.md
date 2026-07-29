# W5c.2a — review of record #1 — SHIP

Reviewer: Fable 5 (owner rule: codex implements, Fable reviews). Branch
`w5-c2a`, commit `eb581bd`. 19 files, +1369/-125. Design authority:
`docs/research/w5-provider-research-report.md` §4.1, §4.2, R5, R6.

Scope was deliberately the READ side plus the revision spine; the durable
mutations are W5c.2b.

## Verdict per binding criterion

1. **Feature negotiation — PASS with one carried note.** `welcome_features()`
   now advertises `provider_management_v1` and `account_rotation_v1` beside the
   existing four. `account_rotation_v1` is genuinely served (W5c.1). See the
   P3 below on `provider_management_v1`.

2. **`account.list` additive — PASS.** `revision: Option<u64>` and the two new
   vectors all carry `skip_serializing_if`, and `descriptors` is untouched in
   shape and type. Wire v1 already tolerates unknown object fields, so an old
   client is unaffected.

3. **`provider.list` — PASS.** New request/response pair; provider endpoint,
   model inventory, auth methods, availability and default all live here rather
   than being duplicated onto descriptors. `ProviderApiFamilyWire` and
   `ProviderAvailabilityWire` are `#[non_exhaustive]` with `#[serde(other)]
   Unknown` from their first release, exactly as §4.1 requires.

4. **Closed-enum freeze — PASS.** `AuthMethod` and `CredentialStatus` are
   untouched.

5. **Revision spine — PASS, and this is the strongest part of the patch.**
   `finalize_management_command_receipt` allocates the revision and finalizes
   the receipt inside the caller's single `Immediate` transaction.
   `next_management_revision_in_transaction` bumps via a compare-and-set
   (`WHERE singleton = 1 AND management_revision = <observed>`) and errors when
   it does not update exactly one row, so the counter can neither skip nor
   repeat. `ensure_committed_management_revision` repairs a pre-v6 receipt
   exactly once — its `UPDATE` is guarded by `final_revision IS NULL`, and an
   already-revisioned receipt replays through a fast path instead of
   allocating. Schema v6 adds both columns with `CHECK` constraints.

6. **`revision_conflict` — PASS.** Stable constant plus
   `ErrorData::RevisionConflict { expected_revision, current_revision }`:
   bounded, structured, no secrets, golden-pinned with the other stable codes.

7. **R7 — PASS.** `account_list` and `provider_list` are synchronous reads of
   `facade.management.read()` with no `.await` on the connection task and no
   inline endpoint probing. An unavailable snapshot answers `draining` +
   retryable rather than blocking. `ManagementSnapshot` holds `{revision,
   descriptors, providers}` behind one mutex, so a read structurally cannot
   pair account data with a different revision — the coherent-snapshot rule
   from §4.2 enforced by shape rather than by convention.

## Audit integrity — mutations re-executed by the reviewer

Both claimed pins re-executed independently; both **KILLED at runtime**:

| # | Mutation | Result |
|---|---|---|
| M-A | Let the receipt transaction commit despite a failed `final_revision` write (`?` → `.unwrap_or(1)`) | KILLED — `expect_err("trigger must abort finalization")` |
| M-B | Bypass the replay fast path in `ensure_committed_management_revision` (`final_revision` → `None`) | KILLED — second ensure raised the missing-revision claim error instead of replaying revision 3 |

`management_receipt_and_revision_roll_back_together` is the good kind of test:
it installs a SQLite trigger that aborts the revision write, then asserts the
receipt rolled back to `pending` and the counter stayed at 0 — driving the real
`Store::finalize_login_receipt`, not a double. Given the W5b finding (four
tests driving a fence-reimplementing fake), that distinction was checked
explicitly here.

## Findings

- **[P3] `connection.rs:1379` — `provider_management_v1` over-advertises.**
  Only `provider.list` exists; `provider.configure` is W5c.2b. §4.1 tells
  clients to "hide/disable only the methods whose feature is absent", so a
  client gating a configure affordance on this string ships a control that
  cannot work. Defensible reading: the string covers the provider-read family
  and W5c.2b adds a separate `provider_configure_v1`. **Recorded as a binding
  requirement on W5c.2b.** Not blocking: no client consumes it yet, and the
  TUI that will (W5d) is authored after W5c.2b lands.
- **[P3] `accounts.rs:502` — the provider registry is hardcoded.** Model lists
  are literals (`gpt-5.6`), which is acceptable while `provider.configure` does
  not exist, but the `_ =>` catch-all reports an unknown provider as
  `Available`, `enabled: true`, with Anthropic's default model. An unknown
  provider should not render as healthy. Fix alongside the configurable
  registry in W5c.2b.

## Gate (reviewer-run, per-crate, detached)

clippy `--workspace --all-targets -D warnings` clean. Ledger 1004 → 1014.

protocol 23 · accounts 20 · core 41 · provider 47 · **daemon 148** · daemond 86
· **rpc 49** · tui 465 · cli 21 · **store 37** · tools 69 · client 18 ·
verify 1 — all 0 failed.

codex reported ~60 failures across five crates; every one was a denied
TCP/Unix socket bind inside its sandbox. The reviewer gate has sockets and is
authoritative: all green.

## Incident (reviewer error, recorded)

Mid-review I ran `git checkout -- crates/haider-store/src/event_store.rs` to
revert a mutation while the branch was still **uncommitted**, which discarded
all 194 lines of that file's W5c.2a work. Recovered by reconstructing the file
from the diff captured during review and re-verifying against the surviving
tests and migration (37 store tests green, clippy clean). **Rule going forward:
commit the patch under review before executing any mutation**, so every revert
is bounded by a commit rather than by the index.

## Verdict

**SHIP.** Additive wire only, no existing shape retyped, tolerant enums from
release one, the revision spine atomic and monotonic under an injected-failure
test, R7 preserved, coherent snapshot enforced structurally. Two P3s carried
into W5c.2b, neither blocking.
