# W8a — permissions core: registry truth, process_exec, PermissionRequired, user shell

AUTHORITY: docs/research/w8-permissions-research.md (read WHOLE, first).
Its central finding binds: the W4 `EffectBroker` path IS the approval
authority — this wave consolidates and closes gaps. Creating a second
permission authority is the named failure mode; do not.

## Scope (daemon/core/protocol/rpc/tools — NO haider-tui)

1. **Canonical tool registry.** One daemon-owned registry (reuse frozen
   `ToolManifest`: provider name, schema, normalized effect classes,
   dispatch mode) sourcing BOTH `TurnToolFactory::definitions()` and a
   new read-only inventory snapshot (research §W8a-1/-9). The advertised
   set stays exactly the executable set (law 1).
2. **`process_exec` naming.** Advertise `process_exec` (not `exec`) to
   new provider turns; the dispatcher ACCEPTS legacy `exec` for
   recovery/in-flight history (research §W8a-2, risk 3). Never advertise
   both.
3. **`RunState::PermissionRequired`.** Permission menus park in
   `PermissionRequired` (the sim's vocabulary); recovery dual-reads
   historical `InputRequired + MenuKind::Permission` checkpoints
   (research §W8a-5, risk 5, law 11).
4. **Direct user-shell backend.** A durable, receipt-backed daemon RPC
   (`shell.exec`: command_id, session_id, worker_generation, exact
   command bytes, optional workspace-relative cwd) that invokes
   `EffectBroker::process_exec_user` — PreAuthorized(UserTyped), same
   cwd checks/limits, emits `TurnItem::CommandExecution` + CommandOutput
   deltas + terminal status. No UserMessage, zero provider requests;
   while another run owns the session, reject typed-busy (no parallel
   side-effect lane). `!cd`: REJECT as unsupported in this slice with a
   typed reason (research Q3 option 2 — daemon-owned cwd state is a
   later design; document it).
5. **Inventory read seam.** Expose registered names/effects/defaults +
   remembered session grants for the future /tools screen (a READ — not
   a menu; projected from durable facts).

## Laws

The research's "Minimum W8a laws" 1-11 bind verbatim. Plus the standing
lane laws: tests never inline; mutation docs with RUNTIME failures;
CARGO_INCREMENTAL=0; fmt + workspace clippy -D warnings; test
haider-protocol/tools/store/core/daemon (sandbox socket failures
expected — host gate authoritative); ledger update; protocol changes
ADDITIVE against frozen shapes (menus, effects, items, ToolManifest —
research lists them with line refs; USE them); regenerate goldens if
manifests change; no haider-tui; no Cargo.lock; no versions; leave
changes uncommitted; no git commands. Preserve the named existing test
coverage (research end of §Risks + laws).

## Tests (minimum — beyond preserved coverage)

- Inventory equality: advertised == dispatchable, `process_exec` not
  `exec`, legacy `exec` still dispatches (mutation: advertise a
  non-dispatchable name → fails).
- PermissionRequired parking + dual-read recovery of an old
  InputRequired+Permission checkpoint (mutation: single-read → old
  checkpoint terminalizes → fails).
- shell.exec: receipt idempotency (same command_id once; changed bytes
  under same id rejected), PreAuthorized(UserTyped) journal shape, zero
  provider requests, typed-busy while a run is active, `!cd` typed
  rejection (mutations per law 9/10).
- Inventory read: names/effects/defaults/grants match the registry and
  durable grants (mutation: snapshot fabricates an entry → fails).

Use up to 3 research subagents and 2 verify subagents. Print a final
summary of files changed and tests added.
