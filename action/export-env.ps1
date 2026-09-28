[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$osdk = if ($env:OSDK_ACTION_OSDK) {
    $env:OSDK_ACTION_OSDK
} else {
    Join-Path $env:OSDK_BIN_DIR "osdk.exe"
}
$pathBefore = $env:PATH
$snippet = (& $osdk hook-env --shell powershell | Out-String)
if (-not $?) {
    throw "osdk hook-env failed"
}
Invoke-Expression $snippet

$originalPath = if ($env:OSDK_ORIGINAL_PATH) { $env:OSDK_ORIGINAL_PATH } else { $pathBefore }
if ($env:PATH -ne $originalPath) {
    $suffix = "$([IO.Path]::PathSeparator)$originalPath"
    if (-not $env:PATH.EndsWith($suffix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "osdk activation did not preserve the original PATH suffix"
    }
    $managedPath = $env:PATH.Substring(0, $env:PATH.Length - $suffix.Length)
    foreach ($directory in $managedPath.Split([IO.Path]::PathSeparator, [StringSplitOptions]::RemoveEmptyEntries)) {
        Add-Content -LiteralPath $env:GITHUB_PATH -Value $directory -Encoding utf8
    }
}

function Write-GitHubEnvironment {
    param([Parameter(Mandatory)][string]$Name)
    $value = [Environment]::GetEnvironmentVariable($Name)
    $delimiter = "OSDK_$([guid]::NewGuid().ToString('N'))"
    Add-Content -LiteralPath $env:GITHUB_ENV -Value "$Name<<$delimiter" -Encoding utf8
    Add-Content -LiteralPath $env:GITHUB_ENV -Value $value -Encoding utf8
    Add-Content -LiteralPath $env:GITHUB_ENV -Value $delimiter -Encoding utf8
}

$managedNames = if ($env:OSDK_MANAGED_ENV) { $env:OSDK_MANAGED_ENV.Split(',') } else { @() }
foreach ($name in $managedNames) {
    if ($name) {
        Write-GitHubEnvironment $name
    }
}
Write-GitHubEnvironment "OSDK_MANAGED_ENV"
