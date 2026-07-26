# Patch brief W2a/C4a — permission engine + effect journal + change ledger

Own crates/haider-tools (new impl) + a small haider-core extension (+ tests/). Read
haider-protocol (EffectClass/EffectIntent/AuthorizationVerdict/EffectPhase/Menu/BoundedResult,
EventPayload::Effect) + the seam header in haider-core/src/lib.rs + CONVENTIONS.md.

Deliver in haider-tools:
1. EffectBroker: normalize(op) -> EffectIntent (args_digest = blake3 of canonical args JSON);
   authorize(intent, policy) -> AuthorizationVerdict (policy = allowlist/asklist/denylist per
   EffectClass with digest-bound always-allow rules: an "always" stores class+digest, and a
   mutated op (new digest) re-asks — protocol law); journal every phase as EventPayload::Effect
   envelopes through a JournalSink trait (actor supplies it later — test double here).
2. Change ledger: record fs-write effects per (session, turn): paths touched + summary —
   queryable (the verify gate predicate consumes this in W4).
3. First real tools behind the broker: fs_read, fs_list, fs_search (read-class, bounded
   results w/ ArtifactRef overflow via a CasSink trait double), and fs_patch (FsWrite class:
   intent→authorize→journal dispatched→apply (string-replace w/ preimage check)→outcome +
   ledger entry). NO process_exec yet (C2's second half).
Oracle: digest-bound always-allow (same op allowed, mutated op re-asks); deny blocks the
apply; four-phase journal ordering (intent<authorized<dispatched<outcome, crash between
dispatch/outcome representable as Unknown); preimage-mismatch → typed conflict error; ledger
per-turn attribution; bounded-result overflow to CAS ref.
Gate: full workspace. Leave uncommitted.
