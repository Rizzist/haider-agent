# OData sees packages still in moderation; choco search does not.
param([ValidateSet('Check', 'Push')][string]$Mode)

$ErrorActionPreference = 'Stop'
$Version = $env:PACKAGE_VERSION
if ($Version -notmatch '^\d+\.\d+\.\d+\.\d+$') { throw "Invalid PACKAGE_VERSION '$Version'" }
$Uri = "https://community.chocolatey.org/api/v2/Packages(Id='haider',Version='$Version')"

function Get-ExactPackagePresence {
  for ($Attempt = 1; $Attempt -le 3; $Attempt++) {
    try {
      $Response = Invoke-WebRequest -Uri $Uri -TimeoutSec 30 -SkipHttpErrorCheck
      if ($Response.StatusCode -eq 404) { return $false }
      if ($Response.StatusCode -eq 200) {
        [xml]$Entry = $Response.Content
        $Root = $Entry.SelectSingleNode("/*[local-name()='entry']")
        $Id = $Entry.SelectSingleNode("/*[local-name()='entry']/*[local-name()='id']")
        $Title = $Entry.SelectSingleNode("/*[local-name()='entry']/*[local-name()='title']")
        $FoundVersion = $Entry.SelectSingleNode("/*[local-name()='entry']/*[local-name()='properties']/*[local-name()='Version']")
        if ($null -eq $Root -or $null -eq $Id -or $null -eq $Title -or $null -eq $FoundVersion -or
            $Id.InnerText -ne $Uri -or $Title.InnerText -ne 'haider' -or $FoundVersion.InnerText -ne $Version) {
          throw "OData 200 response did not identify haider $Version"
        }
        return $true
      }
      if ($Response.StatusCode -notin @(408, 429, 500, 502, 503, 504)) {
        throw "OData query returned HTTP $($Response.StatusCode)"
      }
      $Problem = "OData query returned HTTP $($Response.StatusCode)"
    } catch {
      if ($_.Exception.Message -like 'OData 200 response*' -or $_.Exception.Message -like 'OData query returned HTTP 4*') { throw }
      $Problem = $_.Exception.Message
    }
    if ($Attempt -eq 3) { throw "Could not establish whether haider $Version exists: $Problem" }
    Start-Sleep -Seconds $Attempt
  }
}

if ($Mode -eq 'Check') {
  if (Get-ExactPackagePresence) {
    'present=true' >> $env:GITHUB_OUTPUT
    Write-Output "haider $Version is already on Chocolatey (including moderation queue)."
  } else {
    'present=false' >> $env:GITHUB_OUTPUT
    Write-Output "haider $Version is absent from Chocolatey."
  }
  exit 0
}

if ([string]::IsNullOrWhiteSpace($env:CHOCO_API_KEY)) {
  "haider $Version was NOT pushed: repository secret CHOCO_API_KEY is missing or empty." >> $env:GITHUB_STEP_SUMMARY
  throw 'Repository secret CHOCO_API_KEY is missing or empty; nothing was pushed.'
}
if ($env:PACKAGE_PRESENT -eq 'true') {
  "haider $Version already exists on Chocolatey; push skipped." >> $env:GITHUB_STEP_SUMMARY
  Write-Output "PASS: haider $Version already exists; push skipped."
  exit 0
}
if ($env:PACKAGE_PRESENT -ne 'false') { throw 'The keyless exact-version check did not complete; refusing to push blind.' }

$NupkgPath = Join-Path 'dist-choco' "haider.$Version.nupkg"
$PushOutput = & choco push $NupkgPath --source https://push.chocolatey.org/ --api-key $env:CHOCO_API_KEY 2>&1
$PushExit = $LASTEXITCODE
$PushOutput | Write-Output
if ($PushExit -eq 0) {
  "Pushed haider $Version to Chocolatey moderation." >> $env:GITHUB_STEP_SUMMARY
  Write-Output "PASS: pushed haider $Version"
  exit 0
}

try {
  $PresentAfterFailure = Get-ExactPackagePresence
} catch {
  "haider $Version was NOT pushed (choco exit $PushExit); post-push feed status unknown." >> $env:GITHUB_STEP_SUMMARY
  Write-Error "choco push failed (exit $PushExit); post-push feed query failed: $_"
  exit $PushExit
}
if ($PresentAfterFailure) {
  "haider $Version now exists on Chocolatey after choco exit $PushExit; idempotent no-op." >> $env:GITHUB_STEP_SUMMARY
  Write-Output "PASS: haider $Version exists after failed push; idempotent no-op."
  exit 0
}
"haider $Version was NOT pushed (choco exit $PushExit)." >> $env:GITHUB_STEP_SUMMARY
Write-Output "::error::Chocolatey push failed (exit $PushExit). Regenerate the key at https://community.chocolatey.org/account and update the repository secret CHOCO_API_KEY if rejected. haider $Version was NOT pushed."
exit $PushExit
