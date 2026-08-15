<#
.SYNOPSIS
Installs osdk from GitHub Releases.

.DESCRIPTION
Downloads the requested Windows release archive, verifies its SHA-256 checksum,
and installs osdk.exe and osdk-shim.exe.

.PARAMETER Version
Release version with or without the v prefix. Defaults to latest.

.PARAMETER InstallDir
Destination directory for the binaries.

.PARAMETER Repository
GitHub owner/repository name.

.PARAMETER BaseUrl
GitHub or mirror base URL.

.PARAMETER Target
Rust target triple used in the release asset name.

.PARAMETER SkipVerify
Skips SHA-256 verification.
#>
[CmdletBinding()]
param(
    [string]$Version = $(if ($env:OSDK_VERSION) { $env:OSDK_VERSION } else { "latest" }),
    [string]$InstallDir = $(if ($env:OSDK_BIN_DIR) {
        $env:OSDK_BIN_DIR
    } else {
        Join-Path ([Environment]::GetFolderPath("LocalApplicationData")) "Programs\osdk\bin"
    }),
    [string]$Repository = $(if ($env:OSDK_REPOSITORY) {
        $env:OSDK_REPOSITORY
    } else {
        "lejunyang/one-sdk"
    }),
    [string]$BaseUrl = $(if ($env:OSDK_DOWNLOAD_BASE_URL) {
        $env:OSDK_DOWNLOAD_BASE_URL
    } else {
        "https://github.com"
    }),
    [string]$Target = $(if ($env:OSDK_TARGET) { $env:OSDK_TARGET } else { "" }),
    [switch]$SkipVerify
)

$ErrorActionPreference = "Stop"

if (-not $Target) {
    $architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($architecture) {
        "X64" { $Target = "x86_64-pc-windows-msvc" }
        default {
            throw "Unsupported Windows architecture: $architecture. Pass -Target to override."
        }
    }
}

if ($Version -eq "latest") {
    $releasePath = "latest/download"
} else {
    $tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
    $releasePath = "download/$tag"
}

$BaseUrl = $BaseUrl.TrimEnd("/")
$archive = "osdk-$Target.zip"
$releaseUrl = "$BaseUrl/$Repository/releases/$releasePath"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("osdk-install-" + [guid]::NewGuid())

try {
    New-Item -ItemType Directory -Force -Path $tempDir | Out-Null
    $archivePath = Join-Path $tempDir $archive
    Write-Host "Downloading $releaseUrl/$archive"
    Invoke-WebRequest -UseBasicParsing -Uri "$releaseUrl/$archive" -OutFile $archivePath

    $skipFromEnvironment = $env:OSDK_SKIP_VERIFY -eq "1"
    if (-not $SkipVerify -and -not $skipFromEnvironment) {
        $checksumsPath = Join-Path $tempDir "SHA256SUMS"
        Invoke-WebRequest `
            -UseBasicParsing `
            -Uri "$releaseUrl/SHA256SUMS" `
            -OutFile $checksumsPath
        $checksumLine = Get-Content $checksumsPath | Where-Object {
            $_ -match "^[0-9a-fA-F]{64}\s+\*?$([regex]::Escape($archive))$"
        } | Select-Object -First 1
        if (-not $checksumLine) {
            throw "Checksum for $archive is missing from SHA256SUMS."
        }
        $expected = ($checksumLine -split "\s+")[0].ToLowerInvariant()
        $actual = (Get-FileHash -Algorithm SHA256 $archivePath).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            throw "Checksum verification failed for $archive."
        }
    }

    $unpackDir = Join-Path $tempDir "unpack"
    Expand-Archive -Path $archivePath -DestinationPath $unpackDir
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    foreach ($binary in @("osdk.exe", "osdk-shim.exe")) {
        $source = Join-Path $unpackDir $binary
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "$archive does not contain $binary."
        }
        Copy-Item -Force -LiteralPath $source -Destination (Join-Path $InstallDir $binary)
    }

    Write-Host "Installed osdk and osdk-shim to $InstallDir"
    $pathEntries = $env:PATH -split [System.IO.Path]::PathSeparator
    if ($InstallDir -notin $pathEntries) {
        Write-Host "Add $InstallDir to PATH to run osdk."
    }
} finally {
    if (Test-Path -LiteralPath $tempDir) {
        Remove-Item -Recurse -Force -LiteralPath $tempDir
    }
}
