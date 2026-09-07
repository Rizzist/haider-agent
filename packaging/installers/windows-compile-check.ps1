# Compile the real installer with disposable, non-executable payload bytes.
# No signing, lifecycle execution, release output or checksum sidecars.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$work = Join-Path ([IO.Path]::GetTempPath()) ('haider-inno-check-' + [guid]::NewGuid())
New-Item -ItemType Directory $work | Out-Null
try {
    & python (Join-Path $PSScriptRoot '../../scripts/windows_installer_inputs.py') fixture --work $work
    if ($LASTEXITCODE -ne 0) { throw 'Inno compile fixture preparation failed' }
    $iscc = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
    if (!(Test-Path $iscc)) { throw 'Inno Setup 6 is required' }
    & $iscc '/Qp' (Join-Path $work 'windows.iss')
    if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compile check failed' }
    Write-Host 'PASS: Inno Setup compiled windows.iss with the three-member synthetic payload.'
} finally {
    Remove-Item $work -Recurse -Force
}
