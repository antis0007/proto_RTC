param(
  [switch]$ResetLocalState = $false,
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$ExtraArgs
)

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir '..')).Path
Set-Location $RepoRoot

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

if ($ResetLocalState) {
  # Repo-local GUI/client state
  Get-ChildItem -Path (Join-Path $RepoRoot 'data') -Force -ErrorAction SilentlyContinue | ForEach-Object {
    Remove-IfExists $_.FullName
  }

  # Common per-user dirs (best effort)
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

if (-not $env:SERVER_PUBLIC_URL) { $env:SERVER_PUBLIC_URL = 'http://127.0.0.1:8443' }

if (Test-Path 'apps/desktop_gui/Cargo.toml') {
  cargo run -p desktop_gui -- @ExtraArgs
} else {
  Write-Warning 'apps/desktop_gui not found; falling back to apps/desktop.'
  if (-not $env:CLI_USERNAME) { $env:CLI_USERNAME = 'gui-user' }
  cargo run -p desktop -- --server-url $env:SERVER_PUBLIC_URL --username $env:CLI_USERNAME @ExtraArgs
}
