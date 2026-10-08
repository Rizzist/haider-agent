# Verify the exact shipped Windows ZIP on a Server Core image without VC++ redist.
param([Parameter(Mandatory = $true)][string]$Artifact)

$ErrorActionPreference = 'Stop'
$Artifact = (Resolve-Path -LiteralPath $Artifact).Path
$Sidecar = "$Artifact.sha256"
if (!(Test-Path -LiteralPath $Sidecar -PathType Leaf)) {
  throw "missing checksum sidecar: $Sidecar"
}
$Name = [IO.Path]::GetFileName($Artifact)
if (!$Name.EndsWith('.zip', [StringComparison]::OrdinalIgnoreCase)) {
  throw "expected a Windows ZIP: $Artifact"
}
$Checksum = (Get-Content -LiteralPath $Sidecar -Raw).Trim()
$Match = [regex]::Match($Checksum, '^([0-9a-fA-F]{64})\s+\*?([^\s]+)$')
if (!$Match.Success -or $Match.Groups[2].Value -cne $Name) {
  throw "checksum sidecar does not name the exact ZIP: $Sidecar"
}
$ActualHash = (Get-FileHash -LiteralPath $Artifact -Algorithm SHA256).Hash
if ($ActualHash -ine $Match.Groups[1].Value) {
  throw "Windows ZIP checksum mismatch: $Artifact"
}
Write-Host "verified ZIP SHA-256 $ActualHash $Name"

$Manifest = Get-Content -LiteralPath 'Cargo.toml' -Raw
$VersionMatch = [regex]::Match($Manifest, '(?m)^version = "([0-9]+\.[0-9]+\.[0-9]+)"\r?$')
if (!$VersionMatch.Success) { throw 'workspace version is absent from Cargo.toml' }
$Version = $VersionMatch.Groups[1].Value
if ($env:GITHUB_REF -like 'refs/tags/*' -and $env:GITHUB_REF_NAME -cne "v$Version") {
  throw "tag $env:GITHUB_REF_NAME does not match workspace version $Version"
}

$Build = [Environment]::OSVersion.Version.Build
$Tag = switch ($Build) {
  26100 { 'ltsc2025' }
  20348 { 'ltsc2022' }
  default { throw "no matching Server Core tag for host build $Build" }
}
$Image = "mcr.microsoft.com/windows/servercore:$Tag"
$Stage = Join-Path $env:RUNNER_TEMP ("haider-clean-" + [guid]::NewGuid().ToString('N'))
try {
  New-Item -ItemType Directory -Path $Stage | Out-Null
  Expand-Archive -LiteralPath $Artifact -DestinationPath $Stage
  $BundleName = [IO.Path]::GetFileNameWithoutExtension($Name)
  $Bundle = Join-Path $Stage $BundleName
  foreach ($Exe in 'haider.exe', 'haider-tui.exe', 'haiderd.exe') {
    if (!(Test-Path -LiteralPath (Join-Path $Bundle $Exe) -PathType Leaf)) {
      throw "Windows ZIP is missing $Exe"
    }
  }

  docker pull --quiet $Image
  if ($LASTEXITCODE -ne 0) { throw "docker pull $Image failed, rc=$LASTEXITCODE" }
  $ImageDigest = docker image inspect --format '{{index .RepoDigests 0}}' $Image
  if ($LASTEXITCODE -ne 0 -or !$ImageDigest) { throw "cannot identify pulled image $Image" }
  Write-Host "clean launch image $ImageDigest; host build $Build"

  # Windows PowerShell is available in Server Core. EncodedCommand avoids
  # host/container quoting differences while executing all shipped siblings.
  $Probe = @'
$ErrorActionPreference = 'Stop'
if (Test-Path 'C:\Windows\System32\vcruntime140.dll') {
  throw 'Server Core image has the VC++ redistributable installed'
}
Write-Host "Server Core OS version $([Environment]::OSVersion.Version)"
$Version = '__VERSION__'
foreach ($Exe in 'haider.exe', 'haider-tui.exe', 'haiderd.exe') {
  $Output = & ("C:\h\" + $Exe) --version 2>&1
  if ($LASTEXITCODE -ne 0) {
    throw "$Exe --version failed on clean Server Core, rc=$LASTEXITCODE"
  }
  $Expected = ([IO.Path]::GetFileNameWithoutExtension($Exe)) + ' ' + $Version
  $Actual = (@($Output) -join "`n").Trim()
  if ($Actual -cne $Expected) {
    throw "$Exe --version mismatch: expected '$Expected', got '$Actual'"
  }
  Write-Host "clean launch passed: $Exe $Actual"
}
'@
  $Probe = $Probe.Replace('__VERSION__', $Version)
  $Encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($Probe))
  docker run --rm --isolation process -v "${Bundle}:C:\h:ro" $Image `
    powershell.exe -NoProfile -NonInteractive -EncodedCommand $Encoded
  if ($LASTEXITCODE -ne 0) {
    throw "Windows release ZIP clean launch failed, rc=$LASTEXITCODE (-1073741515 = STATUS_DLL_NOT_FOUND)"
  }
} finally {
  if (Test-Path -LiteralPath $Stage) { Remove-Item -LiteralPath $Stage -Recurse -Force }
}
