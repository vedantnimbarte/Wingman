<#
.SYNOPSIS
  Fill the winget manifest templates for a released tag.

.DESCRIPTION
  Copies packaging/winget/*.yaml into a versioned output folder with __VERSION__
  and __SHA256__ replaced. The SHA256 is downloaded from the release's
  `.sha256` sidecar (produced by upload-rust-binary-action) unless supplied.

.EXAMPLE
  ./scripts/stamp-winget.ps1 -Version 0.1.0
  # writes dist/winget/0.1.0/*.yaml ready to submit to microsoft/winget-pkgs
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    # Optional explicit SHA256 of wingman-x86_64-pc-windows-msvc.zip.
    [string]$Sha256,
    [string]$OutDir = "dist/winget"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$templateDir = Join-Path $repoRoot "packaging/winget"
$asset = "wingman-x86_64-pc-windows-msvc.zip"
$base = "https://github.com/vedantnimbarte/Wingman/releases/download/v$Version"

if (-not $Sha256) {
    $shaUrl = "$base/$asset.sha256"
    Write-Host "Fetching checksum from $shaUrl"
    # The sidecar is typically "<sha>  <filename>"; take the first token.
    $line = (Invoke-WebRequest -Uri $shaUrl -UseBasicParsing).Content.Trim()
    $Sha256 = ($line -split '\s+')[0]
}
$Sha256 = $Sha256.ToUpper()

$dest = Join-Path $repoRoot (Join-Path $OutDir $Version)
New-Item -ItemType Directory -Force -Path $dest | Out-Null

Get-ChildItem -Path $templateDir -Filter *.yaml | ForEach-Object {
    $text = Get-Content -Raw -Path $_.FullName
    $text = $text.Replace("__VERSION__", $Version).Replace("__SHA256__", $Sha256)
    $outFile = Join-Path $dest $_.Name
    Set-Content -Path $outFile -Value $text -Encoding utf8
    Write-Host "wrote $outFile"
}

Write-Host ""
Write-Host "Stamped winget manifests for v$Version in $dest"
Write-Host "Validate with:  winget validate --manifest $dest"
Write-Host "Then submit the folder to microsoft/winget-pkgs under"
Write-Host "  manifests/v/VedantNimbarte/Wingman/$Version/"
