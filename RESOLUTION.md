# Wave 973 integration, Sol round 16

Merged `origin/wave-973` at `a8b218079422dd0a70c6997a56fc6a4f5d4b051f` into the model list fixes lane.

`crates/haider-daemon/src/android_runtime_tests.rs` conflicted because the lane had repaired the recovered-checkpoint fixture's effect history while the wave had extracted that fixture into `checkpoint_test_support.rs`. The shared helper already includes the same ordered Intent → Authorized → Dispatched → Outcome → CheckpointRecorded history, and asserts that the checkpoint is indexed. The resolution uses that shared helper and removes the duplicate local definition. It retains the lane's recovered Android RPC assertions for allowed paths, outside/reserved paths, worker generation, undo, redo, and rollback, along with the wave's general support for creation/write/deletion fixtures and durable postimages.

The shared helper's explicit fake-model session also initializes `model_catalog_key: None`, matching the lane's session-creation contract and preserving its configured-model control.

The imported activity-settings retry test used the former `(provider, model)` settings API. It now uses `LastModelPick` and retains all theme, notification, verbosity, dirty-intent and retry assertions. The same test checks legacy, configured and fetched saved picks, including their owner metadata, so an unrelated activity-setting write cannot discard saved model settings.

Parent bootstrap and both current composers use one account lookup that follows an exact session account pin. A pin bypasses default-account selection, so an account reply that changes the default must preserve an unchanged pinned producer while its matching catalog scope remains available. The pinned account's incarnation must still match; changing that producing account retires its former model. Four production-library cases cover parent and child composers during account-before-provider delivery, both unchanged and changed pinned producers, with both terminal sizes rendered. The round 16 evidence contains the immutable old-code comparison and the continuity mutation definition.

`test-baseline.txt` was derived from the merged source and the new regression tests using `cargo run --locked --offline -p xtask -- test-count --update`. The resulting count is **6,774**. Neither parent's number was chosen. The round 16 `integrated-baseline-update.log` records the actual command and exit 0.

The imported Android application changes are part of the wave. The model list repair adds no Android application UI change. Its standalone Rust feature, the shared fixture, and the full workspace gates are checked on the merged candidate.

A fresh wave read before the final gates advanced to `b3aeb633b9f24c51d07cb901bb532ad907b71782`. Its two conflicts are resolved semantically. The sidecar tests now join the writer before their final observations, and use the wave's complete seed-projection barrier while warming a live writer. The lane's committed-coverage helper and regression controls remain. The stalled-outbox fixture retains the lane's shared parking barrier, verifies that its first event fits the byte budget, waits for confirmed Busy admission, and then sends a deliberately oversized pressure event. This satisfies the wave's requirement to establish busy replay before applying pressure. Both sets of race-stress entries remain.

The second integration also imports the wave's recency fixture, which checks query work with metadata tripwires rather than a 500 ms scheduling bound, and its parent-exit liveness fixture. Their original behavior assertions remain. Final test-count derivation and isolated recency execution are recorded separately in the round 16 evidence.

After resolving the second merge and adding the two delayed-provider child controls, the actual xtask update derived an intermediate **6,812** tests. `expanded-baseline-update.log` records that command, its merged working-source hashes, and exit 0. The four exact-pin producer controls then brought the actual `xtask test-count --update` result to **6,816**. `pinned-baseline-update.log` records the working-source hashes, command and exit 0. The final committed count is checked again by the final gates.

Continuation on 2026-10-20 merged fresh wave `56e4a29a3bfa58f422f705302b28b0a15648531b`. Only `test-baseline.txt` conflicted. The imported macOS packaging changes remain intact. The existing verified xtask executable (unchanged xtask sources) actually re-derived **6,816** under the build-slot wrapper; neither parent baseline was selected. The continuation receipt is `17-sol-r16-cap2/gates/integrated-baseline.json`.

The continuation's complete matrix found that the source-test reachability guard could not see the partial-write module declared inside `accounts_tests.rs`, although its 15 tests had executed in the daemon library. The module is now declared directly in the existing `cfg(test)` section of `accounts.rs`. Its explicit fixture imports use account-module-only visibility. The guard, test bodies and baseline count remain intact; the failed receipt is retained and the repaired candidate is checked separately.

---

# Wave 973 merge resolution

The round 9 merge integrates `a8b218079422dd0a70c6997a56fc6a4f5d4b051f` into the terminal UI lane based on `a97534c62a6005545df9796db68a475a14fc9ffd`.

In `session::chip_apply`, the envelope remains authoritative for the event timestamp and activity provenance, including replay, background and parked branch routes. The lane's question creation and matching answer/close updates remain before transcript reduction, so the previous open menu can still be matched. Neither side's behavior is discarded.

The keyboard input tests now construct real child envelopes instead of passing bare timestamps to the changed API. The baseline was re-derived as **6,950 tests** with `cargo run --locked -p xtask -- test-count --update` on the resolved tree. It is checked again on the committed merged candidate.

Round 9 also repairs visible child question input ownership, retires old keyboard input after accepted external editor writes, carries remote connection attempt identity through request/reply mapping and keyboard input handling, and conditions paste-specific decoder tests on the paste feature. The pre-existing overlay text observation F4 is deferred to lane 974 as instructed.

The continuation also integrates fresh wave `b3aeb633b9f24c51d07cb901bb532ad907b71782` after its file-lock and CLI lifecycle updates. The only new conflict is the shared stress-case catalog: retain all thirty appended wave rows and this lane's `tui-frame-publication` row, preserving the six-column format and unique case IDs. Place the frame-publication row with the existing TUI cases so future wave additions at the end do not conflict with this lane's appended row. The TUI repair code remains unchanged. The test baseline was re-derived as **6986 tests** from this integrated source before committing.

The next continuation integrates wave `56e4a29a3bfa58f422f705302b28b0a15648531b`. Its only additions since the prior merged wave are macOS installer identity checks and their Python tests. The sole merge conflict is the shared baseline; xtask re-derived it as **6986 tests** on the integrated tree. All Rust sources, Cargo manifests, Cargo.lock, vendor sources and Rust test drivers remain byte-identical to `6da30807`; source equivalence and evidence reuse are recorded with the continuation receipts.

## Catalog fixes round 17 integration

Merged wave `6fa2a72d4199b798834cee2d72c5fd2767a6373b` after implementing C5. Both lanes' resolution records remain above. The RPC public exports retain the catalog coordinate helpers and the incoming `ShellSetupErrorWire`. The merged baseline is derived by xtask before the merge commit; final gates run on that merge commit.

The actual merged `xtask test-count --update` derived **7198** source test markers. The round 17 `merged-preparation` receipt records the command and exit status.

The merged all-target clippy gate found two imported input-dwell fixtures missing the catalog fields introduced by this lane. Both now use the existing `metadata_provider` helper, which records their declared Static rows in `static_models`, leaves `configured_models` empty and has no fetched `catalog_scope`. All input assertions remain intact; source marker count stays 7,198.

Round 17 also carries the recorded coordinate into account-switch cache preflight. A raw creation alias otherwise reaches the exact pricing registry and unnecessarily requires an unknown-cost approval. A native wire-request regression covers the recorded alias, an exact-model control and an unlisted-literal control; it also checks that preflight preserves the public model spelling.
