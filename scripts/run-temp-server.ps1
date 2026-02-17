param(
  [string]$BindIp,
  [int]$Port,
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
        $name  = $parts[0].Trim()
        $value = $parts[1].Trim().Trim('"')
        Set-Item -Path "Env:$name" -Value $value
      }
    }
  }
}

if ($PSBoundParameters.ContainsKey('BindIp') -or $PSBoundParameters.ContainsKey('Port')) {
  $resolvedBindIp = if ($PSBoundParameters.ContainsKey('BindIp')) { $BindIp } else { '127.0.0.1' }
  $resolvedPort = if ($PSBoundParameters.ContainsKey('Port')) { $Port } else { 8443 }
  $env:SERVER_BIND = "$resolvedBindIp`:$resolvedPort"
  if (-not $env:SERVER_PUBLIC_URL) { $env:SERVER_PUBLIC_URL = "http://$resolvedBindIp`:$resolvedPort" }
}

if (-not $env:TEMP_DB) {
  $env:TEMP_DB = Join-Path ([System.IO.Path]::GetTempPath()) ("proto_rtc_temp_server_{0}.db" -f ([System.Guid]::NewGuid().ToString('N')))
}

if (-not $env:SERVER_BIND) { $env:SERVER_BIND = '127.0.0.1:8443' }
if (-not $env:SERVER_PUBLIC_URL) { $env:SERVER_PUBLIC_URL = 'http://127.0.0.1:8443' }

# FIX: avoid sqlite:///C:/... which becomes "/C:/" in some parsers on Windows
$env:DATABASE_URL = "sqlite:$($env:TEMP_DB)"

if ($env:SERVER_BIND) { $env:APP__BIND_ADDR = $env:SERVER_BIND }
elseif (-not $env:APP__BIND_ADDR) { $env:APP__BIND_ADDR = '127.0.0.1:8443' }
$env:APP__DATABASE_URL = $env:DATABASE_URL
if (-not $env:APP__LIVEKIT_API_KEY) { $env:APP__LIVEKIT_API_KEY = $(if ($env:LIVEKIT_API_KEY) { $env:LIVEKIT_API_KEY } else { 'devkey' }) }
if (-not $env:APP__LIVEKIT_API_SECRET) { $env:APP__LIVEKIT_API_SECRET = $(if ($env:LIVEKIT_API_SECRET) { $env:LIVEKIT_API_SECRET } else { 'devsecret' }) }
if (-not $env:APP__LIVEKIT_URL -and $env:LIVEKIT_URL) { $env:APP__LIVEKIT_URL = $env:LIVEKIT_URL }

Write-Host "Starting temporary server with DB: $($env:TEMP_DB)"
Write-Host "Bind: $($env:SERVER_BIND)"
Write-Host "DATABASE_URL: $($env:DATABASE_URL)"

try {
  cargo run -p server -- @ExtraArgs
} finally {
  if (Test-Path $env:TEMP_DB) {
    Remove-Item -Path $env:TEMP_DB -Force -ErrorAction SilentlyContinue
  }
}
