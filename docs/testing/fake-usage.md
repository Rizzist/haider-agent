# Fake usage, reset and plan server for end-to-end tests

The real debug `haiderd` can read usage meters, consume an OpenAI reset
credit and poll the Haider Code plan against a local fake server. Use it
for reset-apply, drain/restart and plan-poller proofs on the real binary.
It never contacts a real provider. Same safety model as the
fake-OAuth hook (`docs/testing/fake-oauth.md`, accounts-ux lane).

## Pieces

| Piece | Where | Role |
| --- | --- | --- |
| Fake server | `scripts/test-support/fake_usage_server.py` | WHAM `usage`, `rate-limit-reset-credits`, holdable `…/consume`, Haider Code `/v1/account`; `/__control`, `/__state`, `/__log` |
| Daemon hook | `crates/haider-daemon/src/debug_fake_usage.rs` | `HAIDER_TEST_FAKE_USAGE_ORIGIN` routes the usage/reset/plan HTTP seams to the fake |

## Safety rules (enforced in code)

- **Debug/test builds only.** The module is `#[cfg(debug_assertions)]`. A
  `--release` `haiderd` must not contain `HAIDER_TEST_FAKE_USAGE_ORIGIN`
  (`strings target/release/haiderd | grep -c HAIDER_TEST_FAKE_USAGE_ORIGIN`
  prints `0`).
- **Explicit opt-in.** Nothing changes unless `HAIDER_TEST_FAKE_USAGE_ORIGIN`
  is set, and it is honoured only while `HAIDER_TEST_FAKE_PROVIDER` is also
  set. The daemon then prints `TEST MODE — usage meters, reset consume and the
  Haider Code plan use the fake server at …` on stderr.
- **Loopback only.** Exactly `http://127.x.y.z:<port>`, no path, query,
  fragment or credentials. A refused value (or a missing
  `HAIDER_TEST_FAKE_PROVIDER`) disables usage, reset and plan networking in
  that process; it never falls back to the live endpoints.
- **Real credentials are never redirected.** A bearer goes to the fake only
  if it is fake-minted: a JWT whose `iss` equals the fake origin, or an
  opaque key starting `hk-haider-test-fake-`. Any other credential is refused
  locally (`test_mode_real_credential_refused`) and sent nowhere. The server
  also classifies every bearer and logs `FOREIGN` if one ever arrives.
- **Isolated profile.** Always use a private `HOME`, `HAIDER_PROFILE_DIR`,
  `HAIDER_RUNTIME_DIR`, `HAIDER_CODEX_AUTH_PATH`, and
  `HAIDER_DISCOVERY_DISABLED=1 HAIDER_DEVICE_DISCOVERY_DISABLED=1`. For a hard
  network fence, run the daemon under a `sandbox-exec` profile that denies
  non-loopback `network-outbound`.

## Run it

```sh
python3 scripts/test-support/fake_usage_server.py --ready-file /tmp/fu.json \
  --write-codex-auth "$HOME/codex-auth.json" &
export HAIDER_TEST_FAKE_USAGE_ORIGIN=$(python3 -c 'import json;print(json.load(open("/tmp/fu.json"))["origin"])')
export HAIDER_TEST_FAKE_PROVIDER='[]'    # required co-opt-in (a fake-provider script)
export HAIDER_CODEX_AUTH_PATH=$HOME/codex-auth.json
target/debug/haiderd                     # then account.oauth_import {source:"codex"}
```

- `account.oauth_import` of `codex` yields an `openai-oauth` account whose
  tokens are fake-issued; `usage.report` and `usage.reset_apply` then reach
  the fake.
- `vault.stage` + `account.login_api` for `haider-code` with the ready file's
  `haider_code_key` activates the plan poller against `/v1/account`.
- `POST /__control {"hold_consume":true}` parks the next consume until
  `{"release":true}`; use it for drain and restart-with-claim-in-flight runs.
  `/__state` lists every consume's `redeem_request_id`, so a retried POST is
  visible.
