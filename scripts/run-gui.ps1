$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir '..')).Path
Set-Location $RepoRoot

New-Item -ItemType Directory -Force -Path 'data' | Out-Null
New-Item -ItemType Directory -Force -Path 'logs' | Out-Null

if (Test-Path '.env') {
  Get-Content '.env' | ForEach-Object {
    if ($_ -match '^\s*#' -or $_ -match '^\s*$') { continue }
    $parts = $_ -split '=', 2
    if ($parts.Count -eq 2) {
      [System.Environment]::SetEnvironmentVariable($parts[0].Trim(), $parts[1].Trim().Trim('"'), 'Process')
    }
  }
}

if (-not $env:SERVER_PUBLIC_URL) { $env:SERVER_PUBLIC_URL = 'http://127.0.0.1:8443' }

if (Test-Path 'apps/desktop_gui/Cargo.toml') {
  cargo run -p desktop_gui -- @args
} else {
  Write-Warning 'apps/desktop_gui not found; falling back to apps/desktop.'
  if (-not $env:CLI_USERNAME) { $env:CLI_USERNAME = 'gui-user' }
  cargo run -p desktop -- --server-url $env:SERVER_PUBLIC_URL --username $env:CLI_USERNAME @args
}
