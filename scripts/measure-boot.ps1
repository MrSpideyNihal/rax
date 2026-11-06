<#
.SYNOPSIS
    Measures guest boot time and the idle cost of a running guest.
.DESCRIPTION
    Writes unedited results to a new dated folder. Requires the guest files
    from scripts/fetch-guest.ps1 and a release build.
.EXAMPLE
    ./scripts/measure-boot.ps1 -Passes 5
#>
[CmdletBinding()]
param(
    [ValidateRange(1, 100)][int]$Passes = 5,
    [ValidateRange(64, 65536)][int]$RamMib = 512,
    [ValidateRange(1, 120)][int]$IdleSampleSecs = 10,
    [ValidatePattern('^[a-z0-9-]+$')][string]$Topic = 'vm-boot'
)

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

$session = Join-Path 'benchmarks/results' ("{0}-{1}" -f (Get-Date -Format 'yyyy-MM-dd'), $Topic)
if (Test-Path $session) { throw "$session already exists. Results are never overwritten." }
New-Item -ItemType Directory -Path $session | Out-Null

$common = @(
    'vm', 'run', '--kernel', 'guest/vmlinuz-virt', '--initrd', 'guest/initramfs-virt',
    '--ram-mib', $RamMib
)

foreach ($pass in 1..$Passes) {
    $arguments = $common + @('--timeout-secs', 30, '--report', "$session/boot-$pass.json")
    $process = Start-Process ./target/release/rax.exe -ArgumentList $arguments -NoNewWindow `
        -PassThru -Wait -RedirectStandardOutput 'guest/measure.log' `
        -RedirectStandardError 'guest/measure.err'
    if ($process.ExitCode -ne 0) { throw "boot pass $pass failed" }
}

# Idle: leave a guest sitting at a shell, then sample the host process.
$empty = 'guest/empty-stdin.txt'
Set-Content -Path $empty -Value '' -NoNewline
$arguments = $common + @('--shell', '--timeout-secs', ($IdleSampleSecs + 12))
$process = Start-Process ./target/release/rax.exe -ArgumentList $arguments -NoNewWindow `
    -PassThru -RedirectStandardInput $empty -RedirectStandardOutput 'guest/measure.log' `
    -RedirectStandardError 'guest/measure.err'
Start-Sleep -Seconds 8
$process.Refresh()
$cpuStart = $process.TotalProcessorTime
$clock = [Diagnostics.Stopwatch]::StartNew()
Start-Sleep -Seconds $IdleSampleSecs
$process.Refresh()
$cpuSeconds = ($process.TotalProcessorTime - $cpuStart).TotalSeconds
$wallSeconds = $clock.Elapsed.TotalSeconds

[ordered]@{
    schema_version       = 1
    benchmark            = 'vm-idle'
    ram_mib              = $RamMib
    logical_cpus         = [Environment]::ProcessorCount
    sample_seconds       = [math]::Round($wallSeconds, 2)
    host_cpu_seconds     = [math]::Round($cpuSeconds, 3)
    percent_of_one_core  = [math]::Round(100 * $cpuSeconds / $wallSeconds, 2)
    working_set_mib      = [math]::Round($process.WorkingSet64 / 1MB, 1)
    private_memory_mib   = [math]::Round($process.PrivateMemorySize64 / 1MB, 1)
} | ConvertTo-Json | Set-Content -Encoding ascii "$session/idle.json"

Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
Write-Host "Results are in $session"
