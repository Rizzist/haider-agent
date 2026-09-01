# oauthflake — deterministic rotating-refresh contention proof (v0.0.969)

Date: 2026-09-01

Branch creation base: `origin/wave-969` at
`eddd42f8dd29d86ed24a0cd9314bc79197af529d`. The remote-tracking ref advanced
by two commits while this lane was running; the lane was not rebased or merged,
as required by `LANE-COMMON.md`.

## Verdict

The observed `left: 2, right: 1` is a scheduling race in the test, not evidence
that the product submits the superseded generation-one refresh token twice.

The old test asserted an aggregate request count after only one
`yield_now`. That did not establish that the second resolver had entered before
the first resolver persisted generation two. If the second resolver started
late, its second refresh request was legitimate:

- the test registration is `SerializedRotating`
  (`crates/haider-daemon/src/oauth.rs:789-800`);
- the fake rotation returns `expires_in: 120`
  (`crates/haider-daemon/src/oauth_tests.rs:823-830`);
- the refresh threshold is `max(300, expires_in / 2)`, so
  `120.saturating_sub(300) == 0` and generation two is refresh-due at its issue
  time (`crates/haider-daemon/src/oauth.rs:4698-4704`);
- serialized resolution refreshes when `now >= refresh_after`
  (`crates/haider-daemon/src/oauth.rs:5462-5469`).

Therefore a late resolver may read durable generation two and submit its
rotated refresh token, making the aggregate count two without replaying the
superseded token. The fake already recorded token fingerprints
(`oauth_tests.rs:663-676`), but the old test did not assert them.

The production critical section is sound and was not changed. Independent
`FileVault` handles contend on one alias-specific OS file lock
(`crates/haider-accounts/src/file_vault.rs:184-203`), with a direct mutation pin
at `crates/haider-accounts/src/file_vault_tests.rs:152-181`. The broker acquires
that lease before re-reading the bundle (`oauth.rs:6000-6010`), adopts a newer
generation without a request (`oauth.rs:6020-6034`), persists uncertainty before
the irreversible request (`oauth.rs:6053-6068`), and persists the rotated bundle
before dropping the lease (`oauth.rs:6150-6159`).

## Change

Only `crates/haider-daemon/src/oauth_tests.rs` changed; product `oauth.rs` and
the file-vault implementation are untouched.

- The fake server publishes a lost-wake-safe `Notify` only after it has parsed
  the generation-one refresh request (`oauth_tests.rs:410-467,663-690`).
- The second real `FileVault` handle is wrapped by a test-only delegating vault
  that notifies only when its real OS-lock attempt returns contention
  (`oauth_tests.rs:3969-4016`).
- The first response is released only after both exact events: generation one
  reached the endpoint, and the second resolver observed the physical alias
  lease as busy (`oauth_tests.rs:4042-4077`). There is no scheduling yield or
  observation sleep in the named test.
- The assertion now pins the submitted refresh-token fingerprint to the seeded
  generation-one token, in addition to the aggregate count and durable
  generation-two bundle (`oauth_tests.rs:4092-4117`).

This creates the interleaving the test claims to verify. Removing the lease,
moving the re-read outside it, or releasing it before durable apply now fails
the named mutation instead of selecting a different valid scenario.

## Wait and keepalive laws

Registry #94 is satisfied with one continuous absolute deadline. Immediately
before the first resolver starts, the test computes:

```text
refresh_deadline = start + TOKEN_TIMEOUT
endpoint-entry wait + remaining contention wait <= TOKEN_TIMEOUT = 15 seconds
```

Both `timeout_at` calls reuse that same deadline, so the second wait gets only
the remainder after the first; it cannot reset the budget to 30 seconds. The
15-second value matches the rotating broker's shared transport: the broker
clones `SharedHttpTransport` (`oauth.rs:5199-5204`), whose default request
ceiling is 15 seconds (`http_transport.rs:10-13,25-31`). No timeout was widened
and no sleep was added.

Registry #95 is not implicated. These waits orchestrate a loopback token HTTP
POST and a local OS-lock event; they do not hold a negotiated daemon/RPC
connection idle while waiting on external state. The HTTP client and server
tasks continue servicing the request while the test awaits notifications.

## CI evidence and citation audit

The supplied three-failure claim is wrong for two of its three run IDs:

- [run 33447488010](https://github.com/Rizzist/haider-agent/actions/runs/33447488010),
  [Linux job 99669739804](https://github.com/Rizzist/haider-agent/actions/runs/33447488010/job/99669739804)
  logs this test `ok` at line 2656; its Windows job also passes it.
- [run 33456746558](https://github.com/Rizzist/haider-agent/actions/runs/33456746558),
  [Linux job 99698221578](https://github.com/Rizzist/haider-agent/actions/runs/33456746558/job/99698221578)
  logs this test `ok` at line 2659; its Windows job also passes it.
- [run 33493578743 attempt 1](https://github.com/Rizzist/haider-agent/actions/runs/33493578743/attempts/1),
  [Linux job 99810493668](https://github.com/Rizzist/haider-agent/actions/runs/33493578743/job/99810493668)
  contains the actual `left: 2, right: 1` failure. Its
  [attempt-two Linux job](https://github.com/Rizzist/haider-agent/actions/runs/33493578743/job/99822612703)
  passes the same test.

The cited pass is correct: commit
[`8d91389`](https://github.com/Rizzist/haider-agent/commit/8d91389b7bd2b143aca96b151f179b356619ed96)
passes in both Linux and Windows jobs of
[xplat run 33478526082](https://github.com/Rizzist/haider-agent/actions/runs/33478526082).
`git diff --quiet` confirms no changes to `oauth.rs`, `oauth_tests.rs`, or
`file_vault.rs` from `d75a8ea` through `8d91389` and `2b8beeb`; scheduling, not
source drift, selected the different old-test outcome.

`LANE-COMMON.md`'s heading still says “968,” and its stated base `8952219` is
drifted. Neither was used as Git authority. The turn-performance SIGKILL ledger
is only proposed in the supplied turn-performance evidence; that evidence says
shipment remains blocked until the external-ledger matrix exists. It does not
exercise this OAuth rotating-refresh fixture and was not treated as proof of
the named OAuth behavior.

## Verification

All build/test commands used the mandated environment:

```text
RUST_MIN_STACK=8388608
HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac
HAIDER_TEST_SIBLINGS_PREBUILT=1        # daemon tests
CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=0
```

Free space was checked before every Cargo build/test command and remained well
above the 700 MiB stop floor.

- `cargo build -p haider-cli -p haider-daemond`: PASS. Prebuilt
  `target/debug/haider` is 103,138,416 bytes and `haiderd` is 185,048,656 bytes;
  the daemon exceeds registry #64's 10 MiB floor.
- Focused exact test: PASS, 1/1.
- Quiet final-tree proof: PASS, 50/50 separate test-process invocations.
- CPU-hog final-tree proof: PASS, 50/50 separate invocations. Owned `yes` PID
  46869 was confirmed alive before and after the batch, then killed and reaped
  exactly.
- `cargo test -p haider-daemon --quiet`: PASS. Unit target 913 passed / 3
  ignored; `session_hub_tests` 103 passed; smoke and state-machine targets 1/1
  each; doc tests pass.
- `cargo clippy -p haider-daemon --tests --no-deps -- -D warnings`: PASS.
- `cargo fmt --all -- --check`: PASS.
- `git diff --check`: PASS.
- `bash scripts/check-unsafe-counts.sh`: PASS, production 189 / test 16.

## CI error registry walk

- #45/#77: unsafe counts, formatting, diff hygiene, focused behavior, and the
  full affected-crate suite pass.
- #64/#67: both sibling binaries were explicitly prebuilt; `haiderd` exceeds
  the identity-size floor.
- #72/#74: discovery stayed disabled; the test uses only temporary loopback
  server and vault roots.
- #94: both new event waits share the one absolute production token-request
  deadline; there are no sleeps or widened/reset timeouts.
- #95: no negotiated product connection is retained across either local test
  event wait.
- Remaining registry classes were checked against this test-only OAuth fixture
  change and are not affected. No new CI error class was discovered.

The supplied lane inputs remain unedited and uncommitted. All lane work remains
uncommitted for the orchestrator.

SHIP
