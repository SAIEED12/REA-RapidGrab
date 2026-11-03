# Builds the release GUI and refreshes the dist bundle plus the Desktop shortcut.
#
#   powershell -ExecutionPolicy Bypass -File tools\build-dist.ps1
#   powershell -ExecutionPolicy Bypass -File tools\build-dist.ps1 -NoShortcut
#
# The resulting dist\RapidGrab.exe is what the shortcut launches; keep using
# this script so dist never falls behind the source tree again.
[CmdletBinding()]
param(
    [switch]$NoShortcut
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    Write-Host 'Building rapidgrab-gui (release)...'
    cargo build --release -p rg-gui
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }

    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Force $dist | Out-Null
    Copy-Item (Join-Path $root 'target\release\rapidgrab-gui.exe') (Join-Path $dist 'RapidGrab.exe') -Force
    Copy-Item (Join-Path $root 'assets\logo.ico') (Join-Path $dist 'RapidGrab.ico') -Force

    if (-not $NoShortcut) {
        $desktop = [Environment]::GetFolderPath('Desktop')
        $shortcutPath = Join-Path $desktop 'RapidGrab.lnk'
        $shell = New-Object -ComObject WScript.Shell
        $shortcut = $shell.CreateShortcut($shortcutPath)
        $shortcut.TargetPath = Join-Path $dist 'RapidGrab.exe'
        $shortcut.WorkingDirectory = $dist
        $shortcut.IconLocation = (Join-Path $dist 'RapidGrab.ico') + ',0'
        $shortcut.Save()
        Write-Host "Shortcut refreshed: $shortcutPath"
    }

    Get-Item (Join-Path $dist 'RapidGrab.exe') |
        Select-Object FullName, Length, LastWriteTime |
        Format-List
}
finally {
    Pop-Location
}
