# Preview the standalone "New download" popup without a browser.
#
# Builds a small native-messaging frame (little-endian u32 length + UTF-8
# JSON, same as the Chrome host) and pipes it into rapidgrab-popup.exe.
#
#   powershell -ExecutionPolicy Bypass -File tools\preview-popup.ps1
#   powershell -ExecutionPolicy Bypass -File tools\preview-popup.ps1 -Profile Release
#   powershell -ExecutionPolicy Bypass -File tools\preview-popup.ps1 -Url "https://example.com/movie/file.mp4" -FileName "movie.mp4" -Size 52428800 -ContentType "video/mp4"
#
# Width controls the simulated display scaling (-Dpi 144 for 150%); the popup
# is rendered at the same pixel size either way. Profile picks target\debug
# or target\release\rapidgrab-popup.exe.
[CmdletBinding()]
param(
    [string]$Url = "https://scontent-fdac24-5.fna.fbcdn.net/v/t42.1790-2/AQN6GyYVlQ5cJhaKP9xm2Kf4PdP7hQbZEwV1bYvPPVB3jY2-LdeXA.mp4?_nc_cat=101&_nc_oc=abc",
    [string]$FileName = "AQN6GyYVlQ5cJhaKP9xm2Kf4PdP7hQbZEwV1bYvPPVB3jY2-LdeXA.mp4",
    [string]$Referer = "https://www.facebook.com/",
    [long]$Size = 0,
    [string]$ContentType = "",
    [string]$Profile = "debug"
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "target\$Profile\rapidgrab-popup.exe"
if (-not (Test-Path $exe)) {
    throw "Popup binary not found: $exe (build first: cargo build -p rg-cli)"
}

# JSON-escape the few string fields (URLs contain no control characters).
function Escape-Json([string]$s) {
    return $s.Replace('\', '\\').Replace('"', '\"')
}

$req = @{
    url                 = $Url
    filename            = $FileName
    referer             = $Referer
    cookies             = $null
    user_agent          = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"
    content_disposition_filename = $null
    content_type        = if ($ContentType) { $ContentType } else { $null }
    content_length      = if ($Size -gt 0) { $Size } else { $null }
    source              = "media"
    auto_start          = $false
    output              = $null
    confirmed           = $false
    dont_ask_again      = $false
}

$json = $req | ConvertTo-Json -Compress
$bytes = [System.Text.Encoding]::UTF8.GetBytes($json)
$frame = New-Object byte[] ($bytes.Length + 4)
[Array]::Copy([BitConverter]::GetBytes([uint32]$bytes.Length), 0, $frame, 0, 4)
[Array]::Copy($bytes, 0, $frame, 4, $bytes.Length)

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$proc = [System.Diagnostics.Process]::Start($psi)
$proc.StandardInput.BaseStream.Write($frame, 0, $frame.Length)
$proc.StandardInput.BaseStream.Flush()
$proc.StandardInput.Close()
$proc.WaitForExit() | Out-Null
Write-Host "Popup exited with code $($proc.ExitCode). Result frame:"
$proc.StandardOutput.ReadToEnd() | Write-Host
