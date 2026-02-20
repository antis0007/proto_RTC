param(
    [ValidateSet("Temp", "Permanent", "None")]
    [string]$ServerMode = "Temp",

    [int]$ClientCount = 2,

    [string[]]$Usernames = @("alice", "bob"),

    [string]$ServerUrl = "http://127.0.0.1:8443",

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
$ClientsRoot = Join-Path $DataRoot "clients"
$UploadsRoot = Join-Path $DataRoot "uploads"
$ServerDataRoot = Join-Path $DataRoot "server"
$LogsRoot = Join-Path $RepoRoot "logs"

New-Item -ItemType Directory -Force -Path $DataRoot, $ClientsRoot, $UploadsRoot, $ServerDataRoot, $LogsRoot | Out-Null

# ----------------------------
# Helper: ensure usernames list length
# ----------------------------
if ($ClientCount -lt 1) {
    throw "ClientCount must be >= 1"
}

if ($Usernames.Count -lt $ClientCount) {
    $start = $Usernames.Count + 1
    for ($i = $start; $i -le $ClientCount; $i++) {
        $Usernames += "user$i"
    }
}

$Usernames = $Usernames[0..($ClientCount - 1)]

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

# Temp DB path (only used in temp mode)
$TempDbPath = Join-Path ([System.IO.Path]::GetTempPath()) ("proto_rtc_temp_server_{0}.db" -f ([System.Guid]::NewGuid().ToString("N")))
if ($ResetDb -and (Test-Path $TempDbPath)) {
    Remove-Item -Force $TempDbPath -ErrorAction SilentlyContinue
}

# ----------------------------
# Build once (avoids exe lock from cargo run per client)
# ----------------------------
$GuiProfile = if ($Release) { "release" } else { "debug" }
$GuiExe = Join-Path $RepoRoot "target\$GuiProfile\desktop_gui.exe"

if (-not $NoBuild) {
    Write-Host "Building desktop GUI once ($GuiProfile)..."
    if ($Release) {
        cargo build -p desktop_gui --release
    } else {
        cargo build -p desktop_gui
    }
    if ($LASTEXITCODE -ne 0) { throw "GUI build failed." }
}

if (-not (Test-Path $GuiExe)) {
    throw "GUI executable not found: $GuiExe"
}

# ----------------------------
# Launch server (optional)
# ----------------------------
$ServerProc = $null
if (-not $NoServer -and $ServerMode -ne "None") {
    $ServerEnv = @{}
    $ServerLog = Join-Path $LogsRoot ("server_{0}.log" -f (Get-Date -Format "yyyyMMdd_HHmmss"))

    switch ($ServerMode) {
        "Temp" {
            # Force temp/local server settings
            $ServerEnv["DATABASE_URL"] = "sqlite:$TempDbPath"
            $ServerEnv["SERVER_BIND_ADDR"] = "127.0.0.1:8443"
            $ServerEnv["SERVER_PUBLIC_URL"] = $ServerUrl

            if ($ResetDb) {
                # If your server expects a file path inside sqlite:..., delete the file path portion
                $dbFilePath = $TempDbPath
                if (Test-Path $dbFilePath) {
                    Remove-Item -Force $dbFilePath -ErrorAction SilentlyContinue
                }
            }

            Write-Host "Starting TEMP server..."
            Write-Host "  DATABASE_URL=$($ServerEnv["DATABASE_URL"])"
            Write-Host "  SERVER_BIND_ADDR=$($ServerEnv["SERVER_BIND_ADDR"])"
            Write-Host "  SERVER_PUBLIC_URL=$($ServerEnv["SERVER_PUBLIC_URL"])"
        }
        "Permanent" {
            Write-Host "Starting PERMANENT/local-config server (uses your normal env/.env defaults)"
            # No forced overrides here unless you want them.
        }
    }

    # Build server first to reduce startup delays
    if ($Release) {
        cargo build -p server --release
    } else {
        cargo build -p server
    }
    if ($LASTEXITCODE -ne 0) { throw "Server build failed." }

    # Start server with env overrides (Start-Process -Environment may not exist on older PS)
    # So we launch via powershell child process and set env in that process.
    $serverCmdLines = @()
    foreach ($kv in $ServerEnv.GetEnumerator()) {
        $escapedVal = $kv.Value.Replace("'", "''")
        $serverCmdLines += "`$env:$($kv.Key) = '$escapedVal'"
    }

    $serverCargoArgs = if ($Release) { "run --release -p server" } else { "run -p server" }
    $serverCmdLines += "Set-Location '$($RepoRoot.Replace("'", "''"))'"
    $serverCmdLines += "cargo $serverCargoArgs *>> '$($ServerLog.Replace("'", "''"))'"

    $serverPsScript = $serverCmdLines -join "; "

    $ServerProc = Start-Process -FilePath "powershell.exe" `
        -ArgumentList @("-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", $serverPsScript) `
        -PassThru `
        -WindowStyle Normal

    Write-Host "Server launched (PID=$($ServerProc.Id)). Log: $ServerLog"
    Start-Sleep -Seconds 2
}

# ----------------------------
# Launch N clients with isolated per-user dirs
# ----------------------------
Write-Host ""
Write-Host "Launching $ClientCount client(s) against $ServerUrl"
Write-Host "GUI binary: $GuiExe"
Write-Host ""

$ClientProcs = @()

for ($i = 0; $i -lt $ClientCount; $i++) {
    $username = $Usernames[$i]
    $clientRoot = Join-Path $ClientsRoot $username

    # Create isolated per-client "profile" roots.
    # GUI code resolves MLS dir from HOME/USERPROFILE/LOCALAPPDATA, so we override all.
    $homeRoot = Join-Path $clientRoot "home"
    $userProfileRoot = Join-Path $clientRoot "profile"
    $localAppDataRoot = Join-Path $clientRoot "localappdata"
    $tempRoot = Join-Path $clientRoot "temp"
    $tmpRoot = Join-Path $clientRoot "tmp"

    New-Item -ItemType Directory -Force -Path `
        $clientRoot, $homeRoot, $userProfileRoot, $localAppDataRoot, $tempRoot, $tmpRoot | Out-Null

    # Helpful place for GUI state if HOME is used:
    New-Item -ItemType Directory -Force -Path (Join-Path $homeRoot ".proto_rtc") | Out-Null

    $clientLog = Join-Path $LogsRoot ("gui_{0}_{1}.log" -f $username, (Get-Date -Format "yyyyMMdd_HHmmss"))

    # Launch GUI in a child PowerShell with per-process env overrides.
    # This gives each client its own MLS keystore + temp dirs without separate builds.
    $clientCmd = @(
        "`$env:HOME = '$($homeRoot.Replace("'", "''"))'",
        "`$env:USERPROFILE = '$($userProfileRoot.Replace("'", "''"))'",
        "`$env:LOCALAPPDATA = '$($localAppDataRoot.Replace("'", "''"))'",
        "`$env:TEMP = '$($tempRoot.Replace("'", "''"))'",
        "`$env:TMP = '$($tmpRoot.Replace("'", "''"))'",
        # Optional quality-of-life vars:
        "`$env:RUST_LOG = 'info'",
        "Set-Location '$($RepoRoot.Replace("'", "''"))'",
        "& '$($GuiExe.Replace("'", "''"))' *>> '$($clientLog.Replace("'", "''"))'"
    ) -join "; "

    $p = Start-Process -FilePath "powershell.exe" `
        -ArgumentList @("-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", $clientCmd) `
        -PassThru `
        -WindowStyle Normal

    $ClientProcs += $p

    Write-Host ("[{0}] {1} -> PID {2}" -f $i, $username, $p.Id)
    Write-Host ("     HOME={0}" -f $homeRoot)
    Write-Host ("     USERPROFILE={0}" -f $userProfileRoot)
    Write-Host ("     LOCALAPPDATA={0}" -f $localAppDataRoot)
    Write-Host ("     TEMP={0}" -f $tempRoot)
    Write-Host ("     Log={0}" -f $clientLog)

    Start-Sleep -Milliseconds 500
}

Write-Host ""
Write-Host "Done."
Write-Host "Login in each GUI with:"
Write-Host "  Server URL: $ServerUrl"
Write-Host "  Usernames : $($Usernames -join ', ')"
Write-Host ""
if ($ServerProc) {
    Write-Host "Server PID: $($ServerProc.Id)"
}
Write-Host "Tip: To run remote clients, copy/build the GUI on those machines and run this script with:"
Write-Host "  -NoServer -ServerMode None -ServerUrl http://<your-server-ip>:8443"
