<#
.SYNOPSIS
    Builds the single-folder RAX: rax.exe and Android, with nothing else.
.DESCRIPTION
    The result runs from any folder. It writes only beside itself, so deleting
    the folder removes RAX. Sharing this folder also shares Android-x86, so
    its licences apply, including offering the source of its GPL parts. Check them
    before sharing: the image includes a com.android.chrome package.
.EXAMPLE
    ./scripts/make-portable.ps1 -Out dist/RAX
#>
[CmdletBinding()]
param([string]$Out = 'dist/RAX')

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

cargo build --release -p rax-cli --locked
if ($LASTEXITCODE -ne 0) { throw 'build failed' }

foreach ($file in 'guest/kernel', 'guest/initrd.img', 'guest/android-x86_64-9.0-r2.iso') {
    if (-not (Test-Path $file)) { throw "$file is missing. Run scripts/fetch-android.ps1 first." }
}

New-Item -ItemType Directory -Force -Path "$Out/guest" | Out-Null
Copy-Item target/release/rax.exe $Out
Copy-Item LICENSE "$Out/LICENSE.txt"
Copy-Item guest/kernel, guest/initrd.img, guest/android-x86_64-9.0-r2.iso "$Out/guest"
@'
RAX

Double-click rax.exe. There is nothing to install.

Everything stays in this folder. To remove RAX, delete the folder.

data.img holds your Android apps and files.
Drag an .apk onto the window while Android is running to install it.
Only install apps you have the right to use.

Android is Android-x86 9.0-r2, https://www.android-x86.org
Android is a trademark of Google LLC. RAX is not affiliated with Google.
'@ | Set-Content -Encoding ascii "$Out/README.txt"

$bytes = (Get-ChildItem $Out -Recurse -File | Measure-Object Length -Sum).Sum
Write-Host ("{0}: {1:N0} MB" -f $Out, ($bytes / 1MB))
