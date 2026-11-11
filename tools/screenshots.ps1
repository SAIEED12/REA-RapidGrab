# Captures Phase 6 release screenshots (guidance + automation helpers).
#
# Automated popup shots use the installed/debug popup exe with synthetic frames
# (same approach as tools/preview-popup.ps1). GUI shots are best taken manually
# at 100% display scaling so DWM chrome and layout match the target sizes.
#
#   powershell -ExecutionPolicy Bypass -File tools\screenshots.ps1 -Popups
#   powershell -ExecutionPolicy Bypass -File tools\screenshots.ps1 -All
#
# Manual GUI captures required:
#   - 1920x1080: main window showing a queue with running/queued/completed jobs
#     (include a completed card with a virus-check chip when available).
#   - 640x520:   main window at minimum size.
# Save outputs to docs\screenshots\.

[CmdletBinding()]
param(
    [switch]$Popups,
    [switch]$Gui,
    [switch]$All,
    [string]$OutDir = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $root "docs\screenshots" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$doPopups = $Popups -or $All -or (-not $Popups -and -not $Gui)
$doGui = $Gui -or $All

if ($doPopups) {
    $preview = Join-Path $env:TEMP "opencode\preview-popup.ps1"
    if (-not (Test-Path $preview)) {
        Write-Warning "Preview helper not found at $preview. Create synthetic popup frames and capture with tools/preview-popup.ps1 logic."
    } else {
        $captures = @(
            @{ Json = Join-Path $env:TEMP "opencode\capture-youtube.json"; Png = Join-Path $OutDir "popup-youtube.png" },
            @{ Json = Join-Path $env:TEMP "opencode\capture-bangla.json";  Png = Join-Path $OutDir "popup-bangla.png" },
            @{ Json = Join-Path $env:TEMP "opencode\capture-facebook.json"; Png = Join-Path $OutDir "popup-facebook.png" }
        )
        foreach ($c in $captures) {
            if (Test-Path $c.Json) {
                try {
                    & powershell -ExecutionPolicy Bypass -File $preview -JsonPath $c.Json -PngPath $c.Png -WaitMs 2500
                    Write-Host "Saved $($c.Png)"
                } catch {
                    Write-Warning "Failed to capture $($c.Png): $_"
                }
            } else {
                Write-Warning "Missing capture frame: $($c.Json)"
            }
        }
    }
    Write-Host "Also capture a Content-Disposition intercept popup and save as $(Join-Path $OutDir 'popup-content-disposition.png')"
}

if ($doGui) {
    Write-Host "Manual GUI screenshots (100% scaling):"
    Write-Host "  1) Resize main window to 1920x1080 with a representative queue -> $(Join-Path $OutDir 'gui-1920x1080.png')"
    Write-Host "  2) Resize main window to 640x520 (minimum) -> $(Join-Path $OutDir 'gui-640x520.png')"
    Write-Host "Tip: use Win+Shift+S or Snipping Tool on the focused window after sizing."
}

Write-Host "Screenshot output directory: $OutDir"
