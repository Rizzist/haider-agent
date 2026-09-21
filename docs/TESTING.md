# Testing guide

## Race-window tests

Tests whose result depends on scheduling, timeouts, concurrent writers,
interleaved streams, or ownership handoff must prove more than one lucky pass.
Before a new race-window test lands:

1. Add it to `scripts/race-stress-cases.tsv`. Use `linux` whenever the behavior
   is relevant there; reserve `macos` for host/filesystem/process behavior that
   must be exercised on macOS.
2. Run at least **20 iterations under continuous CPU load** with the production
   assertions unchanged. A zero-match test invocation is a failure, not a pass.
3. Run Linux cases in the prepared Colima Linux environment and macOS cases on
   the host. Keep one test thread so the controlled load, not unrelated suite
   concurrency, supplies the scheduling pressure.
4. Attach the per-test pass count and command to the change evidence. A failure
   must be fixed, or quarantined by a reviewed CI-registry entry that links the
   failing output. Never weaken the assertion to make the loop green.

On the shared development Mac, the entire harness invocation is itself a build
and therefore goes through the machine build-slot wrapper:

```sh
CARGO_TARGET_DIR="$PWD/target" \
  /Users/rizzist/Developer/haiderharness/runtime/build-slot.sh race-stress -- \
  bash scripts/race-stress.sh --under-load --iterations 20 \
  --platform macos --results /path/to/evidence/race-macos.tsv
```

Inside the prepared Linux container, use the same script with `--platform linux`.
The harness precompiles each selected test binary, runs one bounded load
generator, reaps it on every exit, and rejects any Cargo result that does not
report exactly one passing selected test. `cargo run -p xtask -- race-catalog`
checks that every explicitly named race/concurrency test remains registered.
Timing tests without a race word in their name still require a catalog row;
review owns that semantic judgment.

## Platform-sensitive fixtures and goldens

Fixtures and goldens containing platform shell wording, Windows path syntax,
or per-OS tool inventories must be target-qualified. A generic fallback plus
target-specific siblings is allowed only when a source consumer references both
and selects between them by target. Run
`cargo run -p xtask -- platform-goldens`; the same audit is included in
`xtask check` and the CI lint job.
