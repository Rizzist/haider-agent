# googleoauth — Google Antigravity via the official ACP agent (Part A) and linked SuperGrok / Kimi Code logins (Part B)

Date: 2026-09-04
Lane: `lane-970-googleoauth`, based on `wave-970` @ `39d38929` (which already carries `oauthcapture`)
Verdict: see the last line of this document.

---

## 0. Claim audit (done before any edit)

Every `file:line` in the two briefs and in `googleoauth-analysis.md` was grepped for its construct
before it was relied on. The analysis cited an older worktree (`wt-965`), so line numbers had drifted
while the constructs remained correct.

### Part A citations (from `googleoauth-analysis.md`)

| Citation | Verdict |
| --- | --- |
| `haider-daemon/src/oauth.rs:189` — sanctioned registration doc | CORRECT (lands inside that doc comment) |
| `haider-daemon/src/oauth.rs:301` — OpenAI OAuth registration | CORRECT — line 301 is `SANCTIONED_PROVIDER_REGISTRATIONS`'s first entry, `provider_id: OPENAI_OAUTH_PROVIDER_NAME` at 302 |
| `haider-daemon/src/oauth.rs:332` — Anthropic OAuth | CORRECT — `authorization_endpoint: "https://claude.ai/oauth/authorize"` |
| `haider-accounts/src/oauth.rs:1` — versioned token bundles | CORRECT |
| `haider-accounts/src/file_vault.rs:1` — default file vault | CORRECT |
| `haider-daemon/src/provider_registry.rs:1389` — Gemini registered API-key-only | **DRIFTED** — the `GEMINI_PROVIDER_NAME` arm of `builtin_or_unknown` is at **1403**; construct correct |
| `haider-provider/src/gemini.rs:158` / `:383` | **DRIFTED** — both land on unrelated lines; the API-key-bound adapter and its SSE request path are present under different numbers |
| `haider-daemon/src/accounts.rs:7880` — account-backed factory | **DRIFTED** — 7880 is "Adapter-effective provider profile fields"; the actual `match (provider, auth_method)` factory is at **8312**, with OAuth arms at 8474 / 8500 / 8530 / 8586 |
| `haider-protocol/src/credential.rs:59` — account descriptor | CORRECT (derive line immediately above the struct) |
| `haider-cli/src/run.rs:278` — `--account` selection | CORRECT — `"--account" if account.is_none() =>` |
| `haider-protocol/src/usage.rs:293` — usage snapshot states | CORRECT |
| `docs/jsonl-run-contract-v1.md:61` / `docs/provider-lockdown-v1.md:30` | CORRECT (tool-call identity / fixed envelope) |

### Part B citations (from `LANE-BRIEF-googleoauth.md`)

| Citation | Verdict |
| --- | --- |
| `haider-daemon/src/oauth.rs:~412/~437` — `grok-oauth` device flow, auth.x.ai, proxy, Grok CLI headers | **CORRECT for the flow, DRIFTED for the headers.** The Grok `SanctionedOAuthRegistration` spans 406-444 (issuer `https://auth.x.ai` at 408, device endpoint at 409, `flow_mode: OAuthFlowMode::DeviceCode` at 441). The Grok CLI client identifier/version headers are **not in `oauth.rs`** — they live at `haider-provider/src/openai.rs:673-696` (`apply_grok_subscription_headers`) with consts at `openai.rs:93-95` |
| `haider-provider/src/usage.rs:~99-169` — SuperGrok / X Premium meter | CORRECT — `UsageMeterEndpoint::GrokOauth` at 98-100, `GROK_OAUTH_USAGE_URL` at 122, header block 143-169, tier parsed from `subscriptionTier` |
| `haider-provider/src/openai.rs:~1533` | CORRECT — line 1533 is the doc comment for `new_grok_subscription` |
| `haider-provider/src/oauth_identity.rs:~82-190` — `kimi-oauth` identity | CORRECT — `KimiOAuthIdentitySource` at 82, impl at 175-187 returning `Ok(None)` with reason "Kimi Code device OAuth returns no ID token or profile identity" |
| `KIMI_REFRESH_REJECTED_TTL` in `oauth.rs` | CORRECT — `oauth.rs:67`, used at 6096 |

### One structural correction the briefs did not anticipate

Haider has **two** discovery mechanisms, and both already name Grok and Kimi:

- **Layer A — `OAUTH_IMPORT_SOURCE_SPECS` (`haider-daemon/src/oauth.rs:1020-1049`)**: a pre-existing
  one-shot **import** that COPIES the credential into Haider's vault and makes **Haider the refresh
  owner**. It already lists `codex`, `claude-code`, `kimi-code` and `grok-cli`.
- **Layer B — the credential source registry** that `oauthcapture` landed
  (`haider-accounts/src/source_registry.rs`): a **read-through link** where the origin CLI stays the
  refresh owner and the refresh token is never deserialized. It had only `CodexHome` and
  `ClaudeFile`.

Part B is a layer-B extension. This distinction matters for safety, not just tidiness: as
Section 2 shows, both Grok and Kimi rotate their refresh tokens with reuse detection, so the
**existing layer-A import path for `kimi-code` and `grok-cli` is a latent single-holder hazard** —
using it takes over a rotating credential and will eventually log the origin CLI out. That is
flagged here as a finding; layer A was left untouched in this lane.

Layer A's `kimi-code` spec also points at `~/.kimi/credentials/kimi-code.json`, which is the
**legacy** Python CLI's location. The current Kimi Code CLI uses `~/.kimi-code/` (Section 2.2).

---

## 1. Part A — design

### 1.1 What was built and what was deliberately not

Implemented Option B from `googleoauth-analysis.md`: Haider supervises **Google's official
`antigravity-acp` agent** as a subprocess and speaks ACP to it. Google owns the OAuth, the models and
the sessions. **No Google token ever enters Haider's vault.** Explicitly NOT built: direct Google
OAuth, any call to `cloudcode-pa.googleapis.com` / `daily-cloudcode-pa.googleapis.com`, any
`v1internal` endpoint, and any change to the existing API-key `gemini` provider.

### 1.2 Ground truth: the protocol and the artefact were both verified first-hand

Rather than implement from the analysis prose, the ACP v1 JSON schema (170 `$defs`) was read
directly, and **Google's actual binary was downloaded, hashed, extracted and run** in an isolated
sandbox with no credentials. `docs/testing/v0.0.970/_acp-wire-facts.md` records the result. The
handshake the real agent returned:

- `protocolVersion: 1` (an integer, not a string)
- `agentInfo: {name: "antigravity-acp", title: "Google Antigravity", version: "agy_acp_server_1.1.1"}`
- `authMethods`: **`oauth-personal`**, `oauth-business`, `gemini-api-key`, `agent-platform`
- `agentCapabilities`: `loadSession: true`, `sessionCapabilities: {list, resume}`,
  `auth: {logout}`, `promptCapabilities: {image, audio, embeddedContext}`
- The agent resolves its profile from **`$GEMINI_HOME`** (its own stderr says so) and writes
  `$GEMINI_HOME/antigravity-acp/settings.json`

That handshake settles three design points the analysis could only assume: the auth method id is
real and must be pinned to `oauth-personal` (never falling back to the API-key or ADC methods that
sit right next to it in the same list); logout is a negotiated capability that the agent does
advertise; and per-account isolation is achieved through `GEMINI_HOME`.

Measured cost of the child process, first-hand on darwin-aarch64:

| measurement | value |
| --- | --- |
| archive | 316,014,828 bytes |
| extracted footprint | 906,608 KiB (~885 MiB), two Mach-O arm64 files |
| cold start, spawn to `initialize` response | **14.75 s** |
| child RSS at handshake | **230,176 KiB (~225 MiB)** |

This is a separate, large process: roughly 885 MiB on disk and ~225 MiB resident per running account,
with a ~15 s cold start. Linux is about 2.0 GB extracted.

### 1.3 Execution kind, and why agent-executed tools are display-only

ACP is an agent runtime, not a GenerateContent transport. The mapping to Haider's stream is:

| ACP | Haider |
| --- | --- |
| `agent_message_chunk` | `StreamEvent::TextDelta` |
| `agent_thought_chunk` | `StreamEvent::ReasoningDelta` |
| `tool_call` / `tool_call_update` | `StreamEvent::ServerToolUse` / `ServerToolResult` — **display-only** |
| `StopReason` | exactly one terminal `Finish` (`cancelled` is an outcome, never an error) |
| `usage_update` | ignored for accounting (see below) |

The display-only rule is load-bearing. Google's agent executes its own tools inside its own process;
if those surfaced as `ToolCallStart`/`ToolCallEnd` they would re-enter Haider's local dispatch loop
and be executed a **second** time. Haider already has the right vocabulary for this — the
`ServerToolUse` variant exists for Anthropic server tools and OpenAI hosted search — so ACP tool
activity rides it and same-family replay rides `ProviderOpaque`. Because the adapter emits ordinary
`StreamEvent`s, the durable-event pipeline, the JSONL run contract and replay parity apply unchanged.

`usage_update` is defined in ACP as `{used, size, cost?}` = "tokens currently in context" and total
context-window size. That is occupancy, not billing and not subscription quota, so it is not folded
into a billing `Usage`. In practice Antigravity never sends it at all.

### 1.4 Quota and the accounts calendar

**No structured quota or plan is exposed over ACP.** Verified twice: the live protocol matrix probe
of the agent recorded no usage fields anywhere, and the one other client that ships this integration
states plainly that it reports neither paid tier nor remaining subscription quota. Therefore a
Google account's meter reports `Unavailable` with a stable reason rather than a fabricated number,
and Google's published five-hour / weekly refresh cadence is shown only as explanatory plan text —
never converted into a synthetic reset timestamp. Absent provider data is never rendered as zero.

A related trap, recorded because it changes error handling: Antigravity surfaces quota and
subscription failures as **unstructured prose**, and one can arrive inside a turn that still ends
with `end_turn`. A successful stop reason does not prove a successful turn.

### 1.5 Installer

Google's registry entry publishes **no digest and no size** — the registry format defines an optional
`sha256` and Google simply declined to populate it, although other agents in the same registry do.
So Haider owns the pin. All five platform archives were downloaded and hashed first-hand
(`docs/testing/v0.0.970/_antigravity-pins.md`); each value was then cross-checked against an
unrelated third party's published table and **all five matched exactly**, and the sizes match
`Content-Length` from a `HEAD`.

Notable installer facts, all verified rather than assumed:

- Archives are ZIP containing exactly two flat files: the executable and a sibling helper
  (`localharness_external`). Both must validate before activation.
- Only Linux takes an extra argv, `--uid=` with an empty value.
- There is no Intel-macOS build; that host gets a typed unsupported-platform error, never a fallback.
- `dl.google.com` gzips the archive in transit, so integrity must be checked against the bytes on
  disk after transfer decoding — a naive length check against the encoded body mismatches.

### 1.6 Policy posture

Per the owner's 2026-09-03 direction this ships **enabled by default with no policy gate**, carrying
a clear warning instead: Google's published terms restrict third-party access to Gemini
subscriptions/Antigravity, Google ships this ACP agent for editors and reportedly does not enforce
the clause, and the user proceeds at their own risk. This overrides the release-gate recommendation
in `googleoauth-analysis.md`, which predates the owner's decision. The underlying facts are
unchanged and remain worth stating: Google's Antigravity terms do say third-party software accessing
Antigravity breaches the agreement, Google's own documentation approves **Zed** by name and not
Haider, and the claim that the clause is unenforced is hearsay with no primary public source.

---

## 2. Part B — research findings, per CLI, with sources

Read-only research against each CLI's public source. **No real credential file on this machine was
opened, listed or read at any point**, in this research or in the tests.

### 2.1 Grok CLI (SuperGrok / X Premium)

**Which binary is official.** `xai-org/grok-build` (Apache-2.0, Rust, a genuine source drop
including the whole auth subsystem), shipped as npm `@xai-official/grok` — whose npm maintainer is
`xai-security <security@x.ai>` — and as the Homebrew cask `grok-build`. The popular
`superagent-ai/grok-cli` (npm `grok-dev`, ~3.5k stars) is **community-built and explicitly not
affiliated with xAI**; it is API-key only and never writes `auth.json`, though it does share the
`~/.grok` directory for its own settings files. Anything in `~/.grok/auth.json` is the official
CLI's.

| question | answer |
| --- | --- |
| Credential file | `~/.grok/auth.json` (macOS, Linux); `%USERPROFILE%\.grok\auth.json` on Windows — the CLI reads `USERPROFILE`, deliberately not `dirs::home_dir()` |
| Root override | **`GROK_HOME`** (whole directory; used verbatim, empty falls back to default). `GROK_AUTH_PATH` moves only the file; `GROK_AUTH` supplies inline JSON read-only. `XDG_CONFIG_HOME` is **not** honoured and `GROK_CONFIG_DIR` does not exist |
| Format | a JSON **object keyed by scope** `"{issuer}::{client_id}"`; the consumer SuperGrok login is keyed `https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828`. Other keys may include `xai::api_key` (an API key, not OAuth) and a legacy `https://accounts.x.ai/sign-in` |
| Fields | `key` (the access token, a JWT), `refresh_token`, `expires_at`, `create_time`, `auth_mode` (`oidc` for a browser SuperGrok login), `user_id`, `email`, names, `team_*`, `organization_*`. No `id_token` is persisted |
| Store type | plain JSON, `0600` on Unix / current-user ACL on Windows. **No keychain, no keyring, no encryption** — the CLI's own source says "The data is stored in plaintext; OS file permissions are the only protection." Siblings: `auth.json.lock` (advisory flock), `auth.json.corrupt.<millis>` |
| Identity | **present in the store** (`email`, `user_id`, names) — no API call needed |
| Tier | **not a stored field.** Derivable offline from the JWT `tier` claim (0 free, 1 supergrok, 2 x_basic, 3 x_premium, 4 x_premium_plus, 5 supergrok_heavy, 6 supergrok_lite, 7 supergrok_plus) but that can be stale; the live authority is the subscription endpoint |

**Refresh ownership — rotating, single-holder.** `auth.x.ai` rotates the refresh token and applies
reuse detection. The CLI's own source is unambiguous: a comment that "a refresh token can be used
only once"; a `ROTATION_GRACE_MS` constant described as a bound on "how long an IdP may still accept
a refresh token it has already rotated", warning that re-sending it "trips the IdP's reuse detection
and revokes a successor a sibling may hold"; an invariant that the disk write must precede any
network I/O "else a sibling process can reuse the not-yet-rotated RT and the IdP returns
`invalid_grant`"; an flock taken before the irreversible call; and sibling-rotation adoption logic
with a telemetry event for it. Empirically, third parties report `invalid_grant` / "Refresh token has
been revoked" against this endpoint.

**Consequence for Haider: never spend this refresh token.** Read the access token, let it expire, let
the CLI renew. (Coexistence via write-back is theoretically possible — the CLI hot-reloads
`auth.json` and adopts sibling-rotated tokens — but it would require Haider to take the CLI's flock
and become a co-writer of someone else's credential. Out of scope and not done.)

*Sources:* `xai-org/grok-build` — `crates/codegen/xai-grok-shell/src/auth/{model,storage,manager,config}.rs`,
`auth/manager/{refresh_chain,lock}.rs`, `auth/oidc/refresh.rs`, `auth/refresh/oidc_refresher.rs`,
`crates/codegen/xai-dirs/src/lib.rs`, `crates/codegen/xai-grok-shell-base/src/util/secure_file.rs`,
`crates/codegen/xai-grok-shell/src/tier.rs`, and the CLI's own
`docs/user-guide/{02-authentication,05-configuration}.md`; the public
`https://auth.x.ai/.well-known/openid-configuration`; npm `@xai-official/grok`; Homebrew cask
`grok-build`.

### 2.2 Kimi Code CLI

**Which binary is official.** Two official products from the `MoonshotAI` org, and the subject has
migrated between them: the current **`MoonshotAI/kimi-code`** (TypeScript, MIT, npm
`@moonshot-ai/kimi-code`, binary `kimi`, data root `~/.kimi-code/`) and the legacy
**`MoonshotAI/kimi-cli`** (Python, Apache-2.0, PyPI `kimi-cli`, data root `~/.kimi/`), which the
current repo's own migration guide says is being phased out. A naming trap worth recording:
`kimi-cli` is the **official** name on PyPI but an **unofficial** package on npm.

| question | answer |
| --- | --- |
| Credential file | current: `~/.kimi-code/credentials/kimi-code.json`; legacy: `~/.kimi/credentials/kimi-code.json`. Same relative path under two roots |
| Slot-name caveat | for a non-default region/endpoint the file is `credentials/kimi-code-env-<first 16 hex of sha256>.json` instead — a global-region login does **not** land in `kimi-code.json` |
| Root override | **`KIMI_CODE_HOME`** (current, the only data-root override), `KIMI_SHARE_DIR` (legacy). `XDG_CONFIG_HOME` is **not** honoured |
| Fields | `access_token`, `refresh_token`, `expires_at` (absolute unix **seconds**), `expires_in`, `scope`, `token_type` |
| Identity | **none whatsoever** — no email, no user id, no plan. Identity is API-only (`GET /me`). This confirms the assumption already encoded at `haider-provider/src/oauth_identity.rs:175-187` |
| Store type | plain JSON, dir `0700` / file `0600`, atomic tmp+fsync+rename. **No keyring in the current CLI.** The legacy Python CLI did use a keyring but actively migrates entries out of it into the plain file and logs "Keyring storage is deprecated" |
| Tier / plan | never in the store; API-only |

**Revocation tombstone.** After a rejected refresh the CLI rewrites the file in place with
**empty-string tokens** and `expires_at: 0`, keeping `scope`/`token_type`. A reader that only checks
"does the file exist" would mistake a logged-out account for a valid one. Haider classifies an empty
access token as revoked.

**Refresh ownership — rotating, single-holder.** The token endpoint always returns a new
`refresh_token` (the CLI hard-errors on a response lacking one), and the CLI takes a `proper-lockfile`
mutex whose docblock says it will "fail closed rather than refreshing with no lock and **racing
refresh_token rotation**". Its 401 handler comments that "another process rotated the refresh_token
while we were mid-flight", re-reads the file, adopts a peer's token if it changed, and otherwise
writes the revocation tombstone that forces a re-login. The legacy Python CLI mirrors this and its
test suite names the semantics outright. Two unrelated third-party integrators independently
designed around the same hazard — one keeping "independent refresh-token chains" via its own login
because "sharing it would cause refresh-token races", the other treating the CLI's file as
"read-only: it never uses the refresh token and never rewrites the credential file".

**Consequence for Haider: identical to Grok — read the access token, never refresh.**

*Sources:* `MoonshotAI/kimi-code` — `packages/oauth/src/{storage,types,oauth,oauth-manager,token-state,managed-kimi-code,managed-userinfo,managed-usage,region}.ts`,
`apps/kimi-code/src/utils/paths.ts`, `docs/en/configuration/{data-locations,env-vars}.md`,
`docs/en/guides/migration.md`, and the migration golden fixture
`packages/migration-legacy/test/fixtures/golden/.kimi/credentials/kimi-code.json`;
`MoonshotAI/kimi-cli` — `src/kimi_cli/auth/oauth.py`, `src/kimi_cli/share.py`,
`tests/auth/test_oauth_refresh.py`.

### 2.3 What this means for the implementation

Both CLIs are **single-holder rotating credentials, exactly like ChatGPT's**. Neither store is
protected by anything but file permissions, so both are trivially readable — the danger is entirely
on the write/refresh side. Part B therefore links them the same way `oauthcapture` links Codex:
read-through at resolution time, access token only, `refresh_token` never deserialized into a
Haider-owned type, and no Haider code path that could call either token endpoint for a linked row.

---

## 3. What was built

Five slices, each fenced to its own files so they could run concurrently.

### 3.1 ACP protocol core — `crates/haider-provider/src/acp/`

`wire.rs` (serde types, camelCase, every optional `#[serde(default)]`, unknown fields ignored,
integer `protocolVersion`, `AuthMethod` defaulting to `agent` when `type` is absent, `SessionUpdate`
internally tagged with a catch-all so an unknown future variant is ignored rather than fatal),
`codec.rs` (incremental newline-delimited JSON framer), `client.rs` (supervised connection:
JSON-RPC correlation, bounded pending map, inbound-request handler trait whose default REFUSES every
`fs/*` and `terminal/*` call, bounded stderr ring, cancel/terminate/kill/reap), `antigravity.rs`
(the `Provider` impl), `mod.rs`.

Design points worth recording:

- **Lazy authenticate.** `session/new` is attempted first; only the documented
  `-32000 "Authentication required"` triggers `authenticate`, so a warm profile needs no browser
  round trip. `oauth-personal` is matched exactly and only when the method is `agent`-typed —
  there is no fallback to `oauth-business`, `gemini-api-key` or `agent-platform`, all three of which
  the real agent advertises in the same list.
- **Backpressure, not `try_send`**: updates and terminals are delivered with `send().await`, so no
  model output or terminal event can be silently dropped.
- **`CancelOnDrop`**: abandoning a turn detaches a `session/cancel`; armed on timeout, disarmed on a
  real terminal.
- The stderr ring **redacts the OAuth URL**, since 1.1.1 prints it on stderr.

### 3.2 Installer — `crates/haider-daemon/src/antigravity_install.rs`

Release-owned pin table (five platforms), an injectable fetch seam, hostile-by-default extraction,
immutable version directories, an atomic active pointer, and leases.

**Leases carry no staleness timeout.** A lease is an advisory exclusive lock on a uniquely named
file, created under a temp name, locked, then renamed in — so a lease is never observable unlocked
under its final name, and "the lock is free" means "the holder is gone", decided by the kernel
rather than by a guessed interval. This follows `crates/haider-store/src/profile_lock.rs`.

### 3.3 Daemon wiring

`ProviderApiFamilyWire::AcpAgent` is the execution discriminator (additive; the wire transcript grew
from 182 to 183 entries append-only, every prior byte identical). `BUILTIN_PROVIDER_NAMES` went
13 -> 14 with its arity pin updated honestly. The registry arm carries no base URL, OAuth auth, and
an **empty** inventory with no default model, following the `grok-oauth` law: a subscription agent's
own catalog is the only inventory truth, and seeding one here would suppress auto-discovery.

An agent-owned account **never resolves a vault secret**. The arm inside the secret-bearing factory
is a typed refusal that fails closed; real construction runs through a credential-free
`build_agent_owned` seam entered *before* secret resolution. Three secondary construction paths
(compaction promotion, rotation-after-failure, cross-provider fallback) each called `resolve_secret`
unconditionally, so the no-vault law would have held only on the first resolution — they were
funnelled through one shared seam.

### 3.4 TUI surfaces

`/login google` (and the explicit `/login google-antigravity oauth`); an API-key form is refused and
names the separate `gemini` account. The first-login disclosure names Google's agent, its
proprietary licence and its **measured** cost (~316 MB download, ~885 MiB installed, ~225 MiB
resident, ~15 s cold start), and shows the terms warning verbatim. Proceeding appends one
acknowledgement record to a JSONL journal (`{version, subject, acknowledged_at_ms, warning}`,
idempotent per subject) that carries no URL, query or token. The accounts screen then carries the
standing warning badge and a `google-antigravity (ACP)` source badge.

Because Google's agent holds the token in its own `$GEMINI_HOME`, the daemon enrols no credential
source for the alias, so the badge is derived from the account row and pushed through the *same*
`push_account_source_lines` renderer — every timestamp Haider cannot know renders `unknown` rather
than being invented, and a daemon-supplied source suppresses the derived one.

### 3.5 Part B — linked Grok and Kimi sources

`CredentialSourceKind::{GrokHome, KimiCodeHome}` and `CredentialSourceRefreshOwner::{GrokCli,
KimiCli}`. One kind serves both Kimi roots because `~/.kimi-code` and `~/.kimi` lay the credential
down at the same relative path with the same fields. Discovery defaults are `~/.grok`,
`~/.kimi-code` and `~/.kimi`, honouring `GROK_HOME` / `KIMI_CODE_HOME` / `KIMI_SHARE_DIR` exactly as
`CODEX_HOME` is honoured.

The load-bearing invariant is enforced **structurally, not by convention**: `LinkedGrokEntry` and
`LinkedKimiFile` declare no `refresh_token` field at all, so serde skips it and the rotating
credential never enters Haider-owned memory; and one shared `linked_access_material()` mints every
linked bundle with `refresh_token: None` by construction, so no future kind can start owning
rotation. `crates/haider-daemon/src/oauth.rs` is **byte-identical** — Grok's and Kimi's issuer and
client id are read out of the existing `SANCTIONED_PROVIDER_REGISTRATIONS` allowlist rather than
redeclared, so LANE-COMMON's "do not touch `oauth.rs`" held.

Behaviour worth noting: a Grok `xai::api_key` entry is never linked as an OAuth account; several
OIDC entries are disambiguated by the consumer authority prefix and a still-ambiguous store is a
typed failure rather than a guess; Kimi's empty-token tombstone is classified **revoked** rather
than valid; a Kimi endpoint-slot file (`kimi-code-env-<hex>.json`) is linked when unique and a typed
failure when several exist; and the 250 ms watcher fingerprints the path actually resolved, so a
slot rotation wakes reconciliation instead of waiting for the 15-20 s fallback.

## 4. Known gaps and everything unverified

Stated plainly, because several of these bear on whether this lane is shippable.

### 4.1 The model catalog is not populated — a real functional gap

Deliverable 5's model *policy* is implemented and tested (prefer `gemini-3.8-flash-high` only on an
exact offer, otherwise the agent's declared default; reasoning variants stay distinct; a withdrawn
model is refused with a selection remedy rather than silently substituted; slugs are never parsed
structurally). What is missing is the **transport that fills the catalog**: the ACP
`NewSessionResponse` type decodes only `sessionId`, and nothing yet decodes an `availableModels`
list or the `model` select config option.

The consequence is honest but real: an Antigravity turn currently refuses with "published no model
catalog" rather than fabricating a list. That is the correct failure direction — the design forbids
inventing models — but it means **no end-to-end turn can complete yet**.

This was not guessed at deliberately. My live probe only got as far as `initialize`; `session/new`
requires authentication (it returns `-32000`), so the exact field name carrying the catalog was
never observed first-hand. Closing this needs one authenticated `session/new` observation.

### 4.2 Nothing was executed against Google's live service

No Google account was available, so none of the following was exercised: a real OAuth sign-in, an
authenticated session, a real turn, permission mapping against real tool calls, session resume,
logout, or the quota-error path. The ACP client is proven against a fixture agent over duplex
transports; the real binary was proven only as far as `initialize`. Per the analysis's own release
rule ("do not advertise a platform until its official binary has passed the live launch matrix"),
**no platform should be advertised on this evidence alone.**

### 4.3 Platform coverage

Only `darwin-aarch64` was executed. The other four pins were measured by download and hash but their
binaries were never run. Windows child spawn/terminate/reap, Windows file permissions, and Windows
credential-root resolution (`%USERPROFILE%\.grok\auth.json`) are **by inspection**. Three tests are
`#[cfg(unix)]` (subprocess reap, world-writable refusal, symlink-escape) because they need
POSIX-specific facilities; no other test is platform-gated and nothing was gated to reach green.

### 4.4 Smaller unverified items

- The installer's HTTP path was never executed (no network in tests, by rule). Its redirect policy is
  tested through a pure origin predicate — `dl.google.com.evil.example`, `cdn.dl.google.com`,
  plaintext and `storage.googleapis.com` are all refused — but the reqwest wiring and the
  `Accept-Encoding: identity` header that addresses the gzip caveat are unexercised code.
- The ZIP64 branch is unexercised: all five pinned archives are under 4 GiB with two entries. It
  exists so a future ZIP64 pin is not spuriously refused.
- `ACP_CONTEXT_LIMIT` is an assumption mirroring the `gemini-3*` limit. ACP exposes the real window
  only through `usage_update.size`, which Antigravity never sends.
- `max_turn_requests -> FinishReason::MaxTokens` is a judgement call; Haider has no exact counterpart
  for "the agent hit its own request ceiling".
- Vision is declared `Unsupported` even though the live agent advertises
  `promptCapabilities.image`, because this lane sends text blocks only. Declaring it that way is
  deliberate: admitting the provider with vision *undeclared* would let the composer accept a pasted
  image that the prompt builder then silently drops.
- `ToolCallStatus` and `ToolCallContent` value sets were not enumerated from the schema, so both
  decode tolerantly with catch-alls rather than inventing variant names.
- The first-login disclosure's install consent is not conditioned on real install state — there is no
  wire query for "is the agent installed", so the Starting phase says it will install the agent if
  absent. Explicit and never silent, but not state-aware.
- `GROK_AUTH_PATH` and the inline `GROK_AUTH` variable are not supported: the source registry models
  a root plus a fixed relative path. `GROK_HOME` is supported.
- Kimi's non-default endpoint slot is resolved by a unique-glob fallback, not by reading the CLI's
  `config.toml` `oauth.key`. Several candidates produce a typed failure rather than a guess.

### 4.5 Process and policy notes

- `codex` / gpt-5.6 was unavailable for this lane (usage limit until 2026-09-09), so the
  implementation ran on Claude subagents instead of the CLAUDE.md default.
- LANE-COMMON says to leave the work uncommitted and to avoid a workspace-wide clippy; the lane
  instruction for this task requires a commit on the lane branch and a full workspace gate. The
  later, more specific instruction was treated as controlling, as `oauthcapture` did before.
- LANE-COMMON's "do not touch `crates/haider-daemon/src/oauth.rs`" held: that file is byte-identical.
- This machine is shared with the `memwindow` lane. Free disk fell from 16 GB to as low as 1 GB
  during the run, driven by two 13-16 GB `target/` trees. Nothing outside this lane's own tree was
  pruned or touched.
