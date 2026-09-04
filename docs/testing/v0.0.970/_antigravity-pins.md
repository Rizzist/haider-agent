# Release-owned pin table — Google Antigravity ACP agent 1.1.1

Google's ACP registry entry publishes **no digest and no size** (the registry format defines an
optional `sha256`; Google declined to populate it, though other agents in that registry do). Haider
therefore owns the integrity pin.

**Provenance.** Every digest and size below was measured first-hand on 2026-09-04 by downloading the
archive from the URL shown and hashing the bytes on disk (`shasum -a 256`). Each value was then
cross-checked against an unrelated third party's independently published table (t3code's
`antigravityRelease.ts`, "checked on 2026-09-03") and **all five matched exactly**. Sizes also match
the `Content-Length` returned by a `HEAD` to each URL (`Last-Modified: Wed, 02 Sep 2026`).

Agent version: **1.1.1**. Registry entry: `antigravity-acp`, publisher Google LLC, license
proprietary (`https://antigravity.google/terms`).

| platform key | archive URL | size (bytes) | sha256 |
| --- | --- | --- | --- |
| `darwin-aarch64` | `https://dl.google.com/agy-extensions/releases/macos/agy-acp-server-agy_acp_server_1.1.1-darwin-arm64.zip` | 316014828 | `fdfa915652cdb7ba8085cc8fffed072cbe009251aa2c951aabdda07a8c28a189` |
| `linux-x86_64` | `https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-x86_64.zip` | 681969407 | `38f62d01b32deb0907b3d39a71ec301fd36369f6ffd1cf262d4af385177f79df` |
| `linux-aarch64` | `https://dl.google.com/agy-extensions/releases/linux/agy-acp-server-agy_acp_server_1.1.1-linux-arm64.zip` | 656572786 | `ed69e64b308fcb123ab54bf3277bf9cb0d651064f885ea5aab0ff520c7175398` |
| `windows-x86_64` | `https://dl.google.com/agy-extensions/releases/windows/agy-acp-server-agy_acp_server_1.1.1-windows-x86_64.zip` | 468238392 | `47cb50eef14f0a4655d78cfcfda869bcea7aaee5f9787e936bc2935ea612c3b8` |
| `windows-aarch64` | `https://dl.google.com/agy-extensions/releases/windows/agy-acp-server-agy_acp_server_1.1.1-windows-arm64.zip` | 468521191 | `35f4b1f47ba6a3fea7b0a3e30010df5ea73a64b4f0e7cf991cddc673ddfbcafc` |

There is **no `darwin-x86_64`** (Intel macOS) build. That host must fail with a typed
"unsupported platform" error and must never fall back to another archive.

## Archive contents

Each archive holds exactly TWO flat files at the archive root — no directory prefix:

| entry | role |
| --- | --- |
| `agy_acp_server.par` (`agy_acp_server.exe` on Windows) | the ACP executable |
| `localharness_external` (`localharness_external.exe` on Windows) | sibling helper, must exist before activation |

Verified by listing the darwin-aarch64 archive:

```
802163856  09-02-2026 23:03   agy_acp_server.par
116766704  09-02-2026 23:03   localharness_external
---------                     -------
918930560                     2 files
```

Both are Mach-O 64-bit arm64 executables; the helper ships mode `0555`.

## Launch argv

| platform | argv after the executable |
| --- | --- |
| `darwin-aarch64` | (none) |
| `linux-x86_64`, `linux-aarch64` | `--uid=` (empty value — required, from the registry entry) |
| `windows-x86_64`, `windows-aarch64` | (none) |

## Measured footprint (darwin-aarch64, first-hand)

- Extracted: **906,608 KiB ~= 885 MiB**.
- Cold start from spawn to the `initialize` response: **14.75 s**.
- Child RSS at handshake: **230,176 KiB ~= 225 MiB**.

Linux is roughly 2.0 GB extracted (third-party measurement, not verified in this lane), so a caller
must not assume the macOS footprint on other platforms.

## Transfer caveat

`dl.google.com` gzips these archives in transit. The pinned size and digest describe the ZIP file
itself, so integrity must be checked against the bytes that land on disk **after** transfer decoding.
A comparison against a gzip-encoded body length will mismatch.
