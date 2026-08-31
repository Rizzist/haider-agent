# Lane 968 hygiene — verification evidence

## Verdict and boundaries

Branch `lane-968-hygiene` started from the current merge-forwarded
`origin/wave-968` commit `2e68a1993ee603c4f86148e686d9b7a7eab64b48`.
Guard #77 ran before product edits and passed with `production=188` and
`test=16`.

Items 1, 3, 5, 6, and 7 are closed, and the production hook
acknowledgement-retention defect in item 2 is closed. One requested test-only
cleanup is intentionally not closed:
`crates/haider-daemon/src/hooks_tests.rs` is a protected file in the common
968 brief, so its current raw 1,200 ms sleep was not edited. The production
retention fix is covered by deterministic transition tests in a new, allowed
test module.

Item 4 also has a requirements conflict. Runtime help is catalog-derived and
both named QA checks pass, but the unchanged catalog QA check can pass only by
statically parsing a complete `HELP_TEXT` mirror. Removing that mirror to
satisfy the client contract makes the mandated unchanged check fail. The
runtime currently derives from the locally linked shared catalog rather than a
received running-daemon `command.list` cache. These conflicts keep the overall
lane verdict at `NO_SHIP` even though all product suites and the two named
QA-gate checks are green.

The parallel-owned worker-supervisor retirement, observe-cache soak,
idle-exit, `session_hub/rpc.rs`, and runtime shutdown paths are untouched.
The common brief's protected OAuth files, `hooks_tests.rs`, and daemond test
support are also untouched. The work remains uncommitted for the orchestrator.

## Citation audit

| Brief citation | Verdict | Current construct |
| --- | --- | --- |
| `accounts.rs:582 area` | correct | `StagedSecret` begins at `accounts.rs:582`; expiry scheduling is now `:679-693`. |
| `hooks_tests.rs:3354` raw sleep | drifted by one | The raw sleep is currently `hooks_tests.rs:3353`; `:3354` is the assertion. |
| WAL connection setup | drifted | `open_connection` is now `event_store.rs:22973`; the per-connection pragma block is `:22977-22994`. |
| `docs/client-contract-v1.md:70-74` | drifted | The command-catalog authority and no-mirror law are now `:75-79`. |
| `app.rs:12941-12943` | correct on entry, moved by fix | Those lines held the faulty argument activation before the helper was inserted; the shared fixed activation is now `app.rs:12912-12971`. |
| provider/method contract `w5-provider-research-report.md:480-488` | correct | Provider-derived dynamic slots are `:480-484`; provider-then-method is explicit at `:488`. |

## Per-item reproduce, fix, verify

All Cargo commands below used the mandated 8 MiB stack, disabled discovery,
the `test-mac` device name, disabled incremental compilation/debug info, and
had a disk preflight above 700 MiB. Daemon-spawning CLI tests additionally used
fresh prebuilt siblings and `HAIDER_TEST_SIBLINGS_PREBUILT=1`.

### 1. Staged-secret storage releases at the advertised TTL

Red: the paused-clock mutation pin
`staged_secret_storage_is_released_at_ttl_without_a_followup_rpc` retained one
resident entry after advancing the full five-minute TTL. It never prints,
compares, or otherwise exposes secret material.

Fix: connection-owned entries now live behind an `Arc<Mutex<_>>`. Staging arms
a Tokio expiry wake using a weak reference, so the timer releases expired
zeroizing storage at the TTL without extending the connection lifetime. The
existing lazy sweeps remain defense in depth.

Green:

```text
cargo test -p haider-daemon staged_secret_storage_is_released_at_ttl_without_a_followup_rpc
1 passed; 0 failed
```

The full daemon package later passed 906 library tests with 3 pre-existing
live-provider tests ignored, 103 session-hub integration tests, and every
remaining target.

### 2. Hook `ack_pending` retention

Red: the extracted state-transition pin
`successful_retry_releases_ack_pending_retention` showed that a scope retained
after a failed ACK path was never released after its durable retry succeeded.

Fix: current-page handling failures are removed from the successful ordered
set before being retained; a successful durable retry releases only its
successfully ACKed scope; a failed flush retains the ordered scopes and cannot
authorize terminal trust cleanup. The transition is isolated in
`retain_failed_run_ack` plus `reconcile_run_ack_retention` and covered in all
three directions. After an independent verifier found that the initial test
manually supplied an already-empty successful set, the exact failure
transition was extracted and the test now asserts both removal from the
successful set and insertion into the blocked set.

Green:

```text
cargo test -p haider-daemon ack_retention
3 passed; 0 failed
```

Unresolved request: the unrelated subscriber-process test still contains
`tokio::time::sleep(Duration::from_millis(1_200))` at protected
`hooks_tests.rs:3353`. Replacing that sleep with a budget-derived/event-driven
wait requires authority to edit the file the common brief explicitly forbids.
No test was weakened, ignored, or moved to hide it.

### 3. WAL `journal_size_limit`

Red: the pragma assertion observed SQLite's uncapped default `-1` instead of
the intended 8 MiB bound.

Fix: every store connection now applies `PRAGMA journal_size_limit=8388608`
after selecting WAL mode. The existing `wal_autocheckpoint=1000` behavior is
unchanged.

Green: `wal_journal_allocation_is_capped_after_checkpoint_reset` first grows
the WAL beyond the cap, performs a restart checkpoint/reset, and asserts the
physical `-wal` file is at most the configured limit. It separately pins
`wal_autocheckpoint == 1000`.

```text
cargo test -p haider-store wal_journal_allocation_is_capped_after_checkpoint_reset
1 passed; 0 failed
cargo test -p haider-store
all targets passed
```

### 4. TUI help and command catalog

Red source reproduction reported:

```text
missing_from_help=['attach']
absent_from_COMMANDS=['monitors']
```

Fix: the runtime help rows are built from the same
`command_catalog_items("", true, dynamic_slots)` projection that serves
`command.list`; `/help` itself is excluded and custom commands retain their
separate section. `monitors` is a real session command, so it was registered
as a feature-gated client-view command rather than deleting its dispatcher.
Its overlay now includes an explicit `use /monitors` refresh affordance.

`HELP_TEXT` remains only as an unused compatibility fixture because the
unchanged named QA check statically parses that symbol. Runtime rendering does
not import or read it. This tension is recorded rather than weakening or
editing the check. It nevertheless remains a literal mirror and therefore
does not satisfy the strict “never a hand list” contract.

Green:

```text
cargo test -p haider-tui help_command_rows_are_derived_from_the_authoritative_catalog
1 passed; 0 failed
t0.tui.catalog_help_command_list_pin
PASS — command.list=COMMANDS count=42; HELP_TEXT=COMMANDS-minus-self count=41;
       both 118x36 and 80x24 painted; clean daemon teardown
```

The check file was not edited.

### 5. `/login` provider-then-method activation

Red: two palette activations discarded the provider and failed to open the
provider API-key card.

Fix: Enter, mouse activation, and Tab completion now replace only the active
argument fragment while retaining completed slots. `/login` therefore
progresses from provider to method without fabricating or duplicating args.

Green:

```text
cargo test -p haider-tui login_palette_activation_preserves_provider_then_method_slots
1 passed; 0 failed
t0.tui.palette_activation_closure
PASS — login stage0_provider_preserved=true; stage1_key_card=true;
       monitors catalog activation also PASS; clean daemon teardown
```

The unchanged exhaustive QA check completed all 42 catalog activations in
433,004 ms.

### 6. Peer UX pair

#### Missing authentication on `haider account add`

Red: the black-box pin found JSON parse errors on stderr and the command could
fall toward daemon startup without an actionable typed envelope.

Fix: missing auth material is a non-retryable `invalid_argument`/`EX_USAGE`
refusal. Its hint names `--api-key-env <VAR>`, `--api-key-stdin`, and
intentional `--no-auth`. A second execution-level guard refuses before any
provider snapshot or RPC.

Green: `account_add_without_auth_refuses_with_typed_actionable_hint` asserts
exit 2, a typed stdout JSON envelope, empty stderr, all three remedies, and no
daemon PID; `direct_add_execution_without_auth_refuses_before_any_request`
asserts zero requests.

#### Per-session model identity in error envelopes

Red: a flagless pre-session failure serialized the profile-global packaged
default as though a failed session had bound it.

Fix: request construction may still use the configured default, but error
serialization before an accepted result carries only an explicitly supplied
model (or the explicit fake-provider model). Accepted results continue to use
daemon session binding truth.

Green: `flagless_run_without_an_active_account_exits_65_with_remedy` now pins
`model: null` and proves the packaged default bytes are absent from stdout and
stderr. Explicit-provider/model compatibility controls
`unknown_run_provider_surfaces_daemon_create_refusal` and
`configured_custom_model_reaches_chat_wire_verbatim_despite_catalog` also
pass. The complete CLI package passed, including 117/117 black-box CLI tests.

### 7. Provider display-name catalog drift guard

Red: the registry summary's serialized `model_details[0].display_name` was
null even though the provider registry model carried `Fixture frontier-a`.

Fix: `ModelDetailWire` gained an additive optional `display_name` field and
the registry projection copies the authoritative discovered-model label.
Absence is skipped when serialized, preserving older wire bytes.

Green:

```text
cargo test -p haider-daemon summaries_report_pickable_discovered_models_not_profile_literals
1 passed; 0 failed
cargo test -p haider-rpc --test model_details_tests
4 passed; 0 failed
```

The registry pin asserts the exact display label and the RPC test proves both
round-trip preservation and omission compatibility.

## Integrated verification

- `cargo test -p haider-store`: all targets passed.
- `cargo test -p haider-rpc`: all targets passed, including 97/97 wire
  goldens.
- `cargo test -p haider-daemon`: 906 passed, 3 pre-existing live-provider
  tests ignored; 103/103 session-hub integration tests and all other targets
  passed.
- `cargo test -p haider-tui`: all unit, integration, render, and doc-test
  targets passed after the catalog-derived help viewport expectation was
  updated.
- `cargo test -p haider-cli`: all targets passed with fresh siblings,
  including 117/117 CLI black-box tests.
- Package-scoped all-target Clippy for `haider-store`, `haider-rpc`,
  `haider-daemon`, `haider-tui`, and `haider-cli`, with `-D warnings`: passed.
- `scripts/qa-gate/run.sh test`: 29/29 passed.
- `t0.tui.catalog_help_command_list_pin`: PASS without check edits.
- `t0.tui.palette_activation_closure`: PASS without check edits.
- Fresh `target/debug/haider` is 102,889,104 bytes; fresh
  `target/debug/haiderd` is 184,538,320 bytes, above registry #64's 10 MiB
  floor.
- `cargo fmt --all -- --check`, `git diff --check`, the unmerged-index scan,
  and the conflict-marker scan passed.
- Final guard #77 rerun passed at `production=188`, `test=16`.
- `cargo run -q -p xtask -- test-count --update` updated the baseline from
  4,313 to 4,322.

## CI error registry walk

| Registry class | Result |
| --- | --- |
| #1/#2/#3/#4/#6/#35/#36/#37/#38/#39 | New wire/model fields, error variants, constructors, matches, and collection ownership compile across every affected all-target package; complete package tests pass. |
| #5/#27/#28/#31/#40/#44/#55/#89 | No platform-specific production behavior or process runner changed. macOS was executed; other hosts are by inspection. |
| #7/#23/#34 | No manifest, dependency, migration, or lockfile change. The WAL fix is a per-connection pragma, not a schema migration. |
| #8/#9/#10/#11/#12/#13/#14/#15/#16/#17/#18/#19 | Final package-scoped deny-warnings Clippy and formatting/diff checks cover dead code, imports, ownership, locks across await, complexity, casts, iterator/range, lint, and formatting classes. |
| #20/#21/#48/#54 | New behavioral tests are in declared crate test modules, all Cargo tests use the mandated 8 MiB stack, and the test baseline was updated from 4,313 to 4,322. |
| #22/#33/#42/#87/#92/#93 | No process-global subscriber, benchmark sampler, cold-start timing, thread-count fence, or maintenance-loop cadence changed. |
| #24/#50/#76 | Registry display names now flow through one additive optional wire field; exact-label and older-absence byte compatibility are both pinned. |
| #25/#52/#57/#59 | Full TUI tests and both exact 118x36/80x24 QA repaints cover the changed help/monitor presentation; no benchmark claim or roster grammar changed. |
| #26/#41/#46/#47/#51/#53/#69/#72/#74/#88/#90 | No runtime-root, walker, profile-lock, executable discovery, staging publication, or sparse-file behavior changed. Tests use scratch profiles with native discovery disabled. |
| #29/#60/#64/#67/#71 | Fresh CLI/daemon siblings were built; daemon-spawning CLI tests used the prebuilt-sibling flag; `haiderd` is 184,538,320 bytes and exact live QA checks cleaned up their daemons. |
| #30/#49/#61/#68 | Hook retention has named success/failure/current-page transition assertions; secret TTL, account refusal, model scrubbing, WAL size, login closure, help catalog, and display-name claims each have named behavioral assertions with actual-value diagnostics. |
| #32/#63/#78 | No release, tag, archive, or external shell utility action occurred. |
| #43/#58/#66/#70 | Descriptor sweeping, CAS thresholds, STT, and workflow triggers are untouched. |
| #45/#77 | Unsafe-count guard ran before edits and is rerun in the closing pass; no unsafe code was added. |
| #56/#65/#80/#85/#91 | CLI failures use typed `invalid_argument`, `no_active_account`, and stable exit/envelope semantics rather than errno or timing inference. |
| #62 | The optional wire addition does not change an existing return type. All in-tree struct literals were updated; older/newer wire absence compatibility is pinned. Downstream Rust struct-literal source compatibility is not claimed. |
| #73 | Product guarantees use behavioral tests. The unchanged catalog QA check itself still contains its pre-existing static source parser; this lane did not add or edit that check. |
| #75/#79/#81/#82/#83/#84/#86 | No worker/hub/process ownership, detach, output-reader, paused process, or exit-observation path changed. |
| #94 | No new derived outer deadline was added. The staged-secret timer uses the already-advertised `SECRET_TTL`; the protected raw 1,200 ms hooks-test sleep remains the sole unresolved requested cleanup. |
| #95 | Secret expiry is a local timer and does not wait on external state or hold a negotiated connection idle; QA keepalive/cleanup evidence passes. |

No new CI error class was discovered. The unresolved protected raw sleep is an
existing #94 cleanup request, not a newly introduced class.

## Closing verification

The TUI/catalog verifier returned `NO_SHIP`. It independently confirmed the
provider/method login fix, but found that the compiled public `HELP_TEXT`
mirror violates the no-shadow contract, runtime help is not sourced from a
running-daemon catalog cache, the unchanged QA check still describes
`monitors` as missing after it was registered, and the added command makes the
check's pre-existing #94 lifecycle arithmetic stale. No check file was edited.

The storage/hooks verifier first found a mutation-coverage gap in the
current-page ACK failure pin. After the transition/test repair, re-review
returned `SHIP` for the production retention fix, TTL release, and WAL cap.
The overall storage verdict remains `NO_SHIP` only because protected
`hooks_tests.rs` still contains the requested raw sleep at `:3353` and its
paired sleep at `:3404`.

The CLI/provider verifier returned `SHIP` for items 6 and 7: typed no-auth
refusal, zero-RPC backstop, pre-session model scrubbing, explicit-model
controls, exact registry display-name propagation, and additive older-wire
omission all passed. It also corrected this record not to claim downstream
Rust struct-literal source compatibility for the new public optional field.

NO_SHIP
