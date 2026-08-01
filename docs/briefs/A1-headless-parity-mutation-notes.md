# A1 headless-parity mutation notes

Each production mutation below is paired with an external runtime test.
“Expected RUNTIME failure” means the named test must fail through a peer
assertion, typed error assertion, or black-box process assertion; compile-only
failure is not the claimed evidence.

| Production mutation | Runtime observer | Expected RUNTIME failure |
|---|---|---|
| Drop the `account.list` read, choose a profile fallback, or hardcode Anthropic for a flagless request. | `haider-client/tests/headless_run_tests.rs::flagless_bootstrap_creates_on_active_provider_and_published_default_model` and `haider-cli/tests/cli_tests.rs::flagless_run_without_an_active_account_exits_65_with_remedy` | The peer no longer observes `account.list` before `provider.list`/create, the pinned `openai-oauth` create provider changes, or the fresh-profile process stops returning `no_active_account` with exit 65 and its remedy. |
| Resolve the model from `account.list.provider_defaults`, skip `provider.list`, ignore `default_model`, or omit the first-model fallback. | `haider-client/tests/headless_run_tests.rs::flagless_bootstrap_creates_on_active_provider_and_published_default_model` and `flagless_bootstrap_falls_back_to_first_published_model` | The peer request order changes, the deliberately misleading account-list default reaches create, or the create model differs from the provider summary's default/first slug. |
| Restore the CLI's `fake|anthropic` allowlist or reject an unknown name before create. | `haider-cli/tests/cli_tests.rs::unknown_run_provider_surfaces_daemon_create_refusal` and `run_parser_pins_outputs_timeouts_and_permission_flags` | The parser returns usage 2, or the process no longer exposes the daemon's `invalid_argument` create refusal and exit 76. |
| Reintroduce the parser's Anthropic-only model requirement or stop honoring provider defaults when `--model` is absent. | `haider-cli/tests/cli_tests.rs::run_parser_pins_outputs_timeouts_and_permission_flags` and the two flagless peer fixtures | The open provider request is rejected before bootstrap, or the pinned provider-list model never reaches create. |
| Remove resolved identity from the final result/JSON, change print bytes, or omit the additive fields from pre-acceptance JSON. | `haider-client/tests/headless_run_tests.rs` flagless result assertions and `haider-cli/tests/cli_tests.rs::print_and_json_outputs_pin_bytes_schema_and_nulls`, `flagless_run_without_an_active_account_exits_65_with_remedy`, and `unknown_run_provider_surfaces_daemon_create_refusal` | `HeadlessRunResult` loses the resolved pair, the ten-field `haider.run.v1` object changes, null/error identity shape changes, or the frozen print bytes differ. |
| Route `no_active_account` to a generic software/protocol failure or omit its remedy. | `haider-cli/tests/cli_tests.rs::run_exit_codes_are_table_driven` and `flagless_run_without_an_active_account_exits_65_with_remedy` | Exit 65 changes, the JSON error code is not `no_active_account`, or stderr lacks the actionable TUI remedy. |

