param(
  [switch]$ResetLocalState = $false,
  [string]$Profile = 'default',
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$ExtraArgs
)

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir '..')).Path
Set-Location $RepoRoot

# Basic dirs that should always exist
New-Item -ItemType Directory -Force -Path 'data' | Out-Null
New-Item -ItemType Directory -Force -Path 'logs' | Out-Null

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

function Remove-IfExists([string]$PathToRemove) {
  if (Test-Path $PathToRemove) {
    Remove-Item -Path $PathToRemove -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "Deleted: $PathToRemove"
  }
}

# Profile-specific dirs (lets multiple users run from same repo without clobbering each other)
$SafeProfile = ($Profile -replace '[^A-Za-z0-9._-]', '_')
$ClientRoot = Join-Path $RepoRoot (Join-Path 'data/clients' $SafeProfile)
$ClientTemp = Join-Path $ClientRoot 'temp'
$ClientHome = Join-Path $ClientRoot 'home'
$ClientState = Join-Path $ClientRoot 'state'
$ClientCache = Join-Path $ClientRoot 'cache'
$ClientLogs = Join-Path $ClientRoot 'logs'

if ($ResetLocalState) {
  # Wipe only this profile's repo-local state; do not nuke all data/clients/*
  Remove-IfExists $ClientRoot

  # Best-effort wipe common appdata stores (global/local machine caches) if you want a truly clean test
  $localAppData = [Environment]::GetFolderPath('LocalApplicationData')
  $commonClientDirs = @(
    (Join-Path $localAppData 'proto_rtc'),
    (Join-Path $localAppData 'proto-rtc'),
    (Join-Path $localAppData 'ProtoRTC'),
    (Join-Path $localAppData 'proto_RTC')
  )
  foreach ($dir in $commonClientDirs) {
    if (Test-Path $dir) {
      Get-ChildItem -Path $dir -Force -ErrorAction SilentlyContinue | ForEach-Object {
        if ($_.PSIsContainer -or $_.Name -match 'mls|store|state|sqlite|db|user') {
          Remove-IfExists $_.FullName
        }
      }
    }
  }
}

# Recreate profile dirs AFTER reset (fixes rust temp-dir creation failure)
@($ClientRoot, $ClientTemp, $ClientHome, $ClientState, $ClientCache, $ClientLogs) | ForEach-Object {
  New-Item -ItemType Directory -Force -Path $_ | Out-Null
}

# Force all temp/cache locations into the profile sandbox so multiple clients can run side-by-side
$env:TEMP = $ClientTemp
$env:TMP = $ClientTemp
$env:TMPDIR = $ClientTemp
$env:CARGO_TARGET_DIR = (Join-Path $RepoRoot 'target')  # shared build output is fine; cargo serializes correctly
$env:PROTO_RTC_CLIENT_PROFILE = $SafeProfile
$env:PROTO_RTC_CLIENT_DATA_DIR = $ClientState
$env:PROTO_RTC_CLIENT_HOME = $ClientHome
$env:XDG_DATA_HOME = $ClientState
$env:XDG_CACHE_HOME = $ClientCache
$env:XDG_STATE_HOME = $ClientState
$env:HOME = $ClientHome
$env:USERPROFILE = $ClientHome
$env:LOCALAPPDATA = $ClientState
$env:APPDATA = $ClientState

if (-not $env:SERVER_PUBLIC_URL) { $env:SERVER_PUBLIC_URL = 'http://127.0.0.1:8443' }

Write-Host "Launching GUI profile '$SafeProfile'"
Write-Host "  TEMP=$($env:TEMP)"
Write-Host "  CLIENT_DATA=$($env:PROTO_RTC_CLIENT_DATA_DIR)"
Write-Host "  SERVER_PUBLIC_URL=$($env:SERVER_PUBLIC_URL)"

if (Test-Path 'apps/desktop_gui/Cargo.toml') {
  cargo run -p desktop_gui -- @ExtraArgs
} else {
  Write-Warning 'apps/desktop_gui not found; falling back to apps/desktop.'
  if (-not $env:CLI_USERNAME) { $env:CLI_USERNAME = $SafeProfile }
  cargo run -p desktop -- --server-url $env:SERVER_PUBLIC_URL --username $env:CLI_USERNAME @ExtraArgs
}
