# winget manifests

These manifests are ready to place in the Microsoft Windows Package Manager
community repository (`microsoft/winget-pkgs`). This repository does not submit
them automatically: the first version of a package needs a human-reviewed PR.

## First submission (manual, one time)

```powershell
winget install wingetcreate
wingetcreate new https://github.com/Rizzist/haider-agent/releases/download/v0.0.934/haider-v0.0.934-x86_64-pc-windows-msvc.zip
# Use identifier Rizzist.Haider and match the metadata in these manifests.
wingetcreate submit <generated-manifest-dir>
```

Alternatively, copy the three manifests into
`manifests/r/Rizzist/Haider/0.0.934/` in a fork of `microsoft/winget-pkgs` and
open a PR manually. Preserve both nested portable files: `haider.exe` requires
`haiderd.exe` beside it for live mode.

After Microsoft accepts the package, users install it with:

```powershell
winget install --id Rizzist.Haider --exact
```

## Later updates

The release workflow intentionally does not automate winget because the Diff
Forge reference also leaves submission manual. After the identifier exists, an
operator can generate an update with:

```powershell
wingetcreate update Rizzist.Haider --version X.Y.Z `
  --urls https://github.com/Rizzist/haider-agent/releases/download/vX.Y.Z/haider-vX.Y.Z-x86_64-pc-windows-msvc.zip `
  --submit --token <GITHUB_PAT>
```

The Windows binaries are currently unsigned; the manifest pins the release
archive's published SHA-256 checksum.
