# Provider lockdown v1

Status: normative for `provider_lockdown_v1`  
Contract date: 2026-08-28

Provider lockdown is a daemon-enforced capability ceiling for models supplied
by providers that are not fully trusted. It is not a system-prompt request and
is not session identity. The provider of the model selected for a turn decides
the envelope. A user permission, stored Allow rule, autonomous run, loop, or
headless client cannot widen it.

## Threat model

Lockdown assumes a provider may produce arbitrary tool-call names and
arguments, including calls omitted from the advertised schema. The daemon
therefore reduces the tool schema at each turn boundary and independently
refuses forged calls below the normal permission policy. The design protects
the local shell and process tree, credentials and profile state, peers and SSH
targets, MCP servers, hooks and monitors, checkpoint application, and files
outside a provider-specific sandbox.

It does not make provider output trustworthy. Text and web results remain
untrusted content. Network access is limited to Haider's bounded web/search
tools when those tools are otherwise present; no generic process, shell, MCP,
or arbitrary command route is exposed.

The automatic-hermetic specialization below is stricter than this configured
envelope and exposes no web/search route.

## Fixed envelope

| Capability | Lockdown v1 behavior |
| --- | --- |
| Workspace files | Read, repository-aware glob, and redacted literal/regex search |
| Lockdown sandbox | Read/list and replace files; global quota applies |
| Web | Bounded `web_search` and `web_fetch` when available |
| Interaction | Text response, request-input, todo, and plan tools |
| Delegation | May spawn a child only when the child's selected provider is also Lockdown |
| Read-only inventory | `peer_list` and analogous read-only list tools such as `ssh_list` |
| Shell/process | Refused, including `!`, background tasks, tool aliases, and `ssh_shell` |
| External mutation | `peer_send`, hooks, MCP tools, monitors, and mobile/computer control refused |
| Workspace mutation | Write/edit/delete/rename refused outside the lockdown sandbox |
| Recovery mutation | Checkpoint, undo, redo, rollback, and apply routes refused |
| Sensitive reads | Vault, profile/provider store, environment files, credentials, and `.ssh` refused |

Secret and private-key redaction is always enabled for allowed reads and
searches. A refusal is a typed `RefusedByLockdown { tool, reason }` result. The
raw refusal is journaled before the compact model-facing result is returned.
Quota refusal is `LockdownQuotaExceeded { used, limit }`.

## Sandbox and quota

Each provider receives one directory below
`~/.haider/lockdown/<provider-slug>/`. Directories are mode `0700`, files are
mode `0600`, symbolic-link traversal is refused, and path length is checked
before creation with an error containing the observed length and limit.

The byte limit is global to the machine user, not a profile or provider:
every provider directory beneath `~/.haider/lockdown/` shares one ledger. The
default is 1 GiB. Atomic replacement conservatively charges the physical peak:
the existing target plus the complete private staging file must fit before the
rename. The daemon holds the quota lock across the replacement and ledger
update. On startup it removes recognizable private staging files while holding
that lock, then reconciles the ledger against the real published tree; a crash
before or after the rename cannot create uncharged capacity. Lowering the quota
below actual use is refused.

Use `haider lockdown status`, `haider lockdown quota`, or
`haider lockdown quota --set <bytes>`. Quota changes apply machine-user-wide.

## Trust and turn boundaries

Built-in providers are `Full`. A provider record written before the trust
field existed also decodes as `Full`, including existing custom providers.
New providers also default to `Full`; Lockdown is entered only through an
explicit `--lockdown` create flag, a later trust toggle, or the precise
automatic-hermetic policy below.

Trust can be changed with:

```text
haider provider set <name> --lockdown
haider provider set <name> --full
```

The daemon snapshots provider trust and the reduced tool pack when the next
provider turn begins. Changing trust does not alter an already advertised pack
or an in-flight tool call. Every live session observes the new ceiling at its
next turn boundary. Trust changes are revision-fenced and journaled as
self-sufficient `provider.trust_changed` facts.

## Automatic hermetic specialization

The trigger is evaluated only for the provider active at a session/turn
boundary. It is active exactly when that provider is non-built-in, enabled,
has an injected endpoint, uses the Chat Completions or Anthropic Messages
family, advertises no auth methods, and has no active stored credential. This
last condition matters: if a stored key is active for an auth-optional profile,
the account resolver gives that key priority and automatic hermetic policy does
not activate. An unrelated configured no-auth provider never affects a turn.

Automatic hermetic mode reuses the normal durable lockdown binding, sandbox,
quota, fallback fence, schema reduction, and dispatch refusal. Its allowed
tools are `fs_read`, `fs_glob`, `fs_search`, `fs_write`, `request_input`,
`todo_write`, and `plan`. Web/search, peers, SSH, subagents, processes, and all
other routes are absent. The selected injected provider endpoint is therefore
the daemon's only inference egress for that turn.
An explicit `Lockdown` trust setting composes with this floor and cannot weaken
it to the ordinary configured-lockdown web allowance. Exact override remains
the only way to remove the automatic specialization.

Provider-only `lockdown.status(<provider>)` reports
`activation: auto_hermetic_eligible` with a prospective reason; provider-show
text renders `policy: auto-hermetic when active`. Active-session observation
alone reports `activation: auto_hermetic` and reason
`the active provider is an enabled custom no-auth endpoint`. Set exactly
`HAIDER_AUTO_HERMETIC=0` in both launcher and daemon environment to disable the
automatic branch; explicit configured lockdown still applies. Manual update
checks remain available, while the CLI skips its automatic on-open check for
an explicitly opened session that reports auto-hermetic or a new session whose
active default provider reports auto-hermetic. A session picker with no active
selection does not scan unrelated providers. Existing
`HAIDER_NO_UPDATE_CHECK=1` remains an independent unconditional update-check
disable. An unavailable policy lookup also suppresses the automatic check and
prints an operator-visible reason instead of failing open to release discovery.

## Subagents

A Full parent may select a Lockdown provider for a research child. The child
uses its own provider's envelope, and its returned text is marked as coming
from a lockdown provider. A Lockdown parent may spawn another Lockdown child,
but may not select a Full child; that attempted privilege escape is a typed,
journaled refusal. Inheritance never substitutes the parent's provider trust
for the child's selected provider.

## Client and UI contract

The TUI shows `🔒 lockdown · <provider>` in the existing status line, marks
locked providers/models and child rows, and renders a refused call as a
one-line refusal rather than a failure. Selecting the status segment opens the
envelope and current global quota summary.

Clients must negotiate `provider_lockdown_v1` before calling
`provider.set_trust`, `lockdown.status`, or `lockdown.set_quota`. Session and
subagent observation state may carry
`lockdown: { provider, activation, reason, tools_allowed, quota_used,
quota_limit }`. Raw Pipe
consumers receive self-sufficient `lockdown.refused`, `lockdown.quota`, and
`provider.trust_changed` payloads and must not reconstruct them from prose.
