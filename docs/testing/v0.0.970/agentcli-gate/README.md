# Agentcli gate evidence

The lane report is [../agentcli.md](../agentcli.md). All source changes remain uncommitted.

- `final-input-mirror/`: final full T0 run on the last tested binary pair, normal discovery/order/runner, per-check rows, frozen hashes, raw PTYs and process wait receipts. The completed report passes15/15 with15/15 no-orphan proofs and105/105 natural zero TUI exits; `completion-audit.json` records the final validation.
- `rust/final-test-totals.json`: the final selected log for every workspace crate, with summed libtest counts. Repeated included modules and nested subprocess summaries are explicitly included.
- `rust/input-mirror-ledger.json` (final) plus `rust/postupdate-ledger.json` and `rust/literal-flags-verified-ledger.json`: exact follow-up commands, exit codes, elapsed times and pre-build disk headroom.
- `rust/input-mirror-ref-guard.json`: fetched upstream and lane HEAD both `9270f40286d3181fd22c20600b4ae4f9586b8c1d`.
- `source-sha256.json`: changed source, contract, fixture and test-baseline file hashes. The lane-supplied common/brief/lens evidence and this mutable report directory are excluded.
- Initial full T0 report, `initial-failures/`, `final-targeted/`, `update-diagnostic/`, `history-diagnostic/`, `final-fixed/`, `repaint-diagnostic/`, and `final-verified/`: retained earlier failures and focused diagnostic results. Later green evidence does not rewrite these failures.
- Rust initial failures remain alongside successful reruns: typed documentation fixture, new feature withholding boundaries, unused test import, updater fixture buffering/reap proof, and finite fake-script reuse. `results.tsv` is chronological and includes failed attempts; use the final log selection for the release total.

Execution was on macOS arm64. Windows/Linux behavior is by inspection. Debug PTY durations are correctness-gate observations, not release latency benchmarks. Absolute worktree/target paths in copied Rust logs are replaced by `$WORKTREE`/`$AGENTCLI_TARGET`; raw process and terminal evidence is retained unchanged.
