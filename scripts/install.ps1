# Kipo (`kp`) Windows PowerShell installer (verifies `SHA256SUMS` before installing)
# Repository: https://github.com/junjamrk/kipo

$ErrorActionPreference = "Stop"

$Repo = if ($env:KP_REPO) { $env:KP_REPO } else { "junjamrk/kipo" }
$Version = if ($env:KP_VERSION) { $env:KP_VERSION } else { "latest" }
$InstallDir = if ($env:KP_INSTALL_DIR) { $env:KP_INSTALL_DIR } else { Join-Path $HOME ".kipo\bin" }
$BaseUrl = if ($env:KP_RELEASE_BASE_URL) { $env:KP_RELEASE_BASE_URL.TrimEnd('/') } else { "https://github.com/$Repo/releases" }

$Arch = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()) {
    "Arm64" { "aarch64-pc-windows-msvc" }
    "X64"   { "x86_64-pc-windows-msvc" }
    default { throw "Unsupported Windows architecture: $_" }
}

$AssetName = "kipo-$Arch.zip"
$DownloadPrefix = if ($Version -eq "latest") { "$BaseUrl/latest/download" } else { "$BaseUrl/download/$Version" }

$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("kipo-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    $ArchivePath = Join-Path $TmpDir $AssetName
    $SumsPath = Join-Path $TmpDir "SHA256SUMS"

    Write-Host "==> Downloading $AssetName from $DownloadPrefix..."
    Invoke-WebRequest -Uri "$DownloadPrefix/$AssetName" -OutFile $ArchivePath
    Invoke-WebRequest -Uri "$DownloadPrefix/SHA256SUMS" -OutFile $SumsPath

    $ExpectedSha = $null
    foreach ($line in Get-Content $SumsPath) {
        $trimmed = $line.Trim()
        if ($trimmed -eq "" -or $trimmed.StartsWith("#")) { continue }
        $parts = $trimmed -split '\s+', 2
        if ($parts.Length -eq 2) {
            $name = [System.IO.Path]::GetFileName($parts[1].TrimStart('*'))
            if ($name -eq $AssetName -and $parts[0] -match '^[0-9a-fA-F]{64}$') {
                $ExpectedSha = $parts[0].ToLowerInvariant()
                break
            }
        }
    }
    if (-not $ExpectedSha) {
        throw "No valid SHA-256 entry for $AssetName in SHA256SUMS"
    }

    $ActualSha = (Get-FileHash -Algorithm SHA256 -Path $ArchivePath).Hash.ToLowerInvariant()
    if ($ActualSha -ne $ExpectedSha) {
        throw "SHA-256 mismatch for ${AssetName}: expected $ExpectedSha, got $ActualSha"
    }
    Write-Host "==> Verified SHA-256 ($ActualSha)"

    $ExtractDir = Join-Path $TmpDir "extract"
    Expand-Archive -Path $ArchivePath -DestinationPath $ExtractDir -Force

    $KpExe = Get-ChildItem -Path $ExtractDir -Recurse -Filter "kp.exe" | Select-Object -First 1
    if (-not $KpExe) { throw "Archive $AssetName does not contain kp.exe" }

    $VerOut = & $KpExe.FullName --version
    if ($LASTEXITCODE -ne 0) { throw "Extracted kp.exe failed self-check (--version)" }

    $ConnExe = Get-ChildItem -Path $ExtractDir -Recurse -Filter "kp-connector.exe" | Select-Object -First 1
    if ($ConnExe) {
        & $ConnExe.FullName --version | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "Extracted kp-connector.exe failed self-check (--version)" }
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Path $KpExe.FullName -Destination (Join-Path $InstallDir "kp.exe") -Force
    if ($ConnExe) {
        Copy-Item -Path $ConnExe.FullName -Destination (Join-Path $InstallDir "kp-connector.exe") -Force
    }

    Write-Host "==> Installed Kipo ($VerOut) to $InstallDir"
    Write-Host "    Add $InstallDir to your user PATH if needed, then run: kp demo"
}
finally {
    Remove-Item -Recurse -Force -Path $TmpDir -ErrorAction SilentlyContinue
}
