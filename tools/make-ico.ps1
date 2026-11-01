# Rebuilds assets/logo.ico from the master assets/logo.png.
#
# The master and the vector source (assets/logo.svg) come from the original
# RapidGrab project, so this script only derives the multi-resolution icon; it
# does not draw anything.
#
#   powershell -File tools/make-ico.ps1
#
# Frames are PNG-compressed (16/24/32/48/64/128/256), the same layout the
# upstream rapidgrab.ico uses.
[CmdletBinding()]
param(
    [string]$Master = '',
    [string]$IcoPath = ''
)

Add-Type -AssemblyName System.Drawing

$ErrorActionPreference = 'Stop'

$assets = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) '..\assets'
if (-not $Master)  { $Master  = Join-Path $assets 'logo.png' }
if (-not $IcoPath) { $IcoPath = Join-Path $assets 'logo.ico' }

$Master  = [System.IO.Path]::GetFullPath($Master)
$IcoPath = [System.IO.Path]::GetFullPath($IcoPath)

if (-not (Test-Path $Master)) { throw "master PNG not found: $Master" }

$src = [System.Drawing.Image]::FromFile($Master)

function Get-PngFrame {
    param([System.Drawing.Image]$Src, [int]$S)
    $b = New-Object System.Drawing.Bitmap($S, $S, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($b)
    $g.InterpolationMode  = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode    = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)
    $g.DrawImage($Src, 0, 0, $S, $S)
    $g.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $b.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $b.Dispose()
    $bytes = $ms.ToArray()
    $ms.Dispose()
    return ,$bytes
}

$sizes = 16, 24, 32, 48, 64, 128, 256
$frames = @()
foreach ($s in $sizes) { $frames += ,@($s, (Get-PngFrame $src $s)) }

Write-Host ("master: {0} ({1}x{2})" -f $Master, $src.Width, $src.Height)
$src.Dispose()

$fs = [System.IO.File]::Create($IcoPath)
$bw = New-Object System.IO.BinaryWriter($fs)
$bw.Write([int16]0)                  # reserved
$bw.Write([int16]1)                  # type: icon
$bw.Write([int16]$frames.Count)
$offset = 6 + (16 * $frames.Count)
foreach ($f in $frames) {
    $dim = if ($f[0] -ge 256) { 0 } else { $f[0] }
    $bw.Write([byte]$dim); $bw.Write([byte]$dim)
    $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([int16]1); $bw.Write([int16]32)
    $bw.Write([int]$f[1].Length); $bw.Write([int]$offset)
    $offset += $f[1].Length
}
foreach ($f in $frames) { $bw.Write($f[1]) }
$bw.Flush(); $bw.Dispose(); $fs.Dispose()

$ico = Get-Item $IcoPath
Write-Host ("wrote {0} ({1} bytes, {2} frames: {3})" -f $IcoPath, $ico.Length, $frames.Count, ($sizes -join '/'))
