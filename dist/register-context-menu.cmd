@echo off
setlocal

set "ZAP_EXE=%APPDATA%\zap\bin\zap.exe"
set "ZAPW_EXE=%APPDATA%\zap\bin\zapw.exe"
set "ZAPG_EXE=%APPDATA%\zap\bin\zapg.exe"
set "ICON=%APPDATA%\zap\bin\zap.ico"
set "MAX_CONTEXT_MENU_SELECTIONS=10000"

rem Remove old turbo-delete registrations
reg delete "HKCU\Software\Classes\Directory\shell\turbo-delete" /f >nul 2>nul
reg delete "HKCU\Software\Classes\Directory\Background\shell\turbo-delete" /f >nul 2>nul
reg delete "HKCU\Software\Classes\*\shell\turbo-delete" /f >nul 2>nul
reg delete "HKCU\Software\Classes\AllFilesystemObjects\shell\turbo-delete" /f >nul 2>nul

rem Remove old zap registrations (will be re-created below)
reg delete "HKCU\Software\Classes\Directory\shell\zap" /f >nul 2>nul
reg delete "HKCU\Software\Classes\Directory\Background\shell\zap" /f >nul 2>nul
reg delete "HKCU\Software\Classes\*\shell\zap" /f >nul 2>nul
reg delete "HKCU\Software\Classes\AllFilesystemObjects\shell\zap" /f >nul 2>nul

reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer" /f /v "MultipleInvokePromptMinimum" /t REG_DWORD /d "%MAX_CONTEXT_MENU_SELECTIONS%" >nul
if errorlevel 1 exit /b 1

call :register_menu "HKCU\Software\Classes\AllFilesystemObjects\shell\zap" "%%1"
if errorlevel 1 exit /b 1

call :register_menu "HKCU\Software\Classes\Directory\Background\shell\zap" "%%V"
if errorlevel 1 exit /b 1

echo Zap context menu registered.
exit /b 0

:register_menu
set "ROOT=%~1"
set "TARGET=%~2"

reg add "%ROOT%" /f /v "MUIVerb" /t REG_SZ /d "Zap" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%" /f /v "Icon" /t REG_SZ /d "\"%ICON%\"" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%" /f /v "Position" /t REG_SZ /d "Bottom" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%" /f /v "SubCommands" /t REG_SZ /d "" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%" /f /v "MultiSelectModel" /t REG_SZ /d "Document" >nul
if errorlevel 1 exit /b 1

reg add "%ROOT%\shell\delete-dialog" /f /v "MUIVerb" /t REG_SZ /d "Delete..." >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\delete-dialog" /f /v "MultiSelectModel" /t REG_SZ /d "Document" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\delete-dialog" /f /v "Icon" /t REG_SZ /d "\"%ICON%\"" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\delete-dialog\command" /f /ve /t REG_SZ /d "\"%ZAPG_EXE%\" --batch \"%TARGET%\"" >nul
if errorlevel 1 exit /b 1

reg add "%ROOT%\shell\recycle" /f /v "MUIVerb" /t REG_SZ /d "Move to Recycle Bin" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\recycle" /f /v "MultiSelectModel" /t REG_SZ /d "Document" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\recycle" /f /v "Icon" /t REG_SZ /d "\"%ICON%\"" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\recycle\command" /f /ve /t REG_SZ /d "\"%ZAPW_EXE%\" --batch --silent --yes --recycle \"%TARGET%\"" >nul
if errorlevel 1 exit /b 1

reg add "%ROOT%\shell\zap-delete" /f /v "MUIVerb" /t REG_SZ /d "Zap Delete" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\zap-delete" /f /v "MultiSelectModel" /t REG_SZ /d "Document" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\zap-delete" /f /v "Icon" /t REG_SZ /d "\"%ICON%\"" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\zap-delete\command" /f /ve /t REG_SZ /d "\"%ZAPW_EXE%\" --batch --silent --yes \"%TARGET%\"" >nul
if errorlevel 1 exit /b 1

rem Shred opens the confirmation dialog (never silent): overwriting data is
rem unrecoverable, so the user must always see and confirm what is selected.
reg add "%ROOT%\shell\zap-shred" /f /v "MUIVerb" /t REG_SZ /d "Shred (secure delete)..." >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\zap-shred" /f /v "MultiSelectModel" /t REG_SZ /d "Document" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\zap-shred" /f /v "Icon" /t REG_SZ /d "\"%ICON%\"" >nul
if errorlevel 1 exit /b 1
reg add "%ROOT%\shell\zap-shred\command" /f /ve /t REG_SZ /d "\"%ZAPG_EXE%\" --batch --shred \"%TARGET%\"" >nul
if errorlevel 1 exit /b 1

exit /b 0
