# Patch brief W1/M1 — real-store adapter (the merge seam)

Branch w1-merge in /Users/rizzist/Documents/CODING/haider-agent. Both lanes merged; MemoryStore
still backs the CLI. Deliver:
1. crates/haider-core (or a new small module in haider-cli if cleaner): `SqliteStoreHandle` —
   implements the async StoreHandle by wrapping haider-store's sync EventStore/Cas via
   tokio::task::spawn_blocking (NEVER block a runtime worker; one long-lived connection —
   this adopts the ledgered persistent-connection optimization; move its OPTIMIZATIONS.md row
   to adopted w/ tag). Respect the seam contract in haider-core/src/lib.rs header (store
   assigns seq/committed_at_ms; contiguous from 1; batch never spans sessions).
2. haider-cli run --jsonl: use SqliteStoreHandle at a profile dir (env HAIDER_PROFILE_DIR
   default ~/.haider/dev-profile; create it) instead of MemoryStore. Keep MemoryStore for
   self-test.
3. Tests (the oracle): real-store actor turn — every envelope durable BEFORE broadcast
   (commit-before-publish: subscribe, verify each received envelope is already readable from
   the store); reopen replay — run a turn, drop everything, reopen the store, read() returns
   the identical byte-for-byte envelope sequence; profile-lock exclusivity across two handles.
Gate: full workspace (test/clippy -D warnings/fmt/xtask check, test-count --update). Leave
uncommitted.
