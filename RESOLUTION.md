Merge of origin/wave-973 at b3aeb633 into lane-973-accounts-ux, round 23.

- accounts_tests.rs: preserve the second discovery release required by the lane's two account discoveries; use upstream's fixture-ready TEST_WAIT_TIMEOUT instead of the two-second load bound.
- race-stress-cases.tsv: retain both the lane's OAuth returned-issuer case and every upstream file-lock/fixture-readiness case.
- test-baseline.txt: derived as 6720 by the actual dependency-free xtask test-count --update command on the merged source. Receipt: state/evidence/973-accounts-ux/24-sol-r23-cap3/baseline-only.json (harness-relative). Final integrated source will be counted again after the newest upstream landing.
- Auto-merged production changes are reviewed together with both P2 repairs and gated on this integrated tree. No push is authorized in this round.

Fresh upstream integration: origin/wave-973 advanced to 56e4a29a3bfa58f422f705302b28b0a15648531b (macOS packaging fixes). Its four packaging/test files merged without source conflicts. The only conflict was test-baseline.txt; actual xtask test-count --update re-derived 6720 on the combined source (baseline-only.json). The original b3aeb633 merge and both account repairs remain in parent 5d62c65e.
