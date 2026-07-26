codex
## Findings

No findings.

All seven r2 fixes verify line-level:

1. Cancellation is rechecked after both `put_file` outcomes in [`supervise_process`](/Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:1134); both gated success/failure tests exist at [`process_tools_tests.rs`](/Users/rizzist/haider-run/haider-c2/crates/haider-tools/tests/process_tools_tests.rs:345).
2. [`FileCas::put_reader`](/Users/rizzist/haider-run/haider-c2/crates/haider-store/src/cas.rs:68) hashes each successfully written chunk in the same pass; `put_file` delegates to it at line 208. The mutating-reader test is at [`cas_tests.rs`](/Users/rizzist/haider-run/haider-c2/crates/haider-store/src/cas_tests.rs:23).
3. [`observe_process_leader_exit`](/Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:1315) uses non-reaping `waitid(...NOWAIT)`; sweeping precedes [`reap_process_leader`](/Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:1337). The zombie/PGID invariant is documented at line 1357 and lifecycle ordering is asserted at [`process_tools_tests.rs`](/Users/rizzist/haider-run/haider-c2/crates/haider-tools/tests/process_tools_tests.rs:449).
4. The configurable provider ceiling is declared at [`actor.rs`](/Users/rizzist/haider-run/haider-c2/crates/haider-core/src/actor.rs:59), enforced before dispatch at line 379, and produces typed `LoopLimit`; the small-ceiling test is at [`request_input_tests.rs`](/Users/rizzist/haider-run/haider-c2/crates/haider-core/tests/request_input_tests.rs:196).
5. Provider polling precedes command servicing at [`actor.rs`](/Users/rizzist/haider-run/haider-c2/crates/haider-core/src/actor.rs:403) and line 467. [`defer_submit_or_reject`](/Users/rizzist/haider-run/haider-c2/crates/haider-core/src/actor.rs:1328) enforces the cap with typed `Busy`; the flood test is at [`runtime_tests.rs`](/Users/rizzist/haider-run/haider-c2/crates/haider-core/tests/runtime_tests.rs:514).
6. [`is_secret_env_name`](/Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/shell.rs:186) performs case-insensitive known-name, substring, and word matching. Tests cover `PGPASSWORD`, `MYSQL_PWD`, lowercase tokens, and visible `PATH`/`HOME`.
7. [`supervise_process`](/Users/rizzist/haider-run/haider-c2/crates/haider-tools/src/process.rs:926) records transcript high-water; the flood test asserts `cap + PROCESS_OUTPUT_CHUNK_BYTES` while confirming spill growth at [`process_tools_tests.rs`](/Users/rizzist/haider-run/haider-c2/crates/haider-tools/tests/process_tools_tests.rs:294).

No regressions found while tracing `UserProcessExec::new`, `ShellSession::submit`, `EffectBroker::process_exec_user`, `PreparedProcessExec::new`, `verify_path_identity`, `open_directory_beneath`, `set_anchored_current_dir`, `begin_group_termination`, `reap_process_leader`, `HarnessActor::drive_turn`, `complete_request_input`, `defer_submit_or_reject`, `EffectBroker::close`, and `SessionProjection::apply`.
