param(
  [Parameter(Mandatory = $true, Position = 0)]
  [string]$ServerUrl,
  [Parameter(Position = 1)]
  [string]$Username = 'remote-user',
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$ExtraArgs
)

$ErrorActionPreference = 'Stop'

$env:SERVER_PUBLIC_URL = $ServerUrl
$env:CLI_USERNAME = $Username

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
& (Join-Path $ScriptDir 'run-cli.ps1') @ExtraArgs
