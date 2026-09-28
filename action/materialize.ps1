[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Get-BooleanInput {
    param(
        [Parameter(Mandatory)][string]$Name,
        [AllowEmptyString()][string]$Value,
        [Parameter(Mandatory)][bool]$Default
    )
    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $Default
    }
    switch ($Value.ToLowerInvariant()) {
        "true" { return $true }
        "false" { return $false }
        default { throw "$Name must be true or false, got $Value" }
    }
}

$osdk = if ($env:OSDK_ACTION_OSDK) {
    $env:OSDK_ACTION_OSDK
} else {
    Join-Path $env:OSDK_BIN_DIR "osdk.exe"
}
$globalArgs = [System.Collections.Generic.List[string]]::new()
$globalArgs.Add("--yes")
if ($env:OSDK_ACTION_JOBS) {
    $globalArgs.Add("--jobs")
    $globalArgs.Add($env:OSDK_ACTION_JOBS)
}
if ($env:OSDK_ACTION_SOURCE_MODE) {
    $globalArgs.Add("--source-mode")
    $globalArgs.Add($env:OSDK_ACTION_SOURCE_MODE)
}
if (Get-BooleanInput "offline" $env:OSDK_ACTION_OFFLINE $false) {
    $globalArgs.Add("--offline")
}
if (Get-BooleanInput "require-checksums" $env:OSDK_ACTION_REQUIRE_CHECKSUMS $false) {
    $globalArgs.Add("--require-checksums")
}
if ($env:OSDK_ACTION_ATTESTATIONS) {
    $globalArgs.Add("--attestations")
    $globalArgs.Add($env:OSDK_ACTION_ATTESTATIONS)
}

if (Get-BooleanInput "install-tools" $env:OSDK_ACTION_INSTALL_TOOLS $true) {
    & $osdk @globalArgs install --no-deps
    if (-not $?) {
        throw "osdk install failed"
    }
}

if (Get-BooleanInput "install-deps" $env:OSDK_ACTION_INSTALL_DEPS $true) {
    $depsArgs = [System.Collections.Generic.List[string]]::new()
    $depsArgs.Add("deps")
    if (Get-BooleanInput "frozen" $env:OSDK_ACTION_FROZEN $true) {
        $depsArgs.Add("--frozen")
    }
    if (-not (Get-BooleanInput "allow-deps-tool-install" $env:OSDK_ACTION_ALLOW_DEPS_TOOL_INSTALL $false)) {
        $depsArgs.Add("--no-install-tools")
    }
    & $osdk @globalArgs @depsArgs
    if (-not $?) {
        throw "osdk deps failed"
    }
}
