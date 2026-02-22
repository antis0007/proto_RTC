param(
    [string]$BindIp = "0.0.0.0",
    [int]$Port = 8443,

    # IMPORTANT:
    # For internet clients, set this to your public IP or domain (and port if not 80/443).
    # Examples:
    #   http://203.0.113.10:8443
    #   https://chat.yourdomain.com
    [string]$PublicServerUrl = "",

    # Local GUI account to prefill
    [string]$Username = "alice",

    # Server storage mode
    [ValidateSet("Temp", "Permanent")]
    [string]$ServerMode = "Temp",

    # Optional resets
    [switch]$ResetDb,
    [switch]$ResetUploads,
    [switch]$ResetClientState,

    # Build/runtime options
    [switch]$NoBuild,
    [switch]$Release,

    # If set, do not launch the local GUI, only the server
    [switch]$ServerOnly
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
# Defaults / derived values
# ----------------------------
if ([string]::IsNullOrWhiteSpace($PublicServerUrl)) {
    # Default local URL for convenience. For internet use, set this explicitly.
    $PublicServerUrl = "http://127.0.0.1:$Port"
}

$ProfileName = if ($Release) { "release" } else { "debug" }

$GuiExe = Join-Path $RepoRoot "target\$ProfileName\desktop_gui.exe"
$ServerExe = Join-Path $RepoRoot "target\$ProfileName\server.exe"

# Temp DB path (fixed path so -ResetDb is meaningful)
$TempDbPath = Join-Path $ServerDataRoot "proto_rtc_temp_server.db"

# Per-user local client state root (only for app-specific state if you want isolation)
$ClientRoot = Join-Path $ClientsRoot $Username
$ClientHomeRoot = Join-Path $ClientRoot "home"

# ----------------------------
# Optional resets
# ----------------------------
if ($ResetClientState) {
    Write-Host "Resetting client state for '$Username' under $ClientRoot"
    if (Test-Path $ClientRoot) {
        Remove-Item -Recurse -Force $ClientRoot
    }
}
New-Item -ItemType Directory -Force -Path $ClientRoot, $ClientHomeRoot, (Join-Path $ClientHomeRoot ".proto_rtc") | Out-Null

if ($ResetUploads) {
    Write-Host "Resetting uploads under $UploadsRoot"
    if (Test-Path $UploadsRoot) {
        Remove-Item -Recurse -Force $UploadsRoot
    }
}
New-Item -ItemType Directory -Force -Path $UploadsRoot | Out-Null

if ($ResetDb) {
    if ($ServerMode -eq "Temp") {
        if (Test-Path $TempDbPath) {
            Write-Host "Deleting temp DB: $TempDbPath"
            Remove-Item -Force $TempDbPath -ErrorAction SilentlyContinue
        }
    } else {
        Write-Warning "ResetDb was requested, but ServerMode=Permanent."
        Write-Warning "This script will NOT auto-delete permanent DB files (safer default)."
        Write-Warning "Delete the permanent DB manually based on your DATABASE_URL (.env / env var)."
    }
}

# ----------------------------
# Build
# ----------------------------
if (-not $NoBuild) {
    Write-Host "Building server and desktop GUI ($ProfileName)..."
    if ($Release) {
        cargo build -p server --release
        if ($LASTEXITCODE -ne 0) { throw "Server build failed." }

        if (-not $ServerOnly) {
            cargo build -p desktop_gui --release
            if ($LASTEXITCODE -ne 0) { throw "GUI build failed." }
        }
    } else {
        cargo build -p server
        if ($LASTEXITCODE -ne 0) { throw "Server build failed." }

        if (-not $ServerOnly) {
            cargo build -p desktop_gui
            if ($LASTEXITCODE -ne 0) { throw "GUI build failed." }
        }
    }
}

if (-not (Test-Path $ServerExe)) { throw "Server executable not found: $ServerExe" }
if (-not $ServerOnly -and -not (Test-Path $GuiExe)) { throw "GUI executable not found: $GuiExe" }

# ----------------------------
# Warn if PublicServerUrl is localhost while binding publicly
# ----------------------------
if ($BindIp -eq "0.0.0.0" -and ($PublicServerUrl -match "127\.0\.0\.1|localhost")) {
    Write-Warning "You are binding publicly, but PublicServerUrl points to localhost."
    Write-Warning "Remote clients will not be able to use that URL."
    Write-Warning "Set -PublicServerUrl to your public IP or domain, e.g. http://104.205.186.27:$Port"
}

# ----------------------------
# Start server in a visible console
# ----------------------------
$ServerLog = Join-Path $LogsRoot ("server_{0}.log" -f (Get-Date -Format "yyyyMMdd_HHmmss"))

$ServerEnv = @{}
$bind = "$BindIp`:$Port"

# Correct env vars for your server config loader
$ServerEnv["SERVER_BIND"] = $bind
$ServerEnv["APP__BIND_ADDR"] = $bind

# Public URL (this one is already correct)
$ServerEnv["SERVER_PUBLIC_URL"] = $PublicServerUrl

# Optional but useful
$ServerEnv["NO_PROXY"] = "127.0.0.1,localhost"

if ($ServerMode -eq "Temp") {
    $ServerEnv["DATABASE_URL"] = "sqlite:$TempDbPath"
}

Write-Host ""
Write-Host "Starting server (visible console)..."
Write-Host "  SERVER_BIND_ADDR = $($ServerEnv["SERVER_BIND_ADDR"])"
Write-Host "  SERVER_PUBLIC_URL = $($ServerEnv["SERVER_PUBLIC_URL"])"
if ($ServerEnv.ContainsKey("DATABASE_URL")) {
    Write-Host "  DATABASE_URL      = $($ServerEnv["DATABASE_URL"])"
} else {
    Write-Host "  DATABASE_URL      = (from your .env / environment)"
}
Write-Host ""

# Launch a new PowerShell window and keep it open.
# We set env vars in that process before starting the server exe.
$serverCmdLines = @()
foreach ($kv in $ServerEnv.GetEnumerator()) {
    $escapedVal = $kv.Value.Replace("'", "''")
    $serverCmdLines += "`$env:$($kv.Key) = '$escapedVal'"
}
$serverCmdLines += "Set-Location '$($RepoRoot.Replace("'", "''"))'"
$serverCmdLines += "Write-Host '=== proto_RTC server ===' -ForegroundColor Cyan"
$serverCmdLines += "Write-Host ('Bind: ' + `$env:SERVER_BIND_ADDR)"
$serverCmdLines += "Write-Host ('Public URL: ' + `$env:SERVER_PUBLIC_URL)"
$serverCmdLines += "if (`$env:DATABASE_URL) { Write-Host ('DB: ' + `$env:DATABASE_URL) }"
$serverCmdLines += "Write-Host ''"
$serverCmdLines += "Write-Host 'TIP: Check startup log for bind_addr=0.0.0.0:$Port' -ForegroundColor Yellow"
$serverCmdLines += "Write-Host ''"
$serverCmdLines += "& '$($ServerExe.Replace("'", "''"))' 2>&1 | Tee-Object -FilePath '$($ServerLog.Replace("'", "''"))'"
$serverCmdLines += "Write-Host ''"
$serverCmdLines += "Write-Host 'Server exited. Press any key to close...' -ForegroundColor Yellow"
$serverCmdLines += "`$null = `$Host.UI.RawUI.ReadKey('NoEcho,IncludeKeyDown')"

$serverPsScript = $serverCmdLines -join "; "

$ServerProc = Start-Process -FilePath "powershell.exe" `
    -ArgumentList @("-NoProfile", "-ExecutionPolicy", "Bypass", "-NoExit", "-Command", $serverPsScript) `
    -PassThru `
    -WindowStyle Normal

Write-Host "Server window launched (PID=$($ServerProc.Id))"
Write-Host "Server log file (also shown live in server console): $ServerLog"

# Give the server a moment to start
Start-Sleep -Seconds 2

# ----------------------------
# Start local GUI in visible console
# ----------------------------
if (-not $ServerOnly) {
    $ClientLog = Join-Path $LogsRoot ("gui_{0}_{1}.log" -f $Username, (Get-Date -Format "yyyyMMdd_HHmmss"))

    Write-Host ""
    Write-Host "Starting local GUI client (visible console)..."
    Write-Host "  Username (prefill): $Username"
    Write-Host "  Server URL:         $PublicServerUrl"
    Write-Host ""

    # Notes:
    # - We DO NOT override USERPROFILE/Desktop/etc. so file picker opens to the real user folders.
    # - We only set HOME + app-specific vars for app state isolation.
    # - If your GUI supports CLI args like --server-url / --username, this passes them.
    #   If not implemented yet, it should still run.
    $clientCmdLines = @()
    $clientCmdLines += "`$env:HOME = '$($ClientHomeRoot.Replace("'", "''"))'"
    $clientCmdLines += "`$env:PROTO_RTC_DEFAULT_SERVER_URL = '$($PublicServerUrl.Replace("'", "''"))'"
    $clientCmdLines += "`$env:PROTO_RTC_DEFAULT_USERNAME = '$($Username.Replace("'", "''"))'"
    $clientCmdLines += "Set-Location '$($RepoRoot.Replace("'", "''"))'"
    $clientCmdLines += "Write-Host '=== proto_RTC desktop GUI ($Username) ===' -ForegroundColor Green"
    $clientCmdLines += "Write-Host 'Server URL: $PublicServerUrl'"
    $clientCmdLines += "Write-Host ''"
    $clientCmdLines += "& '$($GuiExe.Replace("'", "''"))' --server-url '$($PublicServerUrl.Replace("'", "''"))' --username '$($Username.Replace("'", "''"))' 2>&1 | Tee-Object -FilePath '$($ClientLog.Replace("'", "''"))'"
    $clientCmdLines += "Write-Host ''"
    $clientCmdLines += "Write-Host 'GUI exited. Press any key to close...' -ForegroundColor Yellow"
    $clientCmdLines += "`$null = `$Host.UI.RawUI.ReadKey('NoEcho,IncludeKeyDown')"

    $clientPsScript = $clientCmdLines -join "; "

    $ClientProc = Start-Process -FilePath "powershell.exe" `
        -ArgumentList @("-NoProfile", "-ExecutionPolicy", "Bypass", "-NoExit", "-Command", $clientPsScript) `
        -PassThru `
        -WindowStyle Normal

    Write-Host "Client window launched (PID=$($ClientProc.Id))"
    Write-Host "Client log file (also shown live in client console): $ClientLog"
}

# ----------------------------
# Final summary / internet checklist
# ----------------------------
Write-Host ""
Write-Host "Done." -ForegroundColor Cyan
Write-Host ""
Write-Host "Internet access checklist:"
Write-Host "  1) Router port-forward TCP $Port -> this PC's LAN IP (same port)"
Write-Host "  2) Windows Firewall: allow inbound TCP $Port"
Write-Host "  3) PublicServerUrl must be your public IP/domain (NOT 127.0.0.1)"
Write-Host "  4) In the server console, confirm bind_addr shows 0.0.0.0:$Port"
Write-Host "  5) Test from phone on mobile data (not your home Wi-Fi)"
Write-Host ""
Write-Host "Quick checks:"
Write-Host "  netstat -ano | findstr :$Port"
Write-Host "  curl http://127.0.0.1:$Port/healthz"
Write-Host "  curl http://<YOUR-LAN-IP>:$Port/healthz"
Write-Host ""
Write-Host "Example using your current public IP:"
Write-Host "  .\scripts\launch-server-and-client.ps1 -BindIp 0.0.0.0 -Port $Port -PublicServerUrl http://104.205.186.27:$Port"
Write-Host ""
Write-Host "Fresh temp reset example:"
Write-Host "  .\scripts\launch-server-and-client.ps1 -ServerMode Temp -ResetDb -ResetUploads -ResetClientState -BindIp 0.0.0.0 -Port $Port -PublicServerUrl http://104.205.186.27:$Port"