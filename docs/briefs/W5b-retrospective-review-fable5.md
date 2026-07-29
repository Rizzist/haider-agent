# W5b / W5b.2 — retrospective review of record (Fable 5)

Reviewer: Fable 5. Owner-mandated correction: **every review/verify pass is
Fable 5 itself; codex implements, it does not review.** W5b (merged `01e01f7`)
and W5b.2/2a (merged `12fe8ab`) shipped on codex-authored reviews of record.
This is the retrospective Fable pass over that work.

Method: read the security-critical paths, then **independently re-execute the
load-bearing mutations** rather than trusting the implementer's
"Verified by revert" annotations. A compile failure never counts as a kill.

## Mutations re-executed

| # | Mutation | Result |
|---|---|---|
| S1 | PKCE downgrade: `challenge = verifier`, `code_challenge_method = "plain"` | **KILLED** |
| S5 | Release the rotated access token when durable persistence fails (`RefreshApplyError::Persist` arm emptied, exhaustiveness preserved) | **KILLED** |
| S3 | Broker-side atomic fence early-returns disabled (`resolve_oauth` + `mark_expired_if_current`) | survived |
| S3b | Actor-side generation CAS disabled at all three sites | survived |
| S3c | S3 **and** S3b together | survived |
| S3d | **Entire five-component fence CAS deleted at all three sites** | survived — *whole daemon suite green (108/108)* |

S1 is worth noting for the right reason: the kill came from the **fake
authorization server** asserting the S256 challenge itself, which is exactly
the design intent — the FAKE-AS is the test authority, not an assertion the
implementer wrote about their own code.

## Finding

**[P2] The OAuth generation fence had zero production coverage. FIXED IN THIS
REVIEW.**

`apply_oauth_refresh` (`accounts.rs:911`), `begin_oauth_refresh` (`:976`) and
`expire_oauth_refresh` (`:1018`) each carry a five-component CAS —
generation, issuer, audience, resource, subject_hash — against the bundle
currently in the vault. That CAS is the durable authority preventing a late
refresh from clobbering a concurrently-replaced credential.

It could be **deleted outright at all three sites with the entire haider-daemon
suite still passing.**

Root cause: the four tests named for this property —
`late_refresh_completion_after_remove_is_generation_fenced`,
`late_refresh_completion_cannot_overwrite_a_newer_bundle_generation`,
`late_refresh_failure_after_remove_readd_cannot_expire_the_replacement`,
`late_refresh_failure_cannot_expire_a_newer_same_alias_generation` — drive
`start_status_actor` (`oauth_tests.rs:1942`), a **test double that
reimplements the fence in the test file**. They pin the double's CAS, not
production's. The real account actor is never on the path.

This is the same false-confidence failure mode found in W5b round 2 (the
secret-sweep that formatted DTOs instead of rendering, and the unasserted
resource binding): a "Verified by revert" annotation on a test that cannot
observe the code it names. It is the third instance, and it is the reason the
mutation-check law requires the *reviewer* to re-execute.

The code was never wrong — the guard is present and correct. The exposure was
that any future refactor could remove it silently.

Fixed by `stale_oauth_fences_cannot_overwrite_or_expire_a_replaced_bundle`
(`accounts_tests.rs:184`), which drives the **real** `start_account_actor`:
the vault holds generation 2, a refresh that began at generation 1 tries to
apply, expire, and begin. Mutation-checked twice — deleting the whole CAS
KILLS it, and disabling the `generation` term alone KILLS it.

## Confirmed sound (no finding)

- **PKCE/redirect**: S256 challenge, exact-redirect match, one exchange per
  flow, ephemeral loopback binding — pinned behaviorally by the fake AS.
- **Persist-before-release (INV-1)**: a refreshed access token is never
  returned when durable persistence fails; the broker additionally
  invalidates so the old refresh token is not retried against a
  possibly-rotating server.
- **Single-flight refresh**: `flights` keyed by
  `(provider, alias, generation, issuer, subject_hash, fence_epoch)`; waiters
  re-read the vault after the flight completes; no lock held across HTTP.
- **Defense in depth is real, not redundant-by-accident**: the broker atomic
  fence and the actor CAS genuinely back each other up — the S3/S3b results
  show either alone still blocks the modelled scenarios. Only the *coverage*
  was missing.

## Carried forward (non-blocking)

- **[P3]** `callback_state_comparison_is_constant_time_and_load_bearing`
  (`oauth_tests.rs:1536`) is a source-text scan; the "load-bearing" half
  actually lives in
  `wrong_missing_duplicate_state_path_host_port_and_non_get_are_rejected`.
  Timing-safety is not runtime-assertable, so the technique is defensible —
  the name over-claims what this test body does.
- **[P3]** `token_response_source_chunks_are_exclusively_owned_and_scrubbed`
  (`:3041`) is likewise a source-text scan, and its literal-count assertion is
  brittle: W5b.2a's third key-bearing transport already broke it once (2 → 3).
- **[P3, from the W5b.2 pass]** `openai_jwks_private_dns_answer_is_rejected_
  before_key_use` survives disabling `blocked_fixed_credential_target` alone —
  a private IP then fails at connect with the same error. Distributed
  coverage, not a bypass.

## Gate

clippy `--workspace --all-targets -D warnings` clean. Test ledger 1003 → 1004.
Per-crate (workspace runs SIGABRT on this box): protocol 23 · accounts 20 ·
core 41 · provider 47 · **daemon 144** · daemond 86 · rpc 45 · tui 465 · cli 21
· store 35 · tools 69 · client 18 · verify 1 — all 0 failed.

## Verdict

**Merged work stands.** No security defect found in W5b/W5b.2; PKCE, the
redirect contract, persist-before-release and the single-flight refresh are all
genuinely pinned. One P2 **coverage** hole in the generation fence is closed
here with a production-actor pin. Three P3 test-technique notes carried
forward, none blocking.
