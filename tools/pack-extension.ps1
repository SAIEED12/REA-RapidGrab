# Packages RapidGrab browser extensions for release.
#
#   powershell -ExecutionPolicy Bypass -File tools\pack-extension.ps1
#   powershell -ExecutionPolicy Bypass -File tools\pack-extension.ps1 -Chrome -KeyPath "$env:USERPROFILE\.rapidgrab-extension.pem"
#
# Chrome/Edge: packs a signed CRX3 when chrome.exe is available and a key is
# provided (or an existing key is found). Always produces an unpacked zip for
# Chrome Web Store / Edge Partner Center upload in dist\extension\.
# Firefox: builds an unsigned XPI (zip) using manifest.firefox.json; AMO signing
# requires `web-ext sign` with API credentials (see docs/release.md).

[CmdletBinding()]
param(
    [switch]$Chrome,
    [switch]$Firefox,
    [string]$KeyPath = "",
    [string]$OutDir = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$src = Join-Path $root "tools\rapidgrab-extension"
if (-not $OutDir) { $OutDir = Join-Path $root "dist\extension" }
$chromeOut = Join-Path $OutDir "chrome"
$firefoxOut = Join-Path $OutDir "firefox"
New-Item -ItemType Directory -Force -Path $chromeOut, $firefoxOut | Out-Null

$doChrome = $Chrome -or (-not $Chrome -and -not $Firefox)
$doFirefox = $Firefox -or (-not $Chrome -and -not $Firefox)

function Copy-Extension([string]$dest, [string]$manifestName) {
    if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    Get-ChildItem $src -File | Where-Object { $_.Name -notlike 'manifest*.json' -or $_.Name -eq $manifestName } | ForEach-Object {
        $target = $_.Name
        if ($target -eq $manifestName) { $target = 'manifest.json' }
        Copy-Item $_.FullName (Join-Path $dest $target) -Force
    }
}

if ($doChrome) {
    $stage = Join-Path $chromeOut "rapidgrab-extension"
    Copy-Extension $stage "manifest.json"
    $zip = Join-Path $chromeOut "rapidgrab-extension.zip"
    if (Test-Path $zip) { Remove-Item $zip -Force }
    Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip -Force
    Write-Host "Chrome/Edge zip: $zip"

    $chromeExe = @(
        "$env:ProgramFiles\Google\Chrome\Application\chrome.exe",
        "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
        "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe",
        "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1

    if ($chromeExe -and $KeyPath -and (Test-Path $KeyPath)) {
        & $chromeExe "--pack-extension=$stage" "--pack-extension-key=$KeyPath" | Out-Null
        $crx = "$stage.crx"
        if (Test-Path $crx) {
            Move-Item $crx (Join-Path $chromeOut "rapidgrab-extension.crx") -Force
            Write-Host "Signed CRX: $(Join-Path $chromeOut 'rapidgrab-extension.crx')"
        }
    } else {
        Write-Host "CRX not packed (provide -KeyPath to an existing PEM and a Chrome/Edge install). Upload the zip to the stores."
    }

    $updates = @'
<?xml version='1.0' encoding='UTF-8'?>
<gupdate xmlns='http://www.google.com/update2/response' protocol='2.0'>
  <app appid='REPLACE_WITH_EXTENSION_ID'>
    <updatecheck codebase='https://example.com/rapidgrab-extension.crx' version='1.5' />
  </app>
</gupdate>
'@
    Set-Content -Path (Join-Path $chromeOut "updates.xml") -Value $updates -Encoding UTF8
    Write-Host "Update manifest template: $(Join-Path $chromeOut 'updates.xml')"
}

if ($doFirefox) {
    $stage = Join-Path $firefoxOut "rapidgrab-extension"
    Copy-Extension $stage "manifest.firefox.json"
    $xpi = Join-Path $firefoxOut "rapidgrab-extension.xpi"
    if (Test-Path $xpi) { Remove-Item $xpi -Force }
    Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $xpi -Force
    Write-Host "Firefox XPI (unsigned): $xpi"
    Write-Host "Sign via AMO / web-ext sign for distribution (see docs/release.md)."
}

Write-Host "Done. Output: $OutDir"
