param(
    [Parameter(Mandatory)][string]$Installer,
    [Parameter(Mandatory)][string]$Manifest,
    [string]$UpgradeInstaller = '',
    [string]$InitialVersion = ''
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:CI -ne 'true' -or $env:RUNNER_OS -ne 'Windows') { throw 'This lifecycle gate must run on a disposable Windows CI runner' }
$Installer = (Resolve-Path $Installer).Path
if (!$UpgradeInstaller) { $UpgradeInstaller = $Installer }
$UpgradeInstaller = (Resolve-Path $UpgradeInstaller).Path
$metadata = Get-Content -Raw $Manifest | ConvertFrom-Json
if (!$InitialVersion) { $InitialVersion = $metadata.version }
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\Haider'
$ownerKey = 'HKCU:\Software\HaiderInstaller'
$arpKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\HaiderHarness_is1'
if ((Test-Path $installDir) -or (Test-Path $ownerKey) -or (Test-Path $arpKey)) { throw 'Refusing to overwrite an existing Haider installation' }
$environment = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
$beforePath = $environment.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
$beforeKind = if ($null -ne $beforePath) { $environment.GetValueKind('Path') } else { $null }
$environment.Close()
$state = Join-Path $env:USERPROFILE '.haider'
if (Test-Path $state) { throw 'This test requires a clean profile so its state fixture cannot mix with real data' }
New-Item -ItemType Directory $state | Out-Null
$sentinel = Join-Path $state 'installer-qa-preserve.txt'
'preserve user state' | Set-Content $sentinel
$expectedStateHash = (Get-FileHash $sentinel).Hash
function Run-Setup([string]$Executable, [string[]]$SetupArgs) {
    $process = Start-Process -FilePath $Executable -ArgumentList $SetupArgs -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "Installer process failed: $($process.ExitCode)" }
}
function Verify-Installed([string]$ExpectedVersion) {
    foreach ($member in $metadata.members.PSObject.Properties) {
        $path = Join-Path $installDir $member.Name
        if ((Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $member.Value) { throw "Installed hash mismatch: $($member.Name)" }
    }
    $expected = @($metadata.members.PSObject.Properties.Name | Sort-Object)
    $actual = @(Get-ChildItem $installDir -Filter 'haider*.exe' | ForEach-Object Name | Sort-Object)
    if (Compare-Object $expected $actual) { throw 'Installed binary member set mismatch' }
    $expectedFiles = @(@($metadata.members.PSObject.Properties.Name) + @('installer-manifest.json', 'unins000.exe', 'unins000.dat') | Sort-Object)
    # Inno stores detached message data when its uninstaller is signed.
    if ((Get-AuthenticodeSignature (Join-Path $installDir 'unins000.exe')).Status -ne 'NotSigned') {
        $expectedFiles = @($expectedFiles + 'unins000.msg' | Sort-Object)
    }
    $actualFiles = @(Get-ChildItem $installDir -Recurse -File | ForEach-Object { $_.FullName.Substring($installDir.Length + 1) } | Sort-Object)
    if (Compare-Object $expectedFiles $actualFiles) { throw 'Installed complete file set mismatch' }
    if ((Get-FileHash (Join-Path $installDir 'installer-manifest.json')).Hash -ne (Get-FileHash $Manifest).Hash) { throw 'Packed manifest bytes mismatch' }
    $packed = Get-Content -Raw (Join-Path $installDir 'installer-manifest.json') | ConvertFrom-Json
    if ($packed.version -ne $metadata.version -or $packed.target -ne $metadata.target) { throw 'Packed release coordinates mismatch' }
    if ((Get-ItemProperty $arpKey).DisplayVersion -ne $ExpectedVersion) { throw 'Add/Remove Programs version mismatch' }
    $path = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (@($path.Split(';') | Where-Object { $_ -ieq $installDir }).Count -ne 1) { throw 'User PATH must contain exactly one install directory' }
    if ((Get-FileHash $sentinel).Hash -ne $expectedStateHash) { throw 'Install modified user state' }
}
try {
    Run-Setup $Installer @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')
    Verify-Installed $InitialVersion
    Run-Setup $UpgradeInstaller @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')
    Verify-Installed $metadata.version
    $binaryVersion = & (Join-Path $installDir 'haider.exe') --version
    if ($LASTEXITCODE -ne 0 -or $binaryVersion -ne "haider $($metadata.version)") { throw 'Installed binary version mismatch' }
    Run-Setup (Join-Path $installDir 'unins000.exe') @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')
    if ((Test-Path $installDir) -or (Test-Path $ownerKey) -or (Test-Path $arpKey)) { throw 'Uninstall left binaries, directory, or registry keys' }
    $environment = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    $afterPath = $environment.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $afterKind = if ($null -ne $afterPath) { $environment.GetValueKind('Path') } else { $null }
    $environment.Close()
    if ($beforePath -cne $afterPath -or $beforeKind -ne $afterKind) { throw 'Uninstall failed to restore original user PATH value and type' }
    if ((Get-FileHash $sentinel).Hash -ne $expectedStateHash) { throw 'Uninstall modified user state' }
    Write-Host "PASS Windows post-pack/lifecycle: exact members and hashes, version transition $InitialVersion -> $($metadata.version), PATH, ARP, registry removal, preserved state. Predecessor is a metadata fixture using current release bytes."
} finally {
    if (Test-Path (Join-Path $installDir 'unins000.exe')) {
        Run-Setup (Join-Path $installDir 'unins000.exe') @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')
    }
    Remove-Item $state -Recurse -Force
}
