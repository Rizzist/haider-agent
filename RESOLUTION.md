Merge of origin/wave-973 at b3aeb633 into lane-973-accounts-ux, round 23.

- accounts_tests.rs: preserve the second discovery release required by the lane's two account discoveries; use upstream's fixture-ready TEST_WAIT_TIMEOUT instead of the two-second load bound.
- race-stress-cases.tsv: retain both the lane's OAuth returned-issuer case and every upstream file-lock/fixture-readiness case.
- test-baseline.txt: derived as 6720 by the actual dependency-free xtask test-count --update command on the merged source. Receipt: state/evidence/973-accounts-ux/24-sol-r23-cap3/baseline-only.json (harness-relative). Final integrated source will be counted again after the newest upstream landing.
- Auto-merged production changes are reviewed together with both P2 repairs and gated on this integrated tree. No push is authorized in this round.
