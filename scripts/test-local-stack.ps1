$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir '..')).Path
Set-Location $RepoRoot

New-Item -ItemType Directory -Force -Path 'data' | Out-Null
New-Item -ItemType Directory -Force -Path 'logs' | Out-Null
New-Item -ItemType Directory -Force -Path 'artifacts/local-stack' | Out-Null

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

if (-not $env:SERVER_BIND) { $env:SERVER_BIND = '127.0.0.1:8443' }
if (-not $env:SERVER_PUBLIC_URL) { $env:SERVER_PUBLIC_URL = 'http://127.0.0.1:8443' }
if (-not $env:DATABASE_URL) { $env:DATABASE_URL = 'sqlite://./data/server.db' }
if ($env:SERVER_BIND) { $env:APP__BIND_ADDR = $env:SERVER_BIND }
elseif (-not $env:APP__BIND_ADDR) { $env:APP__BIND_ADDR = '127.0.0.1:8443' }
if (-not $env:APP__DATABASE_URL) { $env:APP__DATABASE_URL = $env:DATABASE_URL }
if (-not $env:APP__LIVEKIT_API_KEY) { $env:APP__LIVEKIT_API_KEY = $(if ($env:LIVEKIT_API_KEY) { $env:LIVEKIT_API_KEY } else { 'devkey' }) }
if (-not $env:APP__LIVEKIT_API_SECRET) { $env:APP__LIVEKIT_API_SECRET = $(if ($env:LIVEKIT_API_SECRET) { $env:LIVEKIT_API_SECRET } else { 'devsecret' }) }
if (-not $env:APP__LIVEKIT_URL -and $env:LIVEKIT_URL) { $env:APP__LIVEKIT_URL = $env:LIVEKIT_URL }

$ServerLog = 'logs/test-local-server.log'
$ServerErrLog = 'logs/test-local-server.err.log'
$Client1Log = 'logs/test-local-client-1.log'
$Client2Log = 'logs/test-local-client-2.log'
$ArtifactDir = 'artifacts/local-stack'
Get-ChildItem -Path $ArtifactDir -File -ErrorAction SilentlyContinue | Remove-Item -Force
$RunSummary = Join-Path $ArtifactDir 'run-summary.log'
Set-Content -Path $ServerLog -Value ''
Set-Content -Path $ServerErrLog -Value ''
Set-Content -Path $Client1Log -Value '[client1] local stack smoke run'
Set-Content -Path $Client2Log -Value '[client2] local stack smoke run'
Set-Content -Path $RunSummary -Value '[run] local stack smoke run'

function Write-ClientLog {
  param(
    [string]$Path,
    [string]$Message
  )
  Add-Content -Path $Path -Value $Message
  Write-Host $Message
}

function Write-RunOk {
  param([string]$Step)
  Add-Content -Path $RunSummary -Value "[OK] $Step"
  Write-Host "[OK] $Step"
}

function Invoke-ApiRequest {
  param(
    [string]$Method,
    [string]$Uri,
    [int]$ExpectedStatus,
    [string]$OutFile,
    [string]$Body,
    [string]$ContentType,
    [switch]$UseInFile,
    [string]$InFile
  )

  $params = @{
    Uri = $Uri
    Method = $Method
    TimeoutSec = 10
  }
  if ($PSBoundParameters.ContainsKey('Body') -and $null -ne $Body) {
    $params.Body = $Body
  }
  if ($PSBoundParameters.ContainsKey('ContentType') -and $ContentType) {
    $params.ContentType = $ContentType
  }
  if ($UseInFile -and $InFile) {
    $params.InFile = $InFile
  }

  try {
    $resp = Invoke-WebRequest @params
    if ($resp.StatusCode -ne $ExpectedStatus) {
      throw "Expected status $ExpectedStatus but got $($resp.StatusCode) for $Method $Uri"
    }
    if ($OutFile) {
      if ($resp.Content) {
        Set-Content -Path $OutFile -Value $resp.Content -NoNewline
      } else {
        Set-Content -Path $OutFile -Value ''
      }
    }
    return $resp
  } catch {
    if ($_.Exception.Response) {
      $statusCode = [int]$_.Exception.Response.StatusCode
      $reader = New-Object System.IO.StreamReader($_.Exception.Response.GetResponseStream())
      $bodyText = $reader.ReadToEnd()
      if ($OutFile) { Set-Content -Path $OutFile -Value $bodyText }
      throw "Request failed: $Method $Uri (expected $ExpectedStatus got $statusCode). Body: $OutFile"
    }
    throw
  }
}

$serverProcess = Start-Process -FilePath 'cargo' -ArgumentList @('run', '-p', 'server') -RedirectStandardOutput $ServerLog -RedirectStandardError $ServerErrLog -PassThru
$success = $false

try {
  Write-Host "Started server (pid=$($serverProcess.Id)). Waiting for /healthz on $($env:SERVER_PUBLIC_URL) ..."
  $healthy = $false
  for ($i = 0; $i -lt 180; $i++) {
    try {
      $health = Invoke-WebRequest -Uri "$($env:SERVER_PUBLIC_URL)/healthz" -UseBasicParsing -TimeoutSec 2
      if ($health.StatusCode -eq 200) {
        Set-Content -Path (Join-Path $ArtifactDir 'healthz.txt') -Value $health.Content -NoNewline
        $healthy = $true
        break
      }
    } catch {
      Start-Sleep -Seconds 1
    }
  }
  if (-not $healthy) {
    throw "Server never became healthy. See $ServerLog"
  }
  Write-RunOk -Step 'server boot + /healthz'

  $client1 = if ($env:CLIENT1_USERNAME) { $env:CLIENT1_USERNAME } else { 'local-user-1' }
  $client2 = if ($env:CLIENT2_USERNAME) { $env:CLIENT2_USERNAME } else { 'local-user-2' }

  $login1File = Join-Path $ArtifactDir 'client1-login.json'
  $login1 = Invoke-ApiRequest -Method 'POST' -Uri "$($env:SERVER_PUBLIC_URL)/login" -ExpectedStatus 200 -OutFile $login1File -Body (@{ username = $client1 } | ConvertTo-Json -Compress) -ContentType 'application/json'
  $user1Id = ((Get-Content $login1File -Raw) | ConvertFrom-Json).user_id
  Write-ClientLog -Path $Client1Log -Message "[OK] client1 login user_id=$user1Id"

  $login2File = Join-Path $ArtifactDir 'client2-login.json'
  $login2 = Invoke-ApiRequest -Method 'POST' -Uri "$($env:SERVER_PUBLIC_URL)/login" -ExpectedStatus 200 -OutFile $login2File -Body (@{ username = $client2 } | ConvertTo-Json -Compress) -ContentType 'application/json'
  $user2Id = ((Get-Content $login2File -Raw) | ConvertFrom-Json).user_id
  Write-ClientLog -Path $Client2Log -Message "[OK] client2 login user_id=$user2Id"
  Write-RunOk -Step 'two-user login'

  $guilds1File = Join-Path $ArtifactDir 'client1-guilds.json'
  Invoke-ApiRequest -Method 'GET' -Uri "$($env:SERVER_PUBLIC_URL)/guilds?user_id=$user1Id" -ExpectedStatus 200 -OutFile $guilds1File | Out-Null
  $guildId = ((Get-Content $guilds1File -Raw) | ConvertFrom-Json)[0].guild_id
  Write-ClientLog -Path $Client1Log -Message "[OK] client1 guild selected guild_id=$guildId"

  $inviteFile = Join-Path $ArtifactDir 'client1-invite.json'
  Invoke-ApiRequest -Method 'POST' -Uri "$($env:SERVER_PUBLIC_URL)/guilds/$guildId/invites?user_id=$user1Id" -ExpectedStatus 200 -OutFile $inviteFile | Out-Null
  $inviteCode = ((Get-Content $inviteFile -Raw) | ConvertFrom-Json).invite_code
  Write-ClientLog -Path $Client1Log -Message '[OK] invite created'

  $joinFile = Join-Path $ArtifactDir 'client2-join.txt'
  Invoke-ApiRequest -Method 'POST' -Uri "$($env:SERVER_PUBLIC_URL)/guilds/join" -ExpectedStatus 204 -OutFile $joinFile -Body (@{ user_id = [int64]$user2Id; invite_code = $inviteCode } | ConvertTo-Json -Compress) -ContentType 'application/json' | Out-Null
  Write-ClientLog -Path $Client2Log -Message "[OK] client2 joined guild_id=$guildId"

  $channels1File = Join-Path $ArtifactDir 'client1-channels.json'
  Invoke-ApiRequest -Method 'GET' -Uri "$($env:SERVER_PUBLIC_URL)/guilds/$guildId/channels?user_id=$user1Id" -ExpectedStatus 200 -OutFile $channels1File | Out-Null
  $channelId = ((Get-Content $channels1File -Raw) | ConvertFrom-Json)[0].channel_id
  Write-ClientLog -Path $Client1Log -Message "[OK] client1 channel selected channel_id=$channelId"

  $channels2File = Join-Path $ArtifactDir 'client2-channels.json'
  Invoke-ApiRequest -Method 'GET' -Uri "$($env:SERVER_PUBLIC_URL)/guilds/$guildId/channels?user_id=$user2Id" -ExpectedStatus 200 -OutFile $channels2File | Out-Null
  Write-ClientLog -Path $Client2Log -Message "[OK] client2 channel selected channel_id=$channelId"
  Write-RunOk -Step 'channel selection'

  $messageB64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes('pre-e2ee-local-smoke-message'))
  $sendFile = Join-Path $ArtifactDir 'client1-send-message.json'
  Invoke-ApiRequest -Method 'POST' -Uri "$($env:SERVER_PUBLIC_URL)/messages" -ExpectedStatus 200 -OutFile $sendFile -Body (@{ user_id = [int64]$user1Id; guild_id = [int64]$guildId; channel_id = [int64]$channelId; ciphertext_b64 = $messageB64 } | ConvertTo-Json -Compress) -ContentType 'application/json' | Out-Null
  Write-ClientLog -Path $Client1Log -Message '[OK] client1 sent message'

  $messagesFile = Join-Path $ArtifactDir 'client2-messages.json'
  Invoke-ApiRequest -Method 'GET' -Uri "$($env:SERVER_PUBLIC_URL)/channels/$channelId/messages?user_id=$user2Id&limit=20" -ExpectedStatus 200 -OutFile $messagesFile | Out-Null
  $messages = (Get-Content $messagesFile -Raw) | ConvertFrom-Json
  $received = $false
  foreach ($msg in $messages) {
    if ($msg.sender_id -eq [int64]$user1Id -and $msg.ciphertext_b64 -eq $messageB64) {
      $received = $true
      break
    }
  }
  if (-not $received) {
    throw 'Message send/receive verification failed.'
  }
  Write-ClientLog -Path $Client2Log -Message '[OK] client2 received message'
  Write-RunOk -Step 'message send/receive'

  $uploadSource = Join-Path $ArtifactDir 'upload.bin'
  $downloadTarget = Join-Path $ArtifactDir 'client2-download.bin'
  [IO.File]::WriteAllBytes($uploadSource, [Text.Encoding]::UTF8.GetBytes('pre-e2ee-upload-download-smoke'))
  $uploadFile = Join-Path $ArtifactDir 'client1-upload.json'
  Invoke-ApiRequest -Method 'POST' -Uri "$($env:SERVER_PUBLIC_URL)/files/upload?user_id=$user1Id&guild_id=$guildId&channel_id=$channelId&filename=smoke.bin&mime_type=application%2Foctet-stream" -ExpectedStatus 200 -OutFile $uploadFile -UseInFile -InFile $uploadSource | Out-Null
  $fileId = ((Get-Content $uploadFile -Raw) | ConvertFrom-Json).file_id
  Write-ClientLog -Path $Client1Log -Message "[OK] upload file_id=$fileId"

  Invoke-ApiRequest -Method 'GET' -Uri "$($env:SERVER_PUBLIC_URL)/files/$fileId?user_id=$user2Id" -ExpectedStatus 200 -OutFile $downloadTarget | Out-Null
  $srcBytes = [IO.File]::ReadAllBytes($uploadSource)
  $dstBytes = [IO.File]::ReadAllBytes($downloadTarget)
  if ($srcBytes.Length -ne $dstBytes.Length) {
    throw 'Download verification failed: size mismatch.'
  }
  for ($i = 0; $i -lt $srcBytes.Length; $i++) {
    if ($srcBytes[$i] -ne $dstBytes[$i]) {
      throw 'Download verification failed: content mismatch.'
    }
  }
  Write-ClientLog -Path $Client2Log -Message '[OK] download content verified'
  Write-RunOk -Step 'upload/download byte-equality'

  $success = $true
} finally {
  if ($serverProcess -and -not $serverProcess.HasExited) {
    Stop-Process -Id $serverProcess.Id -Force
  }

  if ($success) {
    Write-Host 'PASS: local stack smoke gate completed.'
  } else {
    Write-Host 'FAIL: local stack smoke gate failed.'
  }
  Write-Host 'Artifacts collected:'
  Write-Host "  $ServerLog"
  Write-Host "  $ServerErrLog"
  Write-Host "  $Client1Log"
  Write-Host "  $Client2Log"
  Write-Host "  $ArtifactDir"

  if (-not $success) {
    exit 1
  }
}
