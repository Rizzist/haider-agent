# AHRB report `ahrb-economy-b6e7258c4f013f66`

Profile: `/tmp/ahrb-f7a4ccb2-3115`

**No badge certified.**

## Harness economy

Reference tokenizer: `o200k_base-style` (`ahrb-o200k-base-style-bpe-v1`, vocabulary SHA-256 `bca069c1ed9a057d9adfd579bcaefeac940a799d27a7fd427eb83eac778fc826`).

Reference tariff: `$10.00` per 1M reference request tokens. Completion means **scripted terminal plus verified scripted workspace effect; not real-task success**; it is not real-task success.

Cost interpretation: run cost; tokens_per_completed_task is available only with a verified scripted workspace effect.

| Harness | Model turns | Total reference tokens (o200k_base-style) | Tool calls / batching factor | Last context reference tokens (o200k_base-style) | Completion | Run reference cost | Tokens / verified-effect completion |
|---|---:|---:|---:|---:|---|---:|---:|
| `haider-agent` | 8 | 221308 | 17 / 2.428571 | 35040 | terminal-without-effect | $2.21308000 | unavailable |

### Scripted workspace-effect evidence

Verified: `false`. verified scripted workspace mutation and read-back; proves the benchmark fixture landed, NOT that a real task was solved.

Expected `economy-output.txt` SHA-256 `b97fe6d2349a0c3d4e49df9916f57fd83ff8473142eabf5b086f0263e8156375`, edit call `economy-edit`, and read-back call `economy-verify`.
Observed `economy-output.txt`: before `absent`, after `b97fe6d2349a0c3d4e49df9916f57fd83ff8473142eabf5b086f0263e8156375`, edit success `false` (1 correlated result(s)), read-back path verified `false`, read-back `b97fe6d2349a0c3d4e49df9916f57fd83ff8473142eabf5b086f0263e8156375` (1 correlated result(s)).
Workspace receipts: before `e903988c4dfe00f44f302cd0dace2634d2a2d6991d654f1dd44d05a40b539843`, after `2c71c8846a183ce285a3dad3d9734890acd5ceef12a2c305acfa36d4ee1dd7eb`.

### Advanced economy columns

| cache-eligible fraction (prefix upper bound) | redundant reference tokens | context token curve (reference tokens per primary request) + slope | fixed overhead / turn (reference tokens) | wasted tool calls (deterministic task-proven only) |
|---:|---:|---|---:|---:|
| 0.833165 | 146624 | [6575, 22345, 27748, 30929, 31929, 32813, 33929, 35040] / 2103.166667 reference tokens/request | 6031 | 0 |

Cache note: shared-daemon-sessions can preserve cross-turn process state, but this value remains a serialized-prefix upper bound, not an observed server-cache hit rate. harness-declared cache_control occurrences (separate from cache eligibility): 0 total; per primary request `[0, 0, 0, 0, 0, 0, 0, 0]`.

Curve cross-check (final point equals `last_context_size_tokens`): `true`. Retries: 0 attempts / 0 reference tokens. physical retries reported separately; retry request tokens remain in MVP totals.

### Cache-adjusted economy

Cache regime: `automatic-prefix`. provider cache regime; 0 cache_control breakpoints under automatic-prefix is expected, NOT no caching.

Cache input discount: `0.90` (90%). STATED assumption: fraction of full input price not paid for a cache-eligible token; NOT a measured provider rate.

Effective reference tokens: `55360.479838`. Effective cost: `$0.55360480`. effective cost assuming automatic prefix caching of the eligible prefix at 90% input discount; UPPER-BOUND proxy from serialized-prefix reuse, NOT a measured server cache-hit rate.

Stable prefix preserved fraction: `1.000000`. Cache busts: `0`. Invalidated prefix: `0` reference tokens; per measured turn `[0, 0, 0, 0, 0, 0, 0]`. consecutive primary-request serialized-message-prefix stability (reference-token LCP; ordered invalidation array covers turns N>=2).


## Fingerprint

- Harness: `haider-agent`
- Manifest: `b390dd1e79812ce2088dfd3f935ec7d5c1ce26678f19f15c0d268e39b05543aa`
- Workflows: `50d7011207124d9d83015b29f4ddb2f64710385269ea2819edf150e350816cd8`
- Platform: `macos-aarch64`
- Profile: `quick`
