# OpenAI authorize URL fixtures

`codex-authorize-url.txt` is the upstream default request at
`openai/codex` commit **2cbbf0c9b542a36a1c3284b5e804917635b6f666**,
independently reconstructed on 2026-09-08 from
[login/server.rs](https://github.com/openai/codex/blob/2cbbf0c9b542a36a1c3284b5e804917635b6f666/codex-rs/login/src/server.rs)
and [auth/default_client.rs](https://github.com/openai/codex/blob/2cbbf0c9b542a36a1c3284b5e804917635b6f666/codex-rs/login/src/auth/default_client.rs).
The generator is adapted from the independent verifier's `reference-check.py`;
it pins both source files by SHA-256 and preserves upstream order and encoding.

From this directory, verify or regenerate the reference:

```sh
python3 generate_codex_authorize_url.py --check
python3 generate_codex_authorize_url.py > codex-authorize-url.txt
```

For offline reconstruction, add `--source-dir /path/to/public-source-files`,
containing `server.rs` and `default_client.rs` with the pinned hashes. The script
does not invoke Codex, inspect credentials, or use the Haider URL builder.

Both fixtures use synthetic state `fixture-state_971` and the RFC 7636 example
challenge `E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM`. Upstream inputs use
the public client ID `app_EMoamEEZ73f0CkXaXp7hrann`, default issuer/port,
`http://localhost:1455/auth/callback`, no forced workspace IDs, and the default
originator. Haider also uses synthetic nonce `fixture-nonce_971`.

The Rust authorize test compares the complete raw Haider URL to both its own
fixture and the Codex fixture with these **exhaustive** intentional differences:

| Difference | Exact pinned transformation from Codex to Haider |
| --- | --- |
| Scope subset | Remove `api.connectors.read` and `api.connectors.invoke`; retain `openid profile email offline_access` |
| Scope encoding | Replace `%20` with `+` only inside that exact scope field |
| State position | Move the same synthetic state from after `codex_cli_simplified_flow` to immediately before `code_challenge` |
| OIDC nonce | Insert `nonce=fixture-nonce_971` immediately after `code_challenge_method=S256` |

State and challenge values are identical between fixtures, so no value
substitution is needed during comparison. No other Haider-only parameters are
allowed. Each replacement requires exactly one full literal match. Endpoint,
parameter order, encoding elsewhere, duplicate fields, and empty query fields
remain byte-significant. Only the fixture's single final newline is removed;
the authorize comparison never parses, sorts, or reserializes a query.

The separate token-form test still compares decoded form fields in addition to
its exact raw-body assertion. That helper is not used for the authorize URL.
