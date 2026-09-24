# 973 account identity and selection behavior

This release treats a completed interactive OAuth sign-in as both credential
replacement and an explicit selection: the resulting account becomes the
provider default. This is the sign-in equivalent of confirming an account
switch. Provider-default sessions therefore begin a new cache epoch on their
next request; sessions pinned to an explicit account alias remain pinned.
Background refreshes and credential imports do not change the default.

OAuth re-login matches only public identity coordinates, never token or secret
bytes. Within one provider, account ID is authoritative when both records have
one; otherwise a case-insensitive email match is used. OpenAI and Grok consume
their token identity claims. Anthropic fetches the authenticated
`/api/oauth/profile` document with the required OAuth beta header and records
its account UUID plus organization UUID as one stable coordinate, alongside the
email, display name, and organization type. The organization coordinate keeps
separately scoped subscriptions distinct. Kimi currently exposes no stable
account coordinate to this flow, so a login updates an existing account only
when the operator starts re-login from that alias; a fresh alias remains a
distinct account. If a provider promises a stable profile endpoint but that
endpoint cannot return a valid identity, sign-in fails closed instead of
guessing from token bytes or silently creating an unstable duplicate.

When older rows already duplicate a stable identity, the next login updates the
requested matching alias (or the first matching alias when a fresh alias was
proposed) and makes it active. Other matching rows and their vaulted
credentials are retained. `haider account list` identifies those rows; remove
an unwanted duplicate explicitly with:

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
provider unavailable while another healthy account exists.

An account switch that would invalidate a warmed cache epoch remains a
two-step CLI action. The retry is:

```text
haider account use <alias> --confirm
```
