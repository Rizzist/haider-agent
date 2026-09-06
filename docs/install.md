# Installing Haider

Existing Homebrew, Scoop, Chocolatey, npm and `scripts/install.sh` installation
paths continue to work. Native installers are additional release downloads;
choose one installation method for a given directory.

## Native installers (v0.0.970 release lane)

Download the asset for your architecture and its adjacent `.sha256` from the
[release page](https://github.com/Rizzist/haider-agent/releases). Verify that
checksum before installation. Each installer contains the exact binaries from
the corresponding release archive, including optional sibling executables.
The current wave still reports version 0.0.969; names follow the actual tagged
workspace version, rather than hard-coding 970.

| Platform | Download | Install | Remove |
| --- | --- | --- | --- |
| Windows x64 | `haider-vVERSION-x86_64-pc-windows-msvc-setup.exe` | Run the EXE; default `%LOCALAPPDATA%\Programs\Haider`, added to your user PATH | Settings → Apps → Haider → Uninstall; removes files, owned PATH token and installer registry keys |
| macOS ARM64 / Intel | `haider-vVERSION-TARGET.dmg` (also standalone `.pkg`) | Open DMG and run PKG; installs `/usr/local/bin/haider`, `haiderd`, optional siblings and `uninstall-haider.sh` | `sudo /usr/local/bin/uninstall-haider.sh` |
| Linux ARM64 / x64 | `haider-vVERSION-TARGET.deb` | `sudo apt install ./haider-vVERSION-TARGET.deb`; binaries in `/usr/bin` | `sudo apt remove haider` or `sudo apt purge haider` |
| Linux ARM64 / x64 | `haider-vVERSION-TARGET.rpm` | `sudo dnf install ./haider-vVERSION-TARGET.rpm`; binaries in `/usr/bin` | `sudo dnf remove haider` |

Before upgrading or removing, run `haider daemon stop` for each active profile
and close Haider clients. Lifecycle QA covers stopped applications; the package
managers do not inspect or stop users' profile daemons. Open a new terminal after
Windows installation/removal to observe PATH changes.

Windows interactive uninstall asks before deleting `%USERPROFILE%\.haider`;
silent uninstall preserves it. The macOS script removes the installer receipt,
manifest and supported Haider launch agents (`ai.haidercode.haider` and
`ai.haidercode.haiderd`), and asks before deleting the invoking user's `~/.haider`.
The default answer is No. `--keep-state` and noninteractive input preserve state.
With sudo, the script resolves the original user's home. It refuses changed or
symlinked binaries before deleting any member.

Linux packages contain no user-state lifecycle hooks or conffiles. Both remove
and purge remove package-owned files only; `~/.haider` is never touched or
prompted about. RPM has erase rather than a separate purge operation.

For a manual tarball installation, download the matching
`haider-vVERSION-TARGET-uninstall-haider.sh` and its checksum, then run:

```sh
sh ./haider-vVERSION-TARGET-uninstall-haider.sh --prefix /absolute/install/bin
# To keep state without prompting:
sh ./haider-vVERSION-TARGET-uninstall-haider.sh --prefix /absolute/install/bin --keep-state
```

Use the script from the same release as the installed binaries. It removes only
that release's members and an installed `uninstall-haider.sh`; unrelated files
remain. Package-manager installations should be removed with their manager.

## Signing configuration for release owners

Absent signing credentials emit an explicit `SKIP` line and produce unsigned
assets. Configured but invalid signing or notarization fails the installer job.
SHA-256 sidecars are generated after signing/notarization and post-pack checks.

- Windows: set `WINDOWS_SIGNING_PFX_BASE64` to the base64 PFX containing a code
  signing certificate/private key, and `WINDOWS_SIGNING_PFX_PASSWORD` to its
  password. The Windows SDK SignTool signs the setup EXE and embedded
  uninstaller, with SHA-256 and a DigiCert timestamp. Payload binaries retain
  their archive bytes. No Azure signing account is required for this PFX path.
- macOS binaries: existing `MACOS_SIGNING_CERT_P12_BASE64` and
  `MACOS_SIGNING_CERT_PASSWORD` provide Developer ID Application signing.
- macOS PKG: supply `MACOS_INSTALLER_CERT_P12_BASE64` and
  `MACOS_INSTALLER_CERT_PASSWORD` containing a **Developer ID Installer** identity,
  or include that identity in the existing P12 bundle. Application and Installer
  are different certificate types; an Application-only bundle logs SKIP for PKG.
- Notarization: existing `APPLE_ID`, `APPLE_PASSWORD` (app-specific password),
  and `APPLE_TEAM_ID`. The signed PKG is notarized/stapled before embedding;
  the DMG is then notarized/stapled. Review the job's signing status before
  describing a release as signed or notarized.

See [installer evidence and QA commands](testing/v0.0.970/installers.md) for
what was executed locally and what requires the platform CI runners.
