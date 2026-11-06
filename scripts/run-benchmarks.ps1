<#
.SYNOPSIS
    Runs the WHP benchmark and stores unedited results.
.EXAMPLE
    ./scripts/run-benchmarks.ps1 -Passes 5 -Topic whp
#>
[CmdletBinding()]
param(
    [ValidateRange(1, 100)][int]$Passes = 5,
    [ValidateRange(1, 100000000)][int]$Iterations = 200000,
    [ValidateRange(1, 65536)][int]$MapMib = 1024,
    [ValidatePattern('^[a-z0-9-]+$')][string]$Topic = 'whp'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

cargo build --release -p rax-cli
if ($LASTEXITCODE -ne 0) { throw 'build failed' }

$session = Join-Path 'benchmarks/results' ("{0}-{1}" -f (Get-Date -Format 'yyyy-MM-dd'), $Topic)
if (Test-Path $session) { throw "$session already exists. Results are never overwritten." }
New-Item -ItemType Directory -Path $session | Out-Null

$battery = Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue
if ($battery -and $battery.BatteryStatus -ne 2) {
    Write-Warning 'Running on battery. Note this with the results.'
}

foreach ($pass in 1..$Passes) {
    $out = Join-Path $session "run-$pass.json"
    & ./target/release/rax.exe bench whp --iterations $Iterations --map-mib $MapMib --out $out
    if ($LASTEXITCODE -ne 0) { throw "pass $pass failed" }
}

Write-Host "Results are in $session. Record mains or battery, power plan, and running apps."
