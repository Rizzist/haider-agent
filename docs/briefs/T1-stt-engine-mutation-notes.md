# T1 `haider-stt` + daemon key-vault mutation notes

Every mutation below was EXECUTED on 2026-08-05: applied to production
code, the named observer run ("running 1 test"), the RUNTIME failure
observed (assertion/typed-error/timeout — never a compile error), then the
mutation reverted and the suite re-run green. Two mutations initially
SURVIVED; both survivors were closed by strengthening the law, re-applying
the mutation, and observing the kill (documented inline).

| # | Production mutation (file) | Runtime observer | Observed RUNTIME failure |
|---|---|---|---|
| M1 | Capitalize the Linux data dir: `xdg.join("diffforge")` → `.join("DiffForge")` (`model_dir.rs`). | `haider-stt/tests/model_dir_tests.rs::linux_default_is_lowercase_diffforge_via_xdg_then_local_share` | `left: Some("/home/kim/.data/DiffForge/whisper") right: Some(".../diffforge/whisper")` — the lowercase-Linux trap law. KILLED. |
| M2 | Drop sha256 verification: hash discarded, temp renamed onto the final path unconditionally (`download.rs`). | `haider-stt/tests/download_tests.rs::corrupted_download_is_refused_and_leaves_nothing` | `panicked: mismatched digest must refuse: Ok(".../ggml-test.bin")` — the corrupt artifact installed instead of erroring. KILLED. |
| M3 | "Helpful" hint write-back: `selected_model_hint` rewrites the normalized id to `selected-model.txt` (`catalog.rs`). | `haider-stt/tests/catalog_tests.rs::selected_model_is_a_read_only_hint` | FIRST RUN SURVIVED — the law's sidecar was already normalized (`small.en`), so the write-back was byte-identical. Law strengthened: the sidecar is now non-normalized (`"  SMALL.EN \n"`), so any rewrite changes bytes. Re-applied: `assertion failed: selected-model.txt must never be written`. KILLED after closing. |
| M4 | Emit `Frames` on standby: `frame_batch` seeded unconditionally instead of inside the recording branch (`capture.rs`). | `haider-stt/tests/capture_tests.rs::frames_are_emitted_only_while_recording` | `panicked: standby leaks no audio` — the privacy gate law. KILLED. |
| M5 | Drop the `has_speech` gate in the chunk taker (`chunker.rs`). | `haider-stt/tests/chunker_tests.rs::speechless_audio_yields_no_chunk_even_at_the_force_ceiling` | `panicked: silence must never produce a chunk` — a silent 35 s chunk was cut. KILLED. |
| M6 | Make the "you" suppression unconditional (remove `is_low_energy_capture` gating) (`policy.rs`). | `haider-stt/tests/policy_tests.rs::drop_table_matches_the_ade_rules` | `left: Some(LowEnergyShortToken) right: None` — a HEALTHY capture's "you" was dropped. KILLED. |
| M7 | Drop the Flux exclusion from the model filter (keep `streaming` only) (`deepgram.rs`). | `haider-stt/tests/deepgram_tests.rs::model_fetch_filters_streaming_true_and_excludes_flux` | `left: ["nova-3", "flux-general-en", "nova-2"] right: ["nova-3", "nova-2"]` — a `/v2/listen`-only model reached the dictation list. KILLED. |
| M8 | Remove the 900 s cost-cap arm from the session select loop (`deepgram.rs`). | `haider-stt/tests/deepgram_tests.rs::abandoned_session_self_finalizes_at_the_cap_with_keepalives` | FIRST RUN SURVIVED — the law called `finish()` before asserting, and `finish` settles the session even without a cap (the assertion was vacuous). Law strengthened: the server-observed CloseStream is now asserted BEFORE `finish()` is ever invoked. Re-applied: `panicked: the cap alone must send CloseStream — finish() was never called` (after the 5 s bounded poll). KILLED after closing. |
| M9 | Bypass `secret_surface_facade` in `transcription_secret_get` (serve `self.hub.accounts()` directly, no transport gate) (`session_hub/rpc.rs`). | `haider-daemon::session_hub_private_tests::transcription_secret_surface_is_uds_and_control_only` | The REMOTE-transport get received the secret response instead of the same-UID `capability_denied` — the matches! assertion at the remote-get row failed. KILLED. |
| M10 | Vault write BEFORE the hygiene refusal (store the invalid key, then refuse) (`session_hub/rpc.rs`). | `haider-daemon::session_hub_private_tests::transcription_secret_hygiene_refuses_before_any_vault_write` | `panicked: a refused key must leave no physical vault item` — the vault list was non-empty after the refusals. KILLED. |
| M11 | Remove `FEATURE_TRANSCRIPTION_V1` from `welcome_features()` (`connection.rs`). | `haider-daemon::connection_tests::welcome_features_pin_served_management_families` | Exact feature-set assertion failed with `transcription_v1` missing from the left set. KILLED. |
| M12 | Swallow the per-spawn model check: `let _ = self.warm.prepare(...)` (`local.rs`). | `haider-stt/tests/local_tests.rs::evicted_model_is_typed_model_missing_per_spawn` | `panicked: evicted model fails: Some("transcribed text")` — the CLI ran against an evicted model and "succeeded". KILLED. |
| M13 | Emit per-chunk text instead of CUMULATIVE assembled text on partial frames (`local.rs`). | `haider-stt/tests/local_tests.rs::partial_session_emits_cumulative_frames_and_assembles` | `left: [("chunk 1", false), ("chunk 2", false), …] right: [("chunk 1", false), ("chunk 1 chunk 2", false), …]` — the second frame lost its prefix. KILLED. |

Verdict: 13 executed mutations, 13 kills; 2 survivors (M3, M8) closed by
law strengthening in the same wave — both strengthened laws stay green on
clean code and kill their mutation on re-application.

The download/deepgram/daemon laws use loopback TCP fixtures. In restricted
sandboxes that prohibit `bind(2)` they compile but stop at fixture setup
with `Operation not permitted`; they execute normally in the workspace/CI
runtime where loopback fixtures are permitted.
