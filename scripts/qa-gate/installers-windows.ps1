param(
    [Parameter(Mandatory)][string]$Installer,
    [Parameter(Mandatory)][string]$Manifest,
    [string]$UpgradeInstaller = '',
    [string]$InitialVersion = ''
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (!$IsWindows -or $env:CI -ne 'true' -or $env:RUNNER_OS -ne 'Windows') { throw 'This lifecycle gate must run on a disposable Windows CI runner' }
Write-Host "PHASE lifecycle preflight: PowerShell=$($PSVersionTable.PSVersion) CI=$env:CI RUNNER_OS=$env:RUNNER_OS"
$Installer = (Resolve-Path -LiteralPath $Installer).Path
if (!$UpgradeInstaller) { $UpgradeInstaller = $Installer }
$UpgradeInstaller = (Resolve-Path -LiteralPath $UpgradeInstaller).Path
$Manifest = (Resolve-Path -LiteralPath $Manifest).Path
$metadata = Get-Content -LiteralPath $Manifest -Raw | ConvertFrom-Json
if (!$InitialVersion) { $InitialVersion = $metadata.version }
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\Haider'
$ownerKey = 'HKCU:\Software\HaiderInstaller'
# AppId=HaiderHarness plus Inno's documented _is1 suffix:
# https://jrsoftware.org/ishelp/topic_setup_appid.htm
$arpKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\HaiderHarness_is1'
if ((Test-Path -LiteralPath $installDir) -or (Test-Path -LiteralPath $ownerKey) -or (Test-Path -LiteralPath $arpKey)) { throw 'Refusing to overwrite an existing Haider installation' }
function Get-UserPathSnapshot {
    $environment = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try {
        $value = $null
        $kind = $null
        if ($null -ne $environment) {
            $value = $environment.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            if ($null -ne $value) { $kind = $environment.GetValueKind('Path') }
        }
        # A missing key/value and an existing empty string must stay distinct.
        [pscustomobject]@{ Value = $value; Kind = $kind }
    } finally {
        if ($null -ne $environment) { $environment.Dispose() }
    }
}
$beforePath = Get-UserPathSnapshot
$state = Join-Path $env:USERPROFILE '.haider'
if (Test-Path -LiteralPath $state) { throw 'This test requires a clean profile so its state fixture cannot mix with real data' }
$logDir = Join-Path ([IO.Path]::GetTempPath()) ('haider-installer-qa-' + [guid]::NewGuid())
New-Item -ItemType Directory $logDir | Out-Null
Write-Host "Lifecycle native logs (retained): $logDir"
$sentinel = Join-Path $state 'installer-qa-preserve.txt'
function Run-Setup([string]$Executable, [string]$Phase) {
    $log = Join-Path $logDir "$Phase.log"
    # Start-Process joins ArgumentList; use one string with explicit quotes for
    # the log path. FilePath is separate and safely accepts an executable path
    # with spaces. -Wait includes the loader/uninstaller's descendant processes.
    $arguments = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /LOG="{0}"' -f $log
    Write-Host "PHASE ${Phase}: $Executable $arguments"
    try {
        $process = Start-Process -FilePath $Executable -ArgumentList $arguments -Wait -PassThru
        Write-Host "PHASE $Phase exit code: $($process.ExitCode)"
        if ($process.ExitCode -ne 0) { throw "$Phase failed: exit $($process.ExitCode); executable=$Executable; log=$log" }
    } catch {
        if (Test-Path -LiteralPath $log) { Get-Content -LiteralPath $log -Tail 100 | ForEach-Object { Write-Host $_ } }
        throw
    }
}
function Verify-Installed([string]$ExpectedVersion) {
    Write-Host "PHASE verify installed: ARP version=$ExpectedVersion; hashes, complete file set, manifest, PATH and state"
    foreach ($member in $metadata.members.PSObject.Properties) {
        $path = Join-Path $installDir $member.Name
        if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $member.Value) { throw "Installed hash mismatch: $($member.Name)" }
    }
    $expected = @($metadata.members.PSObject.Properties.Name | Sort-Object)
    $actual = @(Get-ChildItem -LiteralPath $installDir -Filter 'haider*.exe' | ForEach-Object Name | Sort-Object)
    # Check counts first so empty/missing sets produce our membership diagnostic.
    if ($expected.Count -ne $actual.Count -or ($expected.Count -gt 0 -and (Compare-Object $expected $actual))) {
        throw "Installed binary member set mismatch: expected=[$($expected -join ', ')]; actual=[$($actual -join ', ')]"
    }
    $expectedFiles = @(@($metadata.members.PSObject.Properties.Name) + @('installer-manifest.json', 'unins000.exe', 'unins000.dat') | Sort-Object)
    # A clean directory and unchanged AppId reuse unins000 during upgrade.
    $uninstaller = Join-Path $installDir 'unins000.exe'
    if (!(Test-Path -LiteralPath $uninstaller -PathType Leaf)) { throw "Missing uninstaller: $uninstaller" }
    # Inno stores detached message data when its uninstaller is signed.
    $setupSignature = Get-AuthenticodeSignature -LiteralPath $UpgradeInstaller
    $uninstallSignature = Get-AuthenticodeSignature -LiteralPath $uninstaller
    Write-Host "Uninstaller signature: $($uninstallSignature.Status)"
    if ($uninstallSignature.Status -ne 'NotSigned') {
        if ($uninstallSignature.Status -ne 'Valid') { throw "Uninstaller signature verification failed: $($uninstallSignature.Status); $($uninstallSignature.StatusMessage)" }
        $expectedFiles = @($expectedFiles + 'unins000.msg' | Sort-Object)
    }
    if ($ExpectedVersion -eq $metadata.version -and $setupSignature.Status -eq 'Valid' -and $uninstallSignature.Status -ne 'Valid') {
        throw 'Signed upgrade did not install a valid signed uninstaller'
    }
    $actualFiles = @(Get-ChildItem -LiteralPath $installDir -Recurse -File | ForEach-Object { $_.FullName.Substring($installDir.Length + 1) } | Sort-Object)
    if ($expectedFiles.Count -ne $actualFiles.Count -or (Compare-Object $expectedFiles $actualFiles)) {
        throw "Installed complete file set mismatch: expected=[$($expectedFiles -join ', ')]; actual=[$($actualFiles -join ', ')]"
    }
    if ((Get-FileHash -LiteralPath (Join-Path $installDir 'installer-manifest.json')).Hash -ne (Get-FileHash -LiteralPath $Manifest).Hash) { throw 'Packed manifest bytes mismatch' }
    $packed = Get-Content -LiteralPath (Join-Path $installDir 'installer-manifest.json') -Raw | ConvertFrom-Json
    if ($packed.version -ne $metadata.version -or $packed.target -ne $metadata.target) { throw 'Packed release coordinates mismatch' }
    $arpVersion = (Get-ItemProperty -LiteralPath $arpKey).DisplayVersion
    if ($arpVersion -ne $ExpectedVersion) { throw "Add/Remove Programs version mismatch: expected=$ExpectedVersion actual=$arpVersion key=$arpKey" }
    # Keep the original expanded PATH assertion: an expandable preexisting
    # token must not hide a duplicate of the absolute path added by setup.
    $path = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (@(([string]$path).Split(';') | Where-Object { $_ -ieq $installDir }).Count -ne 1) { throw 'User PATH must contain exactly one install directory' }
    if ((Get-FileHash -LiteralPath $sentinel).Hash -ne $expectedStateHash) { throw 'Install modified user state' }
}
$lifecycleError = $null
try {
    New-Item -ItemType Directory $state | Out-Null
    'preserve user state' | Set-Content -LiteralPath $sentinel
    $expectedStateHash = (Get-FileHash -LiteralPath $sentinel).Hash
    Run-Setup -Executable $Installer -Phase install
    Verify-Installed $InitialVersion
    Run-Setup -Executable $UpgradeInstaller -Phase upgrade
    Verify-Installed $metadata.version
    Write-Host 'PHASE verify installed CLI version'
    $binaryVersion = & (Join-Path $installDir 'haider.exe') --version
    if ($LASTEXITCODE -ne 0 -or $binaryVersion -cne "haider $($metadata.version)") { throw "Installed binary version mismatch: exit=$LASTEXITCODE expected=haider $($metadata.version) actual=[$binaryVersion]" }
    Run-Setup -Executable (Join-Path $installDir 'unins000.exe') -Phase uninstall
    Write-Host 'PHASE verify removal: directory, owner/ARP keys, original raw PATH value/type and preserved state'
    foreach ($removedPath in @($installDir, $ownerKey, $arpKey)) {
        if (Test-Path -LiteralPath $removedPath) { throw "Uninstall left binaries, directory, or registry keys: $removedPath" }
    }
    $afterPath = Get-UserPathSnapshot
    # PowerShell can coerce null to an empty string for a .NET string argument.
    if (($null -eq $beforePath.Value) -ne ($null -eq $afterPath.Value) -or
        ![string]::Equals($beforePath.Value, $afterPath.Value, [StringComparison]::Ordinal) -or $beforePath.Kind -ne $afterPath.Kind) {
        $pathDetails = @{ Before = $beforePath; After = $afterPath } | ConvertTo-Json -Compress
        throw "Uninstall failed to restore original user PATH value and type: $pathDetails"
    }
    if ((Get-FileHash -LiteralPath $sentinel).Hash -ne $expectedStateHash) { throw 'Uninstall modified user state' }
    Write-Host "PASS Windows post-pack/lifecycle: exact members and hashes, version transition $InitialVersion -> $($metadata.version), PATH, ARP, registry removal, preserved state. Predecessor is a metadata fixture using current release bytes."
} catch {
    $lifecycleError = $_
    Write-Host "FAIL Windows lifecycle: $($_.Exception.Message); native logs retained at $logDir"
    throw
} finally {
    $cleanupErrors = @()
    try {
        if (Test-Path -LiteralPath (Join-Path $installDir 'unins000.exe')) {
            Run-Setup -Executable (Join-Path $installDir 'unins000.exe') -Phase cleanup-uninstall
        }
    } catch { $cleanupErrors += $_.Exception.Message }
    try {
        if (Test-Path -LiteralPath $state) { Remove-Item -LiteralPath $state -Recurse -Force }
    } catch { $cleanupErrors += $_.Exception.Message }
    if ($cleanupErrors.Count -gt 0) {
        $message = "Lifecycle cleanup failed: $($cleanupErrors -join '; ')"
        if ($null -ne $lifecycleError) { Write-Warning $message }
        else { throw $message }
    }
}
