@echo off
setlocal

pushd "%~dp0" >nul
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0rebuild.ps1" -Full
set "EXIT_CODE=%ERRORLEVEL%"
if "%EXIT_CODE%"=="0" echo Installer created: "%~dp0dist\output\Zap.exe"
popd >nul

exit /b %EXIT_CODE%
