# fix-oauth-and-summaries mutation notes

Two daemon-side fixes: the Anthropic OAuth loopback rejection (owner
screenshot, v0.0.65) and roster-truth session summaries ("0 turns · 0 tok"
until attach). Each mutation below was EXECUTED against the finished patch
on 2026-08-04: mutation applied, the named test observed failing at
runtime (assertion, not compile), mutation reverted.

Root cause of the rejection: 91f8156 moved Anthropic's registered redirect
to Claude Code parity (`http://localhost:<port>/callback`) while
`parse_callback` kept demanding `Host: 127.0.0.1:<port>`. A real browser
sends the authority it navigated to, so every legitimate Anthropic
callback — correct state, correct code — was served the 400 rejection
page (and eight of them killed the flow as `callback_interference`). The
fix threads the flow's own composed authority from `compose_redirect`
into the listener; the accepted-with-correct-state law is pinned live.

| Production mutation | Runtime observer | Expected RUNTIME failure |
|---|---|---|
| M1: collapse `compose_redirect`'s Anthropic authority back to `127.0.0.1:<port>` while keeping the localhost redirect (the exact pre-fix regression shape). | `anthropic_localhost_browser_callback_is_accepted_with_correct_state` (live-shaped: fake provider, reqwest follows the redirect, correct state), plus the authority-law and redirect-shape pins | The browser receives the 400 rejection page instead of `SUCCESS_HTML`; the flow never reaches Ready. Killed by 3 tests. |
| M2: drop the Host-authority comparison in `parse_callback` (accept any Host). | `wrong_missing_duplicate_state_path_host_port_and_non_get_are_rejected`, `anthropic_localhost_authority_accepts_correct_state_and_numeric_stays_foreign` | Foreign-authority rows (`localhost` against a hardened flow, wrong port, numeric against the parity flow) parse as `Code` instead of `Invalid(WrongAddress)`. |
| M3: re-tie the default `flow_ttl` to the 5-minute staged-secret TTL. | `default_flow_ttl_is_at_least_ten_minutes` | The ten-minute floor assertion fails (the user reading the consent page / completing 2FA must not expire the listener). |
| M4: restore the bare pre-fix rejection page ("This callback was rejected." with no reason). | `rejection_pages_state_a_reason_and_retry_guidance` | The page lacks the reason sentence and the retry guidance. |
| M5: count subagent-scoped `UserMessage` envelopes as roster turns in `session_summary_truth`. | `summaries_report_turns_and_tokens_for_unattached_sessions` (committed turns fixture, NO attach anywhere) | `turn_count` reports 3 instead of 2 — child prompts inflate the roster. |
| M6: report `Some(0)` footprint tokens whenever no durable snapshot exists, regardless of committed turns. | `zero_is_only_reported_for_truly_empty_sessions` | The turns-without-snapshot session reports zero tokens; "unknown tokens must never be rendered as zero" fails. |
| M7: `#[serde(deny_unknown_fields)]` on `SessionSummary` (strict wire decoding). | `session_summary_roster_truth_fields_are_additive_and_tolerated` | The newer-daemon summary carrying a future additive field fails to decode; older-client tolerance is broken. |

Wire notes: the three summary fields (`turn_count`, `footprint_tokens`,
`footprint_truth`) are additive and optional; the golden transcript keeps
the pre-roster frame byte-frozen as the older-daemon witness and appends
one enriched frame. Absence means unknown — `Some(0)` is reported
exclusively for truly empty sessions, where zero is exact truth.
