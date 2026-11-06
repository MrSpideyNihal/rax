<#
.SYNOPSIS
    Downloads the Android-x86 image and puts what RAX needs in guest/.
.DESCRIPTION
    Downloads android-x86_64-9.0-r2.iso (921 MiB, Apache-2.0 and GPL-2.0
    parts), checks it against a pinned SHA-256, and copies the kernel and
    initrd out of it. Everything stays inside this folder.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

$name = 'android-x86_64-9.0-r2.iso'
$url = "https://sourceforge.net/projects/android-x86/files/Release%209.0/$name/download"
$expected = 'f7eb8fc56f29ad5432335dc054183acf086c539f3990f0b6e9ff58bd6df4604e'

New-Item -ItemType Directory -Force -Path guest | Out-Null
$iso = Join-Path (Resolve-Path guest) $name
if (-not (Test-Path $iso)) {
    curl.exe --fail --location --retry 3 $url --output "$iso.partial"
    if ($LASTEXITCODE -ne 0) { throw 'download failed' }
    Move-Item "$iso.partial" $iso
}
$hash = (Get-FileHash $iso -Algorithm SHA256).Hash.ToLower()
if ($hash -ne $expected) { throw "$name failed verification: got $hash" }

$image = Mount-DiskImage -ImagePath $iso -PassThru
try {
    $drive = ($image | Get-Volume).DriveLetter
    Copy-Item "${drive}:\kernel", "${drive}:\initrd.img" guest\
}
finally {
    Dismount-DiskImage -ImagePath $iso | Out-Null
}
Write-Host 'Android files are ready in guest/'
