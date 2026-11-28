param(
    [Parameter(Mandatory)][string]$Game,
    [ValidateSet('series-s', 'series-x')][string]$Console = 'series-s',
    [ValidateRange(0, 3)][int]$MipDrop = 1,
    [int]$CardBuffersMB = 1024,
    [int]$CardWindowsMB = 512,
    [int]$Relay = 2,
    [int]$LandThreads = 4,
    [int]$InPlace = 0,
    [switch]$PairsFirst,
    [switch]$NoDeepSkips,
    [switch]$NoWaterFix,
    [int]$ShaderOpt = -1,
    [switch]$WcBar,
    [switch]$Hooks,
    [switch]$NoGpuOnly,
    [switch]$ProbeRays,
    [switch]$RayTracing,
    [switch]$Prewarm,
    [string]$Tag = '',
    [string]$Extra = '',
    [int]$Seconds = 0,
    [string]$ExeDir = 'target\release'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root (Join-Path $ExeDir 'victory.exe')
if (-not (Test-Path -LiteralPath $exe)) {
    $exe = Join-Path $root 'victory.exe'
}
if (-not (Test-Path -LiteralPath $exe)) {
    throw "Release binary not found next to script or in target\release\victory.exe"
}
if (-not (Test-Path -LiteralPath $Game)) {
    throw "Game not found: $Game"
}
if (Get-Process victory -ErrorAction SilentlyContinue) {
    Write-Host "victory.exe is already running." -ForegroundColor Yellow
    exit 1
}

# Clear inherited VICTORY_* except path overrides (keeps default save & cache)
$keep = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR'
Get-ChildItem Env:VICTORY_* -ErrorAction SilentlyContinue | Where-Object { $_.Name -notin $keep } | ForEach-Object { Remove-Item "Env:$($_.Name)" }

$gameDir = Split-Path -Parent (Resolve-Path -LiteralPath $Game)
$set = [ordered]@{
    VICTORY_GAME_DIR             = $gameDir
    VICTORY_PIPELINE_BOOK        = '1'
    VICTORY_LOG                  = 'info'
    VICTORY_FRAME_INTERVAL_MS    = '16.667'

    # RTX 3070 8GB VRAM budget + Scarlett page pool (-1.2 GB texture VRAM)
    VICTORY_DYNAMIC_SLOTS        = '524288'
    VICTORY_MAX_PRELUDES         = '32'
    VICTORY_CARD_BUFFERS_MB      = "$CardBuffersMB"
    VICTORY_CARD_WINDOWS_MB      = "$CardWindowsMB"
    VICTORY_TEXTURE_MIP_DROP     = "$MipDrop"
    VICTORY_TEXTURE_ROOM_PERCENT = '94'
    VICTORY_PAGE_POOL            = '1'
    VICTORY_STAGING_PACK         = '1'
    VICTORY_LEVELS_ON_DEMAND     = '0'

    # Scarlett CPU submit & upload wins (+ 3050 Nsight A/B tweaks)
    VICTORY_WAIT_RESIDENT        = '0'
    VICTORY_JOIN_COPIES          = '0'
    VICTORY_REFRESH_ONCE         = '1'
    VICTORY_CARD_REHASH_RUNS     = '1'
    VICTORY_BACK_OWN_VIEW        = '1'
    VICTORY_WIDE_MERGE           = '1'
    VICTORY_MIRROR_FAST          = '1'
    VICTORY_PIPELINE_WAIT_MS     = '20'
    VICTORY_ASYNC_SUBMIT         = '1'
    VICTORY_QUEUE2               = '1'
    VICTORY_INDIRECT_WINDOW      = '1'
    VICTORY_TEXTURE_NO_WAIT      = '1'
    VICTORY_COPY_PLAIN           = '1'
    VICTORY_WIDE_TEXTURES        = '1'
    VICTORY_INDIRECT_GATHER      = '1'
    VICTORY_LATE_WAIT_LISTS      = '1024'
    VICTORY_CARD_FILLS_BATCH     = '0'
    VICTORY_GATHER_DISPATCH      = '0'
    VICTORY_RELAY                = "$Relay"
    VICTORY_LAND_THREADS         = "$LandThreads"
    VICTORY_INPLACE              = "$InPlace"
    VICTORY_WC_BAR               = "$([int][bool]$WcBar)"
    VICTORY_WC_BAR_TRY           = "$([int][bool]$WcBar)"
    VICTORY_WIDE_BACK            = '1'
    VICTORY_RELAY_COPY           = '1'
    VICTORY_CARD_STEPS           = '2'
    VICTORY_INDIRECT_BATCH       = '2'
    VICTORY_MAP_THREADS          = '1'
    VICTORY_CARD_AWAKE           = '0'
    VICTORY_SHOW_AFTER           = '1'
    VICTORY_JOIN_PARTS           = '8'
    VICTORY_UPLOAD_AHEAD         = '1'
    VICTORY_CARD_BUFFERS_MIN_KB  = '64'
    VICTORY_POOL_DISCARD         = '1'
    VICTORY_FILL_TITLE           = '1'
    VICTORY_PARTIAL_DELTA        = '1'
    VICTORY_BAKE_DELTA           = '1'
    VICTORY_MAP_LOCK_FINE        = '1'
    VICTORY_POOL_CAP             = '1'
    VICTORY_TABLE_MEMO           = '1'
    VICTORY_LIST_ALIAS_EXACT     = '1'
    VICTORY_LIST_HOLD            = '1'
    VICTORY_EARLY_TEXTURES_MS    = '40'
    VICTORY_BAKE_TEXTURES_MS     = '10000'
    VICTORY_HEAP_PASS_MAX        = '0'
    VICTORY_HEAP_CHANGED_MAX     = '0'
    VICTORY_LEAVE_OUT            = '02384a60fda6f2d0'
    VICTORY_WC_NO_BACK           = '1'
    VICTORY_WC_NO_BACK_MIN_KB    = '1024'
    VICTORY_SYNC_IN_PLACE        = '1'
    VICTORY_SPANS_QUICK          = '1'
    VICTORY_SYNC_FINE            = '1'
    VICTORY_HEAP_SYNC_FIRST      = '1'
    VICTORY_HEAP_SYNC_AT_SUBMIT  = '1'
    VICTORY_SOFT_VIEWS           = 'copies'
    VICTORY_SAMPLERS_MADE        = '1'
    VICTORY_TABLE_NAMED_ONLY     = '1'
    VICTORY_QUEUE_MARKS          = '1'
    VICTORY_PORT_CONSTANTS       = '1'
    VICTORY_EARLY_DEPTH          = '1'
}
if ($Hooks) {
    $set.VICTORY_GPU_ONLY = '1'
    $set.VICTORY_KNOWN_WRITES = '1'
}
# Measured on this machine (RTX 3080, 16C/24T), same scene, VICTORY_REFRESH_SPLIT=1:
# pages the journal alone answers need no write-watch ask and no guard re-arm, and the asks are the
# dearest thing the submitting thread does after the copies themselves.
#   without: asks 29.1 ms, guards 21.6 ms of them, walks 16.7 ms, texture refreshes in front 16.5 ms
#   with:    asks 15.6 ms, guards 10.0 ms of them, walks  8.1 ms, texture refreshes in front  7.8 ms
#   frame 79.6 ms -> 69.9 ms, 12.6 -> 14.3-15.0 fps.
if (-not $NoGpuOnly) {
    $set.VICTORY_GPU_ONLY = '1'
    $set.VICTORY_KNOWN_WRITES = '1'
}
if ($ProbeRays -and -not $RayTracing) {
    $set.VICTORY_PROBE_RAYS = '1'
    $set.VICTORY_PLANE_NOW = '1'
} elseif (-not $RayTracing) {
    $set.VICTORY_AMBIENT = '0.8'
    $set.VICTORY_AMBIENT_AIR = '1.5'
}
if ($RayTracing) { $set.VICTORY_BVH_BUILDER = '1' }

# The form a console wave64 program is translated to. Unset, cool_fox's forms win where
# they translate and the exact paired form only takes what nothing else did; the 14
# CS_ShadowRenderTiled* programs then run the approximate forms over 32-lane halves
# (black 16x16 tiles on lit surfaces, upstream issue #62).
if ($PairsFirst) { $set.VICTORY_WAVE_PAIRS_FIRST = '1' }
if ($NoDeepSkips) { $set.VICTORY_WAVE_DEEP_SKIPS = '0' }
if ($ShaderOpt -ge 0) { $set.VICTORY_SHADER_OPT = "$ShaderOpt" }

# Open water: the title's two 64-lane water programs (5dc3b25986622949, 6f225b641c5b0708).
#
# Measured 2026-11-28 on this machine: every form but these two left refused them (converter
# shader.dxil v1: GFX10 code at 0x0: no wave32 lowering for this wave64 program) and 6f22 got no
# pipeline at all, so the water surface ran unshaded = black. Two switches, both named by program,
# give them a form:
#   - their sums are taken under a partial mask (WaveActiveSum over doubles the title counts in),
#     which the translator refuses unless the program is named: VICTORY_WAVE_DIVERGENT_ONLY;
#   - they stay out of the wave-wide form, whose half-wave sums painted 16x16 patches on open
#     water: VICTORY_WAVE_WIDE_NOT.
# Both lists name the two programs only. Given to every program the same gates make the whole
# world flicker (a sum over the wrong lanes), measured 2026-11-28.
#
# The other two water programs (926c46bab3b86bd6, a10179ab7eda0dc3) translate only in the
# wave-wide form, so they are added to it one at a time rather than narrowing the form to them
# (VICTORY_WAVE_WIDE_ONLY would leave every other program without the form it needs):
# VICTORY_WAVE_WIDE_ALSO.
#
# The paired form is NOT forced on every program: its half-wave reductions add up in a
# different order (lower.rs, join_halves), enough to change a shadow tile's classification byte.
# -PairsFirst asks for it; per program it is VICTORY_WAVE_PAIRS_ONLY.
if (-not $NoWaterFix) {
    $set.VICTORY_WAVE_WIDE_NOT = '5dc3b25986622949,6f225b641c5b0708'
    $set.VICTORY_WAVE_DIVERGENT_ONLY = '5dc3b25986622949,6f225b641c5b0708'
    $set.VICTORY_WAVE_WIDE_ALSO = 'a10179ab7eda0dc3,926c46bab3b86bd6'
}

foreach ($k in $set.Keys) { Set-Item -Path "Env:$k" -Value $set[$k] }

# Lab: extra switches for one experiment, "K=V;K2=V2"
if ($Extra) {
    foreach ($pair in $Extra.Split(';')) {
        if (-not $pair.Trim()) { continue }
        $kv = $pair.Split('=', 2)
        Set-Item -Path "Env:$($kv[0].Trim())" -Value $kv[1].Trim()
        Write-Host "extra: $($kv[0].Trim())=$($kv[1].Trim())"
    }
}

if ($Prewarm) {
    Write-Host "Prewarming shaders and pipelines..." -ForegroundColor Cyan
    & $exe shaders $gameDir
    & $exe warm
}

$runsDir = Join-Path $root 'runs'
New-Item -ItemType Directory -Force -Path $runsDir | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$label = if ($Tag) { "$Tag-" } else { '' }
$logPath = Join-Path $runsDir "3070-$label$stamp.log"

Write-Host "=== victory-scarlett RTX 3070 8GB (release, $Console, MipDrop=$MipDrop, PagePool=1) ===" -ForegroundColor Green
Write-Host "Log: $logPath"
$ErrorActionPreference = 'Continue'
if ($Seconds -gt 0) {
    # Timed run through Tee so the log is line-flushed and survives the kill.
    $killer = Start-Job -ScriptBlock {
        param($sec)
        Start-Sleep -Seconds $sec
        Get-Process victory -ErrorAction SilentlyContinue | ForEach-Object {
            Write-Host "timed stop: PID $($_.Id) WS $([int]($_.WorkingSet64 / 1MB)) MB"
            Stop-Process -Id $_.Id -Force
        }
    } -ArgumentList $Seconds
    # cmd's own redirection: raw bytes, no PowerShell line formatting (Tee truncates long lines)
    & cmd.exe /c "`"$exe`" run `"$Game`" --execute --console $Console > `"$logPath`" 2>&1"
    Stop-Job $killer -ErrorAction SilentlyContinue
    Remove-Job $killer -Force -ErrorAction SilentlyContinue
    exit 0
}
& $exe run $Game --execute --console $Console 2>&1 | Tee-Object -FilePath $logPath
exit $LASTEXITCODE
