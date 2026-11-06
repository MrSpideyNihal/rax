<#
.SYNOPSIS
    Downloads the test guest (Alpine Linux kernel and initramfs) to guest/.
.DESCRIPTION
    The guest is not stored in this repository. It is downloaded from Alpine's
    official mirror, and each file is checked against a pinned SHA-256.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

$base = 'https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64/netboot-3.24.4'
$files = @{
    'vmlinuz-virt'   = '1e8ac7aebfab3f0fcb443eae94ad320efbbda51be91b928c32ca1740b44ebe9d'
    'initramfs-virt' = 'cf39ceb8bd232584851218bf827668e07818cba82aeea784b1cc03969d01c3a5'
}

New-Item -ItemType Directory -Force -Path guest | Out-Null
foreach ($name in $files.Keys) {
    $target = Join-Path 'guest' $name
    $partial = "$target.partial"
    curl.exe --fail --silent --show-error --location "$base/$name" --output $partial
    if ($LASTEXITCODE -ne 0) { throw "download of $name failed" }
    $hash = (Get-FileHash $partial -Algorithm SHA256).Hash.ToLower()
    if ($hash -ne $files[$name]) {
        Remove-Item $partial
        throw "$name failed verification: got $hash"
    }
    Move-Item -Force $partial $target
    Write-Host "verified $target"
}
