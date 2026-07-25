# Workspace conventions (enforced)

1. **Tests are separate from code.** Integration tests in each crate's `tests/`;
   any additional test modules go in `*_tests.rs` files. Never `#[cfg(test)] mod`
   inline in source files. `xtask test-count` fails CI if the workspace test count
   drops below `test-baseline.txt`; reducing tests requires an explicit reviewed
   waiver that updates the baseline in the same patch.
2. **10k LOC soft cap per source file** (`xtask loc-lint` warns). Prefer splitting
   modules well before the cap.
3. **Clean, modular, extensible**: typed structs over stringly state; every
   cross-crate surface goes through `haider-protocol`; no crate reaches into
   another's internals.
4. **Schema-affecting patches close all lanes** until merged, fixtures regenerated,
   and generated TypeScript re-emitted (from W1 on).
5. **Review verdicts**: every patch review ends SHIP / SHIP_WITH_FIXES / NO_SHIP,
   journaled. Only SHIP or completed SHIP_WITH_FIXES may tag.
6. **Releases**: every tagged version must pass CI plus the artifact smoke
   (version match + offline self-test) before publish. Never repair a released
   tag — ship the next patch version.
7. **No secrets in the repo, ever.** No real transcripts, tokens, personal paths,
   or internal service references.
8. **Implementer AI agents must never delete or weaken tests.** Test skeletons and
   golden fixtures are authored spec-side; implementations make them pass and add
   coverage upward.
