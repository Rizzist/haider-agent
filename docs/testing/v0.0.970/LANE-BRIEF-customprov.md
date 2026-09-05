# Lane customprov — adding a custom OpenAI-compatible provider asks for key, then discovers models (v0.0.970, gpt-6-astra)
Worktree lane-970-customprov (from origin/wave-970). OWNER (2026-09-05, screenshot of the ACCOUNTS screen): "add a custom provider —
OpenAI-compatible" asks name -> origin -> MODEL ("the model the server serves (e.g. llama3.1:8b) · the key is asked next") before the key.
Owner: "it should just be api key (/v1/models already fetches) — this needs fix."
CLAIM-AUDIT FIRST (verify every line): crates/haider-tui/src/render.rs:1936-1942 renders the Generic card fields name/origin/model;
crates/haider-tui/src/app.rs:999-1000 pins the Generic field order (Name, Origin, Model) and the edit order (Origin, Model);
app.rs:10203 (`card.models = models`) and :10477 (`if card.models.is_empty()`) show a discovered-models path ALREADY reaching the card;
crates/haider-provider/src/catalog.rs:149 `CatalogSource::OpenAiCompatible { origin }` and :278 `discover_models` fetch `{origin}/models`
under a credential-bearing fixed-origin guard — so discovery exists and only the FLOW is wrong.
Deliver: 1. Flow = name -> origin -> API key -> discovery -> model. After the key is entered, the daemon discovers `{origin}/models`
(existing catalog path; typed errors for unreachable/unauthorized/non-compatible documents) and the card shows the discovered list as a
picker with a sensible default (first id, or the server's advertised default if the document has one); create with one keystroke.
2. Fallback, never a dead end: if discovery fails or returns no ids, the card falls back to a manual model field with the typed reason
shown ("server returned 404 for /models — type the model id"), so servers without /models still work. 3. The edit flow for an existing
custom provider gets the same discovery-backed picker; presets (Ollama, LM Studio, OpenCode Zen/Go, HuggingFace, Azure…) keep their
preset defaults but may refresh from discovery. 4. The key is never echoed, logged, or placed in a flash/event; discovery uses the same
redaction as the rest of the accounts screen. 5. Inventory after create is the discovered list (modelcat's list_models sees it).
Tests: flow-order pins (Generic and edit), discovery-success picker goldens at 80/118/160, discovery-failure fallback golden with the typed
reason, no-key-echo pin, fake `/models` server fixture (success / 401 / 404 / non-compatible body / empty ids), existing accounts-screen
and oauthcapture calendar goldens byte-identical except rows this lane changes (say why per golden). Merge origin/wave-970 forward
BEFORE your verdict (LANE-COMMON). Full gate: `cargo test -q --workspace --no-fail-fast`, `cargo clippy --workspace --tests --
-D warnings`, test-count update. docs/testing/v0.0.970/customprov.md. Commit on the lane branch, no trailer, no push. MANDATORY
`VERIFIER: findings=<n> real=<n> noise=<n> — …`. LAST line SHIP or NO_SHIP.
