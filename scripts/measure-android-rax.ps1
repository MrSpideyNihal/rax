<#
.SYNOPSIS
    Measures Android on RAX, matching measure-android-emulator.ps1.
.DESCRIPTION
    Each boot is timed by the guest itself, from kernel start until
    sys.boot_completed, using /proc/uptime. Then one guest is left to settle
    and the host process is sampled. Run from the repository root.
#>
[CmdletBinding()]
param(
    [ValidateRange(1, 20)][int]$Passes = 3,
    [ValidateRange(512, 8192)][int]$RamMib = 2048,
    [ValidateRange(0, 600)][int]$SettleSecs = 300,
    [ValidateRange(1, 300)][int]$IdleSampleSecs = 60,
    [ValidatePattern('^[a-z0-9-]+$')][string]$Topic = 'android-rax'
)

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

$session = Join-Path 'benchmarks/results' ("{0}-{1}" -f (Get-Date -Format 'yyyy-MM-dd'), $Topic)
if (Test-Path $session) { throw "$session already exists. Results are never overwritten." }
New-Item -ItemType Directory -Path $session | Out-Null

$cmdline = 'console=ttyS0 nolapic noapic acpi=off nosmp reboot=k panic=-1 i8042.noaux ' +
    'tsc=reliable tsc_early_khz=3194043 androidboot.hardware=android_x86_64 ' +
    'androidboot.console=ttyS0 androidboot.selinux=permissive nomodeset HWACCEL=0 ' +
    'SRC= DATA= DEBUG='
$quoted = '"' + $cmdline + '"'
$common = @(
    'vm', 'run', '--kernel', 'guest/kernel', '--initrd', 'guest/initrd.img',
    '--disk', 'guest/android-x86_64-9.0-r2.iso', '--ram-mib', $RamMib,
    '--framebuffer', '1280x720', '--cmdline', $quoted
)

$probe = 'guest/boot-probe.txt'
$script = 'while [ "$(getprop sys.boot_completed)" != "1" ]; do sleep 1; done; ' +
    'echo BOOT_DONE_$(cut -d. -f1 /proc/uptime); reboot -p'
Set-Content -Encoding ascii -Path $probe -Value "$script`n"

foreach ($pass in 1..$Passes) {
    $log = 'guest/measure-android.log'
    $arguments = $common + @('--shell', '--timeout-secs', 240, '--report', "$session/boot-$pass.json")
    $process = Start-Process ./target/release/rax.exe -ArgumentList $arguments -NoNewWindow -PassThru `
        -Wait -RedirectStandardInput $probe -RedirectStandardOutput $log `
        -RedirectStandardError 'guest/measure-android.err'
    $match = Select-String -Path $log -Pattern '^BOOT_DONE_(\d+)' | Select-Object -First 1
    $seconds = if ($match) { [int]$match.Matches[0].Groups[1].Value } else { $null }
    [ordered]@{
        schema_version      = 1
        benchmark           = 'android-rax-boot'
        pass                = $pass
        ram_mib             = $RamMib
        boot_completed_secs = $seconds
        exit_code           = $process.ExitCode
    } | ConvertTo-Json | Set-Content -Encoding ascii "$session/pass-$pass.json"
}

$empty = 'guest/empty-stdin.txt'
Set-Content -Path $empty -Value '' -NoNewline
$arguments = $common + @('--shell', '--timeout-secs', ($SettleSecs + $IdleSampleSecs + 60))
$process = Start-Process ./target/release/rax.exe -ArgumentList $arguments -NoNewWindow -PassThru `
    -RedirectStandardInput $empty -RedirectStandardOutput 'guest/measure-android.log' `
    -RedirectStandardError 'guest/measure-android.err'
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
} | ConvertTo-Json | Set-Content -Encoding ascii "$session/idle.json"

Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
Write-Host "Results are in $session"
