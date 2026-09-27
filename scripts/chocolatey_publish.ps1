# OData sees packages still in moderation; choco search does not.
param([ValidateSet('Check', 'Push')][string]$Mode)

$ErrorActionPreference = 'Stop'
$Version = $env:PACKAGE_VERSION
if ($Version -notmatch '^\d+\.\d+\.\d+\.\d+$') { throw "Invalid PACKAGE_VERSION '$Version'" }
$Uri = "https://community.chocolatey.org/api/v2/Packages(Id='haider',Version='$Version')"

# Mock-feed tests can supply this one function before invoking the script.
# Real HTTP tests use the loopback-only override; identity keeps the production URI.
if (-not (Get-Command Invoke-FeedRequest -CommandType Function -ErrorAction SilentlyContinue)) {
function Invoke-FeedRequest([string]$RequestUri, [int]$TimeoutMilliseconds) {
  $TransportUri = $RequestUri
  if ($env:CHOCO_TEST_FEED_URI) {
    $Loopback = $null
    if (-not [Uri]::TryCreate($env:CHOCO_TEST_FEED_URI, [UriKind]::Absolute, [ref]$Loopback) -or
        $Loopback.Scheme -ne 'http' -or $Loopback.Host -notin @('127.0.0.1', 'localhost') -or
        -not [string]::IsNullOrEmpty($Loopback.UserInfo)) {
      throw 'CHOCO_TEST_FEED_URI must be an HTTP loopback URI'
    }
    $TransportUri = $Loopback.AbsoluteUri
  }
  $Client = [System.Net.Http.HttpClient]::new()
  $Client.Timeout = [System.Threading.Timeout]::InfiniteTimeSpan
  $Cancellation = [System.Threading.CancellationTokenSource]::new()
  $Request = [System.Net.Http.HttpRequestMessage]::new([System.Net.Http.HttpMethod]::Get, $TransportUri)
  $Response = $null
  try {
    $Cancellation.CancelAfter($TimeoutMilliseconds)
    $Response = $Client.SendAsync($Request, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead,
      $Cancellation.Token).GetAwaiter().GetResult()
    # Read the complete body while the same total-duration token is active.
    $Content = $Response.Content.ReadAsStringAsync($Cancellation.Token).GetAwaiter().GetResult()
    return [pscustomobject]@{ StatusCode = [int]$Response.StatusCode; Content = $Content }
  } finally {
    if ($null -ne $Response) { $Response.Dispose() }
    $Request.Dispose()
    $Cancellation.Dispose()
    $Client.Dispose()
  }
}
}

function Get-ExactPackageStatus([int]$TimeoutMilliseconds = 30000) {
  try {
    $Response = Invoke-FeedRequest -RequestUri $Uri -TimeoutMilliseconds $TimeoutMilliseconds
  } catch {
    # Connection failures are transient. Identity and HTTP errors below are not.
    return 'transient'
  }
  if ($Response.StatusCode -eq 404) { return 'absent' }
  if ($Response.StatusCode -eq 200) {
    try { [xml]$Entry = $Response.Content }
    catch { throw "OData 200 response was malformed XML for haider $Version" }
    $Root = $Entry.SelectSingleNode("/*[local-name()='entry']")
    $Id = $Entry.SelectSingleNode("/*[local-name()='entry']/*[local-name()='id']")
    $Title = $Entry.SelectSingleNode("/*[local-name()='entry']/*[local-name()='title']")
    $FoundVersion = $Entry.SelectSingleNode("/*[local-name()='entry']/*[local-name()='properties']/*[local-name()='Version']")
    if ($null -eq $Root -or $null -eq $Id -or $null -eq $Title -or $null -eq $FoundVersion -or
        $Id.InnerText -ne $Uri -or $Title.InnerText -ne 'haider' -or $FoundVersion.InnerText -ne $Version) {
      throw "OData 200 response did not identify haider $Version"
    }
    return 'present'
  }
  if ($Response.StatusCode -in @(408, 429) -or
      ($Response.StatusCode -ge 500 -and $Response.StatusCode -le 599)) { return 'transient' }
  throw "OData query returned HTTP $($Response.StatusCode)"
}

function Get-ExactPackagePresence {
  $RequestTimeoutMs = 30000
  if ($env:CHOCO_TEST_FEED_URI -and $env:CHOCO_TEST_PREFLIGHT_TIMEOUT_MS) {
    $RequestTimeoutMs = Read-PositiveSetting 'CHOCO_TEST_PREFLIGHT_TIMEOUT_MS' 30000 30000
  }
  for ($Attempt = 1; $Attempt -le 3; $Attempt++) {
    $Status = Get-ExactPackageStatus -TimeoutMilliseconds $RequestTimeoutMs
    if ($Status -eq 'present') { return $true }
    if ($Status -eq 'absent') { return $false }
    if ($Attempt -eq 3) { throw "Could not establish whether haider $Version exists after 3 preflight queries" }
    Start-Sleep -Seconds $Attempt
  }
}

function Read-PositiveSetting([string]$Name, [int]$Default, [int]$Maximum) {
  $Raw = [Environment]::GetEnvironmentVariable($Name)
  if ([string]::IsNullOrEmpty($Raw)) { return $Default }
  if ($Raw -notmatch '^[1-9][0-9]*$' -or $Raw.Length -gt 9) {
    throw "$Name must be a positive integer no greater than $Maximum"
  }
  $Value = [int]$Raw
  if ($Value -gt $Maximum) { throw "$Name must be a positive integer no greater than $Maximum" }
  return $Value
}

function Confirm-ExactPackage([int]$TimeoutSeconds, [int]$InitialIntervalMs) {
  # Only absence and transient transport/server failures are retried until the deadline.
  # A malformed or mismatched 200 and any non-transient HTTP error fail closed.
  $Clock = [System.Diagnostics.Stopwatch]::StartNew()
  $IntervalMs = $InitialIntervalMs
  do {
    $RemainingMs = $TimeoutSeconds * 1000 - $Clock.ElapsedMilliseconds
    if ($RemainingMs -le 0) { break }
    $RequestMs = [int][Math]::Min(30000, $RemainingMs)
    $Status = Get-ExactPackageStatus -TimeoutMilliseconds $RequestMs
    # A complete exact entity received after the deadline is unconfirmed.
    if ($Clock.ElapsedMilliseconds -ge $TimeoutSeconds * 1000) { break }
    if ($Status -eq 'present') { return $true }
    $RemainingMs = $TimeoutSeconds * 1000 - $Clock.ElapsedMilliseconds
    if ($RemainingMs -le 0) { break }
    Start-Sleep -Milliseconds ([int][Math]::Min($IntervalMs, $RemainingMs))
    $IntervalMs = [Math]::Min(60000, $IntervalMs * 2)
  } while ($true)
  return $false
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

$ConfirmTimeout = Read-PositiveSetting 'CHOCO_CONFIRM_TIMEOUT_SECONDS' 600 900
$ConfirmIntervalMs = Read-PositiveSetting 'CHOCO_CONFIRM_POLL_INTERVAL_MS' 10000 60000
$NupkgPath = Join-Path 'dist-choco' "haider.$Version.nupkg"
$PushOutput = & choco push $NupkgPath --source https://push.chocolatey.org/ --api-key $env:CHOCO_API_KEY 2>&1
$PushExit = $LASTEXITCODE
$PushOutput | Write-Output
try {
  $Confirmed = Confirm-ExactPackage -TimeoutSeconds $ConfirmTimeout -InitialIntervalMs $ConfirmIntervalMs
} catch {
  if ($PushExit -eq 0) {
    "choco push exited 0, but haider $Version was NOT confirmed on the feed: $_" >> $env:GITHUB_STEP_SUMMARY
    Write-Output "::error::Push exited 0 but exact feed confirmation failed: $_"
    exit 1
  }
  "haider $Version was NOT pushed successfully (choco exit $PushExit); exact feed confirmation failed: $_" >> $env:GITHUB_STEP_SUMMARY
  Write-Output "::error::choco push failed (exit $PushExit); exact feed confirmation failed: $_"
  exit $PushExit
}
if ($Confirmed) {
  if ($PushExit -eq 0) {
    "haider $Version was pushed and confirmed on the Chocolatey feed (including moderation)." >> $env:GITHUB_STEP_SUMMARY
    Write-Output "PASS: pushed and confirmed haider $Version"
  } else {
    "haider $Version was confirmed on Chocolatey after choco exit $PushExit; idempotent success." >> $env:GITHUB_STEP_SUMMARY
    Write-Output "PASS: haider $Version exists after failed push; idempotent success."
  }
  exit 0
}
if ($PushExit -eq 0) {
  "choco push exited 0, but haider $Version was NOT confirmed on the feed within $ConfirmTimeout s." >> $env:GITHUB_STEP_SUMMARY
  Write-Output "::error::Push exited 0 but exact feed entity was not confirmed within $ConfirmTimeout s."
  exit 1
}
"haider $Version was NOT pushed successfully (choco exit $PushExit); exact feed entity was NOT confirmed within $ConfirmTimeout s." >> $env:GITHUB_STEP_SUMMARY
Write-Output "::error::Chocolatey push failed (exit $PushExit); exact feed entity was not confirmed within $ConfirmTimeout s."
exit $PushExit
