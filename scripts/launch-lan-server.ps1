param(
  [Parameter(Mandatory = $true, Position = 0)]
  [string]$LanHost,
  [Parameter(Position = 1)]
  [int]$Port = 8443,
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$ExtraArgs
)

$ErrorActionPreference = 'Stop'

$env:SERVER_BIND = "0.0.0.0:$Port"
$env:APP__BIND_ADDR = $env:SERVER_BIND
$env:SERVER_PUBLIC_URL = "http://$LanHost`:$Port"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
& (Join-Path $ScriptDir 'run-server.ps1') @ExtraArgs
