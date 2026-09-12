<#
.SYNOPSIS
Installs osdk from GitHub Releases and sets up shell integration.

.DESCRIPTION
Downloads the requested Windows release archive, verifies its SHA-256 checksum,
and installs osdk.exe and osdk-shim.exe. Afterwards it detects the shells
present on the system, asks which of them to configure, proposes defaults for
the three osdk directories, and writes a replaceable block into each selected
shell's startup file.

Every prompt has a matching parameter, so passing the parameters you care about
lets the installer run unattended.

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

.PARAMETER Shells
Shells to configure: "all", "none", or any of pwsh, powershell, bash, zsh,
fish. Supplying this suppresses the shell selection prompt.

.PARAMETER NoModifyShell
Equivalent to -Shells none: installs the binaries and writes no startup file.

.PARAMETER ConfigDir
Value to export as OSDK_CONFIG_DIR. Suppresses that prompt.

.PARAMETER DataDir
Value to export as OSDK_DATA_DIR. Suppresses that prompt.

.PARAMETER CacheDir
Value to export as OSDK_CACHE_DIR. Suppresses that prompt.

.PARAMETER AcceptDefaults
Never prompts; accepts every proposed default.

.EXAMPLE
.\install.ps1 -Shells pwsh -AcceptDefaults

Installs and configures PowerShell without asking anything.
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
    [switch]$SkipVerify,
    # Seeded from the environment so an already-configured shell keeps its own
    # layout as the proposal. Only the parameters suppress the prompts.
    [string]$Shells = $(if ($env:OSDK_SETUP_SHELLS) { $env:OSDK_SETUP_SHELLS } else { "" }),
    [switch]$NoModifyShell,
    [string]$ConfigDir = $(if ($env:OSDK_CONFIG_DIR) { $env:OSDK_CONFIG_DIR } else { "" }),
    [string]$DataDir = $(if ($env:OSDK_DATA_DIR) { $env:OSDK_DATA_DIR } else { "" }),
    [string]$CacheDir = $(if ($env:OSDK_CACHE_DIR) { $env:OSDK_CACHE_DIR } else { "" }),
    [switch]$AcceptDefaults
)

$ErrorActionPreference = "Stop"

# A bound parameter is an explicit answer; an environment seed is only a
# proposal. PSBoundParameters is the only way to tell the two apart.
$configDirExplicit = $PSBoundParameters.ContainsKey("ConfigDir")
$dataDirExplicit = $PSBoundParameters.ContainsKey("DataDir")
$cacheDirExplicit = $PSBoundParameters.ContainsKey("CacheDir")
$shellsExplicit = $PSBoundParameters.ContainsKey("Shells") -or
    $NoModifyShell -or
    -not [string]::IsNullOrWhiteSpace($env:OSDK_SETUP_SHELLS)
if ($NoModifyShell) {
    $Shells = "none"
}
if (-not $AcceptDefaults -and $env:OSDK_ACCEPT_DEFAULTS -eq "1") {
    $AcceptDefaults = $true
}

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
$binaries = @("osdk.exe", "osdk-shim.exe")

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

# ---------------------------------------------------------------------------
# Shell setup
# ---------------------------------------------------------------------------

$script:BeginMarker = "# >>> osdk initialize >>>"
$script:EndMarker = "# <<< osdk initialize <<<"

function Test-InteractiveHost {
    # A prompt needs a real console on both ends. Redirected input is how
    # `irm ... | iex` and CI arrive here, and neither can answer a question.
    if ($AcceptDefaults) { return $false }
    try {
        if ([Console]::IsInputRedirected) { return $false }
    } catch {
        return $false
    }
    return $true
}

function Get-DefaultStateDirectory {
    param([Parameter(Mandatory)][ValidateSet("config", "data", "cache")][string]$Kind)

    # Mirror what the directories crate derives for osdk on Windows, so
    # accepting the proposal relocates nothing and only makes it explicit.
    $roaming = [Environment]::GetFolderPath("ApplicationData")
    $local = [Environment]::GetFolderPath("LocalApplicationData")
    switch ($Kind) {
        "config" { return (Join-Path $roaming "osdk\config") }
        "data" { return (Join-Path $roaming "osdk\data") }
        "cache" { return (Join-Path $local "osdk\cache") }
    }
}

function Test-UsableDirectory {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Path)

    $script:DirectoryError = $null
    if ([string]::IsNullOrWhiteSpace($Path)) {
        $script:DirectoryError = "the path must not be empty"
        return $false
    }
    try {
        if (-not [System.IO.Path]::IsPathRooted($Path)) {
            $script:DirectoryError = "the path must be absolute"
            return $false
        }
    } catch {
        $script:DirectoryError = "the path is not a valid Windows path"
        return $false
    }
    if ($Path.IndexOfAny([System.IO.Path]::GetInvalidPathChars()) -ge 0) {
        $script:DirectoryError = "the path contains characters Windows does not allow"
        return $false
    }
    if ((Test-Path -LiteralPath $Path) -and
        -not (Test-Path -LiteralPath $Path -PathType Container)) {
        $script:DirectoryError = "the path exists and is not a directory"
        return $false
    }
    try {
        New-Item -ItemType Directory -Force -Path $Path -ErrorAction Stop | Out-Null
    } catch {
        $script:DirectoryError = "the directory could not be created: $($_.Exception.Message)"
        return $false
    }
    # Permission bits do not settle writability on Windows either: inherited
    # denies, read-only media and full volumes all disagree. Write a probe.
    $probe = Join-Path $Path (".osdk-write-probe." + [guid]::NewGuid())
    try {
        [System.IO.File]::WriteAllText($probe, "")
        Remove-Item -LiteralPath $probe -Force -ErrorAction SilentlyContinue
    } catch {
        $script:DirectoryError = "the directory is not writable"
        return $false
    }
    return $true
}

function Get-DocumentsDirectory {
    # PowerShell locates profiles under the Documents *known folder*, which
    # OneDrive and corporate folder redirection both move. Reading the known
    # folder is therefore the only way to land on the profile the shell will
    # actually load -- USERPROFILE\Documents would silently miss it.
    #
    # The known folder comes from the registry and so ignores an overridden
    # USERPROFILE. When the two disagree, the caller is running under a
    # redirected profile (a test sandbox, another user context) and USERPROFILE
    # is the honest answer.
    $documents = [Environment]::GetFolderPath("MyDocuments")
    $realProfile = [Environment]::GetFolderPath("UserProfile")
    if ($env:USERPROFILE -and $realProfile -and
        -not [string]::Equals($env:USERPROFILE, $realProfile, "OrdinalIgnoreCase")) {
        return (Join-Path $env:USERPROFILE "Documents")
    }
    if ([string]::IsNullOrWhiteSpace($documents)) {
        return (Join-Path $env:USERPROFILE "Documents")
    }
    return $documents
}

function Get-ShellProfilePath {
    param([Parameter(Mandatory)][string]$Shell)

    $userHome = if ($env:USERPROFILE) { $env:USERPROFILE } else { $HOME }
    $documents = Get-DocumentsDirectory
    switch ($Shell) {
        "pwsh" {
            return (Join-Path $documents "PowerShell\Microsoft.PowerShell_profile.ps1")
        }
        "powershell" {
            return (Join-Path $documents "WindowsPowerShell\Microsoft.PowerShell_profile.ps1")
        }
        "bash" { return (Join-Path $userHome ".bashrc") }
        "zsh" { return (Join-Path $userHome ".zshrc") }
        "fish" { return (Join-Path $userHome ".config\fish\config.fish") }
    }
    throw "No startup file is known for shell: $Shell"
}

function Get-DetectedShells {
    $detected = [System.Collections.Generic.List[string]]::new()
    # Windows PowerShell ships with the OS, so it is present whether or not the
    # command resolves through the current PATH.
    if (Test-Path -LiteralPath (Join-Path $env:WINDIR "System32\WindowsPowerShell\v1.0\powershell.exe")) {
        [void]$detected.Add("powershell")
    }
    foreach ($candidate in @("pwsh", "bash", "zsh", "fish")) {
        if (Get-Command $candidate -ErrorAction SilentlyContinue) {
            [void]$detected.Add($candidate)
        }
    }
    # The shell running this script counts even when its own name is shadowed.
    $self = if ($PSVersionTable.PSEdition -eq "Core") { "pwsh" } else { "powershell" }
    if (-not $detected.Contains($self)) {
        [void]$detected.Insert(0, $self)
    }
    return , $detected.ToArray()
}

function Resolve-ShellSelection {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$Selection,
        [Parameter(Mandatory)][string[]]$Detected,
        [bool]$AllowIndices = $false
    )

    $script:SelectionError = $null
    $known = @("pwsh", "powershell", "bash", "zsh", "fish")
    if ([string]::IsNullOrWhiteSpace($Selection) -or $Selection -eq "all") {
        return , $Detected
    }
    if ($Selection -eq "none") {
        return , @()
    }

    $resolved = [System.Collections.Generic.List[string]]::new()
    foreach ($token in ($Selection -split "[,\s]+" | Where-Object { $_ })) {
        $name = $null
        if ($AllowIndices -and $token -match '^\d+$') {
            $index = [int]$token
            if ($index -ge 1 -and $index -le $Detected.Count) {
                $name = $Detected[$index - 1]
            }
        } elseif ($known -contains $token.ToLowerInvariant()) {
            $name = $token.ToLowerInvariant()
        }
        if (-not $name) {
            $script:SelectionError = "unknown shell selection ``$token``"
            return $null
        }
        if (-not $resolved.Contains($name)) {
            [void]$resolved.Add($name)
        }
    }
    return , $resolved.ToArray()
}

function Read-ShellSelection {
    param([Parameter(Mandatory)][string[]]$Detected)

    Write-Host "Detected shells:"
    for ($index = 0; $index -lt $Detected.Count; $index++) {
        $shell = $Detected[$index]
        Write-Host ("  {0}) {1,-10} {2}" -f ($index + 1), $shell, (Get-ShellProfilePath $shell))
    }
    while ($true) {
        $answer = (Read-Host "Configure which shells? [numbers/names, all, none] (all)").Trim()
        $selection = Resolve-ShellSelection -Selection $answer -Detected $Detected -AllowIndices $true
        if ($null -ne $selection) {
            return , $selection
        }
        Write-Host "  $script:SelectionError"
    }
}

function Resolve-StateDirectory {
    param(
        [Parameter(Mandatory)][string]$Label,
        [Parameter(Mandatory)][AllowEmptyString()][string]$Current,
        [Parameter(Mandatory)][bool]$IsExplicit,
        [Parameter(Mandatory)][string]$Kind,
        [Parameter(Mandatory)][bool]$Interactive
    )

    if ($IsExplicit -or (-not $Interactive -and -not [string]::IsNullOrWhiteSpace($Current))) {
        if (-not (Test-UsableDirectory -Path $Current)) {
            throw "$Label=$Current is unusable: $script:DirectoryError"
        }
        return $Current
    }

    $proposed = if ([string]::IsNullOrWhiteSpace($Current)) {
        Get-DefaultStateDirectory -Kind $Kind
    } else {
        $Current
    }
    if (-not $Interactive) {
        if (-not (Test-UsableDirectory -Path $proposed)) {
            throw "$Label=$proposed is unusable: $script:DirectoryError"
        }
        return $proposed
    }

    while ($true) {
        $answer = (Read-Host "$Label [$proposed]").Trim()
        $candidate = if ($answer) { $answer } else { $proposed }
        if (Test-UsableDirectory -Path $candidate) {
            return $candidate
        }
        Write-Host "  ${candidate}: $script:DirectoryError"
    }
}

function ConvertTo-PowerShellLiteral {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Value)
    return "'" + $Value.Replace("'", "''") + "'"
}

function ConvertTo-PosixLiteral {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Value)
    return "'" + $Value.Replace("'", "'\''") + "'"
}

function ConvertTo-FishLiteral {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Value)
    # fish treats both backslash and quote as escapes inside single quotes.
    return "'" + $Value.Replace("\", "\\").Replace("'", "\'") + "'"
}

function ConvertTo-PosixPath {
    param([Parameter(Mandatory)][string]$Path)
    # bash, zsh and fish on Windows run under MSYS/Cygwin/WSL layouts where a
    # drive letter is spelled /c/... . Emit that rather than a path their own
    # test builtin cannot resolve.
    if ($Path -match '^([A-Za-z]):[\\/](.*)$') {
        return "/" + $Matches[1].ToLowerInvariant() + "/" + ($Matches[2] -replace '\\', '/')
    }
    return ($Path -replace '\\', '/')
}

function Get-ActivationBlock {
    param(
        [Parameter(Mandatory)][string]$Shell,
        [Parameter(Mandatory)][string]$BinDir,
        [Parameter(Mandatory)][string]$Config,
        [Parameter(Mandatory)][string]$Data,
        [Parameter(Mandatory)][string]$Cache
    )

    if ($Shell -in @("pwsh", "powershell")) {
        $lines = @(
            "`$env:OSDK_CONFIG_DIR = $(ConvertTo-PowerShellLiteral $Config)"
            "`$env:OSDK_DATA_DIR = $(ConvertTo-PowerShellLiteral $Data)"
            "`$env:OSDK_CACHE_DIR = $(ConvertTo-PowerShellLiteral $Cache)"
            "`$osdkBinDir = $(ConvertTo-PowerShellLiteral $BinDir)"
            "if (-not ((`$env:PATH -split [IO.Path]::PathSeparator) -contains `$osdkBinDir)) {"
            "  `$env:PATH = `$osdkBinDir + [IO.Path]::PathSeparator + `$env:PATH"
            "}"
            "`$osdkExe = Join-Path `$osdkBinDir 'osdk.exe'"
            "if (Test-Path -LiteralPath `$osdkExe) {"
            # New-Module + Import-Module -Global is required, not stylistic: a
            # plain Invoke-Expression defines Invoke-OsdkHook in a scope that
            # disappears, after which every prompt raises CommandNotFound.
            "  `$osdkSnippet = (& `$osdkExe activate powershell | Out-String)"
            "  if (`$osdkSnippet) {"
            "    `$null = New-Module -Name osdk-activate ``"
            "      -ScriptBlock ([scriptblock]::Create(`$osdkSnippet)) |"
            "      Import-Module -Global -Force"
            "  }"
            "}"
            "Remove-Variable osdkBinDir, osdkExe, osdkSnippet -ErrorAction SilentlyContinue"
        )
        return ($lines -join "`n")
    }

    $posixBin = ConvertTo-PosixPath $BinDir
    $posixConfig = ConvertTo-PosixPath $Config
    $posixData = ConvertTo-PosixPath $Data
    $posixCache = ConvertTo-PosixPath $Cache

    if ($Shell -eq "fish") {
        $lines = @(
            "set -gx OSDK_CONFIG_DIR $(ConvertTo-FishLiteral $posixConfig)"
            "set -gx OSDK_DATA_DIR $(ConvertTo-FishLiteral $posixData)"
            "set -gx OSDK_CACHE_DIR $(ConvertTo-FishLiteral $posixCache)"
            "set -l _osdk_bin $(ConvertTo-FishLiteral $posixBin)"
            "if not contains -- `$_osdk_bin `$PATH"
            "    set -gx PATH `$_osdk_bin `$PATH"
            "end"
            "if test -x `"`$_osdk_bin/osdk.exe`""
            "    `"`$_osdk_bin/osdk.exe`" activate fish | source"
            "end"
            "set -e _osdk_bin"
        )
        return ($lines -join "`n")
    }

    $lines = @(
        "export OSDK_CONFIG_DIR=$(ConvertTo-PosixLiteral $posixConfig)"
        "export OSDK_DATA_DIR=$(ConvertTo-PosixLiteral $posixData)"
        "export OSDK_CACHE_DIR=$(ConvertTo-PosixLiteral $posixCache)"
        "_osdk_bin=$(ConvertTo-PosixLiteral $posixBin)"
        "case `":`$PATH:`" in"
        "  *`":`$_osdk_bin:`"*) ;;"
        "  *) PATH=`"`$_osdk_bin:`$PATH`" ;;"
        "esac"
        "export PATH"
        "if [ -x `"`$_osdk_bin/osdk.exe`" ]; then"
        "  eval `"`$(`"`$_osdk_bin/osdk.exe`" activate $Shell)`""
        "fi"
        "unset _osdk_bin"
    )
    return ($lines -join "`n")
}

function Write-ShellProfile {
    param(
        [Parameter(Mandatory)][string]$Shell,
        [Parameter(Mandatory)][string]$BinDir,
        [Parameter(Mandatory)][string]$Config,
        [Parameter(Mandatory)][string]$Data,
        [Parameter(Mandatory)][string]$Cache
    )

    $profilePath = Get-ShellProfilePath $Shell
    $parent = Split-Path -Parent $profilePath
    if ($parent -and -not (Test-Path -LiteralPath $parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }

    # Strip any previous managed block so a reinstall replaces rather than
    # stacks, and the user's own lines outside it survive untouched.
    $kept = [System.Collections.Generic.List[string]]::new()
    if (Test-Path -LiteralPath $profilePath -PathType Leaf) {
        $skipping = $false
        foreach ($line in [System.IO.File]::ReadAllLines($profilePath)) {
            if ($line -eq $script:BeginMarker) { $skipping = $true; continue }
            if ($line -eq $script:EndMarker) { $skipping = $false; continue }
            if (-not $skipping) { [void]$kept.Add($line) }
        }
    }

    [void]$kept.Add($script:BeginMarker)
    [void]$kept.Add("# Written by the osdk installer. Rerunning it replaces this")
    [void]$kept.Add("# block; delete the block to remove the integration.")
    foreach ($line in (Get-ActivationBlock -Shell $Shell -BinDir $BinDir `
                -Config $Config -Data $Data -Cache $Cache) -split "`n") {
        [void]$kept.Add($line)
    }
    [void]$kept.Add($script:EndMarker)

    if (Test-Path -LiteralPath $profilePath -PathType Leaf) {
        Copy-Item -LiteralPath $profilePath -Destination "$profilePath.osdk-backup" -Force
    }
    # A POSIX shell on Windows reads its startup file literally: CRLF would
    # leave a trailing carriage return inside every quoted value.
    $newline = if ($Shell -in @("pwsh", "powershell")) { "`r`n" } else { "`n" }
    $encoding = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText(
        $profilePath,
        ($kept -join $newline) + $newline,
        $encoding
    )

    # Read the destination back rather than trusting the write.
    $written = [System.IO.File]::ReadAllText($profilePath)
    if (-not $written.Contains($script:BeginMarker) -or
        -not $written.Contains($script:EndMarker) -or
        -not $written.Contains("OSDK_CONFIG_DIR")) {
        throw "Verification of $profilePath failed after writing the osdk block."
    }
    return $profilePath
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

    Write-Host "Installed osdk and osdk-shim to $InstallDir"

    # Hand the lock back before the first prompt. Directory selection blocks on
    # a human for an unbounded time, and a concurrent installer should not have
    # to wait behind it. The finally block's release then becomes a no-op.
    if ($lockAcquired) {
        $installLock.Dispose()
        $lockAcquired = $false
    }

    # -----------------------------------------------------------------------
    # Shell setup
    # -----------------------------------------------------------------------

    $interactive = Test-InteractiveHost
    $detectedShells = Get-DetectedShells
    $selectedShells = @()
    $selectionDeclined = $false

    if ($shellsExplicit) {
        $selectedShells = Resolve-ShellSelection -Selection $Shells -Detected $detectedShells
        if ($null -eq $selectedShells) {
            throw $script:SelectionError
        }
    } elseif ($AcceptDefaults) {
        $selectedShells = $detectedShells
    } elseif ($interactive) {
        $selectedShells = Read-ShellSelection -Detected $detectedShells
    } else {
        $selectionDeclined = $true
    }

    if ($selectedShells.Count -gt 0) {
        $ConfigDir = Resolve-StateDirectory -Label "OSDK_CONFIG_DIR" `
            -Current $ConfigDir -IsExplicit $configDirExplicit -Kind "config" `
            -Interactive $interactive
        $DataDir = Resolve-StateDirectory -Label "OSDK_DATA_DIR" `
            -Current $DataDir -IsExplicit $dataDirExplicit -Kind "data" `
            -Interactive $interactive
        $CacheDir = Resolve-StateDirectory -Label "OSDK_CACHE_DIR" `
            -Current $CacheDir -IsExplicit $cacheDirExplicit -Kind "cache" `
            -Interactive $interactive

        foreach ($shell in $selectedShells) {
            $written = Write-ShellProfile -Shell $shell -BinDir $InstallDir `
                -Config $ConfigDir -Data $DataDir -Cache $CacheDir
            Write-Host "Configured $shell in $written"
        }

        # Activate in the session that launched the installer, so osdk works
        # without opening a new window. Environment variables set here are
        # process-wide and survive this script's scope; the hook's functions do
        # not, which is why the snippet is imported as a global module.
        $env:OSDK_CONFIG_DIR = $ConfigDir
        $env:OSDK_DATA_DIR = $DataDir
        $env:OSDK_CACHE_DIR = $CacheDir
        $pathEntries = $env:PATH -split [System.IO.Path]::PathSeparator
        if ($InstallDir -notin $pathEntries) {
            $env:PATH = $InstallDir + [System.IO.Path]::PathSeparator + $env:PATH
        }
        try {
            $osdkExe = Join-Path $InstallDir "osdk.exe"
            $snippet = (& $osdkExe activate powershell | Out-String)
            if ($snippet) {
                $null = New-Module -Name osdk-activate `
                    -ScriptBlock ([scriptblock]::Create($snippet)) |
                    Import-Module -Global -Force
                Write-Host "Activated osdk in the current session."
            }
        } catch {
            # The install itself succeeded; only this convenience failed.
            Write-Warning "Could not activate osdk in the current session: $($_.Exception.Message)"
            Write-Host "Open a new shell, or run: & '$InstallDir\osdk.exe' activate powershell | Out-String | Invoke-Expression"
        }
    } else {
        if ($selectionDeclined -and -not $shellsExplicit) {
            Write-Host "No shell was configured. Pass -Shells to set one up unattended."
        }
        $pathEntries = $env:PATH -split [System.IO.Path]::PathSeparator
        if ($InstallDir -notin $pathEntries) {
            Write-Host "Add $InstallDir to PATH to run osdk."
        }
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
