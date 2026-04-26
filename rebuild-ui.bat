@echo off
setlocal
pushd "%~dp0" >nul

:menu
cls
echo.
echo   +-----------------------------------------------------------+
echo   ^|                    Zap build menu                         ^|
echo   +-----------------------------------------------------------+
echo   ^|  1  Debug build        cargo build                         ^|
echo   ^|  2  Release build      cargo build --release               ^|
echo   ^|  3  Optimized build    release-optimized                   ^|
echo   ^|  4  Run tests          cargo test                          ^|
echo   ^|  5  Lint               rustfmt check + clippy all-targets  ^|
echo   ^|  6  Package helpers    PyInstaller + stage bin             ^|
echo   ^|  7  Build installer    Inno Setup from staged bin          ^|
echo   ^|  8  Full rebuild       optimized + helpers + installer     ^|
echo   ^|  9  Clean artifacts    cargo clean + packaging output      ^|
echo   ^|  0  Exit                                                   ^|
echo   +-----------------------------------------------------------+
echo.
set /p "choice=Choice: "

if "%choice%"=="1" call :run_ps -DebugBuild
if "%choice%"=="2" call :run_ps -ReleaseBuild
if "%choice%"=="3" call :run_ps -OptimizedBuild
if "%choice%"=="4" call :run_ps -Test
if "%choice%"=="5" call :run_ps -Lint
if "%choice%"=="6" call :run_ps -PackageHelpers
if "%choice%"=="7" call :run_ps -Installer
if "%choice%"=="8" call :run_ps -Full
if "%choice%"=="9" call :run_ps -Clean
if "%choice%"=="0" goto :done
goto :menu

:run_ps
echo.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0rebuild.ps1" %*
echo.
if %ERRORLEVEL% neq 0 (
    echo Command failed with exit code %ERRORLEVEL%.
) else (
    echo Command completed successfully.
)
pause
goto :eof

:done
popd >nul
exit /b 0
