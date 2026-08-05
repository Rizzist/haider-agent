# G4a — local OSS model providers (Ollama, LM Studio, generic compat)

Owner contract: "also support local OSS Models". Authority:
docs/research/g4-provider-breadth-seam-map.md +
docs/research/g-wave-external-api-research-2.md (§OSS local). Branch:
`g4a-local-oss`. Enterprise (Azure/Bedrock/Vertex) is G4b — NOT here.

## Locked design decisions

1. Two new presets beside HF/Zen/Go (app.rs:6759-6778 pattern):
   Ollama `http://127.0.0.1:11434/v1`, LM Studio
   `http://127.0.0.1:1234/v1`, each with AccountAddKind variant + free
   keybinding on /providers (h/z/g taken — pick free letters, update the
   footer hints). Both configure api_family OpenAiChatCompletions,
   auth_requirement None, provenance Custom.
2. KEYLESS AUTH ARM: build_account_provider (accounts.rs:5127-5135 gap)
   gets an AuthRequirement::None arm → OpenAiCompatibleProvider at
   profile.base_url sending `Authorization: Bearer ollama` placeholder
   (ollama compat wants non-empty; LM Studio ignores it when auth off).
   If the user DOES store a key for such a profile later, the stored key
   wins. TUI: the add flow for auth-None presets SKIPS the key card and
   goes straight to configure + discovery.
3. LAN POLICY (deliberate loosening, scoped): for Custom-provenance
   providers only, allow RFC1918 private ranges (10/8, 172.16/12,
   192.168/16) over http AND https in validate_compatible_origin /
   blocked_credential_target + catalog backstop. KEEP blocking:
   link-local 169.254.0.0/16 (cloud metadata), multicast, public-http.
   Builtin providers unchanged. Pin the full matrix in tests.
4. DECODER TOLERANCE HARDENING (research §generic): the chat decoder
   must tolerate — missing [DONE] sentinel (stream ends on EOF);
   SSE comment/ping lines (`: ...`); absent usage in stream (no
   stream_options assumption — if we currently send stream_options,
   keep, but treat rejection/absence as normal); tool_call ids absent
   (synthesize stable per-index ids); finish_reason "stop" where
   tool_calls were emitted (still complete the tool calls); unknown extra
   fields (reasoning_content, timings) ignored. One law per tolerance.
5. Model discovery: existing CatalogSource::OpenAiCompatible flow works
   (GET {origin}/models). context_window stays None (catalog "never a
   guess") — do NOT probe /api/show in this wave. Models with empty
   discovery → profile stays Unavailable (existing rule) — the TUI
   /providers row hint for local presets should say "start the server,
   then refresh" when discovery is empty/unreachable.
6. No new wire families, no new adapters — this wave is presets + auth
   arm + origin policy + decoder tolerances only.

## Mandatory laws

- LK1 keyless arm builds a provider (factory test) and the placeholder
  Bearer is sent (request golden); stored-key-wins case.
- LK2 preset configure → registry profile persisted with correct family/
  auth/provenance; discovery against a mock /v1/models populates models
  and flips Available (mock server test, existing pattern).
- LK3 origin matrix: 192.168.x http ALLOWED for Custom; 169.254.x
  refused; public http refused; builtin providers still pinned; catalog
  fetch obeys the same matrix.
- LK4-LK9 decoder tolerance laws (one per item in 4), each non-vacuous
  against the scripted SSE fixtures.
- LK10 TUI: key-card skip for auth-None preset (app-level test), footer
  hint updated.
- Goldens: any new request-shape goldens; rpc transcript only if a new
  request type is added (should NOT be — presets ride ProviderConfigure).

## Discipline

Standard: CARGO_INCREMENTAL=0, per-crate tests, fmt at every commit,
ledger truthful, notes + mutation-notes (≥5 executed kills incl. the
origin matrix and two decoder tolerances). No version bumps, no MCP, no
renames, never delete ~/.codex/sessions.
