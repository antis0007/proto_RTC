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

if (-not $env:SERVER_BIND) { $env:SERVER_BIND = '127.0.0.1:8443' }
if (-not $env:SERVER_PUBLIC_URL) { $env:SERVER_PUBLIC_URL = 'http://127.0.0.1:8443' }
if (-not $env:DATABASE_URL) { $env:DATABASE_URL = 'sqlite://./data/server.db' }
if (-not $env:APP__BIND_ADDR) { $env:APP__BIND_ADDR = $env:SERVER_BIND }
if (-not $env:APP__DATABASE_URL) { $env:APP__DATABASE_URL = $env:DATABASE_URL }
if (-not $env:APP__LIVEKIT_API_KEY) { $env:APP__LIVEKIT_API_KEY = $(if ($env:LIVEKIT_API_KEY) { $env:LIVEKIT_API_KEY } else { 'devkey' }) }
if (-not $env:APP__LIVEKIT_API_SECRET) { $env:APP__LIVEKIT_API_SECRET = $(if ($env:LIVEKIT_API_SECRET) { $env:LIVEKIT_API_SECRET } else { 'devsecret' }) }
if (-not $env:APP__LIVEKIT_URL -and $env:LIVEKIT_URL) { $env:APP__LIVEKIT_URL = $env:LIVEKIT_URL }

$serverLog = 'logs/test-local-server.log'
$client1Log = 'logs/test-local-client-1.log'
$client2Log = 'logs/test-local-client-2.log'

$serverProcess = Start-Process -FilePath 'cargo' -ArgumentList @('run', '-p', 'server') -RedirectStandardOutput $serverLog -RedirectStandardError $serverLog -PassThru

try {
  Write-Host "Started server (pid=$($serverProcess.Id)). Waiting for /healthz on $($env:SERVER_PUBLIC_URL) ..."
  $healthy = $false
  for ($i = 0; $i -lt 30; $i++) {
    try {
      $response = Invoke-WebRequest -Uri "$($env:SERVER_PUBLIC_URL)/healthz" -UseBasicParsing -TimeoutSec 2
      if ($response.StatusCode -eq 200) {
        $healthy = $true
        break
      }
    } catch {
      Start-Sleep -Seconds 1
    }
  }

  if (-not $healthy) {
    throw "Server never became healthy. See $serverLog"
  }

  Write-Host 'Server is healthy.'

  $client1 = if ($env:CLIENT1_USERNAME) { $env:CLIENT1_USERNAME } else { 'local-user-1' }
  $client2 = if ($env:CLIENT2_USERNAME) { $env:CLIENT2_USERNAME } else { 'local-user-2' }

  $client1Output = & cargo run -p desktop -- --server-url $env:SERVER_PUBLIC_URL --username $client1 2>&1
  $client1Output | Out-File -FilePath $client1Log -Encoding utf8

  $client2Output = & cargo run -p desktop -- --server-url $env:SERVER_PUBLIC_URL --username $client2 2>&1
  $client2Output | Out-File -FilePath $client2Log -Encoding utf8

  Write-Host "Local stack smoke test passed. Logs:"
  Write-Host "  $serverLog"
  Write-Host "  $client1Log"
  Write-Host "  $client2Log"
} finally {
  if ($serverProcess -and -not $serverProcess.HasExited) {
    Stop-Process -Id $serverProcess.Id -Force
  }
}
