@echo off
setlocal
rem One build path for local use and sanitized release packages.
set "BUILD_CLEAN="
if /i "%~1"=="clean" set "BUILD_CLEAN=-Clean"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\release.ps1" %BUILD_CLEAN%
exit /b %errorlevel%
