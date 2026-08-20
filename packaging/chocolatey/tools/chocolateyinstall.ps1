$ErrorActionPreference = 'Stop'

$version = '0.0.934'
$packageName = 'haider'
$url64 = 'https://github.com/Rizzist/haider-agent/releases/download/v0.0.934/haider-v0.0.934-x86_64-pc-windows-msvc.zip'
$checksum64 = '83dd105968ceb1fe9675a7de5bfc8b4edbb8d34a62d246c49361e098a93e5ca0'
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
