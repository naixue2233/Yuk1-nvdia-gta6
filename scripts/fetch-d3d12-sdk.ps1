# Fetches the D3D12 Agility runtime this fork embeds, into the path
# `apps/victory/build.rs` reads it from.
#
#   ./scripts/fetch-d3d12-sdk.ps1                 # nuget.org, direct
#   ./scripts/fetch-d3d12-sdk.ps1 -Proxy 127.0.0.1:7897
#   ./scripts/fetch-d3d12-sdk.ps1 -Version 1.619.6
#
# The package is Microsoft.Direct3D.D3D12 (a redistributable runtime). Nothing
# here is committed: sdk/ is ignored, and a build without it falls back to the
# in-box d3d12.dll, which on Windows 10 predates Shader Model 6.6 heap direct
# indexing and refuses the root signatures this renderer emits.

[CmdletBinding()]
param(
    [string]$Version = '1.620.2',
    [string]$Proxy,
    [string]$Root = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = 'Stop'
$sdk = Join-Path $Root 'sdk'
$work = Join-Path $sdk "agility-$Version.zip"
$out = Join-Path $sdk 'agility'
$core = Join-Path $out 'build/native/bin/x64/D3D12Core.dll'

New-Item -ItemType Directory -Force -Path $sdk | Out-Null

if (Test-Path -LiteralPath $core) {
    $len = (Get-Item -LiteralPath $core).Length
    Write-Host "already there: $core ($len bytes)"
    exit 0
}

if ($Proxy) {
    $env:HTTPS_PROXY = "http://$Proxy"
    $env:HTTP_PROXY = "http://$Proxy"
    Write-Host "through $Proxy"
}

$url = "https://www.nuget.org/api/v2/package/Microsoft.Direct3D.D3D12/$Version"
Write-Host "downloading $url"
try {
    Invoke-WebRequest -Uri $url -OutFile $work -UseBasicParsing -TimeoutSec 300
} catch {
    throw "download failed: $($_.Exception.Message)"
}
$mb = [math]::Round((Get-Item -LiteralPath $work).Length / 1MB, 1)
Write-Host "got $mb MB"

if (Test-Path -LiteralPath $out) { Remove-Item -LiteralPath $out -Recurse -Force }
Expand-Archive -LiteralPath $work -DestinationPath $out -Force

if (-not (Test-Path -LiteralPath $core)) {
    throw "no $core in the package: unexpected layout for $Version"
}
Remove-Item -LiteralPath $work -Force
$len = (Get-Item -LiteralPath $core).Length
Write-Host ""
Write-Host "D3D12Core.dll: $core ($len bytes)"
Write-Host "next: cargo build --release --bin victory"
Write-Host "      copy '$(Join-Path $out 'build/native/bin/x64/D3D12Core.dll')' to <exe dir>\D3D12\"
Write-Host "      or build the single-file version: sdk.rs writes it out on the first run"
