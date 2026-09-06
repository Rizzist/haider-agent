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
$Payload = (Resolve-Path $Payload).Path
$Manifest = (Resolve-Path $Manifest).Path
New-Item -ItemType Directory -Force $Output | Out-Null
$Output = (Resolve-Path $Output).Path
$metadata = Get-Content -Raw $Manifest | ConvertFrom-Json
if ($metadata.version -ne $Version -or $metadata.target -ne $Target) { throw 'Manifest release mismatch' }
$work = Join-Path ([IO.Path]::GetTempPath()) ('haider-inno-' + [guid]::NewGuid())
New-Item -ItemType Directory $work | Out-Null
$cert = $null
try {
    $files = @()
    foreach ($member in $metadata.members.PSObject.Properties) {
        if ($member.Name -notmatch '^haider[a-zA-Z0-9_-]*\.exe$') { throw "Invalid binary member: $($member.Name)" }
        $source = Join-Path $Payload $member.Name
        if ((Get-FileHash $source -Algorithm SHA256).Hash.ToLowerInvariant() -ne $member.Value) { throw "Payload hash mismatch: $source" }
        $files += ('Source: "{0}"; DestDir: "{{app}}"; Flags: ignoreversion' -f $source)
    }
    if ($files.Count -lt 2 -or !$metadata.members.PSObject.Properties['haider.exe'] -or !$metadata.members.PSObject.Properties['haiderd.exe']) { throw 'Missing required binaries' }
    $files += ('Source: "{0}"; DestDir: "{{app}}"; DestName: "installer-manifest.json"; Flags: ignoreversion' -f $Manifest)
    $files | Set-Content (Join-Path $work 'members.iss') -Encoding utf8
    @(
        "#define ReleaseVersion `"$Version`"",
        "#define ReleaseTarget `"$Target`"",
        "#define OutputPath `"$Output`""
    ) | Set-Content (Join-Path $work 'generated.iss') -Encoding utf8
    Copy-Item (Join-Path $PSScriptRoot 'windows.iss') (Join-Path $work 'windows.iss')
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
    & $iscc @compileArgs (Join-Path $work 'windows.iss')
    if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compilation failed' }
    $installer = Join-Path $Output "haider-v$Version-$Target-setup.exe"
    $resourceVersion = (Get-Item $installer).VersionInfo.ProductVersion
    if ($resourceVersion -ne $Version -and $resourceVersion -ne "$Version.0") { throw "Installer version mismatch: $resourceVersion" }
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
    ) | Set-Content (Join-Path $work 'generated.iss') -Encoding utf8
    & $iscc '/Qp' (Join-Path $work 'windows.iss')
    if ($LASTEXITCODE -ne 0) { throw 'Upgrade fixture compilation failed' }
    $previous = Join-Path $work "haider-v$priorVersion-$Target-setup.exe"
    # Post-pack gate executes the exact built installer and verifies every installed hash.
    & (Join-Path $PSScriptRoot '../../scripts/qa-gate/installers-windows.ps1') -Installer $previous -UpgradeInstaller $installer -InitialVersion $priorVersion -Manifest $Manifest
    if ($cert -and (Get-AuthenticodeSignature $installer).Status -ne 'Valid') { throw 'Installer signature verification failed' }
    $hash = (Get-FileHash $installer -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $([IO.Path]::GetFileName($installer))" | Set-Content "$installer.sha256" -Encoding ascii
} finally {
    foreach ($item in @($cert)) { if ($item) { Remove-Item "Cert:\CurrentUser\My\$($item.Thumbprint)" -Force } }
    Remove-Item $work -Recurse -Force
}
