# QA gate step 3 findings

Installed pair: `haider`/`haiderd` 0.0.967 from `/usr/local/bin`.

Reproduction: `docs/testing/v0.0.967/qa-gate-t0-Syeds-MacBook-Air.local-20260831T182738Z.json`
reported 3/7 PASS and the same four TUI failures as the killed lane. Every row's
owned daemon ended with `alive_after=false`.

Final run: `docs/testing/v0.0.967/qa-gate-t0-Syeds-MacBook-Air.local-20260831T195225Z.json`
validated against `haider.qa-gate.v1` and reported 5/7 PASS. Its only FAILs are
the two product findings below; each has `expected_fail_until = "0.0.968"`, a
value-bearing `defect:` line, and `no_orphan_daemons ... alive_after=false`.

## Adjudications

| Check | Verdict | Evidence and contract |
| --- | --- | --- |
| `t0.tui.catalog_help_command_list_pin` | **PRODUCT FINDING** | `command.list` is the command authority and clients must never mirror names (`docs/client-contract-v1.md:70-74`, repeated at `:974`). The TUI declares a static `HELP_TEXT` mirror (`crates/haider-tui/src/commands.rs:12-14`): it omits catalogued `attach` (`crates/haider-rpc/src/command.rs:138-142`; help jumps from rollback to sessions at `commands.rs:32-33`) and adds out-of-catalog `monitors` (`commands.rs:38`, dispatched at `crates/haider-tui/src/app.rs:13422`). Reproduction values: `missing_from_help=['attach']`, `absent_from_COMMANDS=['monitors']`. The check remains FAIL, carries `expected_fail_until = "0.0.968"`, and its evidence contains the `defect:` contract note. |
| `t0.tui.model_picker_cardinality` | **CHECK BUG** | Cardinality was correct at 36/36 and provider staging/escape passed. The probe failed to recognize the product-supplied refusal `provider endpoint is not configured`. Vertex seed profiles intentionally have no endpoint (`crates/haider-daemon/src/provider_registry.rs:1246-1261`), publish that exact availability reason (`provider_registry.rs:1152-1160`, `:1193-1208`), and unavailable picker rows must paint their reason (`crates/haider-tui/src/app.rs:17246-17261`). The check now compares the painted refusal with `provider.list.availability_reason`, disambiguates provider-prefixed model collisions, and selects the placeholder row from the rendered order instead of assuming it is last. |
| `t0.tui.palette_activation_closure` | **CHECK BUG + PRODUCT FINDING** | Enter activates the highlighted palette row (`crates/haider-tui/src/app.rs:7987-7998`, shared activation at `:12912-12944`). The probe's generic keyword-delta oracle rejected actual command-owned surfaces/refusals, and its RPC connection omitted the client's required 45-second idle ping (`crates/haider-daemon/src/connection.rs:97-125`). Those check bugs are fixed with exact per-command signatures and a derived 15-second keepalive. One real defect remains: the contract defines `/login` as provider-then-method slots (`docs/research/w5-provider-research-report.md:480-488`), but generic palette activation overwrites the composer with only the newest argument (`crates/haider-tui/src/app.rs:12941-12943`). The installed TUI therefore paints `/login api  api` instead of opening the selected provider's key card. The check remains FAIL with `expected_fail_until = "0.0.968"` and a value-bearing `defect:` line. |
| `t0.tui.three_door_parity` | **CHECK BUG** | The receipt and SQLite truth already agreed across palette, typed slash, and `command.invoke`. The probe incorrectly treated `haider session <id> --json` as session-configuration authority even though that single-session observe projection explicitly sets effort/fast to `None` (`crates/haider-cli/src/observe.rs:935-955`). The contract assigns roster truth to `SessionSummary` (`docs/client-contract-v1.md:85-95`, `:950-960`), populated by the daemon from committed metadata (`crates/haider-daemon/src/session_hub/rpc.rs:16819-16830`, `:16861-16920`). The check now reads `session.list` for model/effort/fast/title and retains the observe event tail only for compaction. Its polling sleep was removed. |
| `t0.tui.login_paths` | **PASS / no finding** | Both login paths and masked-secret non-persistence passed in the initial and reproduced runs. |

## Open product defects

`defect: docs/client-contract-v1.md:70-74` — the 0.0.967 terminal help panel is a
shadow command-name catalog and has drifted from the authoritative `command.list`
result (`attach` missing; `monitors` extra). This is expected to remain a truthful
FAIL on 0.0.967 and to flip to PASS without check edits when the 0.0.968 product
fix lands.

`defect: docs/research/w5-provider-research-report.md:480-488` — `/login` is a
two-stage provider-then-method palette, but the shared `PaletteItem::Arg`
activation at `crates/haider-tui/src/app.rs:12941-12943` discards the provider
when the method row is accepted. The observed composer is `/login api  api` and
the API-key card does not open. This is expected to remain a truthful FAIL on
0.0.967 and to flip to PASS without check edits when the 0.0.968 product fix
lands.

## Mutation evidence

With the source temporarily changed to expect
`terminal_kind=__qagate3_deliberate_mutation__`, the installed JSONL path failed
with `terminal_kind expected=__qagate3_deliberate_mutation__ actual=success`.
Its owned daemon ended `alive_after=false`. The expectation was restored to
`success` before the final self-test and t0 runs; the final JSONL row is PASS.
