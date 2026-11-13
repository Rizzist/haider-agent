# 973 account identity and selection behavior

This release treats a completed interactive OAuth sign-in as both credential
replacement and an explicit selection: the resulting account becomes the
provider default. This is the sign-in equivalent of confirming an account
switch. Provider-default sessions therefore begin a new cache epoch on their
next request; sessions pinned to an explicit account alias remain pinned.
Background refreshes and credential imports do not change the default.

OAuth re-login matches only public identity coordinates, never token or secret
bytes. Within one provider, a stable account coordinate is authoritative when
both records have one. Otherwise an email fallback compares addresses after
Unicode NFKC normalization and full case folding, and only while that email
names exactly one stable account among the provider's rows (an email-only row
whose email names several accounts is left for operator review, never bridged).

| Provider | Identity source | Stable dedupe coordinate | Email fallback |
| --- | --- | --- | --- |
| OpenAI (`openai-oauth`) | verified ID token claims | `user:<len>:<sub>;workspace:<len>:<workspace>` (length-prefixed); a workspace-only id is NOT a user and never deduplicates | never |
| Claude (`anthropic-oauth`) | authenticated `/api/oauth/profile` (OAuth beta header) | `account=<account uuid>;organization=<organization uuid>` | yes; the email itself is optional and informational |
| Grok (`grok-oauth`) | ID token claims | token subject | yes |
| Kimi (`kimi-oauth`) | none (device OAuth returns no identity) | none | none |
| Google Antigravity (`google-antigravity`) | none (the supervised agent owns the Google login) | none | none |

Providers without a stable coordinate update an existing account only when the
operator re-logs in through that alias; a fresh alias remains a distinct
account.

Claude profile lookup after a successful token exchange retries transport
errors, 429 and 5xx with a bounded backoff (three attempts, `Retry-After`
honoured up to 2 s). If the profile is still unavailable (or rejects the
token), the login is NOT failed: the account is saved with an unverified
identity, the same synthetic `anthropic:<grant hash>` placeholder that pre-973
rows carry. Unverified rows are never identity-deduplicated and never count as
"provably" another account. They upgrade to the verified coordinate from their
own token at daemon startup and on each later Claude login, and `haider account
list` flags them as suspected duplicates of the provider's other rows.

Legacy upgrades: an OpenAI row that stored only the shared workspace id
recovers its user coordinate offline from its own vaulted ID token; a Claude
row with the synthetic placeholder is probed with its own unexpired access
token. Neither upgrade runs for a row whose refresh may be in flight, and the
OpenAI upgrade skips aliases linked to an enrolled credential source (the
source owns that token). A fresh Claude login may also reach a skipped
(`Expired`/`Revoked`) placeholder row, but only when that row's own token
still proves the same account and exactly one such row matches. A dead
placeholder row whose token no longer works cannot be identified; `/accounts`
offers an in-place re-login for it (Enter on the unusable row, then Enter
again), which upgrades that same row.

Re-login through an existing alias X that authenticates a provably different
account (both records carry stable coordinates and they differ) never rewrites
X. If another row already holds the new account, that row is updated;
otherwise the account is saved under the next free `<provider>` alias. Either
way the saved row becomes active, X keeps its token, identity and label (so
sessions pinned to X are unaffected), and the completion notice names the
saved alias and says X is unchanged and still needs attention.

When older rows already duplicate a stable identity, the next login updates the
requested matching alias (or the first matching alias when a fresh alias was
proposed) and makes it active. Other matching rows and their vaulted
credentials are retained. `haider account list` identifies those rows and
recommends keeping the active row, then a usable one (the same row the TUI
keeps); remove an unwanted duplicate explicitly with:

```text
haider account remove <duplicate-alias> --confirm
```

If that matching alias was originally linked to an external credential source,
the successful interactive grant promotes the alias to Haider-managed OAuth.
The source enrollment is retained but disabled and unlinked, and its file is
never changed or deleted. Explicitly re-enrolling that source later may create
a separate source-owned account; Haider does not silently merge or delete its
credential.

Inventory discovery uses an explicitly pinned account when supplied. Without
one, it prefers the active healthy account, then another healthy account for
the provider, before falling back to the unhealthy default solely to return an
actionable error. A failed account is named in that error and does not mark the
provider unavailable while another healthy account exists. When a provider's
catalog is unavailable, re-probing is backed off per (provider, account alias),
so alternating pinned and default sessions keep independent schedules.

Recorded note: the Claude coordinate `account=…;organization=…` is not
length-prefixed like OpenAI's. Both UUIDs come from an authenticated response
and are sanitized, so a delimiter collision is theoretical; aligning the format
would require migrating stored coordinates and is left as a follow-up.

An account switch that would invalidate a warmed cache epoch remains a
two-step CLI action. The retry is:

```text
haider account use <alias> --confirm
```
