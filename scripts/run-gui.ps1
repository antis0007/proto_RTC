param(
  [switch]$ResetDb = $true,
  [switch]$ResetUploads = $true,
  [switch]$ResetClientState = $false,
  [switch]$Build = $false,
  [string]$BindIp = '127.0.0.1',
  [int]$Port = 8443,
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$ExtraArgs
)

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = (Resolve-Path (Join-Path $ScriptDir '..')).Path
Set-Location $RepoRoot

function Remove-IfExists([string]$PathToRemove) {
  if (Test-Path $PathToRemove) {
    Remove-Item -Path $PathToRemove -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "Deleted: $PathToRemove"
  }
}

# Load .env (optional)
if (Test-Path '.env') {
  Get-Content '.env' | ForEach-Object {
    if ($_ -notmatch '^\s*#' -and $_ -notmatch '^\s*$') {
      $parts = $_ -split '=', 2
      if ($parts.Count -eq 2) {
        $name = $parts[0].Trim()
        $value = $parts[1].Trim().Trim('"')
        Set-Item -Path "Env:$name" -Value $value
      }
    }
  }
}

New-Item -ItemType Directory -Force -Path (Join-Path $RepoRoot 'logs') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $RepoRoot 'data') | Out-Null

# Optional wipe of repo-local client profile state
if ($ResetClientState) {
  $clientsRoot = Join-Path $RepoRoot 'data\clients'
  if (Test-Path $clientsRoot) {
    Get-ChildItem $clientsRoot -Force -ErrorAction SilentlyContinue | ForEach-Object {
      Remove-IfExists $_.FullName
    }
  }
}

# Server uploads dir reset (if your server uses repo-local uploads)
if ($ResetUploads) {
  $uploadsDir = Join-Path $RepoRoot 'data\uploads'
  Remove-IfExists $uploadsDir
  New-Item -ItemType Directory -Force -Path $uploadsDir | Out-Null
}

# Fresh temp DB each launch
if ($ResetDb -or -not $env:TEMP_DB) {
  $env:TEMP_DB = Join-Path ([System.IO.Path]::GetTempPath()) ("proto_rtc_temp_server_{0}.db" -f ([System.Guid]::NewGuid().ToString('N')))
}

# IMPORTANT: set the env names your server actually reads
$env:SERVER_BIND_ADDR   = "$BindIp`:$Port"
$env:SERVER_PUBLIC_URL  = "http://$BindIp`:$Port"
$env:DATABASE_URL       = "sqlite:$($env:TEMP_DB)"

# If your config crate expects APP__* style, set both:
$env:APP__BIND_ADDR     = $env:SERVER_BIND_ADDR
$env:APP__DATABASE_URL  = $env:DATABASE_URL

# Optional LiveKit defaults for local dev
if (-not $env:APP__LIVEKIT_API_KEY)    { $env:APP__LIVEKIT_API_KEY = 'devkey' }
if (-not $env:APP__LIVEKIT_API_SECRET) { $env:APP__LIVEKIT_API_SECRET = 'devsecret' }

$ServerExe = Join-Path $RepoRoot 'target\debug\server.exe'

Write-Host "Starting temp server with:"
Write-Host "  SERVER_BIND_ADDR=$($env:SERVER_BIND_ADDR)"
Write-Host "  SERVER_PUBLIC_URL=$($env:SERVER_PUBLIC_URL)"
Write-Host "  DATABASE_URL=$($env:DATABASE_URL)"

if ($Build -or -not (Test-Path $ServerExe)) {
  Write-Host "Building server..."
  & cargo build -p server
  if ($LASTEXITCODE -ne 0) { throw "cargo build -p server failed" }
}

# Launch built server directly (same reason as GUI: avoid cargo lock churn)
& $ServerExe @ExtraArgs
exit $LASTEXITCODE
