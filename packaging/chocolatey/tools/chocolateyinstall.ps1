$ErrorActionPreference = 'Stop'

$version = '__HAIDER_VERSION__'
$packageName = 'haider'
$url64 = '__HAIDER_WINDOWS_X64_URL__'
$checksum64 = '__HAIDER_WINDOWS_X64_SHA256__'
$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$bundleDir = Join-Path $toolsDir "haider-v$version-x86_64-pc-windows-msvc"

Install-ChocolateyZipPackage `
  -PackageName $packageName `
  -Url64bit $url64 `
  -UnzipLocation $toolsDir `
  -Checksum64 $checksum64 `
  -ChecksumType64 'sha256'

foreach ($binary in @('haider.exe', 'haiderd.exe')) {
  $source = Join-Path $bundleDir $binary
  if (!(Test-Path $source)) {
    throw "Release archive did not contain $binary"
  }
  Move-Item -Path $source -Destination (Join-Path $toolsDir $binary) -Force
}

Remove-Item $bundleDir -Recurse -Force
