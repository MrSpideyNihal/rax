<#
.SYNOPSIS
    Measures an idle Android guest on RAX, matching measure-android-emulator.ps1.
.DESCRIPTION
    Leaves one guest to settle, then samples the host process. Boot time comes
    from scripts/android-boot-time.sh. Run from the repository root.
.EXAMPLE
    ./scripts/measure-android-rax.ps1 -Session benchmarks/results/2026-11-06-android-rax
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Session,
    [ValidateRange(512, 8192)][int]$RamMib = 2048,
    [ValidateRange(0, 600)][int]$SettleSecs = 300,
    [ValidateRange(1, 300)][int]$IdleSampleSecs = 60
)

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)
New-Item -ItemType Directory -Force -Path $Session | Out-Null

$cmdline = 'console=ttyS0 nolapic noapic acpi=off nosmp reboot=k panic=-1 i8042.noaux ' +
    'tsc=reliable tsc_early_khz=3194043 androidboot.hardware=android_x86_64 ' +
    'androidboot.console=ttyS0 androidboot.selinux=permissive nomodeset HWACCEL=0 ' +
    'SRC= DATA= DEBUG='
$quoted = '"' + $cmdline + '"'
$arguments = @(
    'vm', 'run', '--kernel', 'guest/kernel', '--initrd', 'guest/initrd.img',
    '--disk', 'guest/android-x86_64-9.0-r2.iso', '--ram-mib', $RamMib,
    '--framebuffer', '1280x720', '--cmdline', $quoted,
    '--timeout-secs', ($SettleSecs + $IdleSampleSecs + 90)
)

$process = Start-Process ./target/release/rax.exe -ArgumentList $arguments -NoNewWindow -PassThru `
    -RedirectStandardOutput 'guest/measure-android.log' `
    -RedirectStandardError 'guest/measure-android.err'
$null = $process.Handle
Start-Sleep -Seconds (60 + $SettleSecs)
$process.Refresh()
$cpuStart = $process.TotalProcessorTime
$clock = [Diagnostics.Stopwatch]::StartNew()
Start-Sleep -Seconds $IdleSampleSecs
$process.Refresh()
$cpu = ($process.TotalProcessorTime - $cpuStart).TotalSeconds
$wall = $clock.Elapsed.TotalSeconds

[ordered]@{
    schema_version      = 1
    benchmark           = 'android-rax-idle'
    ram_mib             = $RamMib
    settle_secs         = 60 + $SettleSecs
    sample_secs         = [math]::Round($wall, 2)
    percent_of_one_core = [math]::Round(100 * $cpu / $wall, 2)
    working_set_mib     = [math]::Round($process.WorkingSet64 / 1MB, 1)
    private_memory_mib  = [math]::Round($process.PrivateMemorySize64 / 1MB, 1)
} | ConvertTo-Json | Set-Content -Encoding ascii "$Session/idle.json"

Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
Write-Host "Results are in $Session"
