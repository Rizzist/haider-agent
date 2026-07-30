# W5d `/providers` — review of record #1 — SHIP (layout PROVISIONAL)

Implementer AND reviewer: Fable 5. Branch `w5-d2` @ `423c732`. Authority:
report §5.2 — which is explicit that the simulator has NO providers screen,
so this is an owner-directed design, not claimed sim parity.

## The design gate, honestly

§5.2 requires "one owner-approved static screenshot/golden" before
implementation. The owner's later standing order put `/providers` INSIDE
v0.0.15 and said to keep going; no layout reply has arrived. Resolution
recorded here: the screen is built to the layout proposed to the owner
in-session, the golden is **PROVISIONAL**, and the v0.0.15 install probe is
the sign-off point — the owner reshapes it there before it hardens into
law. The render function and this brief both carry the marker. This is a
judgment call bridging two owner instructions, not a claim the gate passed.

## Layout (provisional)

```
PROVIDERS — registry truth · accounts live in /accounts
  <provider>  ● available | ○ <honest reason> | ◌ unknown
    <api family> · <endpoint, safe display>
    models: m1  m2*  m3        (* default; click a chip = set default)
    account: <alias> · <label> · in use   [accounts]
hints
```

- Built-ins stay visible when unavailable, with the daemon's reason
  (§5.2's "Google can say adapter not installed").
- The endpoint is display-only; never interpolated into a shell/browser
  command.
- The account line is a PROJECTION of the accounts snapshot when loaded; an
  honest `— (/accounts)` otherwise — never a guess.
- `/providers` joins the registry/help as a documented non-sim extension.

## Management law (the /accounts §5.1 law applied to management)

Click a model chip → `account.set_default_model` under the
expected-revision CAS. The `*` marker moves ONLY on the correlated,
revision-gated reply; the pending chip pulses `…`. A `revision_conflict`
releases the gate AND refreshes the snapshot (the CAS proved it stale).
Out-of-inventory and already-default clicks refuse locally with honest
messages — no doomed RPC. One in-flight mutation at a time.

## Mutation checks (executed, runtime kills)

| # | Mutation | Result |
|---|---|---|
| M-P1 | Write the default locally before requesting | KILLED |
| M-P2 | Drop the revision comparison in `apply_default_set` | KILLED |

Executed AGAINST THE COMMIT (the commit-before-mutation rule held this
time); clean single-anchor edits; tree restored to HEAD after each.

## Wire

`LiveCommand::ProviderList` (read) + `LiveCommand::SetDefaultModel`
(durable, outbox, CAS payload); `LiveReply::{Providers, DefaultModelSet}`;
driver-side `pending_default_model` correlates failures to the exact
provider and routes `revision_conflict` into the refresh path. Demo mode
answers from `seed_provider_summaries()` through the same reducer seams.

## Not in this cut

- `provider.configure` cards (create/edit custom providers) — W5e; the
  add/edit affordance deliberately absent rather than stubbed as a form.
- Dynamic `/login`//`/model`//`/provider` arg slots from registry data
  (§5.3) — next.
- Keyboard Enter action on a provider row (cursor exists; chips are
  mouse-first) — W5 accessibility follow-up alongside the accounts golden.

## Gate

clippy workspace clean. Ledger 1038 → 1044 (6 new tests). Per-crate gate
green (tui 478).

## Verdict

**SHIP**, with the layout explicitly provisional pending the owner's
install-probe sign-off. The daemon-truth law is pinned and mutation-killed
on both screens; nothing on this screen fabricates state the daemon did not
commit.
