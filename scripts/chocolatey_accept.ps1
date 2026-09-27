param([Parameter(Mandatory=$true)][string]$PackageVersion)

$ErrorActionPreference = 'Stop'
$ChocolateyRoot = 'C:\ProgramData\chocolatey'
$env:ChocolateyInstall = $ChocolateyRoot
$env:PATH = "$ChocolateyRoot\bin;$env:PATH"
$RuntimeDll = 'C:\Windows\System32\vcruntime140.dll'
if (Test-Path $RuntimeDll) { throw 'FAIL: clean Server Core already contains vcruntime140.dll' }
Write-Output 'PASS: clean Server Core has no vcruntime140.dll before install'

try {
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
  $InstallScript = $null
  for ($Attempt = 1; $Attempt -le 3; $Attempt++) {
    try {
      $InstallScript = (Invoke-WebRequest 'https://community.chocolatey.org/install.ps1' -UseBasicParsing -TimeoutSec 90).Content
      break
    } catch {
      if ($Attempt -eq 3) { throw "Chocolatey bootstrap download failed after 3 attempts: $_" }
      Start-Sleep -Seconds (2 * $Attempt)
    }
  }
  Invoke-Expression $InstallScript
  if (!(Test-Path "$ChocolateyRoot\bin\choco.exe")) { throw 'Chocolatey bootstrap did not install choco.exe' }

  choco install haider --version $PackageVersion --source 'C:\pkg;https://community.chocolatey.org/api/v2/' -y --no-progress
  $InstallExit = $LASTEXITCODE
  if ($InstallExit -notin @(0, 3010)) { throw "FAIL: choco install haider exited $InstallExit" }
  Write-Output "PASS: choco install haider completed with exit $InstallExit (bundle install script returned successfully)"

  $Vcredist = @(choco list --exact vcredist140 --limit-output)
  if ($LASTEXITCODE -ne 0 -or !($Vcredist | Where-Object { $_ -match '^vcredist140\|' })) { throw 'FAIL: vcredist140 is not installed' }
  if (!(Test-Path $RuntimeDll)) { throw 'FAIL: vcredist140 installed but vcruntime140.dll is missing' }
  Write-Output "PASS: vcredist140 installed; $RuntimeDll exists"

  $Haider = @(choco list --exact haider --limit-output)
  if ($LASTEXITCODE -ne 0 -or !($Haider -contains "haider|$PackageVersion")) { throw "FAIL: haider $PackageVersion is not installed" }
  foreach ($Exe in @('haider.exe', 'haider-tui.exe', 'haiderd.exe')) {
    if (!(Test-Path "$ChocolateyRoot\lib\haider\tools\$Exe")) { throw "FAIL: installed tools missing $Exe" }
  }
  Write-Output "PASS: haider $PackageVersion installed; --install-bundle ok; all three installed executables present"

  $Shim = "$ChocolateyRoot\bin\haider.exe"
  if (!(Test-Path $Shim)) { throw 'FAIL: haider Chocolatey shim is missing' }
  $VersionOutput = @(& $Shim --version 2>&1)
  if ($LASTEXITCODE -ne 0 -or !(($VersionOutput -join ' ') -match '0\.0\.972')) { throw "FAIL: haider --version exited $LASTEXITCODE or did not report 0.0.972: $VersionOutput" }
  Write-Output "PASS: haider --version through Chocolatey shim: $($VersionOutput -join ' ')"
} finally {
  $Log = "$ChocolateyRoot\logs\chocolatey.log"
  if (Test-Path $Log) {
    Write-Output 'Relevant Chocolatey log lines:'
    Get-Content $Log | Select-String -Pattern 'haider|vcredist140|install-bundle' | Select-Object -Last 50 | ForEach-Object { Write-Output $_.Line }
  }
}
