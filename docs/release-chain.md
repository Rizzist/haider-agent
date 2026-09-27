# Release chain and Android completion gate

## Windows PE import allowlist

The Windows release gate (`python3 scripts/release_packaging.py verify-windows-imports`)
checks every executable and DLL in the release ZIP, directory, or explicit file list.
After normalizing bare import names, it accepts only the reviewed OS modules in
`WINDOWS_OS_IMPORTS` and the Windows `api-ms-win-core-*` / `ext-ms-win-*` API-set
families. The `api-ms-win-crt-*` API sets and `ucrtbase` stay rejected as
dynamic-CRT indicators, even on Windows versions that provide them. The
runtime-family patterns in the script supply rejection labels only. Any other
import, including a previously unseen third-party runtime, fails by default.
Path-qualified imports (drive-relative, drive-absolute, UNC, or containing a
slash) and every colon-qualified stream name also fail: PE import names must
be bare module names, including where a filename spells the default stream
as `name.dll::$DATA`.

The current three 973 release executables have 31 import entries, covering
`advapi32`, `api-ms-win-core-synch-l1-2-0`, `bcrypt`, `bcryptprimitives`,
`combase`, `crypt32`, `gdi32`, `kernel32`, `ntdll`, `ole32`, `oleaut32`,
`shell32`, `user32`, `userenv`, and `ws2_32`. The remaining names below are a
reviewed Windows module reserve for plausible desktop, networking, security,
device, and compatibility imports. A name on this list establishes OS
provenance, not availability on every Windows image. Reserve entries absent
from the current release imports remain unverified on Server Core. The
required release and installer-rehearsal jobs verify the final Windows ZIP's
checksum, extract it into a fresh directory, and launch all three shipped
executables with exact version checks in a matching Server Core container
without the VC++ redistributable. That catches a future ordinary startup
import missing from that image. Delay-loaded imports need execution of their
relevant paths. The allowlist permits both named API-set families; any given
contract or export may still be unavailable on a particular image.

| Allowlisted module or family | Microsoft documentation used for OS provenance |
| --- | --- |
| `api-ms-win-core-*`, `ext-ms-win-*` | [Windows API sets](https://learn.microsoft.com/en-us/windows/win32/apiindex/windows-apisets), [Win32 APIs present on all Windows devices](https://learn.microsoft.com/en-us/uwp/win32-and-com/win32-apis) |
| `ntdll` | [RtlGetVersion](https://learn.microsoft.com/en-us/windows/win32/devnotes/rtlgetversion) |
| `kernel32` | [CreateFileW](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew) |
| `kernelbase` | [GetCommPorts](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getcommports) |
| `advapi32` | [RegOpenKeyExW](https://learn.microsoft.com/en-us/windows/win32/api/winreg/nf-winreg-regopenkeyexw) |
| `rpcrt4` | [UuidCreate](https://learn.microsoft.com/en-us/windows/win32/api/rpcdce/nf-rpcdce-uuidcreate) |
| `user32` | [MessageBoxW](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-messageboxw) |
| `gdi32` | [CreateDCW](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/nf-wingdi-createdcw) |
| `dwmapi` | [DwmInvalidateIconicBitmaps](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/nf-dwmapi-dwminvalidateiconicbitmaps) |
| `uxtheme` | [OpenThemeData](https://learn.microsoft.com/en-us/windows/win32/api/uxtheme/nf-uxtheme-openthemedata) |
| `shcore` | [GetDpiForMonitor](https://learn.microsoft.com/en-us/windows/win32/api/shellscalingapi/nf-shellscalingapi-getdpiformonitor) |
| `psapi` | [EnumProcesses](https://learn.microsoft.com/en-us/windows/win32/api/psapi/nf-psapi-enumprocesses) |
| `version` | [GetFileVersionInfoByHandle](https://learn.microsoft.com/en-us/windows/win32/menurc/getfileversioninfobyhandle) |
| `comctl32` | [InitCommonControlsEx](https://learn.microsoft.com/en-us/windows/win32/api/commctrl/nf-commctrl-initcommoncontrolsex) |
| `shell32` | [ShellExecuteW](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shellexecutew) |
| `shlwapi` | [PathCombineW](https://learn.microsoft.com/en-us/windows/win32/api/shlwapi/nf-shlwapi-pathcombinew) |
| `ole32` | [CoInitializeEx](https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-coinitializeex) |
| `oleaut32` | [SysAllocString](https://learn.microsoft.com/en-us/windows/win32/api/oleauto/nf-oleauto-sysallocstring) |
| `combase` | [CoCreateInstanceFromApp](https://learn.microsoft.com/en-us/windows/win32/api/combaseapi/nf-combaseapi-cocreateinstancefromapp) |
| `bcrypt` | [BCryptOpenAlgorithmProvider](https://learn.microsoft.com/en-us/windows/win32/api/bcrypt/nf-bcrypt-bcryptopenalgorithmprovider) |
| `bcryptprimitives` | [ProcessPrng](https://learn.microsoft.com/en-us/windows/win32/seccng/processprng) |
| `crypt32` | [CertOpenStore](https://learn.microsoft.com/en-us/windows/win32/api/wincrypt/nf-wincrypt-certopenstore) |
| `ncrypt` | [NCryptOpenStorageProvider](https://learn.microsoft.com/en-us/windows/win32/api/ncrypt/nf-ncrypt-ncryptopenstorageprovider) |
| `secur32` | [AcquireCredentialsHandleW](https://learn.microsoft.com/en-us/windows/win32/api/sspi/nf-sspi-acquirecredentialshandlew) |
| `ws2_32` | [WSAStartup](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsastartup) |
| `iphlpapi` | [NotifyAddrChange](https://learn.microsoft.com/en-us/windows/win32/api/iphlpapi/nf-iphlpapi-notifyaddrchange) |
| `winhttp` | [WinHttpOpen](https://learn.microsoft.com/en-us/windows/win32/api/winhttp/nf-winhttp-winhttpopen) |
| `dnsapi` | [DnsQuery_W](https://learn.microsoft.com/en-us/windows/win32/api/windns/nf-windns-dnsquery_w) |
| `netapi32` | [NetUserAdd](https://learn.microsoft.com/en-us/windows/win32/api/lmaccess/nf-lmaccess-netuseradd) |
| `wtsapi32` | [WTSVirtualChannelOpen](https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsvirtualchannelopen) |
| `userenv` | [GetUserProfileDirectoryW](https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-getuserprofiledirectoryw) |
| `powrprof` | [GetActivePwrScheme](https://learn.microsoft.com/en-us/windows/win32/api/powrprof/nf-powrprof-getactivepwrscheme) |
| `setupapi` | [SetupDiGetClassDevsW](https://learn.microsoft.com/en-us/windows/win32/api/setupapi/nf-setupapi-setupdigetclassdevsw) |
| `cfgmgr32` | [CM_Reenumerate_DevNode](https://learn.microsoft.com/en-us/windows/win32/api/cfgmgr32/nf-cfgmgr32-cm_reenumerate_devnode) |

Microsoft [documents `msvcrt.dll` as the legacy Windows CRT](https://learn.microsoft.com/en-us/windows-hardware/drivers/develop/using-the-microsoft-c-runtime-with-user-mode-drivers-and-apps),
but it has no cited API Requirements table here and is not imported by this release;
it remains outside the allowlist.

`comctl32.dll` appears in the historical Inno Setup stub corpus; it is not an
import of the three current release executables.

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
