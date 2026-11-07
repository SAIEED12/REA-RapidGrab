# Removes the RapidGrab native host registration and (optionally) the install dir.
#
#   powershell -ExecutionPolicy Bypass -File tools\uninstall-native-host.ps1
#   powershell -ExecutionPolicy Bypass -File tools\uninstall-native-host.ps1 -RemoveInstallDir
[CmdletBinding()]
param(
    [string]$InstallDir = 'C:\Program Files\RapidGrab',
    [switch]$RemoveInstallDir
)

$ErrorActionPreference = 'Continue'

foreach ($key in @(
    'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.rapidgrab',
    'HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\com.rapidgrab',
    'HKCU:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.rapidgrab',
    'HKCU:\Software\Opera Software\Opera Stable\NativeMessagingHosts\com.rapidgrab',
    'HKCU:\Software\Vivaldi\NativeMessagingHosts\com.rapidgrab',
    'HKCU:\Software\Mozilla\NativeMessagingHosts\com.rapidgrab',
    'HKLM:\Software\Google\Chrome\NativeMessagingHosts\com.rapidgrab',
    'HKLM:\Software\Microsoft\Edge\NativeMessagingHosts\com.rapidgrab'
)) {
    if (Test-Path $key) {
        Remove-Item $key -Recurse -Force
        Write-Host "Removed $key"
    }
}

if ($RemoveInstallDir -and (Test-Path $InstallDir)) {
    Remove-Item $InstallDir -Recurse -Force
    Write-Host "Removed install dir $InstallDir"
}
Write-Host "Done. Fully restart Chrome / Edge / Firefox."
