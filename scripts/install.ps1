$ErrorActionPreference = 'Stop'

$Repo = 'Rizzist/haider-agent'
$Version = $env:HAIDER_VERSION

if ([string]::IsNullOrWhiteSpace($Version)) {
  try {
    $Releases = Invoke-RestMethod `
      -Uri "https://api.github.com/repos/$Repo/releases?per_page=20" `
      -Headers @{ Accept = 'application/vnd.github+json'; 'User-Agent' = 'HaiderInstaller' }
    $Version = @($Releases) |
      Where-Object { -not $_.draft } |
      Select-Object -First 1 -ExpandProperty tag_name
  } catch {
    throw 'Could not determine latest version; set HAIDER_VERSION=vX.Y.Z'
  }
}

if ([string]::IsNullOrWhiteSpace($Version)) {
  throw 'Could not determine latest version; set HAIDER_VERSION=vX.Y.Z'
}

if ($Version.StartsWith('v')) {
  $Tag = $Version
  $VersionNumber = $Version.Substring(1)
} else {
  $Tag = "v$Version"
  $VersionNumber = $Version
}

if ($env:PROCESSOR_ARCHITECTURE -notin @('AMD64', 'x86_64')) {
  throw "Unsupported Windows architecture: $env:PROCESSOR_ARCHITECTURE"
}

$Target = 'x86_64-pc-windows-msvc'
$Artifact = "haider-$Tag-$Target.zip"
$BaseUrl = "https://github.com/$Repo/releases/download/$Tag"
$Temp = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $Temp | Out-Null

try {
  $Zip = Join-Path $Temp $Artifact
  $Sidecar = Join-Path $Temp "$Artifact.sha256"
  $Headers = @{ 'User-Agent' = 'HaiderInstaller' }
  Invoke-WebRequest -Uri "$BaseUrl/$Artifact" -OutFile $Zip -Headers $Headers
  Invoke-WebRequest -Uri "$BaseUrl/$Artifact.sha256" -OutFile $Sidecar -Headers $Headers

  $Expected = Get-Content $Sidecar | ForEach-Object {
    $Parts = $_.Trim() -split '\s+'
    if ($Parts.Length -eq 1 -and $Parts[0] -match '^[a-fA-F0-9]{64}$') {
      $Parts[0].ToLowerInvariant()
    } elseif ($Parts.Length -ge 2) {
      $Name = ($Parts[1] -replace '^\*', '' -replace '^\./', '')
      if ((Split-Path $Name -Leaf) -eq $Artifact) {
        $Parts[0].ToLowerInvariant()
      }
    }
  } | Select-Object -First 1
  if ([string]::IsNullOrWhiteSpace($Expected)) {
    throw "$Artifact.sha256 did not contain a valid checksum"
  }

  $Actual = (Get-FileHash -Algorithm SHA256 -Path $Zip).Hash.ToLowerInvariant()
  if ($Actual -ne $Expected) {
    throw "Checksum mismatch for $Artifact"
  }

  Expand-Archive -Path $Zip -DestinationPath $Temp -Force
  $BundleDir = Join-Path $Temp "haider-$Tag-$Target"
  $Haider = Join-Path $BundleDir 'haider.exe'
  $Daemon = Join-Path $BundleDir 'haiderd.exe'
  if (!(Test-Path $Haider)) {
    throw 'Archive did not contain haider.exe'
  }
  if (!(Test-Path $Daemon)) {
    throw 'Archive did not contain haiderd.exe'
  }

  if (![string]::IsNullOrWhiteSpace($env:HAIDER_INSTALL_DIR)) {
    $InstallDir = $env:HAIDER_INSTALL_DIR
  } else {
    $InstallDir = Join-Path $env:LOCALAPPDATA 'haider\bin'
  }

  New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
  Copy-Item $Haider (Join-Path $InstallDir 'haider.exe') -Force
  Copy-Item $Daemon (Join-Path $InstallDir 'haiderd.exe') -Force

  $UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  $PathParts = @($UserPath -split ';' | Where-Object { $_ -ne '' })
  if ($PathParts -notcontains $InstallDir) {
    $NewPath = ($PathParts + $InstallDir) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $NewPath, 'User')
    Write-Host "Added $InstallDir to your user PATH. Open a new terminal before running haider."
  }

  Write-Host "Installed haider $VersionNumber and haiderd to $InstallDir"
  Write-Host 'Note: Windows binaries are currently unsigned; the release SHA-256 was verified.'
  Write-Host 'If SmartScreen appears, choose More info, then Run anyway.'
  Write-Host 'Run: haider'
} finally {
  Remove-Item $Temp -Recurse -Force -ErrorAction SilentlyContinue
}
