[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

foreach ($name in @("RUNNER_TEMP", "GITHUB_ENV", "GITHUB_PATH", "GITHUB_OUTPUT", "GITHUB_ACTION_PATH")) {
    if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) {
        throw "$name is required"
    }
}

foreach ($name in @(
    "OSDK_ACTION_CACHE",
    "OSDK_ACTION_INSTALL_TOOLS",
    "OSDK_ACTION_INSTALL_DEPS",
    "OSDK_ACTION_FROZEN",
    "OSDK_ACTION_ALLOW_DEPS_TOOL_INSTALL",
    "OSDK_ACTION_OFFLINE",
    "OSDK_ACTION_REQUIRE_CHECKSUMS"
)) {
    $value = [Environment]::GetEnvironmentVariable($name)
    if ($value -and $value -notin @("true", "false")) {
        throw "$name must be true or false, got $value"
    }
}
if ($env:OSDK_ACTION_SOURCE_MODE -and $env:OSDK_ACTION_SOURCE_MODE -notin @("auto", "env")) {
    throw "source-mode must be auto or env, got $env:OSDK_ACTION_SOURCE_MODE"
}
if ($env:OSDK_ACTION_ATTESTATIONS -and $env:OSDK_ACTION_ATTESTATIONS -notin @("off", "if-available", "required")) {
    throw "attestations must be off, if-available, or required, got $env:OSDK_ACTION_ATTESTATIONS"
}

$stateRoot = Join-Path $env:RUNNER_TEMP "osdk"
$binDir = Join-Path $stateRoot "bin"
$dataDir = Join-Path $stateRoot "data"
$cacheDir = Join-Path $stateRoot "cache"
$configDir = Join-Path $stateRoot "config"
New-Item -ItemType Directory -Force -Path $binDir, $dataDir, $cacheDir, $configDir | Out-Null

$directories = [ordered]@{
    OSDK_BIN_DIR = $binDir
    OSDK_DATA_DIR = $dataDir
    OSDK_CACHE_DIR = $cacheDir
    OSDK_CONFIG_DIR = $configDir
}
foreach ($entry in $directories.GetEnumerator()) {
    [Environment]::SetEnvironmentVariable($entry.Key, $entry.Value, "Process")
    Add-Content -LiteralPath $env:GITHUB_ENV -Value "$($entry.Key)=$($entry.Value)" -Encoding utf8
}
$env:PATH = "$binDir$([IO.Path]::PathSeparator)$env:PATH"
Add-Content -LiteralPath $env:GITHUB_PATH -Value $binDir -Encoding utf8

$requested = $env:OSDK_ACTION_VERSION
if ([string]::IsNullOrWhiteSpace($requested)) {
    $actionRef = $env:OSDK_ACTION_REF -replace '^refs/tags/', ''
    $requested = if ($actionRef -match '^v\d+\.\d+\.\d+(?:[-+].+)?$') {
        $actionRef
    } else {
        "latest"
    }
}

$installer = if ($env:OSDK_ACTION_INSTALLER) {
    $env:OSDK_ACTION_INSTALLER
} else {
    Join-Path $env:GITHUB_ACTION_PATH "install.ps1"
}
$installArgs = @{
    Version = $requested
    InstallDir = $binDir
    Repository = $(if ($env:OSDK_ACTION_REPOSITORY) { $env:OSDK_ACTION_REPOSITORY } else { "lejunyang/one-sdk" })
    BaseUrl = $(if ($env:OSDK_ACTION_DOWNLOAD_BASE_URL) { $env:OSDK_ACTION_DOWNLOAD_BASE_URL } else { "https://github.com" })
    NoModifyShell = $true
}
if ($env:OSDK_ACTION_TARGET) {
    $installArgs.Target = $env:OSDK_ACTION_TARGET
}
& $installer @installArgs
if (-not $?) {
    throw "osdk installer failed"
}

$osdk = if ($env:OSDK_ACTION_OSDK) { $env:OSDK_ACTION_OSDK } else { Join-Path $binDir "osdk.exe" }
$versionOutput = (& $osdk --version | Out-String).Trim()
if (-not $?) {
    throw "osdk --version failed"
}
if ($versionOutput -notmatch '^osdk\s+(.+)$') {
    throw "Unexpected osdk --version output: $versionOutput"
}
Add-Content -LiteralPath $env:GITHUB_OUTPUT -Value "version=$($Matches[1])" -Encoding utf8
