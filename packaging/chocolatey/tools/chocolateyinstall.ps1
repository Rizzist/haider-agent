$ErrorActionPreference = 'Stop'

$version = '__HAIDER_VERSION__'
$packageName = 'haider'
$url64 = '__HAIDER_WINDOWS_X64_URL__'
$checksum64 = '__HAIDER_WINDOWS_X64_SHA256__'
$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$bundleDir = Join-Path $toolsDir "haider-v$version-x86_64-pc-windows-msvc-split"

Install-ChocolateyZipPackage `
  -PackageName $packageName `
  -Url64bit $url64 `
  -UnzipLocation $toolsDir `
  -Checksum64 $checksum64 `
  -ChecksumType64 'sha256'

foreach ($binary in @('haider-tui.exe', 'haiderd.exe', 'haider.exe')) {
  $source = Join-Path $bundleDir $binary
  if (!(Test-Path $source)) {
    throw "Release archive did not contain $binary"
  }
}

# Share the executable transaction, including exact sibling version checks,
# with install.ps1 and self-update. Chocolatey creates the public shim later.
& (Join-Path $bundleDir 'haider.exe') --install-bundle $bundleDir $toolsDir
if ($LASTEXITCODE -ne 0) {
  throw "Bundle installation failed with exit code $LASTEXITCODE"
}

# Only the public entrypoint receives a Chocolatey shim.
foreach ($binary in @('haider-tui.exe', 'haiderd.exe')) {
  New-Item -ItemType File -Path (Join-Path $toolsDir "$binary.ignore") -Force | Out-Null
}

Remove-Item $bundleDir -Recurse -Force
