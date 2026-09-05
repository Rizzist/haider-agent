# SSH profiles v1

`ssh_profiles_v1` is Haider's profile-scoped registry of named remote
machines. A saved profile is reusable by CLI, TUI, SDK, and model tools without
placing a credential in a prompt or asking for it again each session.

The existing cross-platform `FileVault` writes each replacement through a new
same-directory temporary file, syncs the file, atomically renames it, and uses
the platform's directory-sync boundary. On Unix it restricts a newly created
vault root to mode `0700` and creates temporary files with mode `0600`. On
Windows those Unix-mode helpers do not install an owner-only ACL; FileVault
inherits the enclosing profile directory's ACL and the owner's Windows
account. `FileVault` does not cryptographically encrypt file
contents, derive an encryption key, or add a Windows ACL layer. SSH passwords,
pasted key material, key-file passphrases, and API keys therefore receive
exactly the same FileVault protection on each platform: no worse and no better.
OS account, enclosing-directory ACL, and disk encryption remain part of the
at-rest threat model. A master-secret or vault-encryption design is separate
from SSH profiles v1 and is not implied by this feature.

The recommended and default authentication choices are a key file by local
path (the private-key bytes never enter the vault) or `ssh-agent`. Passwords
and pasted private keys remain supported. `haider ssh add --key-stdin` prints
a one-line notice before staging bytes that they receive the same FileVault
protection as API keys, that Windows relies on the enclosing profile ACL, and
that Haider does not cryptographically encrypt them.

## Stored schema

Names are mandatory, unique, and match `[a-z0-9._-]{1,32}`. A profile contains
an optional description and this target:

```text
host, port=22, user
auth = key_file | key_material | agent | password
default_cwd?
host_key?
```

`key_file` stores a local path and may refer to a separately vaulted
passphrase. `key_material` and `password` store only a generated vault alias;
the corresponding bytes are separate FileVault records. Public
`ssh.list`, `ssh.show`, CLI JSON, SDK, and model-tool projections omit
authentication entirely. They cannot represent an authentication kind, path,
vault alias, staged reference, password, private key, or passphrase.

Secret input uses the existing authenticated owner-scoped, connection-local
`vault.stage` mechanism (same-UID UDS on Unix and the equivalent owner gate on
Windows) with the `ssh_key_material` or `ssh_password` purpose. A staged
reference is random, expiring, purpose-bound, and single-use. Successful
profile creation moves the bytes into a uniquely named vault record. Failed
creation removes that record; credential replacement retires the prior record.

## Session scope

SSH profile grouping is a session property, not a project concept:

- `all` is the default;
- `allow { names }` exposes only those names; and
- `none` exposes no profile.

`haider run --ssh-profiles all|none|a,b` sets the creation field. The same
field is available on `session.create`; `session.set_ssh_scope` and
`/ssh scope …` adjust it. Narrowed scope is persisted in the profile-scoped
vault before the in-memory cache is published, so daemon restart cannot widen
it to `all`.

Both `ssh_list` and `ssh_shell` enforce scope in the daemon. `ssh_list` omits
out-of-scope rows, so the model cannot learn their names. `ssh_shell` checks
scope before opening a permission request or a connection and returns the
typed `ssh_profile_out_of_scope` refusal.

## Connection and host-key security

The backend is `russh` on every platform. Haider never invokes a system `ssh`
program, never uses `SSH_ASKPASS`, and never puts credentials in argv or the
environment. The daemon owns one authenticated client session per profile and
opens a channel for each command or interactive terminal. That channel model
is the same multiplexing mechanism on every OS. A dropped session reconnects
on the next operation; an idle session disconnects after ten minutes.

Server-key verification is native in the `russh::client::Handler`. The first
successful handshake records the SHA-256 fingerprint (TOFU) before the
connection is made available. Every later handshake compares against it. A
mismatch returns `ssh_host_key_changed { expected, actual }`; there is no
interactive acceptance prompt. Editing the host or port clears the prior pin
because it names a different endpoint.

Remote execution is its own permission effect and defaults to **Ask** in an
interactive session. Autonomous sessions resolve that Ask to ordinary
journaled Allow; `--read-only` explicitly denies it because the remote target
could alias or mutate the workspace. The permission copy names the remote
machine and says its output is untrusted input. Deadlines are bounded by the
enclosing run; stdout/stderr use the same bounded capture rules as the local
process tool. Journal command and result records remain raw; prompt-only
compaction never rewrites receipts.

## Platform matrix

| Capability | Android | Linux | Windows | macOS |
|---|---:|---:|---:|---:|
| Password | yes | yes | yes | yes |
| Key material | yes | yes | yes | yes |
| OpenSSH/PEM key file | yes | yes | yes | yes |
| SSH agent | `ssh_agent_unavailable` | `SSH_AUTH_SOCK` | OpenSSH named pipe | `SSH_AUTH_SOCK` |
| Command channels | yes | yes | yes | yes |
| russh PTY capability | yes | yes | yes | yes |
| v1 interactive PTY transport | yes | yes | yes | yes |
| Multiplexing | russh channels | russh channels | russh channels | russh channels |

The workspace dependency selects `russh`'s `ring` and `rsa` features. `ring`
0.17 is already present in the workspace dependency graph and already builds
in the Android and Windows target matrix; this avoids introducing the AWS-LC
native build toolchain. The `rsa` feature enables pure-Rust RSA key support.
In `russh` 0.63 the key decoder and agent client that older releases exposed
as a separate `russh-keys` crate are re-exported under `russh::keys`, so v1
uses that single, version-aligned dependency rather than adding a second key
crate.

## Deliberate v1 limits

There are no projects/groups, jump hosts, tunnels, or SFTP. `proxy_jump` is
not accepted or stored. Haider does not use OpenSSH `ControlMaster` or
`SSH_ASKPASS`; multiplexing and authentication remain inside `russh`. Closing
an SSH shell closes only that channel and leaves the reusable authenticated
profile session alive.

`haider ssh shell <name>` and `/ssh shell <name>` open a session channel,
request a PTY using the client's `TERM` and current pane dimensions, request a
shell, and then forward terminal bytes and window changes. Output is a
transient event delivered only to the connection that opened the PTY and is
not retained in the registry row; only its byte count is retained. Bounded
fanout or connection-outbox refusal closes the affected transport and PTY
instead of silently dropping terminal bytes. Natural
remote completion records `exited { code? }`. EOF, explicit close, or loss of
the opening client closes only that channel. Human terminals have no model-run
deadline. The one-shot `-- cmd…` form stays bounded by its explicit or
enclosing deadline.

The stable CLI JSON schemas are `haider.ssh.list.v1` and
`haider.ssh.profile.v1`. Their profile values are the public projection above.
