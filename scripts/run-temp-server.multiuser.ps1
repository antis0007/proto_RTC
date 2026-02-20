param(
  [switch]$ResetDb = $true,
  [switch]$ResetUploads = $true,
  [switch]$ResetClientState = $false,
  [switch]$KeepDbName = $true,
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$ExtraArgs
)

$ErrorActionPreference = 'Stop'
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir '..')).Path
Set-Location $RepoRoot

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

if (-not $env:TEMP_DB) {
  $baseName = if ($KeepDbName) {
    'proto_rtc_temp_server_fixed.db'
  } else {
    "proto_rtc_temp_server_{0}.db" -f ([System.Guid]::NewGuid().ToString('N'))
  }
  $env:TEMP_DB = Join-Path ([System.IO.Path]::GetTempPath()) $baseName
}

if ($ResetDb -and (Test-Path $env:TEMP_DB)) {
  Remove-Item -Force $env:TEMP_DB
  Write-Host "Deleted temp DB: $($env:TEMP_DB)"
}

$uploadsRoot = Join-Path ([System.IO.Path]::GetTempPath()) 'proto_rtc_uploads'
if ($ResetUploads -and (Test-Path $uploadsRoot)) {
  Remove-Item -Recurse -Force $uploadsRoot
  Write-Host "Deleted uploads dir: $uploadsRoot"
}

if ($ResetClientState) {
  $repoClientData = Join-Path $RepoRoot 'data/clients'
  if (Test-Path $repoClientData) {
    Remove-Item -Recurse -Force $repoClientData
    Write-Host "Deleted client state root: $repoClientData"
  }
  $globalTemp = [System.IO.Path]::GetTempPath()
  Get-ChildItem -Path $globalTemp -Filter 'proto_rtc_client_*' -Force -ErrorAction SilentlyContinue | ForEach-Object {
    Remove-Item -Recurse -Force $_.FullName -ErrorAction SilentlyContinue
    Write-Host "Deleted legacy client temp dir: $($_.FullName)"
  }
}

$env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { 'info' }
$env:SERVER_BIND_ADDR = if ($env:SERVER_BIND_ADDR) { $env:SERVER_BIND_ADDR } else { '0.0.0.0:8443' }
$env:DATABASE_URL = "sqlite:$($env:TEMP_DB)"
if (-not $env:SERVER_PUBLIC_URL) { $env:SERVER_PUBLIC_URL = 'http://127.0.0.1:8443' }

Write-Host 'Starting temp server with:' -ForegroundColor Cyan
Write-Host "  DATABASE_URL=$($env:DATABASE_URL)"
Write-Host "  SERVER_BIND_ADDR=$($env:SERVER_BIND_ADDR)"
Write-Host "  SERVER_PUBLIC_URL=$($env:SERVER_PUBLIC_URL)"

cargo run -p server -- @ExtraArgs
