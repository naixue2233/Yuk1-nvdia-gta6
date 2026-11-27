@echo off
rem Play the title through victory. Single-file build: the D3D12 Agility SDK
rem core is inside victory.exe and is written out on the first run (to D3D12\
rem beside the executable, or to %LOCALAPPDATA%\victory\d3d12\620\ when that
rem folder cannot be written). No VC++ redistributable, no separate DLLs.
rem
rem Ways to start, in order:
rem   1. drag the title's game_scarlett_master.exe onto this file
rem   2. put this folder inside the title's directory, then run this file
rem   3. Play-GTA6.cmd "D:\path\to\game_scarlett_master.exe"
rem   4. Play-GTA6.cmd "D:\path\to\the\title\folder"
setlocal EnableExtensions
set "HERE=%~dp0"
set "TITLE=game_scarlett_master.exe"
set "EXE=%HERE%victory.exe"
if not exist "%EXE%" (
    echo victory.exe is missing beside this file.
    pause
    exit /b 1
)

set "GAME="
if not "%~1"=="" (
    if exist "%~1\" if exist "%~1\%TITLE%" set "GAME=%~1\%TITLE%"
    if exist "%~1" if not exist "%~1\" set "GAME=%~1"
)
if not defined GAME if exist "%HERE%%TITLE%" set "GAME=%HERE%%TITLE%"
if not defined GAME if exist "%HERE%..\%TITLE%" set "GAME=%HERE%..\%TITLE%"
if not defined GAME (
    echo.
    echo Could not find %TITLE%, the title's own executable.
    echo   - put this folder inside the title's directory and run this file again, or
    echo   - drag the title's %TITLE% onto this file, or
    echo   - Play-GTA6.cmd "D:\path\to\%TITLE%"
    echo.
    pause
    exit /b 1
)
for %%F in ("%GAME%") do set "GAMEDIR=%%~dpF"
rem A quoted path that ends in a backslash loses that quote on Windows: strip it.
if "%GAMEDIR:~-1%"=="\" set "GAMEDIR=%GAMEDIR:~0,-1%"
echo victory : %EXE%
echo game    : %GAME%
echo.

call "%HERE%victory.env.cmd"
set "VICTORY_GAME_DIR=%GAMEDIR%"
rem Texture mip drop by the card's memory, unless the caller picked one:
rem 11 GB and up keep every mip, 9-11 GB drop one, below that two.
if not defined VICTORY_KEEP_MIP_DROP if exist "%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" (
    for /f "usebackq delims=" %%v in (`powershell -NoProfile -ExecutionPolicy Bypass -Command "$k='HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}'; $m=0; foreach($i in 0..3){ $v=(Get-ItemProperty (Join-Path $k ('000'+$i)) -Name qwMemorySize -ErrorAction SilentlyContinue).qwMemorySize; if($v -gt $m){ $m=$v } }; if($m -ge 11GB){0}elseif($m -ge 9GB){1}else{2}"`) do set "VICTORY_TEXTURE_MIP_DROP=%%v"
)

if exist "%HERE%.shaders-warm" goto play
echo First run here: translating and caching the title's shaders once.
echo About 26,000 of them, roughly 80 seconds and up to 6 GB of memory.
"%EXE%" shaders "%GAMEDIR%"
if errorlevel 1 (
    echo The shader pass failed: the title will still run, compiling as it goes.
) else (
    "%EXE%" warm
    if not errorlevel 1 > "%HERE%.shaders-warm" echo done
)
echo.

:play
echo launching...
"%EXE%" run "%GAME%" --execute --console series-s
set "CODE=%ERRORLEVEL%"
echo.
echo victory exited with %CODE%
pause
exit /b %CODE%
