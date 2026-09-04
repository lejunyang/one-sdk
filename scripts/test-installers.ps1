[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent $PSScriptRoot
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("osdk-installer-test-" + [guid]::NewGuid())
$server = $null
$target = "x86_64-pc-windows-msvc"
$archive = "osdk-$target.zip"
$binaries = @("osdk.exe", "osdk-shim.exe")

function Write-BinarySet {
    param(
        [Parameter(Mandatory)][string]$Directory,
        [Parameter(Mandatory)][string]$Label,
        [string[]]$Names = $script:binaries
    )

    New-Item -ItemType Directory -Force -Path $Directory | Out-Null
    foreach ($binary in $Names) {
        [System.IO.File]::WriteAllText(
            (Join-Path $Directory $binary),
            "$Label $binary",
            [System.Text.UTF8Encoding]::new($false)
        )
    }
}

function Assert-BinarySet {
    param(
        [Parameter(Mandatory)][string]$Directory,
        [Parameter(Mandatory)][string]$Label
    )

    foreach ($binary in $script:binaries) {
        $path = Join-Path $Directory $binary
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "Missing installed binary: $path"
        }
        $actual = [System.IO.File]::ReadAllText($path)
        if ($actual -ne "$Label $binary") {
            throw "Unexpected contents for ${path}: $actual"
        }
    }
}

function Assert-NoTransactionDirectory {
    param([Parameter(Mandatory)][string]$Directory)

    $leftovers = @(Get-ChildItem -Force -LiteralPath $Directory | Where-Object {
        $_.PSIsContainer -and
        $_.Name.StartsWith(".osdk-install-")
    })
    if ($leftovers.Count -ne 0) {
        throw "Installer left transaction directories under ${Directory}: $($leftovers.FullName -join ', ')"
    }
}

function Write-TransactionJournal {
    param(
        [Parameter(Mandatory)][string]$TransactionPath,
        [string[]]$Phases = @()
    )

    New-Item -ItemType Directory -Force -Path $TransactionPath | Out-Null
    $records = @("version=1", "phase=initializing") + @($Phases | ForEach-Object {
        "phase=$_"
    })
    [System.IO.File]::WriteAllText(
        (Join-Path $TransactionPath "journal"),
        ($records -join [Environment]::NewLine) + [Environment]::NewLine,
        [System.Text.Encoding]::ASCII
    )
}

function Assert-RecoveryBeforeStagingFailure {
    param(
        [Parameter(Mandatory)][string]$InstallDirectory,
        [Parameter(Mandatory)][string]$ExpectedLabel,
        [Parameter(Mandatory)][string]$StaleTransaction,
        [Parameter(Mandatory)][string]$Installer,
        [Parameter(Mandatory)][string]$BaseUrl
    )

    $global:OsdkTestCopyFailureInjected = $false
    function global:Copy-Item {
        [CmdletBinding()]
        param(
            [Parameter(Mandatory)][string]$LiteralPath,
            [Parameter(Mandatory)][string]$Destination,
            [switch]$Force
        )

        $destinationParent = Split-Path -Parent $Destination
        $transactionName = Split-Path -Leaf (Split-Path -Parent $destinationParent)
        if (-not $global:OsdkTestCopyFailureInjected -and
            (Split-Path -Leaf $destinationParent) -eq "new" -and
            $transactionName.StartsWith(".osdk-install-v2.")) {
            $global:OsdkTestCopyFailureInjected = $true
            throw "injected staging failure after recovery"
        }
        Microsoft.PowerShell.Management\Copy-Item -LiteralPath $LiteralPath -Destination $Destination -Force:$Force
    }
    try {
        $arguments = @{
            Version = "9.8.7"
            Target = $script:target
            InstallDir = $InstallDirectory
            BaseUrl = $BaseUrl
            Repository = "example/one-sdk"
        }
        $operation = {
            & $Installer @arguments
        }.GetNewClosure()
        Invoke-ExpectedFailure -ExpectedMessage "injected staging failure after recovery" -Message "Recovery probe unexpectedly completed installation." -Operation $operation
    } finally {
        Remove-Item -LiteralPath Function:\global:Copy-Item
    }
    if (-not $global:OsdkTestCopyFailureInjected) {
        throw "PowerShell recovery probe did not reach staging."
    }
    Assert-BinarySet -Directory $InstallDirectory -Label $ExpectedLabel
    if (Test-Path -LiteralPath $StaleTransaction) {
        throw "Installer did not clean stale transaction $StaleTransaction"
    }
    Assert-NoTransactionDirectory -Directory $InstallDirectory
}

function New-ReleaseFixture {
    param(
        [Parameter(Mandatory)][string]$Version,
        [switch]$WithoutShim
    )

    $releaseDir = Join-Path $testRoot "http/example/one-sdk/releases/download/v$Version"
    $fixtureDir = Join-Path $testRoot "fixture-$Version"
    $names = if ($WithoutShim) { @("osdk.exe") } else { $binaries }
    Write-BinarySet -Directory $fixtureDir -Label fixture -Names $names
    New-Item -ItemType Directory -Force -Path $releaseDir | Out-Null
    $archivePath = Join-Path $releaseDir $archive
    $sourcePaths = @($names | ForEach-Object { Join-Path $fixtureDir $_ })
    Compress-Archive -LiteralPath $sourcePaths -DestinationPath $archivePath
    $digest = (Get-FileHash -Algorithm SHA256 -LiteralPath $archivePath).Hash.ToLowerInvariant()
    [System.IO.File]::WriteAllText(
        (Join-Path $releaseDir "SHA256SUMS"),
        "$digest  $archive`n",
        [System.Text.UTF8Encoding]::new($false)
    )
    return $releaseDir
}

function Invoke-ExpectedFailure {
    param(
        [Parameter(Mandatory)][scriptblock]$Operation,
        [Parameter(Mandatory)][string]$ExpectedMessage,
        [Parameter(Mandatory)][string]$Message
    )

    try {
        & $Operation
    } catch {
        if (-not $_.Exception.Message.Contains($ExpectedMessage)) {
            throw "Expected failure containing '$ExpectedMessage', got: $($_.Exception.Message)"
        }
        return
    }
    throw $Message
}

try {
    New-Item -ItemType Directory -Force -Path $testRoot | Out-Null
    $env:HOME = Join-Path $testRoot "home"
    $env:USERPROFILE = $env:HOME
    $env:XDG_CACHE_HOME = Join-Path $testRoot "xdg-cache"
    $env:XDG_CONFIG_HOME = Join-Path $testRoot "xdg-config"
    $env:XDG_DATA_HOME = Join-Path $testRoot "xdg-data"
    $env:OSDK_DATA_DIR = Join-Path $testRoot "osdk/data"
    $env:OSDK_CACHE_DIR = Join-Path $testRoot "osdk/cache"
    $env:OSDK_CONFIG_DIR = Join-Path $testRoot "osdk/config"
    $env:OSDK_STORE_DIR = Join-Path $testRoot "osdk/store"
    $env:OSDK_INSTALL_DIR = Join-Path $testRoot "osdk/installs"
    $env:OSDK_SKIP_VERIFY = "0"
    $env:CARGO_HOME = Join-Path $testRoot "cargo"
    $env:RUSTUP_HOME = Join-Path $testRoot "rustup"
    $env:CARGO_TARGET_DIR = Join-Path $testRoot "target"
    $env:TEMP = Join-Path $testRoot "tmp"
    $env:TMP = $env:TEMP
    foreach ($directory in @(
        $env:HOME,
        $env:XDG_CACHE_HOME,
        $env:XDG_CONFIG_HOME,
        $env:XDG_DATA_HOME,
        $env:OSDK_DATA_DIR,
        $env:OSDK_CACHE_DIR,
        $env:OSDK_CONFIG_DIR,
        $env:OSDK_STORE_DIR,
        $env:OSDK_INSTALL_DIR,
        $env:CARGO_HOME,
        $env:RUSTUP_HOME,
        $env:CARGO_TARGET_DIR,
        $env:TEMP
    )) {
        New-Item -ItemType Directory -Force -Path $directory | Out-Null
    }

    $releaseDir = New-ReleaseFixture -Version "9.8.7"
    $null = New-ReleaseFixture -Version "9.8.6" -WithoutShim
    $latestDir = Join-Path $testRoot "http/example/one-sdk/releases/latest/download"
    New-Item -ItemType Directory -Force -Path $latestDir | Out-Null
    Copy-Item -LiteralPath (Join-Path $releaseDir $archive) -Destination $latestDir
    Copy-Item -LiteralPath (Join-Path $releaseDir "SHA256SUMS") -Destination $latestDir

    $listener = [System.Net.Sockets.TcpListener]::new(
        [System.Net.IPAddress]::Loopback,
        0
    )
    $listener.Start()
    $port = ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port
    $listener.Stop()
    $httpRoot = Join-Path $testRoot "http"
    $server = Start-Process `
        -FilePath (Get-Command python).Source `
        -ArgumentList @("-m", "http.server", $port, "--bind", "127.0.0.1", "--directory", $httpRoot) `
        -RedirectStandardOutput (Join-Path $testRoot "http.stdout.log") `
        -RedirectStandardError (Join-Path $testRoot "http.stderr.log") `
        -PassThru
    $baseUrl = "http://127.0.0.1:$port"
    $ready = $false
    for ($attempt = 0; $attempt -lt 50; $attempt++) {
        try {
            $null = Invoke-WebRequest -UseBasicParsing -Uri $baseUrl
            $ready = $true
            break
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $ready) {
        throw "Fixture HTTP server did not start."
    }

    $installer = Join-Path $repoRoot "install.ps1"
    $installDir = Join-Path $testRoot "custom bin"
    Write-BinarySet -Directory $installDir -Label old
    & $installer `
        -Version "9.8.7" `
        -Target $target `
        -InstallDir $installDir `
        -BaseUrl $baseUrl `
        -Repository "example/one-sdk"
    Assert-BinarySet -Directory $installDir -Label fixture
    Assert-NoTransactionDirectory -Directory $installDir

    # Stop the next invocation at its first stage copy. This proves stale
    # recovery itself preserved/restored the old set before a reinstall can
    # conceal a destructive recovery decision.
    $initializingDir = Join-Path $testRoot "initializing-recovery-bin"
    Write-BinarySet -Directory $initializingDir -Label old
    $initializingTransaction = Join-Path $initializingDir ".osdk-install-v2.initializing"
    New-Item -ItemType Directory -Path $initializingTransaction | Out-Null
    Assert-RecoveryBeforeStagingFailure -InstallDirectory $initializingDir -ExpectedLabel old -StaleTransaction $initializingTransaction -Installer $installer -BaseUrl $baseUrl

    $stagingDir = Join-Path $testRoot "staging-recovery-bin"
    Write-BinarySet -Directory $stagingDir -Label old
    $stagingTransaction = Join-Path $stagingDir ".osdk-install-v2.staging"
    Write-TransactionJournal -TransactionPath $stagingTransaction -Phases @("staging")
    $stagingNew = Join-Path $stagingTransaction "new"
    New-Item -ItemType Directory -Path $stagingNew, (Join-Path $stagingTransaction "old") | Out-Null
    Copy-Item -LiteralPath (Join-Path $testRoot "fixture-9.8.7/osdk.exe") -Destination (Join-Path $stagingNew "osdk.exe")
    Assert-RecoveryBeforeStagingFailure -InstallDirectory $stagingDir -ExpectedLabel old -StaleTransaction $stagingTransaction -Installer $installer -BaseUrl $baseUrl

    $preparedDir = Join-Path $testRoot "prepared-recovery-bin"
    Write-BinarySet -Directory $preparedDir -Label old
    $preparedTransaction = Join-Path $preparedDir ".osdk-install-v2.prepared"
    Write-TransactionJournal -TransactionPath $preparedTransaction -Phases @("staging", "prepared")
    $preparedNew = Join-Path $preparedTransaction "new"
    New-Item -ItemType Directory -Path $preparedNew, (Join-Path $preparedTransaction "old") | Out-Null
    foreach ($binary in $binaries) {
        Copy-Item -LiteralPath (Join-Path $testRoot "fixture-9.8.7/$binary") -Destination (Join-Path $preparedNew $binary)
    }
    Assert-RecoveryBeforeStagingFailure -InstallDirectory $preparedDir -ExpectedLabel old -StaleTransaction $preparedTransaction -Installer $installer -BaseUrl $baseUrl

    $promotingDir = Join-Path $testRoot "promoting-recovery-bin"
    Write-BinarySet -Directory $promotingDir -Label old
    $promotingTransaction = Join-Path $promotingDir ".osdk-install-v2.promoting"
    Write-TransactionJournal -TransactionPath $promotingTransaction -Phases @("staging", "prepared", "promoting")
    $promotingNew = Join-Path $promotingTransaction "new"
    $promotingOld = Join-Path $promotingTransaction "old"
    New-Item -ItemType Directory -Path $promotingNew, $promotingOld | Out-Null
    foreach ($binary in $binaries) {
        Move-Item -LiteralPath (Join-Path $promotingDir $binary) -Destination (Join-Path $promotingOld $binary)
        Copy-Item -LiteralPath (Join-Path $testRoot "fixture-9.8.7/$binary") -Destination (Join-Path $promotingNew $binary)
    }
    Move-Item -LiteralPath (Join-Path $promotingNew "osdk.exe") -Destination (Join-Path $promotingDir "osdk.exe")
    Assert-RecoveryBeforeStagingFailure -InstallDirectory $promotingDir -ExpectedLabel old -StaleTransaction $promotingTransaction -Installer $installer -BaseUrl $baseUrl

    $crashRecoveryDir = Join-Path $testRoot "crash-recovery-bin"
    Write-BinarySet -Directory $crashRecoveryDir -Label old
    $staleTransaction = Join-Path $crashRecoveryDir ".osdk-install-crashed"
    $staleNew = Join-Path $staleTransaction "new"
    $staleOld = Join-Path $staleTransaction "old"
    New-Item -ItemType Directory -Force -Path $staleNew, $staleOld | Out-Null
    foreach ($binary in $binaries) {
        Move-Item `
            -LiteralPath (Join-Path $crashRecoveryDir $binary) `
            -Destination (Join-Path $staleOld $binary)
        Copy-Item `
            -LiteralPath (Join-Path $testRoot "fixture-9.8.7/$binary") `
            -Destination (Join-Path $staleNew $binary)
    }
    Move-Item `
        -LiteralPath (Join-Path $staleNew "osdk.exe") `
        -Destination (Join-Path $crashRecoveryDir "osdk.exe")
    & $installer `
        -Version "9.8.7" `
        -Target $target `
        -InstallDir $crashRecoveryDir `
        -BaseUrl $baseUrl `
        -Repository "example/one-sdk"
    Assert-BinarySet -Directory $crashRecoveryDir -Label fixture
    Assert-NoTransactionDirectory -Directory $crashRecoveryDir

    $latestInstallDir = Join-Path $testRoot "latest-bin"
    $env:OSDK_VERSION = "latest"
    $env:OSDK_TARGET = $target
    $env:OSDK_BIN_DIR = $latestInstallDir
    $env:OSDK_DOWNLOAD_BASE_URL = $baseUrl
    $env:OSDK_REPOSITORY = "example/one-sdk"
    & $installer
    Assert-BinarySet -Directory $latestInstallDir -Label fixture
    Assert-NoTransactionDirectory -Directory $latestInstallDir

    $incompleteDir = Join-Path $testRoot "incomplete-bin"
    Write-BinarySet -Directory $incompleteDir -Label old
    Invoke-ExpectedFailure `
        -ExpectedMessage "does not contain osdk-shim.exe" `
        -Message "Installer accepted an archive without osdk-shim.exe." `
        -Operation {
            & $installer `
                -Version "9.8.6" `
                -Target $target `
                -InstallDir $incompleteDir `
                -BaseUrl $baseUrl `
                -Repository "example/one-sdk"
        }
    Assert-BinarySet -Directory $incompleteDir -Label old
    Assert-NoTransactionDirectory -Directory $incompleteDir

    $lockedDir = Join-Path $testRoot "locked-bin"
    Write-BinarySet -Directory $lockedDir -Label old
    $lockPath = Join-Path $lockedDir ".osdk-install.lock"
    $heldLock = [System.IO.File]::Open(
        $lockPath,
        [System.IO.FileMode]::OpenOrCreate,
        [System.IO.FileAccess]::ReadWrite,
        [System.IO.FileShare]::None
    )
    try {
        Invoke-ExpectedFailure `
            -ExpectedMessage "Another installer is updating $lockedDir" `
            -Message "Installer ignored an active installation lock." `
            -Operation {
                & $installer `
                    -Version "9.8.7" `
                    -Target $target `
                    -InstallDir $lockedDir `
                    -BaseUrl $baseUrl `
                    -Repository "example/one-sdk"
            }
    } finally {
        $heldLock.Dispose()
    }
    Assert-BinarySet -Directory $lockedDir -Label old
    Assert-NoTransactionDirectory -Directory $lockedDir

    $rollbackDir = Join-Path $testRoot "rollback-bin"
    Write-BinarySet -Directory $rollbackDir -Label old
    $global:OsdkTestPromotionFailureInjected = $false
    function global:Move-Item {
        [CmdletBinding()]
        param(
            [Parameter(Mandatory)][string]$LiteralPath,
            [Parameter(Mandatory)][string]$Destination
        )

        $sourceParent = Split-Path -Leaf (Split-Path -Parent $LiteralPath)
        if (-not $global:OsdkTestPromotionFailureInjected -and
            $sourceParent -eq "new" -and
            (Split-Path -Leaf $LiteralPath) -eq "osdk-shim.exe" -and
            (Split-Path -Leaf $Destination) -eq "osdk-shim.exe") {
            $global:OsdkTestPromotionFailureInjected = $true
            throw "injected osdk-shim promotion failure"
        }
        Microsoft.PowerShell.Management\Move-Item `
            -LiteralPath $LiteralPath `
            -Destination $Destination
    }
    try {
        Invoke-ExpectedFailure `
            -ExpectedMessage "injected osdk-shim promotion failure" `
            -Message "Installer ignored an injected promotion failure." `
            -Operation {
                & $installer `
                    -Version "9.8.7" `
                    -Target $target `
                    -InstallDir $rollbackDir `
                    -BaseUrl $baseUrl `
                    -Repository "example/one-sdk"
            }
    } finally {
        Remove-Item -LiteralPath Function:\global:Move-Item
    }
    if (-not $global:OsdkTestPromotionFailureInjected) {
        throw "PowerShell promotion failure hook was not reached."
    }
    Assert-BinarySet -Directory $rollbackDir -Label old
    Assert-NoTransactionDirectory -Directory $rollbackDir

    [System.IO.File]::WriteAllText(
        (Join-Path $releaseDir "SHA256SUMS"),
        "$('0' * 64)  $archive`n",
        [System.Text.UTF8Encoding]::new($false)
    )
    $invalidChecksumDir = Join-Path $testRoot "invalid-checksum"
    Invoke-ExpectedFailure `
        -ExpectedMessage "Checksum verification failed for $archive" `
        -Message "Installer accepted an invalid checksum." `
        -Operation {
            & $installer `
                -Version "9.8.7" `
                -Target $target `
                -InstallDir $invalidChecksumDir `
                -BaseUrl $baseUrl `
                -Repository "example/one-sdk"
        }
    foreach ($binary in $binaries) {
        if (Test-Path -LiteralPath (Join-Path $invalidChecksumDir $binary)) {
            throw "Checksum failure installed $binary."
        }
    }

    Write-Host "PowerShell installer smoke tests passed."
} finally {
    if ($server -and -not $server.HasExited) {
        Stop-Process -Id $server.Id -Force
        $server.WaitForExit()
    }
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -Recurse -Force -LiteralPath $testRoot
    }
}
