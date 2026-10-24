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

# Optional Authenticode signing. Unsigned binaries trip SmartScreen and make
# a mass-deletion tool look suspicious to antivirus heuristics. Set
# ZAP_SIGN_CERT_THUMBPRINT to the SHA-1 thumbprint of a code-signing
# certificate in the certificate store to enable it; ZAP_SIGN_TIMESTAMP_URL
# overrides the RFC 3161 timestamp server. Without the variable the build
# is unchanged and unsigned.
function Find-SignTool {
    $Command = Get-Command "signtool.exe" -ErrorAction SilentlyContinue
    if ($Command) {
        return $Command.Source
    }

    $KitsBin = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
    $Found = Get-ChildItem -LiteralPath $KitsBin -Filter "signtool.exe" -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } |
        Sort-Object FullName -Descending |
        Select-Object -First 1
    if ($Found) {
        return $Found.FullName
    }

    throw "signtool.exe not found. Install the Windows SDK or add signtool to PATH."
}

function Sign-Files {
    param([string[]]$Paths)

    $Thumbprint = $env:ZAP_SIGN_CERT_THUMBPRINT
    if (-not $Thumbprint) {
        Write-Host "Signing skipped (ZAP_SIGN_CERT_THUMBPRINT is not set)." -ForegroundColor DarkGray
        return
    }

    Write-Step "Sign $($Paths.Count) file(s)"
    $SignTool = Find-SignTool
    $TimestampUrl = if ($env:ZAP_SIGN_TIMESTAMP_URL) { $env:ZAP_SIGN_TIMESTAMP_URL } else { "http://timestamp.digicert.com" }
    Invoke-Checked $SignTool "sign" "/sha1" $Thumbprint "/fd" "SHA256" "/tr" $TimestampUrl "/td" "SHA256" "/d" "Zap" @Paths
    Invoke-Checked $SignTool "verify" "/pa" @Paths
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

function Stage-Binaries {
    Write-Step "Stage binaries in bin"
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

    Copy-RequiredFile (Join-Path $OptimizedTargetDir "zap.exe") (Join-Path $BinDir "zap.exe")
    Copy-RequiredFile (Join-Path $OptimizedTargetDir "zapw.exe") (Join-Path $BinDir "zapw.exe")
    Copy-RequiredFile (Join-Path $OptimizedTargetDir "zapg.exe") (Join-Path $BinDir "zapg.exe")
    Copy-RequiredFile (Join-Path $Root "assets\manifests\zapg.exe.manifest") (Join-Path $BinDir "zapg.exe.manifest")
    Copy-RequiredFile (Join-Path $Root "assets\manifests\zapw.exe.manifest") (Join-Path $BinDir "zapw.exe.manifest")
    Copy-RequiredFile (Join-Path $Root "assets\branding\zap.ico") (Join-Path $BinDir "zap.ico")

    Sign-Files @(
        (Join-Path $BinDir "zap.exe"),
        (Join-Path $BinDir "zapw.exe"),
        (Join-Path $BinDir "zapg.exe")
    )
}

function Build-Installer {
    Write-Step "Inno Setup installer"
    $InnoCompiler = Find-InnoCompiler
    New-Item -ItemType Directory -Force -Path (Join-Path $DistDir "output") | Out-Null
    # Keep the installer version in sync with Cargo.toml.
    $Version = (Select-String -Path (Join-Path $Root "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
    Invoke-Checked $InnoCompiler "/DMyAppVersion=$Version" (Join-Path $DistDir "zap.iss")
    Require-File $InstallerPath
    Sign-Files @($InstallerPath)
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
    # Context-menu registration is built into zapg.exe; -PackageHelpers now only stages binaries.
    if ($PackageHelpers) { Stage-Binaries }
    if ($Installer) { Build-Installer }
    if ($Full) {
        Build-Optimized
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
