# P1 — masking sweep — executed mutation ledger

Protocol per kill (the U2 discipline): apply the mutation, run the named
law ("running 1 test" observed), record the runtime failure verbatim,
revert (`git checkout` against the committed implementation —
commit-BEFORE-mutation, a lesson paid for once this lane when a
pre-commit checkout wiped the uncommitted render.rs sweep and it had to
be reapplied), re-run green. Full `cargo test -p haider-tui` green (81
binaries, 0 failures) before and after the campaign.

## 6 executions — 6 kills

1. **Mutation (owner-mandated class — mask-verbatim on a NEW surface):**
   the `/accounts` row renders `row.identity` verbatim (the mask branch
   deleted) — `haider-tui/src/render.rs` `render_accounts`. KILLED by
   `accounts_rows_mask_by_default_and_r_reveals_per_visit`
   (`p1_masking_sweep_tests.rs:116`): `panicked: no raw identity on
   open`. Second independent killer observed in the same run:
   `device_labels_mask_on_both_screens_with_per_screen_reveal_pins`
   (the accounts screen hosts both rows), and
   `w5d_accounts_tests::accounts_screen_renders_the_sim_hierarchy`'s
   masked-form asserts stand behind those. Reverted, green.
2. **Mutation:** the `revealed = false` reset dropped from
   `enter_accounts` — `haider-tui/src/app.rs`. KILLED by
   `accounts_rows_mask_by_default_and_r_reveals_per_visit`
   (`p1_masking_sweep_tests.rs:168`): `panicked: the visit after a ⌃C
   exit STILL opens masked`. The kill landed EXACTLY at the
   Sub-Escape-Lane assert — the esc lane alone would have rescued this
   mutation (`exit_accounts` carries its own reset), proving U2's
   survivor lesson is load-bearing here and was rightly baked in from
   the start. Reverted, green.
3. **Mutation (mask-verbatim on a second NEW surface):** the shared
   device-section label rendered verbatim, reveal pin ignored —
   `haider-tui/src/render.rs` `push_device_candidates_section`. KILLED
   TWICE: by `device_labels_mask_on_both_screens_with_per_screen_reveal_pins`
   (`p1_masking_sweep_tests.rs:186`): `panicked: /accounts: the label
   opens masked`, AND by the extended
   `d2_device_discovery_tests::device_section_lists_candidates_with_freshness`
   (`d2_device_discovery_tests.rs:198`): `panicked: supported row:
   number · source · provider · MASKED account label · freshness` — the
   leak is caught by the P1 law and by the surface's own suite.
   Reverted, green.
4. **Mutation:** the device-import receipt interpolates
   `descriptor.identity` raw — `haider-tui/src/live.rs` `DeviceImported`
   arm. KILLED by `import_and_oauth_receipts_mask_the_identity_always`
   (`p1_masking_sweep_tests.rs:264`): `panicked: the import receipt
   masks the identity: ✓ imported openai → codex-cli · you@work.com ·
   ChatGPT` — the failure message shows the exact leak the law exists
   to stop. (`d2`'s extended receipt assert stands behind it.)
   Reverted, green.
5. **Mutation (mask-verbatim on the login card):** `LoginStage::Done`
   renders the identity verbatim — `haider-tui/src/render.rs`
   `login_lines`. KILLED by the extended
   `w3c3_login_tests::a_committed_login_shows_the_descriptor_identity_and_no_secret`
   (`w3c3_login_tests.rs:383`): `panicked: the identity renders masked`.
   Reverted, green.
6. **Mutation:** the `/providers` reveal made sticky — BOTH resets
   dropped (`enter_providers` + `exit_providers`) —
   `haider-tui/src/app.rs`. KILLED by
   `device_labels_mask_on_both_screens_with_per_screen_reveal_pins`
   (`p1_masking_sweep_tests.rs:226`): `panicked: /providers: a new visit
   opens masked`. Reverted, green.

## Verdicts

| # | Seam | Verdict |
|---|---|---|
| 1 | `/accounts` row mask (NEW surface, verbatim) | KILLED at the P1 law, second killer + w5d behind it |
| 2 | accounts enter-door reset | KILLED at the ⌃C Sub-Escape-Lane (esc lane alone would rescue — lane pre-hardened) |
| 3 | device-label mask (NEW surface, verbatim) | KILLED at the P1 law AND the extended d2 suite |
| 4 | import receipt raw identity | KILLED — failure message displays the exact leak |
| 5 | login Done mask (verbatim) | KILLED at the extended w3c3 law |
| 6 | providers per-visit pin (both resets) | KILLED at the re-visit assert |

Not separately executed, covered by construction + law: the OAuth
completion receipt shares execution #4's seam class and its own assert
in `import_and_oauth_receipts_mask_the_identity_always`
(`p*****@e******.invalid` + no-raw) — a verbatim mutation there fails
that assert identically.

## Review of record (coordinator, executed post-lane)

| RV7 | cross-screen flag leak: the `/accounts` `r` arm also assigns
`self.providers.revealed = self.accounts.revealed` (`app.rs`) |
**SURVIVED the lane's campaign** — every masking suite
(`p1_masking_sweep_tests`, extended `w5d`/`d2`/`w3c3`) stayed green.
Honest gap: every render-level walk is RESCUED by the enter-door resets
— `enter_providers` scrubs the leaked flag before any frame can draw
it, so a walk cannot distinguish true pin isolation from
rescue-by-reset; the lane's isolation claim was pinned only in the walk
direction, not the flag direction. Law added:
`reveal_pins_are_surface_isolated_in_state_not_just_at_render` — binds
the FLAGS in both directions (r on `/accounts` must not touch
`providers.revealed`; mirror for `/providers`) plus the render walks as
the behavior half. Re-executed against the applied mutation ("running 1
test" observed): KILLED at `p1_masking_sweep_tests.rs:254`: `panicked:
a reveal on /accounts must not touch the /providers pin (state
isolation)`. Reverted (`git checkout app.rs`), suite 5/5 green. |
