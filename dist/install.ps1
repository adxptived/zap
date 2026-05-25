$ErrorActionPreference = "Stop"

$expectedHash = "F0CF6954D5C3CC7D2B583D9265148891EBB868AD8162F8D9EDD286FEBA10B494"
$installerUrl = "https://github.com/adxptived/zap/releases/latest/download/Zap.exe"

$installerPath = Join-Path $env:TEMP 'Zap.exe'
Start-BitsTransfer $installerUrl $installerPath -Description 'Downloading Zap from GitHub Releases' -DisplayName 'Downloading Zap' -TransferType Download

if ($expectedHash -eq "__ZAP_SHA256__") {
    Write-Error "Installer hash is not set. Publish with automate-release.py before distributing install.ps1."
    exit 1
}

$actualHash = (Get-FileHash -Path $installerPath -Algorithm SHA256).Hash
if ($actualHash -ne $expectedHash) {
    Remove-Item -Force $installerPath -ErrorAction SilentlyContinue
    Write-Error "SHA256 hash mismatch! Expected: $expectedHash, Got: $actualHash"
    exit 1
}
Write-Host 'Hash verified' -ForegroundColor green

Write-Host 'Installing Zap' -ForegroundColor cyan
& $installerPath /VERYSILENT | Out-Null

Write-Host 'Successfully Installed Zap' -ForegroundColor green
