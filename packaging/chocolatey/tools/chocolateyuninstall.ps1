$ErrorActionPreference = 'Stop'

$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition

foreach ($binary in @('haider.exe', 'haiderd.exe')) {
  $path = Join-Path $toolsDir $binary
  if (Test-Path $path) {
    Remove-Item $path -Force
  }
}
