# B6b provider-buttons mutation notes

Every mutation below was EXECUTED on 2026-08-02: applied to production code,
the named observer run to a RUNTIME failure (assertion, never a compile
error), then reverted. All observers live in
`crates/haider-tui/tests/b6b_provider_buttons_tests.rs` unless noted.

| # | Production mutation (applied → reverted) | Runtime observer | Observed RUNTIME failure |
|---|---|---|---|
| M1 | `open_oauth_add`: map `AccountAddKind::KimiOAuth` to `"openai-oauth"` (the plausible copy-paste). | `kimi_and_gemini_buttons_dispatch_the_daemon_flows` (plus the other three laws, which all read the card/wire provider) | `assertion left == right failed — left: "openai-oauth", right: "kimi-oauth"` on the card, and the issued `LiveCommand::OAuthStart` carries the wrong provider. Not degenerate: the expectation is a wire-truth literal, not a constant shared with production. |
| M2 | Drop the `FEATURE_ACCOUNT_OAUTH_DEVICE_V1` gate from the `KimiOAuth` hit arm (open unconditionally). | `ungated_provider_button_is_honest` | `no kimi card without the feature` — the card opens against a daemon that never advertised the device flow (the W5e-1b `unknown session method` class). |
| M3 | Make `daemon_lists_provider` return `true` unconditionally. | `ungated_provider_button_is_honest` | `no gemini card without the listing` — the masked key card opens against a pre-B6a daemon whose `provider.list` never mentions gemini. |
| M4 | Delete the `("kimi", "oauth")` arm from the `/login` match (falls to the generic oauth flash). | `slash_login_kimi_oauth_and_gemini_api_parse` | `assertion left == right failed — left: Launcher, right: Accounts`: the slash route never jumps home, no card, no request. |
| M5 | Remove the `+ Kimi (OAuth)` / `+ Gemini (API)` entries from `push_account_add_buttons` (the ONE shared row). | `providers_screen_shares_the_same_buttons` | `kimi renders on /providers` — the shared row loses both buttons on both screens (the render and hit-region assertions). |
| M6 | Regress the mock registry's gemini row to the pre-B6a `Unknown / "adapter not installed"` stub. | `w5d_providers_tests::providers_screen_renders_the_provisional_layout` (and the flipped `w5e3_picker_tests` gemini pins) | The `gemini ● available` / family / `models: gemini-3*` pins fail — the flipped expectations are load-bearing against a stale seed, not restatements of it. |

Degenerate-fixture audit: the new laws assert provider ids, feature names,
screen transitions, and rendered strings as LITERALS in the tests — none are
imported from the production constants they pin, so a mutated constant cannot
satisfy its own expectation. The seed flip (M6) is the one place the tests
and the mock share data; M6 exists precisely to prove the pins fail when the
seed lies.
