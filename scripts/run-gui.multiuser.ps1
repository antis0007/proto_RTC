param(
  [string]$Profile = '',
  [string]$Username = '',
  [switch]$ResetLocalState = $false,
  [switch]$ResetAllLocalState = $false,
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$ExtraArgs
)

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir '..')).Path
Set-Location $RepoRoot

function Slugify([string]$s) {
  if ([string]::IsNullOrWhiteSpace($s)) { return '' }
  $x = $s.Trim().ToLowerInvariant()
  $x = ($x -replace '[^a-z0-9._-]+', '-')
  $x = ($x -replace '-{2,}', '-')
  return $x.Trim('-')
}

function Remove-IfExists([string]$PathToRemove) {
  if (Test-Path $PathToRemove) {
    Remove-Item -Path $PathToRemove -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "Deleted: $PathToRemove"
  }
}

if (Test-Path '.env') {
  Get-Content '.env' | ForEach-Object {
    if ($_ -notmatch '^\s*#' -and $_ -notmatch '^\s*$') {
      $parts = $_ -split '=', 2
      if ($parts.Count -eq 2) {
        [System.Environment]::SetEnvironmentVariable($parts[0].Trim(), $parts[1].Trim().Trim('"'), 'Process')
      }
    }
  }
}

if ([string]::IsNullOrWhiteSpace($Profile)) {
  if (-not [string]::IsNullOrWhiteSpace($env:CLI_USERNAME)) {
    $Profile = $env:CLI_USERNAME
  } elseif (-not [string]::IsNullOrWhiteSpace($Username)) {
    $Profile = $Username
  } else {
    $Profile = 'default'
  }
}
$Profile = Slugify $Profile
if ([string]::IsNullOrWhiteSpace($Profile)) { $Profile = 'default' }

if ([string]::IsNullOrWhiteSpace($Username)) {
  if (-not [string]::IsNullOrWhiteSpace($env:CLI_USERNAME)) {
    $Username = $env:CLI_USERNAME
  } else {
    $Username = $Profile
  }
}

$clientRoot = Join-Path $RepoRoot (Join-Path 'data/clients' $Profile)
$clientLocalAppData = Join-Path $clientRoot 'localappdata'
$clientAppData = Join-Path $clientRoot 'appdata'
$clientTemp = Join-Path $clientRoot 'temp'
$clientLogs = Join-Path $clientRoot 'logs'
$clientConfig = Join-Path $clientRoot 'config'
$clientState = Join-Path $clientRoot 'state'

@($clientRoot, $clientLocalAppData, $clientAppData, $clientTemp, $clientLogs, $clientConfig, $clientState, (Join-Path $RepoRoot 'logs')) | ForEach-Object {
  New-Item -ItemType Directory -Force -Path $_ | Out-Null
}

if ($ResetAllLocalState) {
  Remove-IfExists (Join-Path $RepoRoot 'data/clients')
  $globalTemp = [System.IO.Path]::GetTempPath()
  Get-ChildItem -Path $globalTemp -Filter 'proto_rtc_client_*' -Force -ErrorAction SilentlyContinue | ForEach-Object {
    Remove-IfExists $_.FullName
  }
}

if ($ResetLocalState) {
  Remove-IfExists $clientRoot
  @($clientRoot, $clientLocalAppData, $clientAppData, $clientTemp, $clientLogs, $clientConfig, $clientState) | ForEach-Object {
    New-Item -ItemType Directory -Force -Path $_ | Out-Null
  }
  # Also delete legacy temp/profile dirs created by older scripts/builds.
  $globalTemp = [System.IO.Path]::GetTempPath()
  @(
    (Join-Path $globalTemp ("proto_rtc_client_{0}" -f $Profile)),
    (Join-Path $globalTemp ("proto-rtc-client-{0}" -f $Profile))
  ) | ForEach-Object { Remove-IfExists $_ }
}

# Process-scoped isolation so multiple GUI instances from the same repo do not share local stores.
$env:CLI_USERNAME = $Username
$env:PROTO_RTC_CLIENT_USER = $Username
$env:PROTO_RTC_CLIENT_PROFILE = $Profile
$env:PROTO_RTC_CLIENT_ROOT = $clientRoot
$env:PROTO_RTC_CLIENT_STATE_DIR = $clientState
$env:PROTO_RTC_CLIENT_CONFIG_DIR = $clientConfig
$env:PROTO_RTC_CLIENT_LOG_DIR = $clientLogs

# Strong isolation for crates that use dirs/temp APIs.
$env:LOCALAPPDATA = $clientLocalAppData
$env:APPDATA = $clientAppData
$env:TEMP = $clientTemp
$env:TMP = $clientTemp
$env:XDG_DATA_HOME = $clientLocalAppData
$env:XDG_CONFIG_HOME = $clientConfig
$env:XDG_STATE_HOME = $clientState

if (-not $env:SERVER_PUBLIC_URL) { $env:SERVER_PUBLIC_URL = 'http://127.0.0.1:8443' }

Write-Host "Launching GUI profile='$Profile' username='$Username'" -ForegroundColor Cyan
Write-Host "  SERVER_PUBLIC_URL=$($env:SERVER_PUBLIC_URL)"
Write-Host "  LOCALAPPDATA=$env:LOCALAPPDATA"
Write-Host "  TEMP=$env:TEMP"

if (Test-Path 'apps/desktop_gui/Cargo.toml') {
  cargo run -p desktop_gui -- @ExtraArgs
} else {
  Write-Warning 'apps/desktop_gui not found; falling back to apps/desktop.'
  cargo run -p desktop -- --server-url $env:SERVER_PUBLIC_URL --username $env:CLI_USERNAME @ExtraArgs
}
