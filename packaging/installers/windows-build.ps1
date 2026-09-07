param(
    [Parameter(Mandatory)][string]$Payload,
    [Parameter(Mandatory)][string]$Manifest,
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$Target,
    [Parameter(Mandatory)][string]$Output
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($Version -notmatch '^\d+\.\d+\.\d+$' -or $Target -ne 'x86_64-pc-windows-msvc') { throw 'Invalid Windows release coordinates' }
$Payload = (Resolve-Path -LiteralPath $Payload).Path
$Manifest = (Resolve-Path -LiteralPath $Manifest).Path
New-Item -ItemType Directory -Force $Output | Out-Null
$Output = (Resolve-Path -LiteralPath $Output).Path
$work = Join-Path ([IO.Path]::GetTempPath()) ('haider-inno-' + [guid]::NewGuid())
New-Item -ItemType Directory $work | Out-Null
$cert = $null
$buildError = $null
function Assert-InstallerVersion([string]$Path, [string]$ExpectedVersion) {
    $info = (Get-Item -LiteralPath $Path).VersionInfo
    $raw = [string]$info.ProductVersion
    # Trim only boundary whitespace/NUL padding; never hide an embedded mismatch.
    $normalized = $raw -replace '^[\s\x00]+|[\s\x00]+$', ''
    $details = [ordered]@{
        FileVersion = $info.FileVersion
        ProductVersion = $info.ProductVersion
        FileVersionRaw = [string]$info.FileVersionRaw
        ProductVersionRaw = [string]$info.ProductVersionRaw
        ProductVersionLength = $raw.Length
        ProductVersionCharCodes = @($raw.ToCharArray() | ForEach-Object { 'U+{0:X4}' -f [int]$_ })
        NormalizedProductVersion = $normalized
    } | ConvertTo-Json -Compress
    # Write directly as well as throwing: ConciseView can truncate/pad exceptions.
    Write-Host "Installer version resource ($Path): $details"
    # PowerShell's culture-aware -eq/-ceq can ignore embedded NUL characters.
    if (![string]::Equals($normalized, $ExpectedVersion, [StringComparison]::Ordinal) -and
        ![string]::Equals($normalized, "$ExpectedVersion.0", [StringComparison]::Ordinal)) {
        throw "Installer version mismatch: expected $ExpectedVersion or $ExpectedVersion.0; $details"
    }
}
try {
    # Share the hash-checked renderer with the pre-tag compile check.
    & python (Join-Path $PSScriptRoot '../../scripts/windows_installer_inputs.py') render `
        --payload $Payload --manifest $Manifest --version $Version --target $Target --output $Output --work $work
    if ($LASTEXITCODE -ne 0) { throw "Windows installer input preparation failed: exit $LASTEXITCODE" }
    $iscc = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
    if (!(Test-Path $iscc)) { throw 'Inno Setup 6 is required' }
    $compileArgs = @('/Qp')
    if ($env:WINDOWS_SIGNING_PFX_BASE64 -and $env:WINDOWS_SIGNING_PFX_PASSWORD) {
        $pfx = Join-Path $work 'signing.pfx'
        [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($env:WINDOWS_SIGNING_PFX_BASE64))
        $password = ConvertTo-SecureString $env:WINDOWS_SIGNING_PFX_PASSWORD -AsPlainText -Force
        $cert = Import-PfxCertificate -FilePath $pfx -CertStoreLocation Cert:\CurrentUser\My -Password $password
        Remove-Item $pfx -Force
        $signTool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" | Sort-Object FullName -Descending | Select-Object -First 1
        if (!$signTool) { throw 'Windows SDK signtool.exe is required for configured signing' }
        $thumb = @($cert | Where-Object HasPrivateKey)[0].Thumbprint
        $compileArgs += '/DSignEnabled'
        $compileArgs += ('/Shaider=$q{0}$q sign /sha1 {1} /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 $f' -f $signTool.FullName, $thumb)
        Write-Host 'Windows signing enabled: installer and uninstaller; release payload bytes remain unchanged.'
    } else {
        Write-Host 'SKIP Windows signing: WINDOWS_SIGNING_PFX_BASE64 or WINDOWS_SIGNING_PFX_PASSWORD is absent.'
    }
    Write-Host "PHASE compile release: version=$Version target=$Target compiler=$iscc"
    & $iscc @compileArgs (Join-Path $work 'windows.iss')
    if ($LASTEXITCODE -ne 0) { throw "Inno Setup release compilation failed: exit $LASTEXITCODE" }
    $installer = Join-Path $Output "haider-v$Version-$Target-setup.exe"
    Write-Host 'PHASE verify release version resource'
    Assert-InstallerVersion -Path $installer -ExpectedVersion $Version
    # Build an older-metadata fixture from the same payload to exercise an actual
    # version transition. It is test-only, unsigned, and never enters Output.
    $parts = $Version.Split('.')
    if ([int]$parts[2] -gt 0) { $priorVersion = "$($parts[0]).$($parts[1]).$([int]$parts[2] - 1)" }
    else { $priorVersion = '0.0.0' }
    if ($priorVersion -eq $Version) { throw 'Lifecycle fixture requires a release newer than 0.0.0' }
    @(
        "#define ReleaseVersion `"$priorVersion`"",
        "#define ReleaseTarget `"$Target`"",
        "#define OutputPath `"$work`""
    ) | Set-Content -LiteralPath (Join-Path $work 'generated.iss') -Encoding utf8
    Write-Host "PHASE compile fixture: version=$priorVersion (current release payload, unsigned)"
    & $iscc '/Qp' (Join-Path $work 'windows.iss')
    if ($LASTEXITCODE -ne 0) { throw "Upgrade fixture compilation failed: exit $LASTEXITCODE" }
    $previous = Join-Path $work "haider-v$priorVersion-$Target-setup.exe"
    Assert-InstallerVersion -Path $previous -ExpectedVersion $priorVersion
    # Post-pack gate executes the exact built installer and verifies every installed hash.
    Write-Host "PHASE lifecycle gate: $priorVersion -> $Version"
    # This is a PowerShell script: terminating errors propagate; LASTEXITCODE is
    # reserved for the native python/ISCC/haider invocations, not script success.
    & (Join-Path $PSScriptRoot '../../scripts/qa-gate/installers-windows.ps1') -Installer $previous -UpgradeInstaller $installer -InitialVersion $priorVersion -Manifest $Manifest
    Write-Host 'PHASE verify release signature'
    if ($cert) {
        $signature = Get-AuthenticodeSignature -LiteralPath $installer
        if ($signature.Status -ne 'Valid') { throw "Installer signature verification failed: $($signature.Status); $($signature.StatusMessage)" }
    } else { Write-Host 'SKIP signature verification: installer signing was not configured.' }
    Write-Host "PHASE write SHA256 sidecar: $installer.sha256"
    $hash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $([IO.Path]::GetFileName($installer))" | Set-Content -LiteralPath "$installer.sha256" -Encoding ascii
    Write-Host "PASS Windows installer build and post-pack checks: $installer"
} catch {
    $buildError = $_
    Write-Host "FAIL Windows installer build: $($_.Exception.Message)"
    throw
} finally {
    $cleanupErrors = @()
    foreach ($item in @($cert)) {
        if ($item) {
            try { Remove-Item -LiteralPath "Cert:\CurrentUser\My\$($item.Thumbprint)" -Force }
            catch { $cleanupErrors += $_.Exception.Message }
        }
    }
    try { Remove-Item -LiteralPath $work -Recurse -Force }
    catch { $cleanupErrors += $_.Exception.Message }
    if ($cleanupErrors.Count -gt 0) {
        $message = "Windows build cleanup failed: $($cleanupErrors -join '; ')"
        if ($null -ne $buildError) { Write-Warning $message }
        else { throw $message }
    }
}
