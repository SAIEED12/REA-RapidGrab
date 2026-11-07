# Installs the RapidGrab native host and registers it for Chrome, Edge and Firefox.
#
#   powershell -ExecutionPolicy Bypass -File tools\install-native-host.ps1 -ExtensionId faheppogmplejcdedinijdganfphjpmj
#   powershell -ExecutionPolicy Bypass -File tools\install-native-host.ps1 -ExtensionId <id> -InstallDir "$env:LOCALAPPDATA\Programs\RapidGrab" -Profile Release
#
# The default install dir (C:\Program Files\RapidGrab) needs an elevated
# PowerShell for the file copy; registry keys are per-user (HKCU) so no
# admin is needed for those. Use -InstallDir under %LOCALAPPDATA% to avoid
# elevation entirely.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ExtensionId,
    [string]$InstallDir = 'C:\Program Files\RapidGrab',
    [string]$Profile = 'Release',
    [string]$FirefoxExtensionId = '',
    [switch]$SkipHklm
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$hostExe = Join-Path $root "target\$Profile\rapidgrab-host.exe"
if (-not (Test-Path $hostExe)) {
    throw "Host binary not found: $hostExe (build it first with: cargo build -p rg-cli --profile $Profile)"
}

New-Item -ItemType Directory -Force $InstallDir | Out-Null
Copy-Item $hostExe (Join-Path $InstallDir 'rapidgrab-host.exe') -Force
$installedHost = Join-Path $InstallDir 'rapidgrab-host.exe'
Write-Host "Installed host: $installedHost"

# --- Chrome / Edge / Chromium (HKCU, no admin) -----------------------------
$chromeManifest = @{
    name               = 'com.rapidgrab'
    description        = 'RapidGrab native host'
    path               = $installedHost
    type               = 'stdio'
    allowed_origins    = @("chrome-extension://$ExtensionId/")
} | ConvertTo-Json -Depth 4
$hostsDir = Join-Path $InstallDir 'hosts'
New-Item -ItemType Directory -Force $hostsDir | Out-Null
$chromePath = Join-Path $hostsDir 'com.rapidgrab.json'
Set-Content $chromePath $chromeManifest -Encoding UTF8

foreach ($key in @(
    'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.rapidgrab',
    'HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\com.rapidgrab',
    'HKCU:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.rapidgrab',
    'HKCU:\Software\Opera Software\Opera Stable\NativeMessagingHosts\com.rapidgrab',
    'HKCU:\Software\Vivaldi\NativeMessagingHosts\com.rapidgrab'
)) {
    New-Item -Path $key -Force | Out-Null
    Set-ItemProperty -Path $key -Name '(default)' -Value $chromePath -Type String
    Write-Host "Registered Chromium host: $key"
}

if (-not $SkipHklm) {
    foreach ($key in @(
        'HKLM:\Software\Google\Chrome\NativeMessagingHosts\com.rapidgrab',
        'HKLM:\Software\Microsoft\Edge\NativeMessagingHosts\com.rapidgrab'
    )) {
        try {
            New-Item -Path $key -Force | Out-Null
            Set-ItemProperty -Path $key -Name '(default)' -Value $chromePath -Type String
            Write-Host "Registered (HKLM): $key"
        }
        catch {
            Write-Warning "HKLM registration skipped (needs elevation): $key"
        }
    }
}

# --- Firefox ---------------------------------------------------------------
# Firefox reads the manifest from the profile directory (or the registry).
# allowed_extensions uses the add-on ID, not the chrome-extension:// origin:
# install the add-on as a temporary add-on and use its ID from about:addons.
if ($FirefoxExtensionId) {
    $mozManifest = @{
        name               = 'com.rapidgrab'
        description        = 'RapidGrab native host'
        path               = $installedHost
        type               = 'stdio'
        allowed_extensions = @($FirefoxExtensionId)
    } | ConvertTo-Json -Depth 4
    $mozPath = Join-Path $hostsDir 'com.rapidgrab.moz.json'
    Set-Content $mozPath $mozManifest -Encoding UTF8

    $mozKey = 'HKCU:\Software\Mozilla\NativeMessagingHosts\com.rapidgrab'
    New-Item -Path $mozKey -Force | Out-Null
    Set-ItemProperty -Path $mozKey -Name '(default)' -Value $mozPath -Type String
    Write-Host "Registered Firefox host (registry): $mozKey -> $mozPath"
    Write-Host "For profile-dir installs, also copy $mozPath into your Firefox profile folder."
} else {
    Write-Host "Firefox host manifest written to $hostsDir\com.rapidgrab.moz.json (pass -FirefoxExtensionId to register it)."
}

# --- RapidGrab's own local copy (dev/refresh) ------------------------------
$localDir = Join-Path $env:LOCALAPPDATA 'REA-RapidGrab'
New-Item -ItemType Directory -Force $localDir | Out-Null
Copy-Item $chromePath (Join-Path $localDir 'com.rapidgrab.json') -Force
Write-Host "Refreshed local manifest: $localDir\com.rapidgrab.json"
Write-Host "Done. Fully restart Chrome / Edge / Firefox, then click the RapidGrab icon with the GUI open."
