# H1 observe surface — mutation notes

Every row names a production mutation and the runtime assertion that must fail.
The fixtures use distinct literal values; parked permission and parked input
exist simultaneously and are never inferred from one degenerate case.

| Mutation | Killing test | Expected RUNTIME failure |
|---|---|---|
| Rename/remove `haider.observe.v1`, an object `kind`, or an overview field. | `observe_json_schemas_are_goldened_and_secret_free` | The exact `observe_status.json`, `observe_sessions.json`, or `observe_session.json` bytes differ. |
| Collapse `PermissionRequired` and `InputRequired` into one blocked state. | `session_observe_distinguishes_parked_states_and_never_leaks_secret_material` | Simultaneous `observe-permission` / `observe-input` fixtures no longer produce literal `parked_permission` and `parked_input`. |
| Serialize menu bodies/options/answers or opaque event bodies into `session.observe`. | `session_observe_distinguishes_parked_states_and_never_leaks_secret_material` | `sk-vault-observe-sentinel-7a4e` or `oauth-refresh-observe-sentinel-4c91` appears in the response JSON. |
| Invent a TUI roster callsign when the daemon omitted one. | `observe_json_schemas_are_goldened_and_secret_free` | The daemon-named subagent fixture no longer matches the exact depth golden; `callsign: null` fixtures remain null. |
| Route status update availability into download, staging, install-layout, transaction, marker, or lock code. | `status_reports_update_availability_without_mutating` | `download_calls != 0`, a `.haider-update-stage-*` entry appears, or the literal `.haider-update.lock` / `.haider-update-transaction.json` exists. |
| Wrap stream events, narrow them to known payload kinds, emit CRLF, or omit the trailing LF. | `watch_streams_are_lf_framed_raw_envelopes_and_tolerate_additive_kinds` | The exact raw-envelope round trip for `future_observe_kind_v99` or the one-LF/no-CR assertions fail. |
| Advance a cursor over a gap or reconnect from zero. | `watch_recovers_exactly_after_gap_and_forwards_additive_raw_envelopes` | The output sequence differs from `[1, 2, 3]`, additive payload fields disappear, or the second attach does not carry literal `after_seq: 1`. |
| Capture client replay loss after attach instead of before it. | `replay_overflow_during_attach_is_detected_and_resumed` | A 400-event attach burst drops the caught-up marker invisibly and the five-second stream deadline expires. |
| Send Control instead of View on an observe attachment. | `watch_recovers_exactly_after_gap_and_forwards_additive_raw_envelopes` | The peer's literal `AttachMode::View` assertion fails. |
| Prefer a newer queued cross-branch run over the currently executing run. | `session_observe_distinguishes_parked_states_and_never_leaks_secret_material` | `observe-active-versus-queued` reports `branch-queued` instead of literal `branch-executing`. |
| Read branch membership/heads outside the captured journal prefix. | `session_observe_distinguishes_parked_states_and_never_leaks_secret_material` | The two branch-created facts cease to appear in creation order or a branch coordinate exceeds the digest's sealed `head_seq`. |
| Omit scoped facts from the human overview/depth formats. | `human_views_include_the_scoped_observation_facts` | A literal branch list, footprint, timestamp, permission description, or daemon subagent chip disappears. |
| Route `--no-spawn` through `ensure_daemon`, panic on a missing endpoint, or return generic failure. | `no_daemon_no_spawn_paths_are_typed_69_and_do_not_start_a_daemon` | Exit differs from literal 69 or a socket, `store.sqlite`, or `daemon.log` appears. |
| Invent observe-specific exit codes. | `exit_codes_match_the_headless_table` | The literal `[2, 65, 69, 70, 74, 76, 77, 124, 130]` table or a typed mapping differs. |
| Close response methods instead of retaining unknown-method tolerance. | `observe_methods_preserve_older_client_unknown_method_tolerance` | The actual appended `session.observe` response no longer decodes as `Unknown` through the local pre-H1 response enum. |
| Remove `session_observe_v1` from Welcome. | `welcome_features_pin_served_management_families` | The exact advertised feature set is missing the served method family. |
