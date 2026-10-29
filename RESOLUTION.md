# Wave 973 merge resolution

The round 9 merge integrates `a8b218079422dd0a70c6997a56fc6a4f5d4b051f` into the terminal UI lane based on `a97534c62a6005545df9796db68a475a14fc9ffd`.

In `session::chip_apply`, the envelope remains authoritative for the event timestamp and activity provenance, including replay, background and parked branch routes. The lane's question creation and matching answer/close updates remain before transcript reduction, so the previous open menu can still be matched. Neither side's behavior is discarded.

The keyboard input tests now construct real child envelopes instead of passing bare timestamps to the changed API. The baseline was re-derived as **6,950 tests** with `cargo run --locked -p xtask -- test-count --update` on the resolved tree. It is checked again on the committed merged candidate.

Round 9 also repairs visible child question input ownership, retires old keyboard input after accepted external editor writes, carries remote connection attempt identity through request/reply mapping and keyboard input handling, and conditions paste-specific decoder tests on the paste feature. The pre-existing overlay text observation F4 is deferred to lane 974 as instructed.

The continuation also integrates fresh wave `b3aeb633b9f24c51d07cb901bb532ad907b71782` after its file-lock and CLI lifecycle updates. The only new conflict is the shared stress-case catalog: retain all thirty appended wave rows and this lane's `tui-frame-publication` row, preserving the six-column format and unique case IDs. Place the frame-publication row with the existing TUI cases so future wave additions at the end do not conflict with this lane's appended row. The TUI repair code remains unchanged. The test baseline was re-derived as **6986 tests** from this integrated source before committing.

The next continuation integrates wave `56e4a29a3bfa58f422f705302b28b0a15648531b`. Its only additions since the prior merged wave are macOS installer identity checks and their Python tests. The sole merge conflict is the shared baseline; xtask re-derived it as **6986 tests** on the integrated tree. All Rust sources, Cargo manifests, Cargo.lock, vendor sources and Rust test drivers remain byte-identical to `6da30807`; source equivalence and evidence reuse are recorded with the continuation receipts.
