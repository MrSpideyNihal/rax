<#
.SYNOPSIS
    Measures the Android Studio emulator as RAX's comparison baseline.
.DESCRIPTION
    Runs cold boots with no window, then samples idle memory and CPU. The AVD
    is started read-only, so it is left unchanged. No game is installed.
.EXAMPLE
    ./scripts/measure-android-emulator.ps1 -Avd lf34 -Passes 3
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidatePattern('^[A-Za-z0-9._-]+$')][string]$Avd,
    [ValidateRange(1, 20)][int]$Passes = 3,
    [ValidateRange(0, 600)][int]$SettleSecs = 60,
    [ValidateRange(1, 300)][int]$IdleSampleSecs = 30,
    [ValidateRange(30, 900)][int]$BootTimeoutSecs = 300,
    [ValidatePattern('^[a-z0-9-]+$')][string]$Topic = 'android-emulator-baseline'
)

$ErrorActionPreference = 'Continue'
Set-Location (Split-Path -Parent $PSScriptRoot)

$sdk = Join-Path $env:LOCALAPPDATA 'Android\Sdk'
$emulator = Join-Path $sdk 'emulator\emulator.exe'
$adb = Join-Path $sdk 'platform-tools\adb.exe'
$familyPattern = '^(emulator|qemu-system|netsimd|crashpad_handler)'

$session = Join-Path 'benchmarks/results' ("{0}-{1}" -f (Get-Date -Format 'yyyy-MM-dd'), $Topic)
if (Test-Path $session) { throw "$session already exists. Results are never overwritten." }
if (Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match $familyPattern }) {
    throw 'An emulator is already running. Close it first.'
}
New-Item -ItemType Directory -Path $session | Out-Null

function Get-Family {
    Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match $familyPattern }
}

function Get-Boot {
    (& $adb -s emulator-5554 shell getprop sys.boot_completed 2>$null | Out-String).Trim()
}

& $adb start-server 2>$null | Out-Null
$version = (& $emulator -version 2>$null | Select-Object -First 1)

foreach ($pass in 1..$Passes) {
    $availableBefore = [math]::Round((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory / 1KB)
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $arguments = @(
        '-avd', $Avd, '-no-window', '-no-audio', '-no-boot-anim',
        '-no-snapshot', '-read-only', '-port', '5554'
    )
    $process = Start-Process -FilePath $emulator -ArgumentList $arguments -PassThru -WindowStyle Hidden

    $bootSeconds = $null
    while ($clock.Elapsed.TotalSeconds -lt $BootTimeoutSecs) {
        if ((Get-Boot) -eq '1') {
            $bootSeconds = [math]::Round($clock.Elapsed.TotalSeconds, 1)
            break
        }
        Start-Sleep -Milliseconds 500
    }

    $result = [ordered]@{
        schema_version       = 1
        benchmark            = 'android-emulator-baseline'
        emulator_version     = "$version"
        pass                 = $pass
        boot_completed_secs  = $bootSeconds
        available_mib_before = $availableBefore
    }

    if ($null -ne $bootSeconds) {
        Start-Sleep -Seconds $SettleSecs
        $start = @{}
        foreach ($item in Get-Family) { $start[$item.Id] = $item.TotalProcessorTime.TotalSeconds }
        $sample = [Diagnostics.Stopwatch]::StartNew()
        Start-Sleep -Seconds $IdleSampleSecs
        $wall = $sample.Elapsed.TotalSeconds
        $cpu = 0.0
        $family = @(Get-Family)
        foreach ($item in $family) {
            $before = if ($start.ContainsKey($item.Id)) { $start[$item.Id] } else { 0.0 }
            $cpu += $item.TotalProcessorTime.TotalSeconds - $before
        }
        $result.settle_secs = $SettleSecs
        $result.sample_secs = [math]::Round($wall, 2)
        $result.percent_of_one_core = [math]::Round(100 * $cpu / $wall, 2)
        $result.percent_of_all_cores = [math]::Round(100 * $cpu / $wall / [Environment]::ProcessorCount, 2)
        $result.working_set_mib = [math]::Round((($family | Measure-Object WorkingSet64 -Sum).Sum) / 1MB, 1)
        $result.private_memory_mib = [math]::Round((($family | Measure-Object PrivateMemorySize64 -Sum).Sum) / 1MB, 1)
        $result.processes = @($family | ForEach-Object { $_.ProcessName } | Sort-Object -Unique)
    }

    $result | ConvertTo-Json | Set-Content -Encoding ascii "$session/pass-$pass.json"

    & $adb -s emulator-5554 emu kill 2>$null | Out-Null
    $stopped = [Diagnostics.Stopwatch]::StartNew()
    while ((Get-Family) -and $stopped.Elapsed.TotalSeconds -lt 30) { Start-Sleep -Seconds 1 }
    Get-Family | Stop-Process -Force -ErrorAction SilentlyContinue
    if ($process -and -not $process.HasExited) { Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Seconds 5
}

Write-Host "Results are in $session"
