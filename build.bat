@echo off
rem GameReady build: outputs gameready.exe to project root
rem Usage: build.bat          build and copy exe to root (keeps cache for fast rebuild)
rem        build.bat clean    build, copy exe, then wipe target directory
cd /d "%~dp0"
echo [1/2] cargo build --release ...
cargo build --release
if errorlevel 1 (echo BUILD FAILED & exit /b 1)
copy /y "target\release\gameready.exe" "gameready.exe" >nul
for %%A in (gameready.exe) do echo [2/2] OK: %%~fA (%%~zA bytes)
if /i "%~1"=="clean" (
    echo cleaning target ...
    cargo clean
)
