$ErrorActionPreference = 'Stop'

# Chocolatey runs this before an upgrade or uninstall. A haiderd.exe or
# haider-tui.exe still running from this package would hold its image open,
# so the package-folder backup/removal fails with "access denied". Stop only
# processes whose executable lives in this package's tools directory; a
# haider installed any other way is left alone.
$toolsDir = [IO.Path]::GetFullPath((Split-Path -Parent $MyInvocation.MyCommand.Definition))
$prefix = $toolsDir.TrimEnd('\') + '\'

foreach ($name in @('haider', 'haider-tui', 'haiderd')) {
  foreach ($process in @(Get-Process -Name $name -ErrorAction SilentlyContinue)) {
    $path = $null
    try { $path = $process.Path } catch { }
    if ($path -and $path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
      Write-Warning "Stopping $name (PID $($process.Id)) running from $toolsDir"
      Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
      try { $process.WaitForExit(10000) | Out-Null } catch { }
    }
  }
}
