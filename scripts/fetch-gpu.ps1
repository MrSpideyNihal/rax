<#
.SYNOPSIS
    Downloads the graphics translator RAX uses into gpu/.
.DESCRIPTION
    Fetches virglrenderer, libepoxy and their two runtime DLLs from the
    official MSYS2 repository. virglrenderer and libepoxy are checked against
    pinned SHA-256 values. The runtime packages change often, so the newest
    is used and its hash is printed to be pinned later.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

$mirror = 'https://repo.msys2.org/mingw/mingw64'
$pinned = @{
    'mingw-w64-x86_64-virglrenderer-1.3.0-1-any.pkg.tar.zst' = '4818581a8ab74597d24ac16944463b7e1d5c6da26c7991ba1726b3faa952d363'
    'mingw-w64-x86_64-libepoxy-1.5.10-7-any.pkg.tar.zst'     = 'a2dca7211c2c6a3bb335c074167db9929f62da55d566df29aea26e5e88f35bfc'
}

$work = Join-Path $env:TEMP 'rax-gpu-packages'
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $work | Out-Null

function Get-Package([string]$Name, [string]$Expected) {
    $file = Join-Path $work $Name
    curl.exe --fail --silent --show-error --location "$mirror/$Name" --output $file
    if ($LASTEXITCODE -ne 0) { throw "download of $Name failed" }
    $hash = (Get-FileHash $file -Algorithm SHA256).Hash.ToLower()
    if ($Expected -and $hash -ne $Expected) { throw "$Name failed verification: got $hash" }
    Write-Host "$Name $hash"
    & "$env:WINDIR\System32\tar.exe" -xf $file -C $work
    if ($LASTEXITCODE -ne 0) { throw "could not unpack $Name" }
}

foreach ($name in $pinned.Keys) { Get-Package $name $pinned[$name] }

$listing = curl.exe --fail --silent --show-error "$mirror/" | Out-String
foreach ($pattern in 'mingw-w64-x86_64-libgcc-[0-9][^"]*\.pkg\.tar\.zst', 'mingw-w64-x86_64-libwinpthread-[0-9][^"]*\.pkg\.tar\.zst') {
    $found = [regex]::Matches($listing, "href=`"($pattern)`"") |
        ForEach-Object { $_.Groups[1].Value } | Select-Object -Unique
    if (-not $found) { throw "no package matches $pattern" }
    Get-Package ($found | Select-Object -Last 1) $null
}

New-Item -ItemType Directory -Force -Path gpu | Out-Null
$dlls = 'libvirglrenderer-1.dll', 'libepoxy-0.dll', 'libgcc_s_seh-1.dll', 'libwinpthread-1.dll'
foreach ($dll in $dlls) { Copy-Item (Join-Path $work "mingw64\bin\$dll") gpu -Force }
Remove-Item -Recurse -Force $work
Write-Host 'Graphics files are ready in gpu/'
