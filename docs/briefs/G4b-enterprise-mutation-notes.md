# G4b — enterprise providers: mutation notes

Protocol per kill: working tree committed BEFORE the mutation; ONE
single-anchor mutation applied; the named law run with a filter narrow
enough that `running 1 test` was observed; the failure recorded verbatim;
the mutation reverted via `git checkout --`; the same law re-run green.
All ten kills below were EXECUTED on 2026-08-06 against commits
647039c/2a39558/bf6f31c/77bcc5a (tree clean at each start and finish).

Required coverage from the brief — the azure api-key header mode (K2), the
vertex model-in-URL/version-in-body deltas (K3, K4), the fast refusal on
bedrock/vertex (K5, K6), and the effort naming normalization (K1) — plus
the mantle pinning, availability, and gcloud persistence laws.

## K1 — effort naming normalization (LE-x)

- MUTATION: `crates/haider-provider/src/effort.rs` `base_model` — deleted
  `let model = model.strip_prefix("anthropic.").unwrap_or(model);`.
- RUN: `cargo test -p haider-provider --lib
  effort_tests::le_enterprise_model_names_resolve_their_family_rows --
  --exact` → `running 1 test` → FAILED.
- FAILURE: ``assertion `left == right` failed: bedrock prefix normalizes
  to the opus-5 family row — left: [] right: ["low","medium","high",
  "xhigh","max"]`` (effort_tests.rs:120).
- REVERT → green (`1 passed`).

## K2 — azure api-key header mode (LZ1)

- MUTATION: `crates/haider-provider/src/openai.rs` `with_auth_header` —
  routed the `AzureApiKey` arm through the Bearer arm
  (`request.header(AUTHORIZATION, self.authorization_header()?)`).
- RUN: `cargo test -p haider-provider --lib lz1_azure_request` →
  `running 1 test` → FAILED.
- FAILURE: panic `api-key on GET` (openai_tests.rs:1331 — the
  `.expect("api-key on GET")` found no `api-key` header under the Bearer
  mutation).
- REVERT → green.

## K3 — vertex version-in-body / model-out-of-body (LV1)

- MUTATION: `crates/haider-provider/src/anthropic.rs` `request_payload`
  vertex branch — deleted `object.remove("model");` (anthropic_version
  insert kept), so the body carried BOTH fields.
- RUN: `cargo test -p haider-provider --lib lv1_vertex` → `running 1
  test` → FAILED.
- FAILURE: panic `the vertex body must NOT carry a model field`
  (anthropic_tests.rs:766).
- REVERT → green.

## K4 — vertex model-in-URL (LV1)

- MUTATION: `crates/haider-provider/src/anthropic.rs` `new_vertex` —
  replaced the templated
  `format!("{base}/{}:streamRawPredict", provider.model)` with a
  model-free `format!("{base}/models:rawPredict")`.
- RUN: `cargo test -p haider-provider --lib lv1_vertex` → `running 1
  test` → FAILED.
- FAILURE: ``assertion `left == right` failed: the model rides IN THE URL
  — left: ".../publishers/anthropic/models/models:rawPredict" right:
  ".../models/claude-sonnet-4-5@20250929:streamRawPredict"``.
- REVERT → green.

## K5 — fast refusal on bedrock/vertex, construction gate (LE-x)

- MUTATION: `crates/haider-daemon/src/accounts.rs` `anthropic_fast_for` —
  removed the `!matches!(provider, BEDROCK | VERTEX)` guard (model gate
  kept), so the normalized `anthropic.claude-opus-5` re-admitted fast on
  the enterprise ids.
- RUN: `cargo test -p haider-daemon --lib
  provider_tuning_derives_from_metadata -- --test-threads=4` →
  `running 1 test` → FAILED.
- FAILURE: panic `fast must refuse on bedrock for anthropic.claude-opus-5`
  (accounts_tests.rs:7051).
- REVERT → green.

## K6 — fast refusal on bedrock/vertex, toggle gate (LE-x)

- MUTATION: `crates/haider-daemon/src/model_select.rs` `validate_fast` —
  widened the provider `matches!` to include
  `BEDROCK_PROVIDER_NAME | VERTEX_PROVIDER_NAME`.
- RUN: `cargo test -p haider-daemon --lib le_bedrock_and_vertex --
  --test-threads=4` → `running 1 test` → FAILED.
- FAILURE: panic `fast must refuse on bedrock · anthropic.claude-opus-5`
  (model_select_tests.rs:389).
- REVERT → green.

## K7 — mantle endpoint pinning (LB2)

- MUTATION: `crates/haider-provider/src/anthropic.rs`
  `validate_bedrock_mantle_base_url` — replaced the shape match with
  "any `https://` URL accepted".
- RUN: `cargo test -p haider-provider --lib lb2_new_endpoint` →
  `running 1 test` → FAILED.
- FAILURE: panic `non-mantle shape `https://api.anthropic.com/v1/messages`
  must be refused` (anthropic_tests.rs:741).
- REVERT → green.

## K8 — seeded availability requires a credential (LA-x)

- MUTATION: `crates/haider-daemon/src/provider_registry.rs`
  `provider_summary` — dropped the `credentialed` conjunct from
  `seeded_ready` (endpoint conjunct kept).
- RUN: `cargo test -p haider-daemon --lib la_seeded_list_providers --
  --test-threads=4` → `running 1 test` → FAILED.
- FAILURE: ``assertion `left == right` failed — left: Available right:
  Unavailable`` (provider_registry_tests.rs:539 — the credential-less
  bedrock row lit Available).
- REVERT → green.

## K9 — gcloud refresh persists in the vault (LV2)

- MUTATION: `crates/haider-daemon/src/oauth.rs` `refresh_gcloud` —
  replaced the durable `vault.put(&alias, ..) + resolve` with a staged
  MemoryVault handle (fresh token RETURNED but never persisted).
- RUN: `cargo test -p haider-daemon --lib lv2_gcloud_refresh_source --
  --test-threads=4` → `running 1 test` → FAILED.
- FAILURE: ``assertion `left == right` failed: the refresh PERSISTS in the
  vault, not just the returned handle`` (accounts_tests.rs:7381 — the
  vault still held `STALE_GCLOUD_TOKEN_00aa` bytes while the handle
  carried `FRESH_GCLOUD_TOKEN_11bb`).
- REVERT → green.
- Note: a first anchor attempt failed to match (rustfmt had reflowed the
  `vault.put` chain); no partial mutation was left behind — the file was
  untouched until the corrected anchor applied.

## K10 — azure manual-deployment availability fallback (LZ2)

- MUTATION: `crates/haider-daemon/src/provider_registry.rs`
  `seeded_inventory` — dropped the azure-origin custom arm (`_ => false`;
  bedrock/vertex arm kept).
- RUN: `cargo test -p haider-daemon --lib lz2_azure_custom --
  --test-threads=4` → `running 1 test` → FAILED.
- FAILURE: ``assertion `left == right` failed — left: Unavailable right:
  Available`` (provider_registry_tests.rs:714 — the azure profile with
  manual deployments and a stored key went dark).
- REVERT → green.
- Note: same rustfmt-anchor retry as K9 (first anchor did not match; file
  untouched until the corrected single anchor applied).

## Blindspots found and kept honest

- The construction-time fast gate (K5) and the toggle-time gate (K6) are
  SEPARATE seams — a single law over either one leaves the other
  mutable. Both now carry their own kill-verified law.
- The LV1 golden pins the body deltas and the URL template independently
  (K3 vs K4): the URL equality alone would survive a body mutation and
  vice versa.
- `git status --porcelain` verified clean after the final revert; the
  kills left no residue in the tree.

## Review of record (coordinator, executed post-lane)

Read the branch diff (31 files, no dep/lockfile drift — re-verified the
lane's own git-add-A audit). Spot-verdicts:

1. gcloud shell-out is injection-free by construction: fixed argv
   ["auth", "print-access-token"], no shell, stdin null, bounded output,
   Zeroizing stdout, secret-free typed errors. Accepted as-is.
2. Enterprise origin mutability is scoped by explicit provider-name
   match and shape validators; `enterprise_origin_reconfigure_is_shape_
   validated` pins accept AND refuse directions; the pre-existing
   `existing_custom_provider_identity_fields_are_create_only` law
   survives untouched. Honest residual: the refusal of builtin origin
   reconfiguration for NON-enterprise ids predates this wave and rests
   on its own pre-existing guard, not a G4b law.
3. Seeded-inventory predicate reviewed: bedrock/vertex by name, azure by
   Custom-provenance + origin shape — G4a's discovery-only rule intact
   for every other custom (law la_seeded_list… + LZ2 observe it).

Lane's 10 kills spot-checked against the notes; no discrepancies. No
unobserved gate warranting an executed review mutation. Campaign
ACCEPTED. Ledger 2010 confirmed.
