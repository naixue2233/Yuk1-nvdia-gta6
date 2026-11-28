param(
    [string]$Game = 'D:\GTA6.Xbox.v1.0.2.1-CYBERLEEK\game_scarlett_master.exe',
    [int]$Seconds = 75,
    [int]$Relay = 2,
    [int]$LandThreads = 8,
    [string]$ExeDir = 'target-diag\release',
    [string]$Tag = 'diag',
    [string]$Extra = ''
)
# Runs the diagnostics build beside an already running game: no process guard,
# the same VICTORY_* set as play-3070.ps1, raw cmd redirection so long lines
# arrive whole, and a stop by PID (never by name: the user's own victory is up).
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root (Join-Path $ExeDir 'victory.exe')
if (-not (Test-Path -LiteralPath $exe)) { throw "no binary at $exe" }
if (-not (Test-Path -LiteralPath $Game)) { throw "no game at $Game" }

$keep = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR'
Get-ChildItem Env:VICTORY_* -ErrorAction SilentlyContinue | Where-Object { $_.Name -notin $keep } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
$env:VICTORY_FILL_STATS = '1'

$gameDir = Split-Path -Parent (Resolve-Path -LiteralPath $Game)
$set = [ordered]@{
    VICTORY_GAME_DIR = $gameDir
    VICTORY_PIPELINE_BOOK = '1'
    VICTORY_LOG = 'info'
    VICTORY_FRAME_INTERVAL_MS = '16.667'
    VICTORY_DYNAMIC_SLOTS = '524288'
    VICTORY_MAX_PRELUDES = '32'
    VICTORY_CARD_BUFFERS_MB = '1024'
    VICTORY_CARD_WINDOWS_MB = '512'
    VICTORY_TEXTURE_MIP_DROP = '1'
    VICTORY_TEXTURE_ROOM_PERCENT = '94'
    VICTORY_PAGE_POOL = '1'
    VICTORY_STAGING_PACK = '1'
    VICTORY_LEVELS_ON_DEMAND = '0'
    VICTORY_WAIT_RESIDENT = '0'
    VICTORY_JOIN_COPIES = '0'
    VICTORY_REFRESH_ONCE = '1'
    VICTORY_CARD_REHASH_RUNS = '1'
    VICTORY_BACK_OWN_VIEW = '1'
    VICTORY_WIDE_MERGE = '1'
    VICTORY_MIRROR_FAST = '1'
    VICTORY_PIPELINE_WAIT_MS = '20'
    VICTORY_ASYNC_SUBMIT = '1'
    VICTORY_QUEUE2 = '1'
    VICTORY_INDIRECT_WINDOW = '1'
    VICTORY_TEXTURE_NO_WAIT = '1'
    VICTORY_COPY_PLAIN = '1'
    VICTORY_WIDE_TEXTURES = '1'
    VICTORY_INDIRECT_GATHER = '1'
    VICTORY_LATE_WAIT_LISTS = '1024'
    VICTORY_CARD_FILLS_BATCH = '0'
    VICTORY_GATHER_DISPATCH = '0'
    VICTORY_RELAY = "$Relay"
    VICTORY_LAND_THREADS = "$LandThreads"
    VICTORY_WIDE_BACK = '1'
    VICTORY_RELAY_COPY = '1'
    VICTORY_CARD_STEPS = '2'
    VICTORY_INDIRECT_BATCH = '2'
    VICTORY_MAP_THREADS = '1'
    VICTORY_CARD_AWAKE = '0'
    VICTORY_SHOW_AFTER = '1'
    VICTORY_JOIN_PARTS = '8'
    VICTORY_UPLOAD_AHEAD = '1'
    VICTORY_CARD_BUFFERS_MIN_KB = '64'
    VICTORY_POOL_DISCARD = '1'
    VICTORY_FILL_TITLE = '1'
    VICTORY_PARTIAL_DELTA = '1'
    VICTORY_BAKE_DELTA = '1'
    VICTORY_MAP_LOCK_FINE = '1'
    VICTORY_POOL_CAP = '1'
    VICTORY_TABLE_MEMO = '1'
    VICTORY_LIST_ALIAS_EXACT = '1'
    VICTORY_LIST_HOLD = '1'
    VICTORY_EARLY_TEXTURES_MS = '40'
    VICTORY_BAKE_TEXTURES_MS = '10000'
    VICTORY_HEAP_PASS_MAX = '0'
    VICTORY_HEAP_CHANGED_MAX = '0'
    VICTORY_LEAVE_OUT = '02384a60fda6f2d0'
    VICTORY_WC_NO_BACK = '1'
    VICTORY_WC_NO_BACK_MIN_KB = '1024'
    VICTORY_SYNC_IN_PLACE = '1'
    VICTORY_SPANS_QUICK = '1'
    VICTORY_SYNC_FINE = '1'
    VICTORY_HEAP_SYNC_FIRST = '1'
    VICTORY_HEAP_SYNC_AT_SUBMIT = '1'
    VICTORY_SOFT_VIEWS = 'copies'
    VICTORY_SAMPLERS_MADE = '1'
    VICTORY_TABLE_NAMED_ONLY = '1'
    VICTORY_QUEUE_MARKS = '1'
    VICTORY_PORT_CONSTANTS = '1'
    VICTORY_EARLY_DEPTH = '1'
}
foreach ($k in $set.Keys) { Set-Item -Path "Env:$k" -Value $set[$k] }
if ($Extra) { foreach ($pair in $Extra -split ';') { if (-not $pair) { continue }; $i = $pair.IndexOf('='); if ($i -lt 1) { continue }; Set-Item -Path ("Env:" + $pair.Substring(0, $i).Trim()) -Value $pair.Substring($i + 1).Trim() } }

$runsDir = Join-Path $root 'runs'
New-Item -ItemType Directory -Force -Path $runsDir | Out-Null
$logPath = Join-Path $runsDir "3070-$Tag-$(Get-Date -Format 'yyyyMMdd-HHmmss').log"

$before = @(Get-Process victory -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })
$killer = Start-Job -ScriptBlock {
    param($sec, $before)
    Start-Sleep -Seconds $sec
    $mine = @(Get-Process victory -ErrorAction SilentlyContinue | Where-Object { $_.Id -notin $before }) | Select-Object -First 1
    if ($mine) {
        "stopping: PID $($mine.Id) WS $([int]($mine.WorkingSet64 / 1MB)) MB"
        Stop-Process -Id $mine.Id -Force
    } else { 'nothing new to stop' }
} -ArgumentList $Seconds, $before
"cmd: `"$exe`" run `"$Game`" --execute --console series-s > `"$logPath`" 2>&1"
& cmd.exe /c "`"$exe`" run `"$Game`" --execute --console series-s > `"$logPath`" 2>&1"
"cmd exit: $LASTEXITCODE"
Receive-Job $killer -Wait -AutoRemoveJob
Start-Sleep -Seconds 2
"log: $logPath"
(Get-Item $logPath).Length
"=== diagnostics lines ==="
Select-String -Path $logPath -Pattern 'preparing per frame, the submitting thread' | Select-Object -Last 1 | ForEach-Object { $_.Line }
