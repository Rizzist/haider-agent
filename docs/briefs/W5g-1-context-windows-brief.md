# W5g-1 — catalog-driven context windows (real limits, never guessed)

Locked v0.1.0 scope says: "per-model context windows from the capability
doc (real limits, never guessed)." We now discover real `context_window`
values live from the codex catalog (confirmed 2026-07-30) and drop them on
the floor. This patch threads them end to end.

## Split

- **Codex lane (this brief):** haider-provider, haider-rpc, haider-daemon.
- **Fable (separate):** haider-tui identity wiring (UI stays with Claude).

## Contract facts (do not re-derive)

- codex `GET /models` items carry `context_window` (integer, tokens) per
  model. Real slugs: gpt-5.6-sol/terra/luna, gpt-5.5, gpt-5.4,
  gpt-5.4-mini, gpt-5.3-codex-spark, codex-auto-review.
- Anthropic `/v1/models` does NOT return context windows (id/display_name
  only) → the field stays `None` for that source. NEVER guess a number.

## Changes

1. **haider-provider `catalog.rs`** — `DiscoveredModel` gains
   `context_window: Option<u64>` (doc: "The provider's declared context
   window in tokens; None when the provider does not declare one — never a
   guess."). `parse_catalog` reads it from the codex shape; the Anthropic
   arm leaves it `None`. Zero/negative/absurd values: treat `0` as absent
   (`None`); keep anything positive as declared.
2. **haider-rpc `frame.rs`** — ADDITIVE tolerant wire: new struct
   `ModelDetailWire { pub name: String, #[serde(default, skip_serializing_if
   = "Option::is_none")] pub context_window: Option<u64> }` and a new field
   on `ProviderSummaryWire`: `#[serde(default)] pub model_details:
   Vec<ModelDetailWire>`. Old peers omit it → empty vec; DO NOT change the
   existing `models: Vec<String>` field (compat). `model_details` rows are
   the same models, same order, as `models` (pickable-filtered).
3. **haider-daemon `provider_registry.rs`** — the registry already stores
   `Vec<DiscoveredModel>`; `discovered_slugs` grows a sibling that keeps
   `(slug, context_window)` for pickable models; `provider_summary` takes
   and publishes `model_details` alongside `models`. Every construction
   site of `ProviderSummaryWire` compiles again with the new field
   (builtin/unknown profiles publish empty details).

## Laws

- Tests NEVER inline — `tests/` dirs or `*_tests.rs` sibling modules only.
- Every law-bearing test documents its mutation: a comment naming the
  mutation and the expected RUNTIME failure (not a compile failure).
- `cargo fmt` clean; `cargo clippy --workspace --all-targets -- -D
  warnings` clean; `CARGO_INCREMENTAL=0` on every cargo invocation.
- Do not touch `Cargo.lock`, version numbers, or any haider-tui file.
- Do not weaken or delete existing tests; update fixtures additively.

## Tests (spec-side skeletons — implement these, minimum)

- provider: `parse_catalog` keeps a declared codex `context_window`;
  absent field → `None`; `0` → `None`. Anthropic arm always `None`.
  (Mutation: parse arm hardcodes `None` → declared-window test fails at
  runtime.)
- rpc: `ProviderSummaryWire` WITHOUT `model_details` in the JSON
  deserializes with an empty vec (old-daemon tolerance); a round-trip with
  details preserves name+window. (Mutation: drop `serde(default)` →
  tolerance test fails.)
- daemon: a registry seeded with discovered models publishes summaries
  whose `model_details` align 1:1 with `models` (same order) and carry the
  windows; a hidden (non-pickable) model appears in neither. (Mutation:
  details built from unfiltered models → alignment test fails.)

## Out of scope

- TUI identity/context-meter wiring (Fable's half).
- Session output caps (already decoupled — `SESSION_OUTPUT_CAP`).
- Catalog persistence/etag behavior (unchanged).
