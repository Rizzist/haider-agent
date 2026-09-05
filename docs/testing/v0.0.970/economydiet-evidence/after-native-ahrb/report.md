# AHRB report `ahrb-economy-d8debc17f3383383`

Profile: `/tmp/ahrb-2433bf25-10ffe`

**No badge certified.**

## Harness economy

Reference tokenizer: `o200k_base-style` (`ahrb-o200k-base-style-bpe-v1`, vocabulary SHA-256 `bca069c1ed9a057d9adfd579bcaefeac940a799d27a7fd427eb83eac778fc826`).

Reference tariff: `$10.00` per 1M reference request tokens. Completion means **scripted terminal plus verified scripted workspace effect; not real-task success**; it is not real-task success.

Cost interpretation: run cost; tokens_per_completed_task is available only with a verified scripted workspace effect.

| Harness | Model turns | Total reference tokens (o200k_base-style) | Tool calls / batching factor | Last context reference tokens (o200k_base-style) | Completion | Run reference cost | Tokens / verified-effect completion |
|---|---:|---:|---:|---:|---|---:|---:|
| `haider-agent` | 1 | 18225 | 5 / 0.000000 | 6576 | looped | $0.18225000 | unavailable |

### Scripted workspace-effect evidence

Verified: `false`. verified scripted workspace mutation and read-back; proves the benchmark fixture landed, NOT that a real task was solved.

Expected `economy-output.txt` SHA-256 `b97fe6d2349a0c3d4e49df9916f57fd83ff8473142eabf5b086f0263e8156375`, edit call `economy-edit`, and read-back call `economy-verify`.
Observed `economy-output.txt`: before `absent`, after `absent`, edit success `false` (0 correlated result(s)), read-back path verified `false`, read-back `absent` (0 correlated result(s)).
Workspace receipts: before `e903988c4dfe00f44f302cd0dace2634d2a2d6991d654f1dd44d05a40b539843`, after `dc44662b01596a94c63117b4f473e6c21f8e9e80bf15cf78aacad102fef37253`.

### Advanced economy columns

| cache-eligible fraction (prefix upper bound) | redundant reference tokens | context token curve (reference tokens per primary request) + slope | fixed overhead / turn (reference tokens) | wasted tool calls (deterministic task-proven only) |
|---:|---:|---|---:|---:|
| 0.000000 | 0 | [6576] / 0.000000 reference tokens/request | 6031 | 0 |

Cache note: shared-daemon-sessions can preserve cross-turn process state, but this value remains a serialized-prefix upper bound, not an observed server-cache hit rate. harness-declared cache_control occurrences (separate from cache eligibility): 0 total; per primary request `[0]`.

Curve cross-check (final point equals `last_context_size_tokens`): `true`. Retries: 1 attempts / 11649 reference tokens. physical retries reported separately; retry request tokens remain in MVP totals.

### Cache-adjusted economy

Cache regime: `automatic-prefix`. provider cache regime; 0 cache_control breakpoints under automatic-prefix is expected, NOT no caching.

Cache input discount: `0.90` (90%). STATED assumption: fraction of full input price not paid for a cache-eligible token; NOT a measured provider rate.

Effective reference tokens: `18225.000000`. Effective cost: `$0.18225000`. effective cost assuming automatic prefix caching of the eligible prefix at 90% input discount; UPPER-BOUND proxy from serialized-prefix reuse, NOT a measured server cache-hit rate.

Stable prefix preserved fraction: `0.000000`. Cache busts: `0`. Invalidated prefix: `0` reference tokens; per measured turn `[]`. consecutive primary-request serialized-message-prefix stability (reference-token LCP; ordered invalidation array covers turns N>=2).


## Fingerprint

- Harness: `haider-agent`
- Manifest: `e1cae5238327997acf8791dcfd40ec3601a9f69fcc2fc5d086b5c7409563a11c`
- Workflows: `ee6a69428510881f25fc470c0643f45c347a87bb6499ed590a455405af64366c`
- Platform: `macos-aarch64`
- Profile: `quick`
