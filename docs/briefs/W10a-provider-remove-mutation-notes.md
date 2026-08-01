# W10a provider-remove mutation notes

Every mutation below has a separate runtime observer. “Expected RUNTIME
failure” means an assertion, typed response, or restart outcome changes; a
compile-only failure is not the claimed evidence.

| Production mutation | Runtime observer | Expected RUNTIME failure |
|---|---|---|
| Skip the provider-registry save/removal, or stop reapplying the authoritative committed remove receipt during startup reconciliation. | `accounts::accounts_tests::provider_remove_commits_replays_fences_and_beats_restart_resurrection` | The provider remains in the published list or the deliberately stale `providers.json` projection resurrects it after restart. |
| Omit the `provider_models` deletion from `finalize_provider_remove_receipt`, or move it outside the receipt/revision transaction. | `provider_remove_finalization_deletes_model_cache_and_replays` and the actor restart test | The committed removal leaves a readable model-cache row, or receipt/revision/cache truth is no longer one commit. |
| Drop the custom-provenance guard for builtin or factory profiles. | `provider_registry_removes_only_custom_profiles_and_clears_models` and `provider_remove_refuses_release_owned_and_account_referenced_profiles` | A release-owned provider is removed instead of returning typed `release_owned`. |
| Drop the account-reference guard or omit/scramble blocking aliases. | `provider_remove_refuses_release_owned_and_account_referenced_profiles` and `provider_remove_refusal_reason_and_aliases_are_golden` | The custom provider is removed while descriptors remain, or the typed refusal no longer names `alpha-key`/`zeta-key` in stable order. |
| Move receipt replay behind the revision fence or current-registry validation. | `provider_remove_commits_replays_fences_and_beats_restart_resurrection` | A same-body retry after the provider is gone and the revision advanced returns conflict/not-found instead of the original revision-one response. |
| Permit a changed semantic body under the same `command_id`. | `provider_remove_commits_replays_fences_and_beats_restart_resurrection` | Reusing `remove-custom` for `different-provider` succeeds or returns a domain refusal instead of `invalid_argument`. |
| Drop the expected-revision CAS for a genuinely new removal. | `provider_remove_commits_replays_fences_and_beats_restart_resurrection` | The stale fresh command does not return typed `RevisionConflict { expected_revision: 0, current_revision: 2 }`. |
| Remove `provider_remove_v1` from the daemon Welcome feature set. | `connection::connection_tests::welcome_features_pin_served_management_families` | Exact feature discovery omits a method the daemon serves. |
| Remove either additive `provider.remove` request/response or its structured refusal from the transcript. | `compact_ws_bodies_and_length_prefixed_uds_streams_are_golden`, `provider_list_and_management_feature_families_are_golden`, and `provider_remove_refusal_reason_and_aliases_are_golden` | The WS/UDS byte fixture, method-family presence check, or typed refusal JSON changes at runtime. |

