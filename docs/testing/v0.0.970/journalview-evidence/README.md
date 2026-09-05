# Journalview gate evidence

**Final result: PASS.** Workspace tests, Clippy test targets, xtask and all gate
prechecks exited zero on the current merged ref. See `result.json` for exact
environment/status and `workspace.log`, `clippy.log`, and `xtask.log` for raw
results. The test baseline is 4925. `workspace-summary.json` explicitly labels
its aggregate of result lines, including nested fixture subprocesses.

Final source is the uncommitted lane plus resolved content merges through
`38359fd3ba799c3e32a09c414f6f41abb90442bd`. Git HEAD remains `372a2639` because
the shared Git directory is read-only; see `merge-state.txt` and both manifests.
`source-sha256.json` records the 76 changed/new crate files and test baseline
held stable during the final gate. The manifests record incoming content before
the documented rebind cleanup, authoritative test recount, and appended
journalview CI registry walk.

The gate runs on macOS with:

```sh
RUST_MIN_STACK=8388608
HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac
CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=0
CARGO_BUILD_JOBS=2
HAIDER_TEST_SIBLINGS_PREBUILT=1
RUST_TEST_THREADS=4
```

`haider` and `haiderd` were freshly prebuilt before daemon tests. Every
build-capable command checks disk space and stops below 700 MiB. The retained
`gate.sh` was invoked with `RUST_TEST_THREADS=4` inherited from its caller; it
checks the merged upstream ref before and after the full gate. The main commands
are:

```sh
cargo test -q --workspace --locked --no-fail-fast
cargo clippy --workspace --tests --locked -- -D warnings
cargo run -q --locked -p xtask -- check
```

The gate also runs formatting, whitespace, unsafe-count and QA self-checks,
plus the exact new rebind regression as a precheck. `test-count --update` and
the unchanged instruct-pipe pin were exercised after the latest merge.

The scoped logs and golden review in this directory describe the current merge.
The runtime scope precedes the final three rebind cleanup calls; the exact
rebind regression and complete workspace gate exercise the final source.
Goldens were regenerated with repository update flags, then exercised normally
by the full gate. `golden-review.txt` accounts for every changed/new JSONL line.

Historical attempts are kept separately:

- `first-gate/`: exact payload/sequence failures and the terminal lifecycle issue.
- `second-gate/`: the unchanged protected OAuth scheduling failure, its passing
  isolation run, and the passing complete daemon rerun with four threads.
- `green-73fe3f68/`: all checks passed, but a newer upstream ref invalidated that
  tree as the final landing gate.

`verifiers.md` records four real findings, all fixed and independently accepted,
and no rejected findings. No benchmark score or timing improvement is claimed.
