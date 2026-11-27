param(
    [Parameter(Mandatory)][string]$Game,
    [int]$Seconds = 150,
    [int]$Relay = 2,
    [int]$LandThreads = 8,
    [string[]]$Only = @(),
    [switch]$ExeOnly
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$play = Join-Path $PSScriptRoot 'play-3070.ps1'
$exeRel = if ($ExeOnly) { '' } else { 'target-lab\release' }
if ($exeRel) {
    $exe = Join-Path $root (Join-Path $exeRel 'victory.exe')
    if (-not (Test-Path -LiteralPath $exe)) { throw "lab binary missing: $exe (build it first)" }
    $core = Join-Path $root 'target-lab\release\D3D12\D3D12Core.dll'
    if (-not (Test-Path -LiteralPath $core)) { throw "D3D12\D3D12Core.dll missing next to the lab binary" }
}
if (Get-Process victory -ErrorAction SilentlyContinue) { throw "victory.exe is running: close the game first" }

# name = extra env, 'K=V;K2=V2'
$matrix = [ordered]@{
    'probe'    = 'VICTORY_INPLACE_PROBE=1'
    'base'     = ''
    'wcbar'    = 'VICTORY_WC_BAR=1'
    'inplace1' = 'VICTORY_INPLACE=1'
    'inplace2' = 'VICTORY_INPLACE=2;VICTORY_RELAY=0'
    'inplace2all' = 'VICTORY_INPLACE=2;VICTORY_RELAY=0;VICTORY_CARD_BUFFERS_MIN_KB=8388608'
}
$runs = Join-Path $root 'runs'
New-Item -ItemType Directory -Force -Path $runs | Out-Null

foreach ($name in $matrix.Keys) {
    if ($Only.Count -gt 0 -and $Only -notcontains $name) { continue }
    Write-Host "=== $name ===" -ForegroundColor Cyan
    & $play -Game $Game -Relay $Relay -LandThreads $LandThreads -Tag "lab-$name" `
        -Extra $matrix[$name] -Seconds $Seconds -ExeDir $exeRel
    Start-Sleep -Seconds 8
}

# Summarise the newest .out.txt of each experiment
$rows = foreach ($name in $matrix.Keys) {
    if ($Only.Count -gt 0 -and $Only -notcontains $name) { continue }
    $out = Get-ChildItem $runs -Filter "3070-lab-$name-*.out.txt" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime | Select-Object -Last 1
    if (-not $out) { [pscustomobject]@{ exp = $name; fps = 'no output'; note = '' }; continue }
    $text = (Get-Content -LiteralPath $out.FullName -Raw)
    $fps = ([regex]::Matches($text, '([\d.]+) fps, ([\d.]+) ms/frame') | Select-Object -Last 1)
    $copies = ([regex]::Matches($text, 'copies ([\d.]+) ms ([\d.]+) MB') | Select-Object -Last 1)
    $back = ([regex]::Matches($text, '([\d.]+) copying back ([\d.]+) MB') | Select-Object -Last 1)
    $queued = ([regex]::Matches($text, 'nothing of ours queued ([\d.]+) ms') | Select-Object -Last 1)
    $place = ([regex]::Matches($text, 'title memory in place: ([^\r\n]+)') | Select-Object -Last 1)
    $wcbar = ([regex]::Matches($text, 'write-combined buffer''s host copy is in card memory')).Count
    $refused = ([regex]::Matches($text, 'root signature refused')).Count
    $crash = ([regex]::Matches($text, 'unhandled exception 0x')).Count
    [pscustomobject]@{
        exp      = $name
        fps      = if ($fps.Success) { "$($fps.Groups[1].Value) fps / $($fps.Groups[2].Value) ms" } else { '-' }
        copies   = if ($copies.Success) { "$($copies.Groups[1].Value) ms $($copies.Groups[2].Value) MB" } else { '-' }
        back     = if ($back.Success) { "$($back.Groups[1].Value) ms $($back.Groups[2].Value) MB" } else { '-' }
        queued   = if ($queued.Success) { "$($queued.Groups[1].Value) ms" } else { '-' }
        wcbar    = $wcbar
        refused  = $refused
        crash    = $crash
        inplace  = if ($place.Success) { $place.Groups[1].Value.Trim() } else { '' }
    }
}
$rows | Format-Table -AutoSize -Wrap | Out-String -Width 400
