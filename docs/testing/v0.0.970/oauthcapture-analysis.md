# PLAN.md

**Date:** 2026-09-03  
**Scope:** Read-only research; no builds, edits, token reads, vault reads, or Keychain-secret reads were performed.  
**Evidence tags:** `V-CODE` = verified in local code; `V-DOC` = verified in authoritative documentation; `V-REPORT` = verified as a published issue/report, not necessarily confirmed behavior; `A` = assumption, design recommendation, estimate, or legal interpretation.

## Executive decision

- **[A — conclusion]** Haider cannot both silently capture **all** Claude Code credentials on macOS and guarantee **zero** Keychain dialogs. Existing Claude credentials are controlled by per-item Keychain ACLs; an untrusted Haider binary either receives a denial in no-UI mode or must ask the user.
- **[A — recommendation]** Ship a **multi-source linked-account system with exclusive refresh ownership**, not a generic “copy every refresh token” feature.
- **[A — recommendation]** In strict prompt-free mode:
  - Link/delegate Codex accounts from explicitly enrolled `CODEX_HOME` roots.
  - Never request Claude Keychain secret data.
  - Use Anthropic API credentials owned by Haider, or delegate work to the official Claude Code client.
  - Keep direct subscription-token imports behind an unsupported, explicit policy gate—or omit them.
- **[A — implementation gate]** Do not ship automatic Claude Keychain token import as part of the background daemon.

## A. What is broken now

### Current Haider storage and OAuth behavior

- **[V-CODE]** Account descriptors live separately from credentials, and Haider permits exactly one active account per provider. Credentials are resolved through a broker rather than returned to callers: [lib.rs:1–14](/Users/rizzist/haider-run/wt-965/crates/haider-accounts/src/lib.rs:1), [store.rs:182–239](/Users/rizzist/haider-run/wt-965/crates/haider-accounts/src/store.rs:182).
- **[V-CODE]** The current default is the profile-scoped file vault, not macOS Keychain. Its files and directories use restrictive permissions and atomic writes. The code explicitly says the former Keychain default caused prompts for ad-hoc-signed development builds: [file_vault.rs:1–17](/Users/rizzist/haider-run/wt-965/crates/haider-accounts/src/file_vault.rs:1), [accounts.rs:10583–10608](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/accounts.rs:10583).
- **[V-CODE]** Login/OAuth receipts contain descriptor metadata, not credential values: [event_store.rs:1511–1525](/Users/rizzist/haider-run/wt-965/crates/haider-store/src/event_store.rs:1511), [accounts.rs:1–25](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/accounts.rs:1).
- **[V-CODE]** Haider’s OAuth transport is lazy: starting the coordinator does not initialize HTTP until an OAuth exchange actually requires it: [oauth.rs:2683–2717](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:2683), [http_transport.rs:1–32](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/http_transport.rs:1).
- **[V-CODE]** Current discovery considers only one Codex path and one Claude source: an environment override or the default. It does not enumerate multiple configuration roots: [oauth.rs:1014–1088](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:1014), [device_discovery.rs:54–92](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/device_discovery.rs:54).
- **[V-CODE]** Discovery is not automatic adoption. The CLI exposes explicit `account import SOURCE --confirm`, while the TUI only reports adoption availability: [main.rs:478–504](/Users/rizzist/haider-run/wt-965/crates/haider-cli/src/main.rs:478), [observe.rs:1289](/Users/rizzist/haider-run/wt-965/crates/haider-cli/src/observe.rs:1289).
- **[V-CODE]** Imported aliases can be suffixed `-2`, but import receipts identify only a coarse source such as `codex` or `claude-code`, not an individual configuration root. The current candidate ID also includes a credential fingerprint and therefore changes after token rotation: [accounts.rs:6839–6913](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/accounts.rs:6839), [device_discovery.rs:513–540](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/device_discovery.rs:513).

### Screenshot diagnosis

| Display state | Exact meaning in current code | HTTP metering attempted? | Required repair |
|---|---|---:|---|
| **[V-CODE]** OpenAI `credential account id unavailable` | Credential resolution succeeded, but the descriptor lacks `account_identity.account_id`: [usage_report.rs:488–529](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/usage_report.rs:488). OpenAI identity is derived only from the ID token/account ID during import or login: [oauth_identity.rs:113–135](/Users/rizzist/haider-run/wt-965/crates/haider-provider/src/oauth_identity.rs:113). | **[V-CODE]** No; it returns before the usage request. | **[A]** Reconcile the account against its source and backfill the OpenAI account ID and identity metadata, or rerun a Haider-owned login. |
| **[V-CODE]** Any provider `credential unavailable` | The credential broker could not resolve a usable secret from Haider’s vault: [usage_report.rs:60–83](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/usage_report.rs:60). A descriptor can survive after its old credential has disappeared: [accounts.rs:6649–6653](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/accounts.rs:6649). | **[V-CODE]** No. | **[A]** Relink/import/login, write the credential to the current file vault, and then atomically update the descriptor. |
| **[V-CODE]** Anthropic `credential account id unavailable` | Current Anthropic metering does not require an account ID, and Haider’s Anthropic OAuth response produces no stable account identity: [usage_report.rs:510–546](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/usage_report.rs:510), [oauth_identity.rs:158–172](/Users/rizzist/haider-run/wt-965/crates/haider-provider/src/oauth_identity.rs:158). | — | **[A]** If the screenshot literally shows this reason for Anthropic, it reflects a different build or a paraphrased screenshot. In current code the likely Anthropic reason is simply `credential unavailable`. |

- **[V-CODE]** The TUI converts internal underscores to spaces and renders the failure as `meter unavailable · <reason>`: [format.rs:158–166](/Users/rizzist/haider-run/wt-965/crates/haider-tui/src/format.rs:158), [render.rs:3644–3651](/Users/rizzist/haider-run/wt-965/crates/haider-tui/src/render.rs:3644).
- **[A — root cause]** The registered descriptors and the current file-vault contents are out of reconciliation, and the existing single-source discovery cannot find the owner’s other logins.

### Existing Keychain code

- **[V-CODE]** Haider still contains a generic-password Keychain backend using service `ai.haider.agent` and Apple Security.framework. It does not invoke the `security` CLI: [keychain.rs:17–18](/Users/rizzist/haider-run/wt-965/crates/haider-accounts/src/keychain.rs:17), [keychain.rs:54–106](/Users/rizzist/haider-run/wt-965/crates/haider-accounts/src/keychain.rs:54).
- **[V-CODE]** Repository-wide source inspection found no production use of `security find-generic-password` or `security add-generic-password`; the macOS dependencies are `security-framework`: [haider-accounts/Cargo.toml:17–18](/Users/rizzist/haider-run/wt-965/crates/haider-accounts/Cargo.toml:17).
- **[V-CODE]** Claude discovery knows the service `Claude Code-credentials`. Ordinary discovery uses a no-interaction query, but explicit “significant” adoption can perform an interactive Keychain read and prompt: [oauth.rs:80–82](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:80), [oauth.rs:1272–1323](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:1272), [oauth.rs:1555–1644](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:1555).
- **[V-CODE]** A denied or locked native-store lookup currently blocks fallback to Claude’s credentials file; only `Missing` falls back: [oauth.rs:1811–1836](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:1811).
- **[A — fix]** Strict mode must prohibit the interactive adoption path and must not let a denied Keychain probe suppress an otherwise readable, explicitly enrolled file source.

## B. Options

`V` and `A` below use the evidence definitions at the top.

| Option | Prompt-free on macOS? | Freshness / refresh owner | Multi-account support | Cross-platform | ToS risk | Effort |
|---|---|---|---|---|---|---|
| Import every enrolled Codex `auth.json` | **V:** Yes when that root uses file storage. **V:** Current Codex also supports `keyring`, `auto`, and `ephemeral`, so not every current installation has a readable file: [storage.rs](https://github.com/openai/codex/blob/main/codex-rs/login/src/auth/storage.rs), [config schema](https://github.com/openai/codex/blob/main/codex-rs/core/config.schema.json). | **A:** Safe only if Codex remains refresh owner and Haider rereads/delegates. Spending the copied refresh token risks single-use-token races. | **V:** Yes through separate `CODEX_HOME` roots; Codex authentication is scoped to its home, while ordinary configuration profiles do not independently isolate auth: [Codex issue #15410](https://github.com/openai/codex/issues/15410). | **V:** Yes for file-backed stores. | **A:** Medium. OpenAI documents Codex CLI, app server, and SDK usage, but does not expressly authorize arbitrary third-party bearer reuse. | **A:** Medium. |
| Import Claude Code Keychain credentials | **V:** No guarantee. A no-UI query can safely fail, but reading every existing protected item requires ACL authorization. | **A:** Unsafe if both Haider and Claude spend the rotating refresh token. Claude must remain owner or the credential must be transferred exclusively. | **A:** Fragile on macOS. The default service represents one login; hashed alternate services tied to `CLAUDE_SECURESTORAGE_CONFIG_DIR` are only reported, not documented: [Claude issue #79223](https://github.com/anthropics/claude-code/issues/79223). | **V:** macOS-specific. | **V-DOC:** High: Anthropic explicitly forbids third parties from offering Claude.ai login or routing Free/Pro/Max credentials for users: [Anthropic legal and compliance](https://code.claude.com/docs/en/legal-and-compliance). | **A:** High, and incompatible with strict zero-prompt acceptance. |
| Detect Claude file storage | **V:** Yes for readable files. **V:** Official docs place macOS credentials in Keychain; they do not document a switch forcing file storage there: [authentication](https://code.claude.com/docs/en/authentication), [environment variables](https://code.claude.com/docs/en/env-vars). | **A:** Claude remains refresh owner; Haider watches and rereads. | **V/A:** Multiple file roots are practical on Linux/Windows; `CLAUDE_CONFIG_DIR` separates configuration, but official docs do not promise separate macOS secure-storage identities. | **V:** File layout is useful outside macOS; not uniform across platforms. | **V-DOC:** High if Haider directly uses subscription tokens. | **A:** Medium. |
| Run Haider-native OAuth login N times and keep credentials in Haider’s file vault | **V-CODE:** Avoids Claude Keychain. Browser/device authorization still requires user action. Haider already has OpenAI and Anthropic OAuth registrations: [oauth.rs:301–377](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:301). | **A:** Good technically because Haider is sole refresh owner. | **A:** Yes after descriptors, receipts, and login UX gain a stable per-login source/account identity. | **A:** Yes. | **A:** OpenAI remains unsupported/unclear. **V-DOC:** Anthropic subscription login is explicitly unacceptable for a third-party client, even though the code can perform it. | **A:** Medium–high. |
| Hybrid: import Codex files + Haider-native Anthropic OAuth | **A:** Yes with respect to Keychain. | **A:** Codex must remain source-owned; Haider owns Anthropic refresh. | **A:** Yes after multi-source work. | **A:** Mostly. | **V-DOC:** Unacceptable for Anthropic under the published policy. | **A:** Medium. |
| **Recommended:** source-linked accounts; Codex app-server delegation; Anthropic API credentials or official Claude Code delegation | **A:** Yes for Haider’s strict daemon because it never reads Claude Keychain secrets. A broken ACL could still make the official Claude executable show its own prompt, so unattended Claude delegation needs a preflight acceptance test. | **V/A:** Codex/Claude refresh credentials they created; Haider refreshes only Haider-owned credentials. Codex app server explicitly owns persistence and refresh: [Codex app-server README](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md). | **A:** Yes, one enrolled root/source per linked account. | **A:** Yes, subject to official-client availability. | **A:** Lowest available subscription-token risk because Haider never reuses the bearer directly. API keys are the documented Anthropic integration route. | **A:** High initially, low operational risk. |

## C. Recommended design

### C1. Define the supported promise precisely

- **[A — requirement]** “All accounts” means **all explicitly enrolled source roots plus the platform default**, not every arbitrary directory ever supplied through a shell environment variable.
- **[V-DOC]** Codex stores authentication under `CODEX_HOME`; Claude supports relocating configuration with `CLAUDE_CONFIG_DIR`: [Codex storage source](https://github.com/openai/codex/blob/main/codex-rs/login/src/auth/storage.rs), [Claude environment variables](https://code.claude.com/docs/en/env-vars).
- **[A — constraint]** There is no documented machine-wide registry from which Haider can enumerate every historical or shell-specific `CODEX_HOME` or `CLAUDE_CONFIG_DIR`.
- **[A — product wording]** Name the feature “multi-source account linking and reconciliation,” not “capture every device secret.”

### C2. Source registry and discovery

- **[A — design]** Add a profile-scoped source registry containing:
  - Default Codex home plus explicitly added `CODEX_HOME` roots.
  - Default Claude configuration plus explicitly added `CLAUDE_CONFIG_DIR` roots.
  - Haider-native file-vault accounts.
  - Source type, canonical root, user-facing label, enabled state, credential-store mode, refresh owner, and last scan result.
- **[A — security]** Never crawl the entire home directory. Accept bounded explicit paths, canonicalize them, reject unsafe symlink escapes, and retain the existing file-size limit.
- **[V-DOC]** Current Codex `auth.json` contains authentication mode, tokens, last refresh, and optional additional identity fields; token data includes ID token, access token, refresh token, and account ID: [storage.rs](https://github.com/openai/codex/blob/main/codex-rs/login/src/auth/storage.rs), [token_data.rs](https://github.com/openai/codex/blob/main/codex-rs/login/src/token_data.rs).
- **[V-DOC]** Codex access expiry is derived from the JWT, and its manager refreshes within a five-minute window using `https://auth.openai.com/oauth/token`; no fixed lifetime should be hard-coded: [manager.rs](https://github.com/openai/codex/blob/main/codex-rs/login/src/auth/manager.rs).
- **[V-CODE]** Haider currently observes the same OpenAI token endpoint and client ID `app_EMoamEEZ73f0CkXaXp7hrann`: [oauth.rs:301–330](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:301).
- **[V-CODE]** Claude’s parsed structure contains `accessToken`, `refreshToken`, `expiresAt`, optional refresh-token expiry, scopes, subscription type, and client ID: [oauth.rs:2192–2289](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:2192).
- **[V-CODE]** Haider’s observed Claude flow uses `claude.ai/oauth/authorize`, `console.anthropic.com/v1/oauth/token`, and client ID `9d1c250a-e61b-44d9-88ed-5944d1962f5e`: [oauth.rs:332–377](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:332).
- **[A — compatibility warning]** Those Claude endpoints, client ID, scopes, and credential schema are observed implementation details, not a documented third-party OAuth contract.
- **[V-DOC]** Claude provides `claude auth status` with JSON output, and supports non-interactive execution through `claude -p`: [CLI reference](https://code.claude.com/docs/en/cli-usage), [headless mode](https://code.claude.com/docs/en/headless).
- **[A — strict-mode rule]** Do not run `claude auth status` automatically in the background until an unattended macOS test proves the installed Claude binary can access its own item without a dialog. A source may remain `present/unverified` instead.

### C3. Registration model

- **[A — design]** Create one stable Haider account record for every discovered or explicitly enrolled login:

| Field | Proposed meaning |
|---|---|
| `id` | Stable Haider UUID, independent of token contents or rotation. |
| `alias` | User-editable selector used by `/account` and `--account`. |
| `provider` | `openai` or `anthropic`. |
| `auth_kind` | `codex-delegated`, `claude-delegated`, `external-file-mirror`, `haider-oauth`, or `api-key`. |
| `provider_account_id` | Optional exact provider ID where available; required for current OpenAI meter calls. |
| `identity_key` | Stable issuer/subject or provider-account hash used for deduplication. |
| `plan` | Optional verified plan such as Pro/Max; never invented when unavailable. |
| `email_masked` | Display-only masked identity; store full email only if an explicit product requirement justifies it. |
| `source` | Stable source UUID plus source kind and a safe display label; do not use a changing token fingerprint as source identity. |
| `refresh_owner` | `codex`, `claude-code`, `haider`, or `none`. |
| `last_seen_at` | Last successful source reconciliation. |
| `last_refreshed_at` | Source-declared refresh time when available. |
| `access_expires_at` | Parsed JWT expiry or Claude `expiresAt`; nullable. |
| `generation` | Monotonic credential generation for atomic broker updates. |
| `health` | `ready`, `stale`, `source-missing`, `identity-incomplete`, `keychain-restricted`, `relogin-required`, or `policy-blocked`. |

- **[A — privacy]** Tokens and refresh tokens remain exclusively in the appropriate secret store or origin process. They must never enter descriptors, receipts, event payloads, UI state, logs, or debug formatting.
- **[A — migration]** Legacy `openai-oauth` and `anthropic-oauth` records should be reconciled into this model. If no matching secret/source exists, retain the descriptor but mark it `source-missing` instead of presenting it as a usable account.
- **[A — migration]** Recover OpenAI account identity from the enrolled Codex source or a fresh Haider-owned login, then rerun metering. Do not invent an account ID.
- **[A — migration]** Anthropic records do not require account IDs for the current meter path; repair their broker linkage or replace them with a supported credential/delegation mode.

### C4. Refresh and reconciliation ownership

- **[A — invariant]** Exactly one component may spend a refresh token.
- **[V-REPORT]** Codex users have reported that copying one refresh token across homes causes failures because rotation or reuse invalidates another copy: [Codex issue #15410](https://github.com/openai/codex/issues/15410).
- **[V-REPORT]** Claude users have similarly reported rotation and concurrent-client refresh failures: [Claude issue #88583](https://github.com/anthropics/claude-code/issues/88583), [Claude issue #72006](https://github.com/anthropics/claude-code/issues/72006).
- **[A — source-linked Codex]**
  - Codex owns refresh.
  - Haider either delegates through one app-server instance per `CODEX_HOME`, or rereads the atomically replaced `auth.json`.
  - If a fresh credential is needed, request refresh through Codex’s documented app-server account interface rather than posting the copied refresh token.
  - A compatibility mirror may copy the current access token into Haider’s vault, but must not copy or spend the refresh token.
- **[A — source-linked Claude]**
  - Claude Code owns refresh and secret storage.
  - Haider delegates inference to the official CLI/Agent SDK under the selected enrolled source.
  - Haider does not copy access or refresh tokens from macOS Keychain.
  - Do not send synthetic “keepalive” prompts merely to force refresh.
- **[A — Haider-owned credentials]**
  - API keys stay in Haider’s file vault.
  - Any future provider-approved OAuth integration is refreshed only by Haider, under the existing per-account lease/single-flight mechanism.
- **[V-CODE]** Haider already serializes refreshes through its vault lock and persists refreshed credentials before publishing them: [oauth.rs:5535–5616](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:5535), [oauth.rs:6011–6179](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/oauth.rs:6011).
- **[A — correction]** For externally owned accounts, remove the current fallback that lets Haider spend a copied Codex refresh token after source reread fails. Mark the account stale or ask the origin broker to refresh instead.
- **[A — scheduler]** Use filesystem notifications plus a jittered periodic reconciliation pass. Publish only a fully validated newer generation; never select freshness by comparing opaque token strings.

### C5. Selection UX

- **[V-CODE]** Haider already supports `/accounts`, `/account`, `/login`, CLI `--account`, and persisted exact session account aliases: [commands.rs:70–74](/Users/rizzist/haider-run/wt-965/crates/haider-tui/src/commands.rs:70), [run.rs:278–286](/Users/rizzist/haider-run/wt-965/crates/haider-cli/src/run.rs:278), [session.rs:70–91](/Users/rizzist/haider-run/wt-965/crates/haider-protocol/src/session.rs:70).
- **[V-CODE]** An exact session pin currently disables provider rotation; an unpinned session follows the active account: [accounts.rs:8819–8936](/Users/rizzist/haider-run/wt-965/crates/haider-daemon/src/accounts.rs:8819).
- **[A — design]** Extend the accounts screen to show:
  - Alias, provider, plan, masked email.
  - Source label and credential ownership.
  - Last seen/refreshed and access expiry.
  - `ready`, `stale`, `keychain-restricted`, or `policy-blocked`.
  - Active provider default and current-session pin.
- **[A — precedence]** Exact session account → CLI `--account` at session creation → provider active default.
- **[A — safety]** A pinned account must never silently rotate to another owner’s account. If stale, fail with an actionable relink/relogin message.

### C6. Guaranteeing no Keychain dialog

- **[V-DOC]** macOS Keychain applies per-item access controls and trusted-application ACLs. Apps outside an item’s trust may require user approval: [Apple ACL documentation](https://developer.apple.com/documentation/security/access-control-lists), [Apple Keychain access guide](https://support.apple.com/en-ie/guide/mac-help/kychn002/mac).
- **[V-DOC]** “Always Allow” records continuing permission for that application; code identity and designated requirements allow signed updates to retain identity across releases: [Apple code-signing identity](https://developer.apple.com/library/archive/documentation/Security/Conceptual/CodeSigningGuide/AboutCS/AboutCS.html), [Apple signing procedures](https://developer.apple.com/library/archive/documentation/Security/Conceptual/CodeSigningGuide/Procedures/Procedures.html).
- **[A — exact trigger]** A prompt occurs when a data-returning Keychain operation targets an item whose ACL does not already trust the requesting application and interaction is allowed. Unlocking the login Keychain does not override that item ACL.
- **[V — local CLI help]** `security find-generic-password -w` changes output to the password alone; it does not suppress Keychain authorization UI.
- **[V — local CLI help]** `security add-generic-password -T <app>` sets trusted applications while adding/updating an item. `-A` trusts every application and is unacceptable.
- **[A — ACL implication]** Adding Haider to an existing Claude-created item’s ACL is a mutation and needs owner authorization at least once. It is not a read-only, silent solution.
- **[A — one-time weaker mode]** If the owner relaxes the requirement:
  1. Install a Developer-ID-signed `haiderd` with a stable identifier and designated requirement.
  2. Perform one explicit foreground read per Keychain item.
  3. Choose “Always Allow.”
  4. Never change the signer/designated requirement in updates.
  5. Expect another authorization if Claude replaces the item or the trusted identity changes.
- **[A — development limitation]** Ad-hoc or frequently rebuilt binaries cannot promise durable trust and must not offer interactive Claude import.
- **[A — strict implementation]** Add `keychain_interaction = "never"` as the default and enforce:
  - Only no-UI Security.framework queries.
  - No interactive fallback.
  - No `security ... -w`.
  - No background invocation of an origin executable unless its unattended behavior has passed the prompt-canary test.
  - `Denied`, `Locked`, and `InteractionNotAllowed` become visible health states, not fallback prompts.

### C7. Policy gate

- **[V-DOC]** Anthropic states that subscription OAuth is for Claude Code and Anthropic’s own applications; third-party developers must use API keys and may not offer Claude.ai login or route Free/Pro/Max credentials on users’ behalf: [Anthropic legal and compliance](https://code.claude.com/docs/en/legal-and-compliance).
- **[A — consequence]** Importing Claude Code tokens and Haider’s existing Claude-client-ID login are both technically possible but contractually unsafe for a distributed third-party product.
- **[V-DOC]** Anthropic documents `CLAUDE_CODE_OAUTH_TOKEN`/`setup-token` for Claude Code automation, not as authorization for unrelated direct API clients: [Claude authentication](https://code.claude.com/docs/en/authentication).
- **[V-DOC]** OpenAI permits ChatGPT-plan use through official Codex surfaces and provides a programmatic app-server/SDK path: [Using Codex with your ChatGPT plan](https://help.openai.com/en/articles/11369540-using-codex-with-your-chatgpt-plan), [Codex app server](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md).
- **[V-DOC]** OpenAI’s terms prohibit credential sharing and circumvention, but the reviewed material does not contain Anthropic’s equally explicit third-party OAuth prohibition: [OpenAI Terms of Use](https://openai.com/policies/terms-of-use/).
- **[A — legal interpretation]** Direct Codex subscription-token reuse by Haider remains unsupported and carries enforcement risk even when the account owner and machine are the same. Delegating to the official Codex app server is the safer boundary.
- **[A — release gate]** Direct subscription-token compatibility modes require legal review, explicit informed opt-in, prominent unsupported-status disclosure, and a remote kill switch.

## D. Tests and acceptance criteria

- **[A — acceptance]** The implementation lane passes only when all tests below succeed without real credentials.

### Security and prompt tests

- **[A — acceptance]** On macOS with the login Keychain locked, unlocked, and containing a dummy ACL-restricted test item, daemon startup, discovery, reconciliation, metering, and shutdown produce zero authorization dialogs.
- **[A — acceptance]** A Keychain test double records zero interactive data queries in strict mode; attempting one fails the test immediately.
- **[A — acceptance]** `Denied`, `Locked`, timeout, and interaction-not-allowed states complete within the configured deadline and never block daemon startup.
- **[A — acceptance]** Repository checks reject production invocations of `security find-generic-password -w`, `-A`, or interactive Security.framework options.
- **[A — acceptance]** Logs, event receipts, RPC frames, UI snapshots, panic output, and debug formatting are scanned with dummy token sentinels; no sentinel appears outside the test vault/source fixture.

### Multi-source discovery

- **[A — acceptance]** Three enrolled `CODEX_HOME` roots produce three stable account records even when aliases, plans, or emails match.
- **[A — acceptance]** Rotating tokens changes `generation` and expiry but never changes the account or source ID.
- **[A — acceptance]** Duplicate enrollment of the same canonical root is idempotent.
- **[A — acceptance]** Unenrolled arbitrary directories are not scanned.
- **[A — acceptance]** Symlink escape, oversized file, partial write, invalid JSON, missing fields, unreadable source, and source deletion each produce distinct safe health states.
- **[A — acceptance]** Codex `file`, `keyring`, `auto`, and `ephemeral` configurations are distinguished; strict mode never silently accesses a Keychain-backed Codex credential.

### Refresh ownership and races

- **[A — acceptance]** An externally owned Codex account never causes Haider to POST its copied refresh token.
- **[A — acceptance]** After Codex refreshes, Haider observes and publishes the new generation atomically.
- **[A — acceptance]** Simultaneous watcher events and account resolution collapse to one reconciliation.
- **[A — acceptance]** A stale externally owned account fails closed instead of spending its refresh token.
- **[A — acceptance]** Haider-owned OAuth refresh remains single-flight across tasks/processes and persists access and rotated refresh tokens before returning them.
- **[A — acceptance]** Ambiguous refresh transport failures never trigger an automatic second spend of a potentially rotated token.

### Screenshot regression

- **[A — acceptance]** A valid OpenAI credential with no account ID produces `identity incomplete`, performs no meter HTTP request, and offers a relink action.
- **[A — acceptance]** Successful reconciliation backfills the OpenAI ID and enables metering.
- **[A — acceptance]** A descriptor with no vault secret produces `source missing` or `relogin required`, not an apparently registered/healthy row.
- **[A — acceptance]** Anthropic does not report an account-ID requirement.
- **[A — acceptance]** Metering errors differentiate broker failure, missing identity, source restriction, provider response, and unsupported delegated metering.

### Selection and UX

- **[A — acceptance]** Accounts can be selected in the accounts screen, via `/account`, and via `--account`.
- **[A — acceptance]** A persisted session pin resolves the same stable account after token rotation and daemon restart.
- **[A — acceptance]** A pinned stale account never falls through to another paid account.
- **[A — acceptance]** Masked email, plan, source, refresh owner, expiry, and policy status are visible without exposing credential material.

### Policy and release

- **[A — acceptance]** Strict builds contain no automatic Claude subscription-token import.
- **[A — acceptance]** Unsupported direct-token modes are compile-time or policy-gated, default off, auditable, and accompanied by legal approval.
- **[A — acceptance]** The product documentation says “all enrolled sources,” not “all credentials on the machine.”

## E. Risks and ecosystem findings

### Comparable harnesses

- **[V-DOC/CODE]** OpenCode implements its own Codex OAuth flow and credential persistence rather than importing Codex CLI’s rotating refresh credential: [OpenCode Codex plugin](https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/plugin/codex.ts).
- **[V-CODE]** Cline similarly performs its own provider OAuth flow and saves its own OpenAI access/refresh/account data: [Cline CLI auth](https://github.com/cline/cline/blob/main/apps/cli/src/acp/auth.ts), [Cline auth service](https://github.com/cline/cline/blob/main/apps/vscode/src/sdk/auth-service.ts).
- **[V-DOC]** Aider’s supported configuration is API-key based rather than subscription-token import: [Aider API-key documentation](https://aider.chat/docs/config/api-keys.html).
- **[V-REPORT]** Pi add-ons such as `pi-codex-account` and `pi-account-switcher` snapshot or swap account stores. That pattern is convenient but inherits refresh-rotation risk: [pi-codex-account](https://github.com/fadilsflow/pi-codex-account), [Pi account switcher](https://pi.dev/packages/pi-account-switcher).
- **[A — name resolution]** No authoritative, relevant OAuth harness named “rick” was identified. If “Rig” was intended, its account-switching/setup-token pattern should be treated as ecosystem precedent, not proof that direct subscription-token reuse is supported.
- **[A — conclusion]** The safer common architecture is independent OAuth ownership or delegation. Snapshot/swap extensions are operational tools, not a sound concurrent-refresh protocol.

### Material risks

- **[V-DOC/A]** Claude Keychain import cannot satisfy strict zero-prompt coverage for every existing item; this is the primary hard limitation.
- **[V-DOC/A]** Current Codex may use Keychain instead of `auth.json`; file-only discovery can therefore report incomplete coverage.
- **[A]** “All accounts” will remain incomplete when configuration roots were never enrolled.
- **[V-REPORT/A]** Refresh-token rotation can log out the origin client if Haider and the origin both spend the same token.
- **[A]** Provider schema, endpoints, scopes, and OAuth client acceptance can change without notice.
- **[V-DOC/A]** Anthropic can enforce its third-party OAuth restriction without advance notice; imported or Haider-native subscription logins could be revoked.
- **[A]** OpenAI can similarly restrict an undocumented third-party reuse pattern even though the reviewed terms are less explicit.
- **[A]** The file vault avoids macOS dialogs but offers weaker OS-mediated isolation than Keychain; filesystem permissions, backup exposure, and malware in the same user context remain relevant.
- **[A]** Masked email, plan, source paths, and account identifiers remain privacy-sensitive metadata even when tokens are absent.
- **[A]** Delegated official-client operation may not map cleanly onto Haider’s existing raw-provider transport, streaming, metering, and tool semantics; that integration is the principal engineering cost.
- **[A]** Anthropic API-key accounts may not provide the subscription experience the owner originally requested, but they are the documented third-party integration route.
- **[A — final implementation decision]** Ship multi-source registration, stable identity, selection, strict no-dialog enforcement, source-owned refresh, and supported delegation/API credentials. Do not ship automatic Claude Keychain secret capture or shared refresh-token ownership.

SHIP (plan delivered)