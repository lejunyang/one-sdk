<#
.SYNOPSIS
Installs osdk from GitHub Releases.

.DESCRIPTION
Downloads the requested Windows release archive, verifies its SHA-256 checksum,
and installs osdk.exe, osdk-shim.exe, and osdk-aube.exe.

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
$transactionDir = $null
$preserveTransaction = $false
$lockPath = $null
$installLock = $null
$lockAcquired = $false
$binaries = @("osdk.exe", "osdk-shim.exe", "osdk-aube.exe")

function Add-TransactionJournalRecord {
    param(
        [Parameter(Mandatory)][string]$JournalPath,
        [Parameter(Mandatory)][string]$Record
    )

    $bytes = [System.Text.Encoding]::ASCII.GetBytes("$Record`n")
    $stream = [System.IO.FileStream]::new(
        $JournalPath,
        [System.IO.FileMode]::Append,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::Read,
        4096,
        [System.IO.FileOptions]::WriteThrough
    )
    try {
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
}

function Get-TransactionPhase {
    param([Parameter(Mandatory)][string]$TransactionPath)

    $journalPath = Join-Path $TransactionPath "journal"
    if (-not (Test-Path -LiteralPath $journalPath)) {
        return "initializing"
    }
    if (-not (Test-Path -LiteralPath $journalPath -PathType Leaf)) {
        throw "Invalid installer transaction journal: $journalPath"
    }

    $content = [System.Text.Encoding]::ASCII.GetString(
        [System.IO.File]::ReadAllBytes($journalPath)
    )
    $records = [System.Collections.Generic.List[string]]::new()
    $pieces = $content.Split([char]10)
    # Only newline-terminated records are durable. Ignore a torn final append.
    for ($index = 0; $index -lt ($pieces.Length - 1); $index++) {
        [void]$records.Add($pieces[$index].TrimEnd([char]13))
    }
    if ($records.Count -eq 0) {
        return "initializing"
    }
    if ($records[0] -ne "version=1") {
        throw "Invalid installer transaction journal: $journalPath"
    }

    $phase = $null
    for ($index = 1; $index -lt $records.Count; $index++) {
        $record = $records[$index]
        switch ($record) {
            "phase=initializing" {
                if ($phase) {
                    throw "Invalid installer transaction journal: $journalPath"
                }
                $phase = "initializing"
            }
            "phase=staging" {
                if ($phase -ne "initializing") {
                    throw "Invalid installer transaction journal: $journalPath"
                }
                $phase = "staging"
            }
            "phase=prepared" {
                if ($phase -ne "staging") {
                    throw "Invalid installer transaction journal: $journalPath"
                }
                $phase = "prepared"
            }
            "phase=promoting" {
                if ($phase -ne "prepared") {
                    throw "Invalid installer transaction journal: $journalPath"
                }
                $phase = "promoting"
            }
            "phase=committed" {
                if ($phase -ne "promoting") {
                    throw "Invalid installer transaction journal: $journalPath"
                }
                $phase = "committed"
            }
            default {
                throw "Invalid installer transaction journal: $journalPath"
            }
        }
    }
    if (-not $phase) {
        return "initializing"
    }
    return $phase
}

function Restore-InstallerTransaction {
    param(
        [Parameter(Mandatory)][string]$TransactionPath,
        [Parameter(Mandatory)][string]$DestinationDirectory,
        [Parameter(Mandatory)][string[]]$BinaryNames,
        [Parameter(Mandatory)][bool]$RemoveNewInstallations
    )

    $stagedDirectory = Join-Path $TransactionPath "new"
    $backupDirectory = Join-Path $TransactionPath "old"
    $restoreDirectory = Join-Path $TransactionPath "restore"
    New-Item -ItemType Directory -Force -Path $restoreDirectory | Out-Null
    $rollbackErrors = [System.Collections.Generic.List[string]]::new()

    foreach ($binary in $BinaryNames) {
        $staged = Join-Path $stagedDirectory $binary
        $backup = Join-Path $backupDirectory $binary
        $destination = Join-Path $DestinationDirectory $binary
        $restorePath = Join-Path $restoreDirectory $binary
        try {
            if (Test-Path -LiteralPath $backup -PathType Leaf) {
                # Keep the journal backup intact until the whole transaction is
                # cleaned up, making a second crash during recovery replayable.
                Copy-Item -LiteralPath $backup -Destination $restorePath -Force
                if (Test-Path -LiteralPath $destination) {
                    Remove-Item -Force -LiteralPath $destination
                }
                Move-Item -LiteralPath $restorePath -Destination $destination
            } elseif ($RemoveNewInstallations -and
                -not (Test-Path -LiteralPath $staged) -and
                (Test-Path -LiteralPath $destination)) {
                Remove-Item -Force -LiteralPath $destination
            }
        } catch {
            [void]$rollbackErrors.Add($_.Exception.Message)
        }
    }

    if ($rollbackErrors.Count -gt 0) {
        throw "Rollback failed: $($rollbackErrors -join '; ')"
    }
}

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
    foreach ($binary in $binaries) {
        $source = Join-Path $unpackDir $binary
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "$archive does not contain $binary."
        }
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $lockPath = Join-Path $InstallDir ".osdk-install.lock"
    try {
        $installLock = [System.IO.File]::Open(
            $lockPath,
            [System.IO.FileMode]::OpenOrCreate,
            [System.IO.FileAccess]::ReadWrite,
            [System.IO.FileShare]::None
        )
        $lockAcquired = $true
    } catch {
        throw "Another installer is updating $InstallDir (lock: $lockPath): $($_.Exception.Message)"
    }
    # A hard termination bypasses `finally`. Journal phases before "promoting"
    # never touched destinations; only promotion-phase journals need rollback.
    foreach ($stale in @(Get-ChildItem -Force -LiteralPath $InstallDir -Directory | Where-Object {
        $_.Name.StartsWith(".osdk-install-") -or
        $_.Name.StartsWith(".osdk-install.")
    })) {
        $staleNew = Join-Path $stale.FullName "new"
        $staleOld = Join-Path $stale.FullName "old"
        if ($stale.Name.StartsWith(".osdk-install-v2.")) {
            $phase = Get-TransactionPhase -TransactionPath $stale.FullName
            switch ($phase) {
                { $_ -in @("initializing", "staging", "prepared") } {
                    # Partial initialization/staging is disposable.
                }
                "promoting" {
                    if (-not (Test-Path -LiteralPath $staleNew -PathType Container) -or
                        -not (Test-Path -LiteralPath $staleOld -PathType Container)) {
                        throw "Invalid promoting installer transaction: $($stale.FullName)"
                    }
                    Restore-InstallerTransaction `
                        -TransactionPath $stale.FullName `
                        -DestinationDirectory $InstallDir `
                        -BinaryNames $binaries `
                        -RemoveNewInstallations $true
                }
                "committed" {
                    # The complete new set was durable before this record.
                }
                default {
                    throw "Invalid installer transaction phase in $($stale.FullName)"
                }
            }
        } else {
            # Previous installers had markerless journals. Backups prove that
            # promotion began; absent backups are ambiguous and never justify
            # deleting an intact destination.
            $legacyHasBackup = @($binaries | Where-Object {
                Test-Path -LiteralPath (Join-Path $staleOld $_) -PathType Leaf
            }).Count -gt 0
            if ($legacyHasBackup) {
                Restore-InstallerTransaction `
                    -TransactionPath $stale.FullName `
                    -DestinationDirectory $InstallDir `
                    -BinaryNames $binaries `
                    -RemoveNewInstallations $false
            }
        }
        Remove-Item -Recurse -Force -LiteralPath $stale.FullName
    }
    foreach ($binary in $binaries) {
        $destination = Join-Path $InstallDir $binary
        if ((Test-Path -LiteralPath $destination) -and
            -not (Test-Path -LiteralPath $destination -PathType Leaf)) {
            throw "Install destination is not a file: $destination"
        }
    }

    # Stage on the destination filesystem so each final Move-Item is a rename.
    # The journal makes the set rollback-capable, but a flat three-file layout
    # cannot make all names become visible as one atomic filesystem operation.
    $transactionDir = Join-Path $InstallDir (".osdk-install-v2." + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $transactionDir | Out-Null
    $journalPath = Join-Path $transactionDir "journal"
    Add-TransactionJournalRecord -JournalPath $journalPath -Record "version=1"
    Add-TransactionJournalRecord -JournalPath $journalPath -Record "phase=initializing"
    $newDir = Join-Path $transactionDir "new"
    $backupDir = Join-Path $transactionDir "old"
    New-Item -ItemType Directory -Path $newDir, $backupDir | Out-Null
    Add-TransactionJournalRecord -JournalPath $journalPath -Record "phase=staging"
    foreach ($binary in $binaries) {
        Copy-Item `
            -LiteralPath (Join-Path $unpackDir $binary) `
            -Destination (Join-Path $newDir $binary)
    }
    foreach ($binary in $binaries) {
        if (-not (Test-Path -LiteralPath (Join-Path $newDir $binary) -PathType Leaf)) {
            throw "Staging did not produce $binary."
        }
    }
    Add-TransactionJournalRecord -JournalPath $journalPath -Record "phase=prepared"

    $backedUp = @{}
    $promoted = @{}
    try {
        Add-TransactionJournalRecord -JournalPath $journalPath -Record "phase=promoting"
        foreach ($binary in $binaries) {
            $destination = Join-Path $InstallDir $binary
            if (Test-Path -LiteralPath $destination) {
                Move-Item `
                    -LiteralPath $destination `
                    -Destination (Join-Path $backupDir $binary)
                $backedUp[$binary] = $true
            }
        }
        foreach ($binary in $binaries) {
            Move-Item `
                -LiteralPath (Join-Path $newDir $binary) `
                -Destination (Join-Path $InstallDir $binary)
            $promoted[$binary] = $true
        }
        Add-TransactionJournalRecord -JournalPath $journalPath -Record "phase=committed"
    } catch {
        $installError = $_
        try {
            Restore-InstallerTransaction `
                -TransactionPath $transactionDir `
                -DestinationDirectory $InstallDir `
                -BinaryNames $binaries `
                -RemoveNewInstallations $true
        } catch {
            $preserveTransaction = $true
            throw "Installation failed: $($installError.Exception.Message); $($_.Exception.Message)"
        }
        throw $installError
    }

    Write-Host "Installed osdk, osdk-shim, and osdk-aube to $InstallDir"
    $pathEntries = $env:PATH -split [System.IO.Path]::PathSeparator
    if ($InstallDir -notin $pathEntries) {
        Write-Host "Add $InstallDir to PATH to run osdk."
    }
} finally {
    try {
        if ($transactionDir -and (Test-Path -LiteralPath $transactionDir)) {
            if ($preserveTransaction) {
                Write-Warning "Preserving installer recovery files at $transactionDir"
            } else {
                Remove-Item -Recurse -Force -LiteralPath $transactionDir
            }
        }
    } finally {
        try {
            if ($lockAcquired) {
                $installLock.Dispose()
            }
        } finally {
            if (Test-Path -LiteralPath $tempDir) {
                Remove-Item -Recurse -Force -LiteralPath $tempDir
            }
        }
    }
}
