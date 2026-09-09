# Initial tool capability profiles

The daemon can start a session with a smaller schema pack. Set
`HAIDER_TOOL_PROFILE` in the daemon environment before starting it:

| Profile | Initial tools, in catalog order |
| --- | --- |
| `coding` (default) | `list_tools`, `todo_write`, `fs_read`, `fs_glob`, `fs_search`, `fs_write`, `fs_edit`, `process_exec`, `spawn_subagent` |
| `inspection` | `list_tools`, `fs_read`, `fs_glob`, `fs_search` |
| `automation` | `list_tools`, `fs_read`, `fs_glob`, `fs_search`, `fs_write`, `fs_edit`, `process_exec` |
| `discovery` | `list_tools` |

Use `automation` for filesystem/process workflows without task tracking or
delegation and `inspection` for browsing a workspace. Unknown or unset profile names retain `coding`.
The selection applies to the daemon's sessions; changing a client's environment
does not reconfigure a resident daemon. Embedded callers can select the typed
`haider_core::ToolCapabilityProfile` through
`DaemonDependencies::with_tool_capability_profile`.

Profiles select presentation, not authorization. Tool names and effect ceilings
are filtered before selecting the initial pack. In particular, inspection does
not prohibit writes: an authorized mutation tool can still be discovered. Use
grants and permission policy to restrict effects. The Android standalone ceiling
and mobile/computer consent gates still apply. Explicit child or lockdown packs
without `list_tools` retain their complete granted schemas so every granted tool
remains reachable.

`HAIDER_TOOL_EXPOSURE` continues to add comma-separated tool names to the initial
profile. `HAIDER_TOOL_EXPOSURE=all` bypasses profile filtering and exposes the
full authorized catalog. Neither setting grants a tool or an effect.

`list_tools` without a filter lists authorized names. A name or keyword filter
describes and promotes up to eight matches after the discovery receipt commits.
Those discoveries survive subsequent requests, turns, compaction, provider
refresh and workspace selection, subject to the current grant; forks retain the
existing new-session consent boundary. Catalog order determines schema order,
regardless of discovery order. The selected pack digest remains part of the
cache boundary. Profiles do not change prompt history or durable results.

Smaller packs save schema tokens on requests before discovery. Later promotions
can reduce that saving and may require additional model requests. Fixed-script
measurements do not predict real-model task success or provider billing.
