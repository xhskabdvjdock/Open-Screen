#Requires -Version 5.1
<#
  Fetch bundled OCR data into src-tauri/resources (fallback).
  Recording is fully native (WGC + Media Foundation + WASAPI) and needs no
  downloaded engines. Run this only if resources/ is missing, e.g. after
  cloning without LFS:
    powershell -ExecutionPolicy Bypass -File scripts/fetch-engines.ps1
#>
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$res = Join-Path $root "src-tauri/resources"
$tessDir = Join-Path $res "tesseract"
$tessData = Join-Path $tessDir "tessdata"
New-Item -ItemType Directory -Force -Path $tessData | Out-Null

function Download-File([string]$url, [string]$dest) {
  Write-Host "Downloading $url"
  Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $dest
}

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
