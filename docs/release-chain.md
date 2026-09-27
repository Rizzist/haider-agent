# Release chain and Android completion gate

## Windows PE import allowlist

The Windows release gate (`python3 scripts/release_packaging.py verify-windows-imports`)
checks every executable and DLL in the release ZIP, directory, or explicit file list.
After normalizing import names, it accepts only the reviewed OS modules in
`WINDOWS_OS_IMPORTS` and the Windows `api-ms-win-core-*` / `ext-ms-win-*` API-set
families. The `api-ms-win-crt-*` API sets and `ucrtbase` stay rejected as
dynamic-CRT indicators, even on Windows versions that provide them. The
runtime-family patterns in the script supply rejection labels only. Any other
import, including a previously unseen third-party runtime, fails by default.

The current three 973 release executables have 31 import entries, covering
`advapi32`, `api-ms-win-core-synch-l1-2-0`, `bcrypt`, `bcryptprimitives`,
`combase`, `crypt32`, `gdi32`, `kernel32`, `ntdll`, `ole32`, `oleaut32`,
`shell32`, `user32`, `userenv`, and `ws2_32`. The remaining names below are a
reviewed Windows module reserve for plausible desktop, networking, security,
device, media, and compatibility imports. A name on this list says only that
the DLL belongs to Windows; a clean native-Windows launch still proves the
target OS image provides it and the required exports.

| Allowlisted module or family | Microsoft documentation used for OS provenance |
| --- | --- |
| `api-ms-win-core-*`, `ext-ms-win-*` | [API-set loader operation](https://learn.microsoft.com/en-us/windows/win32/apiindex/api-set-loader-operation), [Windows API sets](https://learn.microsoft.com/en-us/windows/win32/apiindex/windows-apisets) |
| `ntdll`, `kernel32`, `kernelbase`, `advapi32`, `rpcrt4`, `profapi`, `win32u` | [Windows loader and legacy module imports](https://learn.microsoft.com/en-us/windows/win32/apiindex/api-set-loader-operation), [Windows module inventory example](https://learn.microsoft.com/en-ie/answers/questions/118838/discrepency-between-powershell-dumpbin-when-duping) |
| `user32`, `gdi32`, `dwmapi`, `uxtheme`, `shcore`, `psapi`, `version` | [User32 and Kernel32](https://learn.microsoft.com/en-us/troubleshoot/windows/win32/user32-kernel32-not-initialize), [DWM and UxTheme](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/nf-dwmapi-dwminvalidateiconicbitmaps), [PSAPI](https://learn.microsoft.com/en-us/windows/win32/psapi/process-status-helper), [version information](https://learn.microsoft.com/en-us/windows/win32/menurc/version-information) |
| `shell32`, `shlwapi`, `ole32`, `oleaut32`, `combase` | [Shell DLL versions](https://learn.microsoft.com/en-us/windows/win32/shell/versions), [COM libraries](https://learn.microsoft.com/en-us/windows/win32/com/the-com-library), [Combase API](https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-cocreateinstancefromapp) |
| `bcrypt`, `bcryptprimitives`, `crypt32`, `ncrypt`, `secur32` | [CNG features](https://learn.microsoft.com/en-us/windows/win32/seccng/cng-features), [Windows cryptographic primitives module](https://learn.microsoft.com/en-us/windows/security/security-foundations/certification/fips-140-validation), [Cryptography API](https://learn.microsoft.com/en-us/windows/win32/seccrypto/cryptography-portal), [SSPI](https://learn.microsoft.com/en-us/windows/win32/secauthn/sspi) |
| `ws2_32`, `iphlpapi`, `winhttp`, `dnsapi`, `netapi32`, `wtsapi32` | [Winsock](https://learn.microsoft.com/en-us/windows/win32/winsock/winsock-reference), [IP Helper](https://learn.microsoft.com/en-us/windows/win32/iphlp/ip-helper-start-page), [WinHTTP](https://learn.microsoft.com/en-us/windows/win32/winhttp/about-winhttp), [DNS](https://learn.microsoft.com/en-us/windows/win32/dns/dns-start-page), [Network Management](https://learn.microsoft.com/en-us/windows/win32/netmgmt/network-management), [Remote Desktop Services](https://learn.microsoft.com/en-us/windows/win32/termserv/terminal-services-portal) |
| `userenv`, `powrprof`, `setupapi`, `cfgmgr32` | [User profiles](https://learn.microsoft.com/en-us/windows/win32/api/userenv/), [power management](https://learn.microsoft.com/en-us/windows/win32/power/power-management-portal), [device installation](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/device-and-driver-installation), [Configuration Manager DLL](https://learn.microsoft.com/en-us/windows/win32/api/cfgmgr32/) |
| `msvcrt`, `msvcp_win` | [Windows system module inventory example](https://learn.microsoft.com/en-ie/answers/questions/118838/discrepency-between-powershell-dumpbin-when-duping) |
| `mf`, `mfplat` | [Media Foundation DLLs](https://learn.microsoft.com/en-us/windows/win32/medfound/media-foundation-portal) |

If a new release PE imports an unfamiliar DLL, inspect the exact import and
identify its Windows OS provenance in Microsoft documentation before editing
the allowlist. Add only the specific module (or documented API-set family),
record the citation here, add positive and negative import fixtures for all
five PE import modes, and rerun the Windows packaging and native clean-Windows
gates. A third-party dependency must be removed or statically linked; adding
its spelling to the allowlist would defeat the release rule. The published 972
ZIP and a deliberately dynamic-CRT build are required negative controls.

Every Haider release must include a signed release Android APK and its SHA-256 sidecar.
A desktop release, successful `release` workflow, successful `android-apk` workflow, or
uploaded Actions artifact alone does not establish release completion.

1. Run the required local platform checks and obtain separate Astra SHIP on the candidate.
2. Require `ci`, `xplat-check`, and `ship-gate` success on the exact candidate SHA. The
   pre-tag helpers are `scripts/release/require-evidence.sh` and `find-evidence.py`.
3. The release owner pushes the immutable version tag; never manually dispatch `release`.
4. Follow both tag-triggered workflows on the peeled tag SHA. `android-apk` checkpoints
   native outputs per ABI, then packages/tests/signs. Rerun failed jobs after eviction.
5. `release` requires the signed APK artifact and checksum before publishing. Immediately
   after upload, it queries the release's assets and fails if the exact APK/sidecar pair is
   absent, empty, or not uploaded.
6. Before installation/promotion or declaring complete, the orchestrator must independently
   run the same live post-publication gate and retain stdout, stderr and exit code:

   ```sh
   python3 scripts/release/require-android-asset.py v0.0.971 \
     57cda337d9c3490cf9852c34291836fb87a091b4 --repo Rizzist/haider-agent
   ```

   Substitute the new tag and full peeled SHA for future releases. The script verifies the
   remote tag commit and queries `gh release view --json tagName,isDraft,url,assets`.
   Required names are `haider-vVERSION-android.apk` and the same name plus `.sha256`.
   Any failure means **RELEASE INCOMPLETE**, with no promotion/completion claim. The gate
   is read-only and does not dispatch workflows, upload assets, or mutate tags.
7. Download the APK and sidecar, verify the checksum and APK `versionName`, and retain
   signing-scheme/static validation and build provenance. The asset-presence gate proves
   attachment; it does not substitute for APK/signing or device verification.

For an already published release missing Android assets, preserve the tag and all existing
assets. Prefer the signed Actions artifact from that tag; add only the missing APK/sidecar,
without `--clobber`, then repeat the live gate. A permitted local fallback must disclose
source tag/SHA, builder, date and hashes in release notes/evidence and use the existing key
outside all worktrees. Give the tag checkout its own Cargo target directory: shared targets
can reuse workspace outputs when checkout mtimes predate cached compiles. If a target was
shared, discard the affected target outputs and rebuild before claiming tag provenance. Never inspect, print, copy, export, modify, or replace signing material.
