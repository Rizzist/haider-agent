# Shell registry v1

`shell_registry_v1` is one daemon-owned table of terminal lifecycles. It
unifies the local `!` path, tool/background process shells, and SSH shell
channels. It does not wrap, replace, or alter the monitor primitive, and it
does not change the subagent UI.

Each row is:

```text
Shell {
  id,
  kind: local | ssh { profile },
  status: starting | running | exited { code? } | closed,
  title,
  cwd_or_host,
  created_at_ms,
  last_activity_ms,
  bytes_out
}
```

Opening registers `starting`, successful spawn/channel setup moves to
`running`, natural completion moves to `exited`, and `shell.close { id }`
moves to `closed`. Close is idempotent. For a local process it signals the
existing supervised termination ladder. For SSH it closes that one channel,
not the profile's authenticated session.

The RPC methods are `shell.list` and `shell.close`. Additive Pipe frames are
`shell.opened`, `shell.state`, and `shell.closed`; each carries the complete
public row, so clients upsert by `id`. The client SDK exposes list, close, and
typed subscription helpers only when the feature bit is present. Feature
absence removes the surface rather than producing a synthetic error.

The existing TUI bottom status strip renders independent, count-pluralized
segments (`1 shell`, `3 shells`, `1 monitor`, …), omitting zero counts. The
shell segment opens the `/shells` activity overlay; the monitor segment routes
to the existing monitor detail surface. No new top bar is introduced.

The CLI JSON list envelope is stable as `haider.shell.list.v1`.
