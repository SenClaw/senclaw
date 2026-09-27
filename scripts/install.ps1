# SenClaw one-line installer for Windows (x64).
#
#   powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/SenClaw/senclaw/main/scripts/install.ps1 | iex"
#
# Options (environment variables):
#   SENCLAW_VERSION=v0.3.0    install a specific release tag (default: latest)
#   SENCLAW_INSTALL_DIR=...   binary directory (default: %USERPROFILE%\.senclaw\bin)

$ErrorActionPreference = "Stop"

$Repo = "SenClaw/senclaw"
$Version = if ($env:SENCLAW_VERSION) { $env:SENCLAW_VERSION } else { "latest" }
$InstallDir = if ($env:SENCLAW_INSTALL_DIR) { $env:SENCLAW_INSTALL_DIR } else { Join-Path $env:USERPROFILE ".senclaw\bin" }

if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
    throw "Only x64 Windows has prebuilt binaries (detected: $env:PROCESSOR_ARCHITECTURE). Build from source with cargo."
}

$Asset = "senclaw-x86_64-pc-windows-msvc.exe"
$Url = if ($Version -eq "latest") {
    "https://github.com/$Repo/releases/latest/download/$Asset"
} else {
    "https://github.com/$Repo/releases/download/$Version/$Asset"
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$Dest = Join-Path $InstallDir "senclaw.exe"

Write-Host "Downloading $Url"
Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing
Write-Host "Installed senclaw -> $Dest"

# Add to user PATH if missing
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ";") -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable("Path", "$InstallDir;$UserPath", "User")
    Write-Host "Added $InstallDir to your user PATH — open a new terminal to use 'senclaw'."
}

& $Dest --version

Write-Host ""
Write-Host "Next steps:"
Write-Host "  senclaw web              # download the Web UI + speech sidecar (first run) and start the daemon"
Write-Host "  senclaw install desktop  # install the native desktop app"
Write-Host "  senclaw --help           # all commands"
