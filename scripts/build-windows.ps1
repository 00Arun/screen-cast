# Builds Screen Cast for Windows 10 (1903+) / 11: NSIS .exe installer + .msi.
# Run in PowerShell:  powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1
#
# Prerequisites (install once):
#   1. Visual Studio 2022 Build Tools with "Desktop development with C++"
#      winget install Microsoft.VisualStudio.2022.BuildTools --override "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --quiet --wait"
#   2. Rust:   winget install Rustlang.Rustup
#   3. CMake:  winget install Kitware.CMake
#   (WebView2 is built into Windows 11 and current Windows 10; the installer adds it if missing.)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

foreach ($tool in @("cargo", "cmake")) {
  if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
    Write-Error "$tool not found. Install the prerequisites listed at the top of this script, then open a new terminal."
  }
}

cargo install tauri-cli --version "^2" --locked
# Every build gets a new version (patch +1). Set $env:NO_BUMP=1 to keep it, or $env:BUMP=minor|major.
if ($env:NO_BUMP -eq "1") { $version = (Get-Content "$root\VERSION" -Raw).Trim() }
else { $part = if ($env:BUMP) { $env:BUMP } else { "patch" }; $version = & "$PSScriptRoot\bump-version.ps1" $part }
Write-Host "Building Screen Cast $version"
Set-Location "$root\src-tauri"
cargo tauri build

Write-Host ""
Write-Host "Installers:"
Get-ChildItem "target\release\bundle\nsis\*.exe", "target\release\bundle\msi\*.msi" -ErrorAction SilentlyContinue | ForEach-Object { Write-Host "  $($_.FullName)" }
