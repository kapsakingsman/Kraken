# Downloads the PDFium build pinned in scripts/pdfium.lock into vendor\pdfium.
# Usage (from the repo root):
#   powershell -ExecutionPolicy Bypass -File scripts\fetch-pdfium.ps1
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # the progress bar makes Invoke-WebRequest very slow on PowerShell 5

$root = Split-Path -Parent $PSScriptRoot
$lock = Get-Content (Join-Path $PSScriptRoot 'pdfium.lock') -Raw | ConvertFrom-StringData
$version = $lock['version']
$platform = 'win-x64'
$expected = $lock[$platform]
if (-not $expected) { throw "No checksum pinned for ${platform} in scripts/pdfium.lock" }

$dest = Join-Path (Join-Path $root 'vendor') 'pdfium'
$versionFile = Join-Path $dest 'VERSION'
if ((Test-Path $versionFile) -and (Select-String -Path $versionFile -SimpleMatch "BUILD=${version}" -Quiet)) {
    Write-Host "PDFium ${version} already present in ${dest}"
    exit 0
}

$archive = Join-Path ([System.IO.Path]::GetTempPath()) "pdfium-${version}-${platform}.tgz"
$url = "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/${version}/pdfium-${platform}.tgz"
Write-Host "Downloading ${url}"
Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing

$actual = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) {
    Remove-Item $archive
    throw "Checksum mismatch for ${platform}: expected ${expected}, got ${actual}"
}

if (Test-Path $dest) { Remove-Item $dest -Recurse -Force }
New-Item -ItemType Directory -Path $dest | Out-Null
tar -xzf $archive -C $dest
if ($LASTEXITCODE -ne 0) { throw "tar failed with exit code ${LASTEXITCODE}" }
Remove-Item $archive
Write-Host "PDFium ${version} ready in ${dest}"
