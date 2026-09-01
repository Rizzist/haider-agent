# oauthlazywin — Windows lazy OAuth transport test repair (v0.0.969)

Date: 2026-09-01

Branch and base: `lane-969-oauthlazywin` at `d750e09`, the supplied
`wave-969` head after the `memdaemon2` merge.

## Verdict

The product is correct. The Windows failure was a platform-dependent source
byte assertion, not an OAuth operation returning before acquiring the lazy
transport.

`oauth_coordinator_constructor_retains_only_the_lazy_transport_handle` does not
execute an OAuth operation on any platform. It reads `oauth.rs` with
`include_str!` and searches for an LF-bearing multiline string
(`oauth_tests.rs:3029-3053`). `include_str!` retains checkout-time line endings.
Unlike the three source/document inputs already protected in `.gitattributes`,
`oauth.rs` has no `eol=lf` attribute (`.gitattributes:5-13`). A Windows
`core.autocrlf` checkout can therefore bake CRLF into the test binary and make
only this search fail.

The exact byte mutation was reproduced locally without changing the worktree:

```text
LF source + LF needle:       true
CRLF source + LF needle:     false
normalized CRLF + LF needle: true
```

No Windows credential, keyring, listener, redirect, path, or URL branch is on
the failing test's execution path. The production Windows credential manager
implementations are guarded by `all(target_os = "windows", not(test))`; tests
select the injected platform seam (`oauth.rs:1541-1552`). The only
platform-specific home lookup chooses `USERPROFILE` before `HOME` on Windows
and is unrelated to coordinator construction or transport acquisition
(`oauth.rs:1185-1193`).

## Product call-graph inspection

The coordinator stores only the zero-sized `SharedHttpTransport` handle
(`oauth.rs:2630-2637`). `new_with_vault` installs that handle and contains no
`.client()` call (`oauth.rs:2683-2718`). The shared client is acquired through
`coordinator_http_client` (`oauth.rs:2650-2657`).

The first real HTTP request on each flow mode is correctly preceded by that
acquisition on every target:

- Device authorization acquires the client at `oauth.rs:3269-3275`, then sends
  its first endpoint request at `oauth.rs:3288-3295`.
- Authorization-code setup validates admission and binds a loopback listener
  before any HTTP is required (`oauth.rs:2997-3063`). After a valid browser
  callback supplies a code, it acquires the client at `oauth.rs:3900-3910` and
  passes it directly to `exchange_authorization_code` at `oauth.rs:3911-3919`.

The adjacent cross-platform runtime test already exercises the latter path:
`code_is_consumed_once_and_listener_rejects_replay` completes the callback,
waits for ready, and asserts exactly one token-endpoint call
(`oauth_tests.rs:3056-3105`). The supplied Windows job reported every daemon
test except the source-byte test passing, and the local OAuth run also passed
this runtime test. A replacement "first operation" test was therefore neither
needed nor added.

## Change and mutation coverage

Only `crates/haider-daemon/src/oauth_tests.rs` changed. The test now normalizes
CRLF to LF in its private source view before performing the existing semantic
checks (`oauth_tests.rs:3030-3032`). This does not accept any product mutation:

- the constructor slice must still contain the lazy transport handle;
- that constructor slice must still contain no `.client()` call; and
- `coordinator_http_client` must still contain the exact
  `inner.transport.client()` chain.

Product `oauth.rs` is unchanged. The test was not weakened, ignored, cfg-gated,
or renamed.

## Citation audit

The brief's quoted source was correct for base `d750e09`: the test began at
`oauth_tests.rs:3029`, and the failing assertion began at `:3047`. After the
two-line portability comment, the function still begins at `:3029` and the
helper assertion begins at `:3049`. The proposed credential/keyring and
loopback candidates were reasonable hypotheses but wrong for this failure
because the failed test performs no runtime operation.

`LANE-COMMON.md` has drifted metadata: its heading and base describe the older
968 lanes, and its generic do-not-touch list conflicts with the dedicated
oauthlazywin brief. The dedicated brief and direct lane instructions explicitly
own `oauth.rs`/`oauth_tests.rs`; this lane changed only the latter plus this
required report.

## Verification

Every Cargo invocation used `CARGO_BUILD_JOBS=1` and the mandated environment:

```text
RUST_MIN_STACK=8388608
HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac
HAIDER_TEST_SIBLINGS_PREBUILT=1        # daemon test and Clippy gates
CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=0
```

Free space was checked before every Cargo command and remained far above the
700 MiB stop floor.

- Sibling prebuild, `cargo build -p haider-cli -p haider-daemond`: PASS.
  `target/debug/haider` is 103,138,384 bytes and `haiderd` is 185,126,384
  bytes; the daemon exceeds registry #64's 10 MiB sentinel.
- Required `cargo test -p haider-daemon oauth`: PASS. Unit target:
  `105 passed; 0 failed`; OAuth-related integration target:
  `1 passed; 0 failed`.
- Required `cargo clippy -p haider-daemon --tests -- -D warnings`: PASS.
- `cargo fmt --all -- --check`, `git diff --check`, conflict-marker scan, and
  unmerged-index check: PASS.
- Windows execution is unavailable in this lane. The CRLF behavior and product
  call graph are therefore Windows-by-inspection; the exact CRLF byte mismatch
  was reproduced locally.

## CI error registry walk

- #19/#45/#77: the scoped Rust test edit compiles without warnings; formatting,
  diff hygiene, and conflict/index checks are included in the final tree gate.
- #21/#54: the Rust test and Clippy processes used the mandated 8 MiB stack.
- #48: no new Rust test file or ledger input was introduced; the existing named
  mutation test was repaired in place.
- #50: the platform-dependent exact-byte pin is the root cause. Normalizing
  only CRLF/LF in the test's source view preserves its semantic assertions.
- #64/#67: exact sibling binaries were prebuilt, the daemon test used
  `HAIDER_TEST_SIBLINGS_PREBUILT=1`, and `haiderd` exceeds 10 MiB.
- #72/#74: discovery stayed disabled; OAuth runtime coverage uses only its
  existing injected catalog, temporary roots, and loopback server.
- #94/#95: this change adds no timeout, deadline, sleep, external-state wait,
  or negotiated connection.
- Remaining registry classes were checked and are unaffected by a test-only
  line-ending normalization. No new CI error class was discovered.

The supplied `LANE-*`, `turnperf/`, and `turnperf2/` inputs remain unedited and
uncommitted. All lane work remains uncommitted for the orchestrator.

SHIP
