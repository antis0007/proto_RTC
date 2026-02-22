param(
    [ValidateSet("Temp", "Permanent", "None")]
    [string]$ServerMode = "Temp",

    [string]$ConfigPath = "scripts/multiuser.profiles.json",

    [switch]$ResetDb,
    [switch]$ResetUploads,
    [switch]$ResetClientState,
    [switch]$NoBuild,
    [switch]$NoServer,
    [switch]$Release
)

$ErrorActionPreference = "Stop"

# ----------------------------
# Paths / repo root
# ----------------------------
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir "..")).Path
Set-Location $RepoRoot

$DataRoot = Join-Path $RepoRoot "data"
$ClientsRootDefault = Join-Path $DataRoot "clients"
$UploadsRoot = Join-Path $DataRoot "uploads"
$ServerDataRoot = Join-Path $DataRoot "server"
$LogsRoot = Join-Path $RepoRoot "logs"

New-Item -ItemType Directory -Force -Path $DataRoot, $UploadsRoot, $ServerDataRoot, $LogsRoot | Out-Null

# ----------------------------
# Read multiuser.profiles.json
# Expected shape:
# {
#   "server_url": "http://127.0.0.1:8443",
#   "base_data_dir": "./data/clients",
#   "users": [
#     { "username": "alice", "display_name": "Alice" },
#     { "username": "bob",   "display_name": "Bob" }
#   ]
# }
# ----------------------------
if (-not (Test-Path $ConfigPath)) {
    throw "Config file not found: $ConfigPath"
}

$configRaw = Get-Content -Raw -Path $ConfigPath
if ([string]::IsNullOrWhiteSpace($configRaw)) {
    throw "Config file is empty: $ConfigPath"
}

try {
    $Config = $configRaw | ConvertFrom-Json
} catch {
    throw "Failed to parse JSON config at $ConfigPath. Error: $($_.Exception.Message)"
}

if (-not $Config.users -or $Config.users.Count -lt 1) {
    throw "Config must contain at least one user under 'users'."
}

$ServerUrl = if ($Config.server_url) { [string]$Config.server_url } else { "http://127.0.0.1:8443" }

# Resolve base data dir (can be relative to repo root)
$ClientsRoot = $null
if ($Config.base_data_dir) {
    $baseDataDirRaw = [string]$Config.base_data_dir
    if ([System.IO.Path]::IsPathRooted($baseDataDirRaw)) {
        $ClientsRoot = $baseDataDirRaw
    } else {
        $ClientsRoot = Join-Path $RepoRoot $baseDataDirRaw
    }
} else {
    $ClientsRoot = $ClientsRootDefault
}

New-Item -ItemType Directory -Force -Path $ClientsRoot | Out-Null

# Normalize user list
$Users = @()
foreach ($u in $Config.users) {
    if (-not $u.username) {
        throw "Each user entry must include 'username'."
    }

    $username = [string]$u.username
    $displayName = if ($u.display_name) { [string]$u.display_name } else { $username }

    # Optional per-user overrides if present in JSON
    $userServerUrl = if ($u.server_url) { [string]$u.server_url } else { $ServerUrl }

    $Users += [PSCustomObject]@{
        Username    = $username
        DisplayName = $displayName
        ServerUrl   = $userServerUrl
    }
}

# ----------------------------
# Optional resets
# ----------------------------
if ($ResetClientState) {
    Write-Host "Resetting client state under $ClientsRoot"
    if (Test-Path $ClientsRoot) {
        Remove-Item -Recurse -Force $ClientsRoot
    }
    New-Item -ItemType Directory -Force -Path $ClientsRoot | Out-Null
}

if ($ResetUploads) {
    Write-Host "Resetting uploads under $UploadsRoot"
    if (Test-Path $UploadsRoot) {
        Remove-Item -Recurse -Force $UploadsRoot
    }
    New-Item -ItemType Directory -Force -Path $UploadsRoot | Out-Null
}

# Temp DB path (only used in Temp mode)
$TempDbPath = Join-Path ([System.IO.Path]::GetTempPath()) ("proto_rtc_temp_server_{0}.db" -f ([System.Guid]::NewGuid().ToString("N")))
if ($ResetDb -and (Test-Path $TempDbPath)) {
    Remove-Item -Force $TempDbPath -ErrorAction SilentlyContinue
}

# ----------------------------
# Build once (avoids exe lock from cargo run per client)
# ----------------------------
$ProfileName = if ($Release) { "release" } else { "debug" }
$GuiExe = Join-Path $RepoRoot "target\$ProfileName\desktop_gui.exe"
$ServerExe = Join-Path $RepoRoot "target\$ProfileName\server.exe"

if (-not $NoBuild) {
    Write-Host "Building desktop GUI ($ProfileName)..."
    if ($Release) {
        cargo build -p desktop_gui --release
    } else {
        cargo build -p desktop_gui
    }
    if ($LASTEXITCODE -ne 0) { throw "GUI build failed." }

    if (-not $NoServer -and $ServerMode -ne "None") {
        Write-Host "Building server ($ProfileName)..."
        if ($Release) {
            cargo build -p server --release
        } else {
            cargo build -p server
        }
        if ($LASTEXITCODE -ne 0) { throw "Server build failed." }
    }
}

if (-not (Test-Path $GuiExe)) {
    throw "GUI executable not found: $GuiExe"
}

# ----------------------------
# Launch server (optional)
# - Uses server.exe directly (faster than cargo run)
# - Uses Tee-Object so output goes to BOTH console and log
# - Uses -NoExit so spawned console stays visible (not black/empty)
# ----------------------------
$ServerProc = $null
if (-not $NoServer -and $ServerMode -ne "None") {
    $ServerLog = Join-Path $LogsRoot ("server_{0}.log" -f (Get-Date -Format "yyyyMMdd_HHmmss"))
    $ServerEnv = @{}

    switch ($ServerMode) {
        "Temp" {
            $ServerEnv["DATABASE_URL"] = "sqlite:$TempDbPath"
            $ServerEnv["SERVER_BIND_ADDR"] = "0.0.0.0:8443"
            $ServerEnv["SERVER_PUBLIC_URL"] = $ServerUrl

            if ($ResetDb) {
                if (Test-Path $TempDbPath) {
                    Remove-Item -Force $TempDbPath -ErrorAction SilentlyContinue
                }
            }

            Write-Host "Starting TEMP server..."
            Write-Host "  DATABASE_URL=$($ServerEnv["DATABASE_URL"])"
            Write-Host "  SERVER_BIND_ADDR=$($ServerEnv["SERVER_BIND_ADDR"])"
            Write-Host "  SERVER_PUBLIC_URL=$($ServerEnv["SERVER_PUBLIC_URL"])"
        }
        "Permanent" {
            Write-Host "Starting PERMANENT/local-config server (normal .env / environment applies)"
        }
    }

    if (-not (Test-Path $ServerExe)) {
        throw "Server executable not found: $ServerExe"
    }

    $serverCmdLines = @()
    foreach ($kv in $ServerEnv.GetEnumerator()) {
        $escapedVal = ([string]$kv.Value).Replace("'", "''")
        $serverCmdLines += "`$env:$($kv.Key) = '$escapedVal'"
    }

    $serverCmdLines += "Set-Location '$($RepoRoot.Replace("'", "''"))'"
    $serverCmdLines += "Write-Host '--- proto_RTC server ---'"
    $serverCmdLines += "Write-Host 'Log: $($ServerLog.Replace("'", "''"))'"
    $serverCmdLines += "Write-Host ''"
    $serverCmdLines += "& '$($ServerExe.Replace("'", "''"))' 2>&1 | Tee-Object -FilePath '$($ServerLog.Replace("'", "''"))' -Append"

    $serverPsScript = $serverCmdLines -join "; "

    $ServerProc = Start-Process -FilePath "powershell.exe" `
        -ArgumentList @("-NoProfile", "-ExecutionPolicy", "Bypass", "-NoExit", "-Command", $serverPsScript) `
        -PassThru `
        -WindowStyle Normal

    Write-Host "Server launched (PID=$($ServerProc.Id))"
    Write-Host "Server log: $ServerLog"
    Start-Sleep -Seconds 2
}

# ----------------------------
# Launch clients with isolated per-user app data dirs
# NOTE:
# - No HOME/USERPROFILE hijacking (fixes file picker default path issue)
# - Requires GUI support for:
#     --server-url
#     --username
#     --display-name
#     --data-dir
# - Uses Tee-Object so logs are visible in client console windows
# ----------------------------
Write-Host ""
Write-Host "Launching $($Users.Count) client(s)"
Write-Host "GUI binary: $GuiExe"
Write-Host "Clients root: $ClientsRoot"
Write-Host ""

$ClientProcs = @()

foreach ($user in $Users) {
    $username = $user.Username
    $displayName = $user.DisplayName
    $userServerUrl = $user.ServerUrl

    $clientRoot = Join-Path $ClientsRoot $username
    $clientDataDir = Join-Path $clientRoot "appdata"
    $clientCacheDir = Join-Path $clientRoot "cache"   # optional extra dir if your app uses it later
    $clientLog = Join-Path $LogsRoot ("gui_{0}_{1}.log" -f $username, (Get-Date -Format "yyyyMMdd_HHmmss"))

    New-Item -ItemType Directory -Force -Path $clientRoot, $clientDataDir, $clientCacheDir | Out-Null

    # Build a small script so the window stays open and shows output.
    $clientCmdLines = @(
        "Set-Location '$($RepoRoot.Replace("'", "''"))'",
        "Write-Host '--- proto_RTC desktop_gui ($($username.Replace("'", "''"))) ---'",
        "Write-Host 'Server URL : $($userServerUrl.Replace("'", "''"))'",
        "Write-Host 'Data dir   : $($clientDataDir.Replace("'", "''"))'",
        "Write-Host 'Log        : $($clientLog.Replace("'", "''"))'",
        "Write-Host ''",
        # Optional logging verbosity:
        "`$env:RUST_LOG = 'info'",
        "& '$($GuiExe.Replace("'", "''"))' --server-url '$($userServerUrl.Replace("'", "''"))' --username '$($username.Replace("'", "''"))' --display-name '$($displayName.Replace("'", "''"))' --data-dir '$($clientDataDir.Replace("'", "''"))' 2>&1 | Tee-Object -FilePath '$($clientLog.Replace("'", "''"))' -Append"
    )
    $clientPsScript = $clientCmdLines -join "; "

    $p = Start-Process -FilePath "powershell.exe" `
        -ArgumentList @("-NoProfile", "-ExecutionPolicy", "Bypass", "-NoExit", "-Command", $clientPsScript) `
        -PassThru `
        -WindowStyle Normal

    $ClientProcs += $p

    Write-Host ("User '{0}' launched -> PID {1}" -f $username, $p.Id)
    Write-Host ("  Server URL : {0}" -f $userServerUrl)
    Write-Host ("  Data Dir   : {0}" -f $clientDataDir)
    Write-Host ("  Log        : {0}" -f $clientLog)
    Write-Host ""

    Start-Sleep -Milliseconds 500
}

# ----------------------------
# Summary
# ----------------------------
Write-Host "Done."
Write-Host ""
Write-Host "Users:"
foreach ($user in $Users) {
    Write-Host ("  - {0} (display: {1})" -f $user.Username, $user.DisplayName)
}
Write-Host ""
if ($ServerProc) {
    Write-Host "Server PID: $($ServerProc.Id)"
}
Write-Host ""
Write-Host "Notes:"
Write-Host "  - Client windows stay open and show stdout/stderr (plus logs are saved)."
Write-Host "  - File picker should use the real Windows user's Desktop/Downloads (not fake profile paths)."
Write-Host "  - This script expects desktop_gui to support CLI args:"
Write-Host "      --server-url --username --display-name --data-dir"
Write-Host ""
Write-Host "Remote client tip:"
Write-Host "  Run with -NoServer -ServerMode None and set server_url in multiuser.profiles.json"
