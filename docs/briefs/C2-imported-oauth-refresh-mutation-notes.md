# C2 imported OpenAI OAuth refresh mutation notes

Every observer below is a runtime law test in `crates/haider-daemon/src/oauth_tests.rs`
or `crates/haider-daemon/src/accounts_tests.rs`; none is inline in production.
“Expected RUNTIME failure” means an assertion, typed error, or timeout after the
mutated build starts—not a compile-only failure.

## Pinned-law reconciliation

C2 **extends rather than supersedes**
`codex_fallback_refresh_is_one_use_and_import_scoped_at_the_broker_call_site`.
The one-use bit remains only the import-scoped eager bootstrap for a Codex
access token whose JWT expiry cannot be parsed. A successful refresh still
clears that bit durably. Later expiry, threshold, or 401 boundaries may refresh
the same imported credential again, but only after the account actor confirms
that the latest alias incarnation is a Codex import. The sanctioned OpenAI
registration remains `Conservative`; ordinary loopback-PKCE credentials never
inherit import scheduling or the rotating-token policy.

| Production mutation | Runtime observer | Expected RUNTIME failure |
|---|---|---|
| Bypass latest-receipt import classification, discard `RefreshFallback { source }`, send a naturally expired Codex import through terminal exit-70 handling, drop the source fingerprint during refresh, treat unchanged auth.json as fresher than Haider's rotated generation, or erase receipt-proven provenance when an actor sees that a winner already advanced the vault. | `accounts::accounts_tests::expired_imported_bundle_refreshes_instead_of_terminal_exit70` | Resolution returns an expiry error, the stale-generation actor probe returns `NotImported`, the second lifecycle POST is absent, generation-one access is restored, or the endpoint sees R1 again instead of the literal R1→R2 fingerprint sequence and durable generation three. |
| Make OpenAI `SerializedRotating` provider-wide, retain the import marker after refresh, treat one-use as a lifetime budget, or apply the marker to PKCE. | `oauth::tests::codex_fallback_refresh_is_one_use_and_import_scoped_at_the_broker_call_site` | The first/import-later POST counts differ, the marker remains set, the later expired generation cannot refresh, or the ten-minute PKCE token refreshes under the legacy 30-second skew. |
| Remove the physical vault-alias lease, move the vault re-read outside it, release before durable Apply, or replay generation N after N+1 lands. | `oauth::tests::concurrent_imported_refreshers_adopt_not_destroy` and the stale-generation actor probe in `accounts::accounts_tests::expired_imported_bundle_refreshes_instead_of_terminal_exit70` | The two independent `FileVault` handles submit the literal generation-one token more than once, the production actor erases import provenance, a contender fails instead of adopting, or durable generation/token bytes differ. |
| Ignore a returned rotated refresh token or replay it after rotation. | `oauth::tests::refresh_never_replays_a_rotated_token` and `oauth::tests::concurrent_imported_refreshers_adopt_not_destroy` | The next encoded provider request contains R1 instead of R2, the endpoint capture differs from the one-element generation-one literal, POST count exceeds one, or the stored refresh token is not `REFRESH_ROTATED_SENTINEL_8c21`. |
| Require rotation on every OpenAI response or clear the refresh token when the response omits it. | `oauth::tests::imported_refresh_retains_token_when_response_does_not_rotate` | The non-rotating success is rejected or the durable generation-two bundle loses the generation-one refresh token. |
| Drop the pre-request uncertainty write, omit/ignore the 300-second rejection tombstone, replay a rejected/uncertain token, or return an untyped/generic invalid-grant error. | `oauth::tests::imported_ambiguous_transport_never_replays_uncertain_token`, `oauth::tests::terminal_invalid_grant_names_reimport_remedy_typed`, and `oauth::tests::malformed_rotating_success_never_leaves_the_old_token_replayable` | A second broker POSTs again, the durable marker is absent or differs from permanent uncertainty after an ambiguous close, `kind`/`reimport_required`/`import_source` differs, the exact `haider import codex` remedy is lost, or malformed success makes the old token reusable. |
| Stop capturing OpenAI OAuth access fingerprints, remove the stale-401 vault comparison, or adopt an actively marked generation. | `accounts::accounts_tests::auth_aware_factory_routes_sanctioned_oauth_descriptors_to_subscription_adapters`, `oauth::tests::imported_codex_401_adopts_new_vault_access_without_refresh_post`, and `oauth::tests::forced_401_reread_never_adopts_an_actively_marked_rotating_bundle` | The factory fingerprint becomes `None`, the already-durable access token is refreshed again, or an uncertainty/rejection-marked access token is returned instead of a typed refusal. |
| Bypass the serialized retry loop, retry ambiguous transport failures, retry statuses other than explicit 429/5xx, remove the three-attempt bound, fail to restore the pre-request bundle after exhausted explicit statuses, or change the 250/500 ms backoff. | `oauth::tests::imported_refresh_retries_only_explicit_statuses_and_restores_bundle`, `oauth::tests::imported_ambiguous_transport_never_replays_uncertain_token`, and `oauth::tests::kimi_refresh_backoff_is_bounded_to_explicit_retryable_statuses` | The endpoint does not observe exactly three explicit-status POSTs, the restored bundle differs byte-for-byte, delay policy changes, an ambiguous close is retried, or a successor can replay a possibly-spent token; imported Codex and Kimi share this serialized runner. |
| Return access before the actor persists the replacement, skip refresh CAS/fence validation, or expose a failed vault write. | `oauth::tests::refresh_vault_failure_never_returns_rotated_access`, `oauth::tests::concurrent_imported_refreshers_adopt_not_destroy`, and `accounts::accounts_tests::stale_oauth_fences_cannot_overwrite_or_expire_a_replaced_bundle` | Rotated access escapes before durable storage, a late generation overwrites its successor, or a persistence failure returns authorization. |
| Include the bounded endpoint body, old/rotated access token, or old/rotated refresh token in a public error/debug surface. | `oauth::tests::terminal_invalid_grant_names_reimport_remedy_typed` and `oauth::tests::no_secret_bytes_in_errors_journal_or_logs` | A literal sentinel appears in formatted public errors. OAuth refresh writes no journal/log record. |
| Change OpenAI authorization-code/PKCE request encoding while generalizing refresh. | `oauth::tests::declared_token_encodings_build_exact_provider_payloads`, existing callback/PKCE suites, and the pinned import-scoped law above | Exact form/JSON fields or content types change, callback suites fail, or the ordinary PKCE bundle refreshes early. |

The HTTP observers bind numeric loopback sockets. Restricted macOS sandboxes
that reject `bind(2)` stop at fixture setup with `Operation not permitted`;
the same tests execute their runtime assertions in the workspace/CI host gate.
