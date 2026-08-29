# Fork-from-prompt end-to-end evidence

## Claim audit

The RPC request has an additive exclusive prompt selector and the response
returns both provenance and an editable draft
(`crates/haider-rpc/src/frame.rs:3226-3248`, `:4294-4313`). The existing hub
tests prove those values through an in-memory connection dispatcher, but do
not drive the complete daemon transport. Commit `fbddf60` is the 966 privilege
repair: explicit SSH scope is snapshotted for the child and remembered
`AllowAlways` facts terminate at the fork audit boundary while creation-time
permission overrides remain policy. Commit `e10476c` repairs publication of
the forked child to clients, and commit `b1e5330` repairs the CLI session-list
projection.

The durable audit can carry an inherited cache segment only when the copied
provider view is byte-identical (`haider-protocol/src/session_fork.rs:132-174`,
`:188-211`). The store derives that candidate from durable provider-view facts;
it is not legitimate to call an arbitrary fake-provider request a cache hit.

## Deterministic design

The suite creates and runs a source session through the real daemon and IPC,
records its exact transcript/head, forks at the selected user-prompt sequence,
and verifies through RPC that: the source transcript/head/terminal state are
unchanged; the child is separately listed and readable; the response contains
the original editable draft and exact provenance; and the child audit contains
the inherited cache segment when durable provider-view evidence supports it.
It also reconstructs effective child permissions and SSH scope through daemon
RPC-visible state, requiring no remembered `AllowAlways` grant to cross the
audit boundary and no scope to widen.

Provider output remains scripted, but preparation is delegated to the real
`AnthropicProvider` renderer. That renderer produces the exact provider-view
ledger while its network stream is replaced at the provider boundary. The
deterministic assertion is therefore the daemon-owned cache-inheritance
decision and the exact inherited cohort on the subsequent child request.
Vendor-reported cache-token billing is still explicitly out of scope.

## Results

`fork_from_prompt_preserves_source_cache_and_privilege_boundaries` passes over
the production daemon transport:

- two source turns reach `Done`; the first executes a real `fs_write` approved
  with the real `AllowAlways` menu option;
- the complete source journal, including the second terminal, is byte-for-byte
  equal before and after the fork;
- `session.fork` at the second user-message sequence returns exact provenance
  and the original prompt as an unsent, attachment-preserving draft;
- child replay excludes that draft prompt and carries a `SessionForked` audit
  with `context_epoch=inherited` and a durable cache segment;
- the subsequent child provider request uses that segment's `cache_route` as
  `cache_cohort`, rather than the new child session identity;
- a saved SSH profile remains `in_scope=false` under the inherited explicit
  deny-all scope; and
- the source inventory has one remembered grant while the child inventory has
  none, proving `AllowAlways` terminates at the audit boundary.

Mutation evidence was executed by temporarily suppressing `created.draft` in
the production RPC projection. The test failed immediately at “prompt fork
returns editable draft”; the mutation was removed before the passing run.

The deterministic suite cannot claim a vendor-billed cache read, because that
requires a paid live provider call and vendor judgment. It does prove every
daemon-owned precondition and routing coordinate below that boundary.
