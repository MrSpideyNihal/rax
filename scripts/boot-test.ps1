<#
.SYNOPSIS
    Boots the test guest, keeps its console log, and fails if it hangs.
.EXAMPLE
    ./scripts/boot-test.ps1 -RamMib 512 -Report result.json
#>
[CmdletBinding()]
param(
    [ValidateRange(64, 65536)][int]$RamMib = 512,
    [ValidateRange(1, 3600)][int]$TimeoutSecs = 30,
    [string]$Report,
    [string]$Log = 'guest/boot.log'
)

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

$arguments = @(
    'vm', 'run',
    '--kernel', 'guest/vmlinuz-virt',
    '--initrd', 'guest/initramfs-virt',
    '--ram-mib', $RamMib,
    '--timeout-secs', $TimeoutSecs
)
if ($Report) { $arguments += @('--report', $Report) }

$process = Start-Process -FilePath ./target/release/rax.exe -ArgumentList $arguments `
    -NoNewWindow -PassThru -RedirectStandardOutput $Log -RedirectStandardError "$Log.err"
$null = $process.Handle  # Keeps ExitCode readable after the process exits.
if (-not $process.WaitForExit(($TimeoutSecs + 10) * 1000)) {
    Stop-Process -Id $process.Id -Force
    Write-Warning 'rax did not stop on its own'
}
Get-Content $Log -Tail 45
Get-Content "$Log.err"
exit $process.ExitCode
