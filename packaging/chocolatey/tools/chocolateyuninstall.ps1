$ErrorActionPreference = 'Stop'

$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition

foreach ($binary in @('haider-tui.exe', 'haiderd.exe', 'haider.exe')) {
  $path = Join-Path $toolsDir $binary
  if (Test-Path $path) {
    Remove-Item $path -Force
  }
  Remove-Item (Join-Path $toolsDir "$binary.ignore") -Force -ErrorAction SilentlyContinue
}
