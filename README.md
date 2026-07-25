# Haider Code — حيدر

A provider-agnostic coding-agent harness: one Rust binary that is a TUI, a headless
runtime, and a per-device daemon, where every piece of interior state is a typed,
evented, queryable contract.

**Status: scaffold train (v0.0.x).** Private repository; license undecided —
all rights reserved until a license lands. Built by AI agents under the
BUILDGUIDE discipline (SHIP-verdict review loop, frozen contracts, N−1 dogfood).

## Install (macOS)

Download the release archive for your architecture, verify the checksum, extract,
and allow the unsigned binary via System Settings → Privacy & Security → Open Anyway.

```
haider --version
haider self-test
```

## Workspace

`crates/haider-protocol` (contracts + golden fixtures) · `haider-store` (journal/CAS) ·
`haider-core` (harness runtime) · `haider-provider` (model adapters) · `haider-tools` ·
`haider-verify` (the verification gate) · `haider-accounts` · `haider-rpc` (client API) ·
`haider-tui` · `haider-cli` (the `haider` binary) · `xtask` (repo guards).

## Rules

See CONVENTIONS.md. Highlights: tests live in `tests/` dirs, never inline; the CI
fails any patch that reduces the workspace test count; source files carry a 10k-LOC
soft cap; schema-affecting patches close all lanes until merged.
