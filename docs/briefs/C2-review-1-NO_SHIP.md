codex
NO_SHIP. The C4a terminal-claim door remains centralized, but authorization provenance, cancellation, process containment, and `request_input` completion are not sound.

### Findings

1. **P1 — Model-originated execution can spoof `PreAuthorized(UserTyped)`.** [`process_exec_user`](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:427>) is public and accepts the same freely constructible `ProcessExec` used by model calls. Passing `None` directly selects [`begin_user_typed`](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:461>), which stamps `UserTyped` without a provenance token ([broker.rs:940](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/broker.rs:940>)). `ShellSession` is only a convention; provenance is not unforgeable at the API boundary.

2. **P1 — Cancellation is not structurally guaranteed to end as one `Cancelled` outcome.** The finalizer maps every supervisor error to `Failed` ([process.rs:565](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:565>)). Consequently, an output-read/output-sink failure ([process.rs:710](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:710>)) or CAS failure after a cancelled large-output command ([process.rs:765](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:765>)) journals `Failed`, contrary to the never-Failed cancellation law. TERM and KILL failures are separately discarded; after KILL the deadline is cleared unconditionally ([process.rs:749](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:749>)), so a surviving child can leave `wait()` and `EffectBroker::close()` hanging with no terminal outcome. Kill failures do not escalate.

3. **P1 — The inline cap is applied after unbounded memory growth.** Every chunk is retained in `transcript` ([process.rs:668](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:668>), [process.rs:715](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:715>)); only after process exit is total size compared with the cap and the entire transcript serialized again for CAS ([process.rs:765](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:765>)). An output-heavy command can exhaust harness memory before overflow protection activates.

4. **P1 — Canonical cwd authorization has the same post-authorization pathname race C4a eliminated.** The cwd is canonicalized for digesting ([process.rs:84](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:84>)), followed by asynchronous authorization/journaling, but spawn later follows the pathname through `current_dir` ([process.rs:461](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:461>), [process.rs:469](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:469>)). Replacing that directory with an outside symlink executes under a different cwd without changing the approved digest.

5. **P1 — Descendant containment ends when the shell exits, not when its process group is empty.** `process_group(0)` correctly creates a separate group ([process.rs:479](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:479>)), but normal completion never sweeps that group. Once the shell exits and redirected pipes close, the supervisor returns and removes the registry entry ([process.rs:728](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:728>), [process.rs:757](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:757>), [process.rs:564](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:564>)), leaving background grandchildren alive and uncontrollable. Descendants that create a new session/process group also escape cancellation’s `killpg`.

6. **P1 — `request_input` does not round-trip the answer to the model.** The only provider request contains the initial user message ([actor.rs:364](</Users/rizzist/haider-run/haider-c2/crates/haider-core/src/actor.rs:364>)). Answering merely journals a `ToolResult` and resumes consuming the existing receive-only provider stream ([actor.rs:833](</Users/rizzist/haider-run/haider-c2/crates/haider-core/src/actor.rs:833>)); no tool-role message or second `stream_turn` request is made. The provider interface itself has no channel for injecting a result into an existing stream ([haider-provider/src/lib.rs:94](</Users/rizzist/haider-run/haider-c2/crates/haider-provider/src/lib.rs:94>)). The fake test passes because its script blindly continues, not because the model receives the answer.

7. **P1 — Cancelling `InputRequired` leaves an orphan menu, while stale answers can hang.** Cancellation returns directly from the menu loop ([actor.rs:784](</Users/rizzist/haider-run/haider-c2/crates/haider-core/src/actor.rs:784>)) without a durable dismissal/abandonment event. The existing projection only clears menus on `MenuAnswered`, so the cancelled menu continues replacing the composer ([projection.rs:182](</Users/rizzist/haider-run/haider-c2/crates/haider-tui/src/projection.rs:182>)). After one answer wins, subsequent `AnswerMenu` commands are not serviced by the active turn’s provider/cancel select ([actor.rs:394](</Users/rizzist/haider-run/haider-c2/crates/haider-core/src/actor.rs:394>)); rejection occurs only after `drive_turn` returns ([actor.rs:328](</Users/rizzist/haider-run/haider-c2/crates/haider-core/src/actor.rs:328>)). A hanging provider therefore leaves a losing surface’s `answer_menu()` awaiting forever.

8. **P1 — `env-view` returns plaintext environment secrets.** `EnvViewEntry` publicly carries `Option<String>` ([shell.rs:25](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/shell.rs:25>)), and the builtin copies every allowlisted value directly with `env::var(name).ok()` ([shell.rs:87](</Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/shell.rs:87>)). There is no redaction or secret-name classification.

### Verified

- All new terminal-phase construction still flows through `claim_terminal`; no parallel `EffectPhase::Outcome` append path was found.
- Process controls include the captured original effect ID in their canonical digest and apply to that captured registry entry.
- Output deltas are lossless base64; signal death produces `exit_code: None`.
- Choice validation is fail-closed, options are server-enumerated, menu IDs are generation/start/counter fenced, and normal item lifecycle ordering is preserved.
- `effect.rs` changes are additive. The prebuilt golden suite replayed old fixtures and the new `Cancelled`/`UserTyped` fixtures successfully.
- Prebuilt request-input, golden, fake-provider, and runtime suites passed: 4 + 21 + 4 + 12 tests. Process tests could not enter their bodies because the read-only sandbox denied `tempfile::tempdir()`.
- `cargo fmt --check`, `git diff --check`, and clean-worktree verification passed.

VERDICT: NO_SHIP
hook: Stop
hook: Stop Completed
tokens used
184,294
NO_SHIP. The C4a terminal-claim door remains centralized, but authorization provenance, cancellation, process containment, and `request_input` completion are not sound.

### Findings

