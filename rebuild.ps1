param(
    [switch]$DebugBuild,
    [switch]$ReleaseBuild,
    [switch]$OptimizedBuild,
    [switch]$Test,
    [switch]$Lint,
    [switch]$PackageHelpers,
    [switch]$Installer,
    [switch]$Full,
    [switch]$Clean
)

$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $MyInvocation.MyCommand.Path
$BinDir = Join-Path $Root "bin"
$DistDir = Join-Path $Root "dist"
$BuildDir = if ($env:ZAP_BUILD_DIR) { $env:ZAP_BUILD_DIR } else { Join-Path $Root "build" }
$PyInstallerDistDir = Join-Path $BuildDir "pyinstaller-dist"
$CargoTargetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root "target" }
$OptimizedTargetDir = Join-Path $CargoTargetRoot "release-optimized"
$InstallerPath = Join-Path $DistDir "output\Zap.exe"

function Write-Step {
    param([string]$Message)
    Write-Host ""
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments
    )

    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed ($LASTEXITCODE): $FilePath $($Arguments -join ' ')"
    }
}

function Require-Command {
    param([string]$Name)

    $Command = Get-Command $Name -ErrorAction SilentlyContinue
    if (-not $Command) {
        throw "Required tool not found on PATH: $Name"
    }

    $Command.Source
}

function Find-InnoCompiler {
    $Command = Get-Command "iscc.exe" -ErrorAction SilentlyContinue
    if ($Command) {
        return $Command.Source
    }

    $Candidates = @(
        "C:\Apps\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles}\Inno Setup 6\ISCC.exe"
    )

    foreach ($Candidate in $Candidates) {
        if ($Candidate -and (Test-Path -LiteralPath $Candidate)) {
            return $Candidate
        }
    }

    throw "Inno Setup compiler not found. Install Inno Setup 6 or add iscc.exe to PATH."
}

function Require-File {
    param([string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Expected file was not produced: $Path"
    }
}

function Copy-RequiredFile {
    param(
        [string]$Source,
        [string]$Destination
    )

    Require-File $Source
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
}

function Build-Debug {
    Write-Step "Cargo debug build"
    Invoke-Checked "cargo" "build"
}

function Build-Release {
    Write-Step "Cargo release build"
    Invoke-Checked "cargo" "build" "--release"
}

function Build-Optimized {
    Write-Step "Cargo optimized release build"
    Invoke-Checked "cargo" "build" "--profile" "release-optimized"
}

function Run-Tests {
    Write-Step "Cargo tests"
    Invoke-Checked "cargo" "test"
}

function Run-Lint {
    Write-Step "Rustfmt check"
    Invoke-Checked "cargo" "fmt" "--" "--check"

    Write-Step "Clippy"
    Invoke-Checked "cargo" "clippy" "--all-targets" "--" "-D" "warnings"
}

function Package-Helpers {
    Write-Step "PyInstaller helper executables"
    $PyInstaller = Require-Command "pyinstaller.exe"
    New-Item -ItemType Directory -Force -Path $DistDir, $BuildDir, $PyInstallerDistDir | Out-Null

    $CommonArgs = @(
        "--noconfirm",
        "--onefile",
        "--distpath", $PyInstallerDistDir,
        "--workpath", $BuildDir,
        "--specpath", $BuildDir,
        "--icon", (Join-Path $Root "assets\branding\zap.ico")
    )

    Invoke-Checked $PyInstaller @CommonArgs "--name" "register-context-menu" (Join-Path $DistDir "register-context-menu.py")
    Invoke-Checked $PyInstaller @CommonArgs "--name" "unregister-context-menu" (Join-Path $DistDir "unregister-context-menu.py")
}

function Stage-Binaries {
    Write-Step "Stage binaries in bin"
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

    Copy-RequiredFile (Join-Path $OptimizedTargetDir "zap.exe") (Join-Path $BinDir "zap.exe")
    Copy-RequiredFile (Join-Path $OptimizedTargetDir "zapw.exe") (Join-Path $BinDir "zapw.exe")
    Copy-RequiredFile (Join-Path $OptimizedTargetDir "zapg.exe") (Join-Path $BinDir "zapg.exe")
    Copy-RequiredFile (Join-Path $Root "assets\manifests\zapg.exe.manifest") (Join-Path $BinDir "zapg.exe.manifest")
    Copy-RequiredFile (Join-Path $Root "assets\manifests\zapw.exe.manifest") (Join-Path $BinDir "zapw.exe.manifest")
    Copy-RequiredFile (Join-Path $PyInstallerDistDir "register-context-menu.exe") (Join-Path $BinDir "register-context-menu.exe")
    Copy-RequiredFile (Join-Path $PyInstallerDistDir "unregister-context-menu.exe") (Join-Path $BinDir "unregister-context-menu.exe")
    Copy-RequiredFile (Join-Path $Root "assets\branding\zap.ico") (Join-Path $BinDir "zap.ico")
}

function Build-Installer {
    Write-Step "Inno Setup installer"
    $InnoCompiler = Find-InnoCompiler
    New-Item -ItemType Directory -Force -Path (Join-Path $DistDir "output") | Out-Null
    Invoke-Checked $InnoCompiler (Join-Path $DistDir "zap.iss")
    Require-File $InstallerPath
}

function Clean-Artifacts {
    Write-Step "Clean Cargo artifacts"
    Invoke-Checked "cargo" "clean"

    Write-Step "Clean packaging artifacts"
    Remove-Item -LiteralPath $BinDir -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $BuildDir -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $DistDir "output") -Recurse -Force -ErrorAction SilentlyContinue
}

Set-Location -LiteralPath $Root

if (-not ($DebugBuild -or $ReleaseBuild -or $OptimizedBuild -or $Test -or $Lint -or $PackageHelpers -or $Installer -or $Full -or $Clean)) {
    $Full = $true
}

try {
    if ($Clean) { Clean-Artifacts }
    if ($DebugBuild) { Build-Debug }
    if ($ReleaseBuild) { Build-Release }
    if ($OptimizedBuild) { Build-Optimized }
    if ($Test) { Run-Tests }
    if ($Lint) { Run-Lint }
    if ($PackageHelpers) { Package-Helpers; Stage-Binaries }
    if ($Installer) { Build-Installer }
    if ($Full) {
        Build-Optimized
        Package-Helpers
        Stage-Binaries
        Build-Installer
    }

    Write-Host ""
    if (Test-Path -LiteralPath $InstallerPath) {
        Write-Host "Done: $InstallerPath" -ForegroundColor Green
    } else {
        Write-Host "Done." -ForegroundColor Green
    }
} catch {
    Write-Host ""
    Write-Host "FAILED: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}
