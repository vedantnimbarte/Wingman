# wingman installer for Windows - downloads a prebuilt `wingman.exe` from the
# latest GitHub Release and installs it onto your PATH.
#
#   irm https://raw.githubusercontent.com/vedantnimbarte/Wingman/main/scripts/install.ps1 | iex
#
# Environment overrides:
#   $env:WINGMAN_INSTALL_DIR   install location  (default: %LOCALAPPDATA%\Programs\wingman)
#   $env:VERSION               pin a release tag (default: latest, e.g. v0.1.0)

$ErrorActionPreference = "Stop"

$Repo = "vedantnimbarte/Wingman"
$Bin  = "wingman"

# Only x86_64 Windows binaries are published today.
$arch = (Get-CimInstance Win32_Processor).Architecture
if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
  throw "No prebuilt Windows arm64 binary yet. Use: cargo install --git https://github.com/$Repo wingman-cli"
}
$target = "x86_64-pc-windows-msvc"
$asset  = "$Bin-$target.zip"

if ($env:VERSION) {
  $url = "https://github.com/$Repo/releases/download/$($env:VERSION)/$asset"
} else {
  $url = "https://github.com/$Repo/releases/latest/download/$asset"
}

$installDir = if ($env:WINGMAN_INSTALL_DIR) { $env:WINGMAN_INSTALL_DIR } `
              else { Join-Path $env:LOCALAPPDATA "Programs\wingman" }

$tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ([System.Guid]::NewGuid()))
try {
  Write-Host "Downloading $asset ..."
  $zip = Join-Path $tmp $asset
  Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing

  # Verify the SHA256 the release workflow publishes alongside every archive.
  # Without this, "the bytes GitHub served me" and "the bytes the release build
  # produced" are the same claim taken on faith.
  $sumFile = "$zip.sha256"
  $haveSum = $true
  try {
    Invoke-WebRequest -Uri "$url.sha256" -OutFile $sumFile -UseBasicParsing
  } catch {
    $haveSum = $false
  }

  if ($haveSum) {
    $expected = ((Get-Content $sumFile -First 1) -split '\s+')[0].Trim().ToLower()
    $actual   = (Get-FileHash -Path $zip -Algorithm SHA256).Hash.ToLower()
    if ([string]::IsNullOrWhiteSpace($expected)) {
      throw "checksum file was empty; refusing to install unverified binary."
    }
    if ($expected -ne $actual) {
      throw "CHECKSUM MISMATCH for $asset`n  expected: $expected`n  actual:   $actual`nRefusing to install. The download was corrupted or tampered with."
    }
    Write-Host "Checksum OK."
  } elseif ($env:WINGMAN_SKIP_CHECKSUM -eq "1") {
    Write-Host "warning: no checksum published for this asset; continuing because WINGMAN_SKIP_CHECKSUM=1."
  } else {
    throw "no checksum published for $asset (expected $url.sha256).`nRefusing to install unverified. Set WINGMAN_SKIP_CHECKSUM=1 to override."
  }

  Expand-Archive -Path $zip -DestinationPath $tmp -Force

  New-Item -ItemType Directory -Path $installDir -Force | Out-Null
  $exe = Get-ChildItem -Path $tmp -Recurse -Filter "$Bin.exe" | Select-Object -First 1
  if (-not $exe) { throw "$Bin.exe not found inside the archive." }
  Copy-Item $exe.FullName (Join-Path $installDir "$Bin.exe") -Force
  Write-Host "Installed $Bin to $installDir\$Bin.exe"

  # Add to the user PATH if it isn't already there.
  $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
  if ($userPath -notlike "*$installDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$installDir", "User")
    Write-Host "Added $installDir to your user PATH - restart your terminal to pick it up."
  }
  Write-Host "Run: $Bin --help"
} finally {
  Remove-Item -Recurse -Force $tmp
}
