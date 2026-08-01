# W9a update mutation notes

The W9a tests are runtime observers, not source-shape substitutes. Every
mutation below has an expected **RUNTIME failure** in a separate test file.

## Discovery and gate

- Replacing the paginated releases listing with `/releases/latest`, dropping
  prereleases, trusting API order, or selecting host hardware instead of the
  compiled architecture fails the two-target prerelease selection test.
- Admitting equal-version downloads, downgrades, malformed tags, duplicate or
  missing exact assets, or invalid SemVer identifiers fails before any
  download call.
- Ignoring a declared HTTP truncation makes the local fixture capable of
  returning a release; the test observes requests and local filesystem side
  effects and deliberately does not assert a network error code.
- Restoring automatic redirects on an authenticated curl command makes the
  local redirect fixture observe a second request. The test also requires the
  fake token to arrive through curl stdin, never argv or the environment.

## Immutable staging

- Changing checksum parsing to reject the workflow's `dist/NAME` spelling, or
  admitting wrong, ambiguous, or basename-mismatched checksums, fails before
  the verifier is called.
- Admitting traversal, absolute paths, symlinks, hardlinks, devices, FIFOs,
  duplicates, extras, missing binaries, oversized binaries, a wrong top
  directory, or a missing executable bit returns a staged capability where
  the test requires refusal.
- Ignoring quarantine-removal, signing, signature verification, CLI smoke, or
  daemon smoke failures changes the exact installed inode/bytes/mode snapshot
  or returns a verified capability.
- The production CLI smoke runs both exact `--version` and the offline
  versioned `self-test`. Admitted binaries are mode 0500, and commit rechecks
  both digests before writing a marker or creating the first backup.
- Reordering transaction acquire/recovery ahead of staging rolls back a
  planted pending new/new pair when a partial transfer fails. The fixture
  requires that marker and pair to remain untouched.

## Pair transaction and recovery

- Faults are injected immediately after all six required durable boundaries:
  daemon backup, CLI backup, daemon rename, CLI rename, install-directory
  fsync, and installed-pair verification. The implementation also injects at
  both durable owner-writable permission publications. Every expected
  **RUNTIME failure** restores the exact old inode/bytes/mode pair and removes
  a successfully consumed marker.
- The restart-entry observer's first operation re-reads both canonical paths;
  the test invokes the real restart orchestrator and requires the first spy
  event to be the exact new daemon/new CLI pair.
- A failing installed-pair verifier returns no restart capability and restores
  the exact old pair.
- Every durable marker phase is planted with its physical filesystem shape:
  no/one/two backups, daemon-new/CLI-old, new/new, and daemon-old/CLI-new
  partial rollback. Recovery restores old/old except for the durable success-
  finalizing phase, which completes target cleanup; neither path accepts a mix.

## Authenticated restart

- Moving peer-credential capture after `UnixStream::into_split`, or using the
  profile lock's diagnostic PID, fails the kernel PID/UID connection test.
- The real-daemon acceptance requires a matching drain, real profile-lock
  release, a newly spawned sibling, a new instance, an increased generation,
  `Ready`, required features, and exact `Welcome.daemon_version`. A second
  signal forces the old child to exit nonzero instead of the asserted graceful
  status.
- The no-daemon fixture requires the endpoint to remain absent after commit.
- A target-version mismatch must stop the retained child, restore the exact
  old pair, remove the marker, restart the old sibling, and return the health
  error rather than success.
- A fake authenticated incumbent that never drains receives one spied signal;
  timeout returns with the marker and both backups still present and sends no
  second signal.

## CLI and daemon smoke

- `haider update` accepts only the bare and `--check` forms; extra arguments
  have the usage exit authority.
- `haiderd --version` has exact stdout, empty stderr, success status, and no
  profile-directory side effect.
