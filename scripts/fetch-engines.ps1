#Requires -Version 5.1
<#
  Fetch bundled engines into src-tauri/resources (fallback).
  Normal clones already contain these files (Git LFS). Run this only if
  resources/ is missing, e.g. after cloning without LFS:
    powershell -ExecutionPolicy Bypass -File scripts/fetch-engines.ps1
#>
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$res = Join-Path $root "src-tauri/resources"
$ffDir = Join-Path $res "ffmpeg"
$tessDir = Join-Path $res "tesseract"
$tessData = Join-Path $tessDir "tessdata"
New-Item -ItemType Directory -Force -Path $ffDir, $tessData | Out-Null

function Download-File([string]$url, [string]$dest) {
  Write-Host "Downloading $url"
  Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $dest
}

# --- ffmpeg (Gyan essentials, ffmpeg.exe only) ---
$ffExe = Join-Path $ffDir "ffmpeg.exe"
if (-not (Test-Path $ffExe)) {
  $zip = Join-Path $env:TEMP "os-ffmpeg.zip"
  Download-File "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip" $zip
  $tmp = Join-Path $env:TEMP "os-ffx"
  if (Test-Path $tmp) { Remove-Item -Recurse -Force $tmp }
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  $exe = Get-ChildItem $tmp -Recurse -Filter "ffmpeg.exe" | Select-Object -First 1
  Copy-Item $exe.FullName -Destination $ffExe
  Remove-Item -Recurse -Force $tmp
  Remove-Item -Force $zip
  Write-Host "ffmpeg ready."
} else { Write-Host "ffmpeg already present." }

# --- tessdata (eng + ara, small) ---
foreach ($lang in @("eng", "ara")) {
  $dest = Join-Path $tessData "$lang.traineddata"
  if (-not (Test-Path $dest)) {
    Download-File "https://github.com/tesseract-ocr/tessdata_fast/raw/main/$lang.traineddata" $dest
  }
}
Write-Host "Note: tesseract binaries come from a local Tesseract-OCR install"
Write-Host "(C:\Program Files\Tesseract-OCR: tesseract.exe + *.dll) or winget:"
Write-Host "  winget install -e --id UB-Mannheim.TesseractOCR --silent"
Write-Host "Done. Re-run: npm run tauri build"
