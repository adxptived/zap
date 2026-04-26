@echo off
setlocal

reg delete "HKCU\Software\Classes\Directory\shell\turbo-delete" /f >nul 2>nul
reg delete "HKCU\Software\Classes\Directory\Background\shell\turbo-delete" /f >nul 2>nul
reg delete "HKCU\Software\Classes\*\shell\turbo-delete" /f >nul 2>nul
reg delete "HKCU\Software\Classes\AllFilesystemObjects\shell\turbo-delete" /f >nul 2>nul

reg delete "HKCU\Software\Classes\Directory\shell\zap" /f >nul 2>nul
reg delete "HKCU\Software\Classes\Directory\Background\shell\zap" /f >nul 2>nul
reg delete "HKCU\Software\Classes\*\shell\zap" /f >nul 2>nul
reg delete "HKCU\Software\Classes\AllFilesystemObjects\shell\zap" /f >nul 2>nul

echo Zap context menu unregistered.
exit /b 0
