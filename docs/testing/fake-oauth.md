# Fake OAuth for end-to-end tests

Real `haiderd`/`haider-tui` binaries can complete a Claude (`anthropic-oauth`)
sign-in against a local fake OAuth + profile server. Use it for account
UI/UX, re-login and identity tests, and for any UI e2e run that needs a
signed-in account (for example transcription UI work). It never contacts a
real provider.

## Pieces

| Piece | Where | Role |
| --- | --- | --- |
| Fake server | `scripts/test-support/fake_oauth_server.py` | `/oauth/authorize` (302 to the daemon's loopback callback, PKCE S256), `/oauth/token` (code and refresh grants), `/api/oauth/profile` (identity bound to each token, scriptable failures), `/__control`, `/__state`, `/__log` |
| Browser driver | `scripts/test-support/fake_oauth_browser.sh` | `$BROWSER` stand-in: follows the authorize hop to the daemon callback; refuses any URL not on the fake origin |
| Daemon hook | `crates/haider-daemon/src/oauth.rs`, `debug_fake_oauth` | `HAIDER_TEST_FAKE_OAUTH_ORIGIN` points the `anthropic-oauth` registration at the fake |
| Self-test | `scripts/tests/test_fake_oauth_server.py` | Server contract (CI runs it) |

## Safety rules (enforced in code)

- **Debug/test builds only.** The hook is `#[cfg(debug_assertions)]` and is
  absent from `release`/`ci` builds. Every stage-7 accounts gate proves this
  on a `--release` `haiderd` with `strings`, which must not contain
  `HAIDER_TEST_FAKE_OAUTH_ORIGIN`.
- **Explicit opt-in.** Nothing changes unless `HAIDER_TEST_FAKE_OAUTH_ORIGIN`
  is set. The daemon then logs `TEST MODE — anthropic-oauth OAuth uses the
  fake server at …` on stderr.
- **Loopback only.** The value must be exactly
  `http://127.x.y.z:<port>`, with no path, query, fragment or credentials.
  Any other value disables Claude OAuth in that process; it never falls back
  to the live endpoints. The server binds only `127.*`, and it only
  redirects to `localhost`/`127.0.0.1` callbacks.
- **Real account tokens cannot be redirected.** The fake registration's
  issuer is the fake origin. The following checks are unconditional:
  - A refresh token is sent only to the registration whose issuer minted
    it (`bundle.issuer == registration.issuer`, checked before the request).
  - A legacy profile probe sends a token only when the bundle's issuer is
    the catalog issuer.
  - As a result, a live `https://claude.ai` bundle is never refreshed or
    probed through the fake, and a fake-issued bundle never reaches the
    live endpoints.
- **No credential ingestion in test mode.** While the hook fakes a
  provider, credential import, device adoption and import heal for that
  provider are refused with a typed `unauthorized` error before any
  credential is read (`require_live_oauth_provider`). Imports always stamp
  the issuer from the never-faked live catalog, so even an imported
  credential could not pass the fake registration's issuer guards.
- **Isolated profile.** Always use a private `HOME`, `HAIDER_PROFILE_DIR`
  and `HAIDER_RUNTIME_DIR` (the vault is a per-profile file vault), and
  `HAIDER_TEST_CLAUDE_CREDENTIAL_STORE=unavailable`. Never point it at an
  owner profile.

## Run it

```sh
python3 scripts/test-support/fake_oauth_server.py --ready-file /tmp/fake.json &
origin=$(python3 -c 'import json;print(json.load(open("/tmp/fake.json"))["origin"])')
export HAIDER_TEST_FAKE_OAUTH_ORIGIN=$origin
export BROWSER=$PWD/scripts/test-support/fake_oauth_browser.sh
export HOME=$(mktemp -d) HAIDER_PROFILE_DIR=$(mktemp -d) HAIDER_RUNTIME_DIR=$(mktemp -d)
export HAIDER_TEST_CLAUDE_CREDENTIAL_STORE=unavailable HAIDER_DISCOVERY_DISABLED=1
target/debug/haider-tui     # /accounts -> + Anthropic (OAuth): the login completes by itself
```

Use `/__control` to choose which account the next sign-in authenticates, or
to script profile failures:

```sh
curl -s -XPOST $origin/__control -H 'content-type: application/json' \
  -d '{"identity":{"account_uuid":"acct-b","organization_uuid":"org-1","email":"b@example.test"}}'
curl -s -XPOST $origin/__control -H 'content-type: application/json' \
  -d '{"profile_statuses":[503,503,503],"retry_after":0}'   # outage: saved unverified
```

- **Tokens:** each token's profile reports the account that was current when
  its code was authorized. The email is optional. `access_ttl` sets the
  token lifetime.
- **Log:** `/__log` lists the method, path, status and a short note per
  request. It never contains a token.

## Extending

`FAKEABLE_PROVIDERS` in `debug_fake_oauth` lists the providers the hook may
stand in for. Only add authorization-code providers whose identity comes
from the grant plus a profile document. A provider that needs a signed ID
token (OpenAI, Grok) would need a fake JWKS/signing path first, and that
path must follow the same rules above.
