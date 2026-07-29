# W4a2 review of record — SHIP (zero findings)

Reviewer: gpt-5.6 (codex), frozen 784ee76, scope 74d62e8..784ee76.

The shell-exec consent boundary holds under attack. P0/P1/P2/P3: none found.

- Approval boundary: no process spawns before a committed CAS approval; approved==spawned is byte-identical (the exact command string is JSON-escaped in the menu, digested, and passed as the sole /bin/sh -c program — no normalization/reparse between approval and spawn); race attacks (second-turn overtake, cancel-then-resubmit, restart-mid-approval, two-same-shape) all hold.
- SHAPE KEY = BLAKE3(exact shell source + canonical cwd + sorted env-allowlist names); class-wide ProcessExec grant rejected. Shell-metacharacter verdict: the FULL -c source is shown+hashed, not /bin/sh/argv[0]/first-token. Scratch-verified: git status→git push --force, ;/&&/|, backticks, $(), nested sh -c, quote/whitespace changes ALL re-prompt; byte-identical + equivalent cwd reuses the grant. One approval scopes to exactly the shell source the user saw.
- Safety: env_clear() — daemon tokens/OAuth/vault do not cross into the child (sentinel absent); cwd confinement (abs/../symlink rejected, O_NOFOLLOW re-walk before spawn); 1 MiB output + 60s timeout → TERM/grace/KILL on the group; restart reconciles Dispatched→exactly-one-Unknown, no rerun (verified); worker seal (lease-only) intact; INV-1/2, R9, R12, R13, R14 intact.
- Mutation audit 3/3 killed+restored (ask→allow, accept-every-digest, drop-cancel-handoff). 919→927, no deletions. Gate: full workspace pass, daemond live 33/33, clippy -D warnings, fmt, 927/927.

Named residual (pre-existing, ledgered process.rs:9): a descendant that deliberately creates a NEW session/process group can escape killpg — process-group containment, not a full sandbox for an explicitly-approved arbitrary shell program.

VERDICT: SHIP
