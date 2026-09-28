[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent $PSScriptRoot
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ("osdk-action-test-" + [guid]::NewGuid())
try {
    $env:RUNNER_TEMP = Join-Path $testRoot "runner"
    $env:GITHUB_ACTION_PATH = $repoRoot
    $env:GITHUB_ENV = Join-Path $testRoot "github-env"
    $env:GITHUB_PATH = Join-Path $testRoot "github-path"
    $env:GITHUB_OUTPUT = Join-Path $testRoot "github-output"
    $env:OSDK_ACTION_TEST_LOG = Join-Path $testRoot "osdk.log"
    $env:OSDK_ACTION_TEST_INSTALLER_LOG = Join-Path $testRoot "installer.log"
    $env:OSDK_ACTION_TEST_ROOT = $testRoot
    New-Item -ItemType Directory -Force -Path $env:RUNNER_TEMP | Out-Null
    foreach ($file in @(
        $env:GITHUB_ENV,
        $env:GITHUB_PATH,
        $env:GITHUB_OUTPUT,
        $env:OSDK_ACTION_TEST_LOG
    )) {
        [IO.File]::WriteAllText($file, "", [Text.UTF8Encoding]::new($false))
    }

    $fakeInstaller = Join-Path $testRoot "fake-installer.ps1"
    [IO.File]::WriteAllText($fakeInstaller, @'
param(
    [string]$Version,
    [string]$InstallDir,
    [string]$Repository,
    [string]$BaseUrl,
    [switch]$NoModifyShell,
    [string]$Target
)
"version=$Version repository=$Repository base=$BaseUrl target=$Target no-shell=$NoModifyShell" |
    Set-Content -LiteralPath $env:OSDK_ACTION_TEST_INSTALLER_LOG -Encoding utf8
'@, [Text.UTF8Encoding]::new($false))

    $fakeOsdk = Join-Path $testRoot "fake-osdk.ps1"
    [IO.File]::WriteAllText($fakeOsdk, @'
$arguments = @($args)
if ($arguments.Count -eq 1 -and $arguments[0] -eq "--version") {
    Write-Output "osdk 9.8.7"
    exit 0
}
if ($arguments.Count -eq 3 -and
    $arguments[0] -eq "hook-env" -and
    $arguments[1] -eq "--shell" -and
    $arguments[2] -eq "powershell") {
    $managedOne = (Join-Path $env:OSDK_ACTION_TEST_ROOT "managed-one").Replace("'", "''")
    $managedTwo = (Join-Path $env:OSDK_ACTION_TEST_ROOT "managed-two").Replace("'", "''")
    $cargoHome = (Join-Path $env:OSDK_ACTION_TEST_ROOT "cache\cargo").Replace("'", "''")
    Write-Output "`$env:PATH = '$managedOne' + [IO.Path]::PathSeparator + '$managedTwo' + [IO.Path]::PathSeparator + `$env:PATH"
    Write-Output "`$env:CARGO_HOME = '$cargoHome'"
    Write-Output "`$env:OSDK_MANAGED_ENV = 'CARGO_HOME'"
    exit 0
}
Add-Content -LiteralPath $env:OSDK_ACTION_TEST_LOG -Value ($arguments -join " ") -Encoding utf8
'@, [Text.UTF8Encoding]::new($false))

    $env:OSDK_ACTION_INSTALLER = $fakeInstaller
    $env:OSDK_ACTION_OSDK = $fakeOsdk
    $env:OSDK_ACTION_VERSION = ""
    $env:OSDK_ACTION_REF = "v9.8.7"
    $env:OSDK_ACTION_REPOSITORY = "example/osdk"
    $env:OSDK_ACTION_DOWNLOAD_BASE_URL = "https://downloads.example.test"
    $env:OSDK_ACTION_TARGET = "x86_64-pc-windows-msvc"
    & (Join-Path $repoRoot "action/setup.ps1")

    if (-not (Select-String -Quiet -SimpleMatch "version=9.8.7" $env:GITHUB_OUTPUT)) {
        throw "setup did not publish the installed osdk version"
    }
    $installerLog = Get-Content -Raw -LiteralPath $env:OSDK_ACTION_TEST_INSTALLER_LOG
    foreach ($expected in @(
        "version=v9.8.7",
        "repository=example/osdk",
        "target=x86_64-pc-windows-msvc",
        "no-shell=True"
    )) {
        if (-not $installerLog.Contains($expected)) {
            throw "installer arguments missing ${expected}: $installerLog"
        }
    }

    $env:OSDK_ACTION_INSTALL_TOOLS = "true"
    $env:OSDK_ACTION_INSTALL_DEPS = "true"
    $env:OSDK_ACTION_FROZEN = "true"
    $env:OSDK_ACTION_ALLOW_DEPS_TOOL_INSTALL = "false"
    $env:OSDK_ACTION_JOBS = "3"
    $env:OSDK_ACTION_SOURCE_MODE = "auto"
    $env:OSDK_ACTION_OFFLINE = "false"
    $env:OSDK_ACTION_REQUIRE_CHECKSUMS = "true"
    $env:OSDK_ACTION_ATTESTATIONS = "required"
    & (Join-Path $repoRoot "action/materialize.ps1")

    $osdkLog = @(Get-Content -LiteralPath $env:OSDK_ACTION_TEST_LOG)
    $expectedCalls = @(
        "--yes --jobs 3 --source-mode auto --require-checksums --attestations required install --no-deps",
        "--yes --jobs 3 --source-mode auto --require-checksums --attestations required deps --frozen --no-install-tools"
    )
    foreach ($expected in $expectedCalls) {
        if ($osdkLog -notcontains $expected) {
            throw "missing osdk call '${expected}': $($osdkLog -join '; ')"
        }
    }

    & (Join-Path $repoRoot "action/export-env.ps1")
    $githubPath = @(Get-Content -LiteralPath $env:GITHUB_PATH)
    foreach ($name in @("managed-one", "managed-two")) {
        $expected = Join-Path $testRoot $name
        if ($githubPath -notcontains $expected) {
            throw "managed PATH does not contain ${expected}: $($githubPath -join '; ')"
        }
    }
    $githubEnv = Get-Content -Raw -LiteralPath $env:GITHUB_ENV
    if (-not $githubEnv.Contains("CARGO_HOME<<OSDK_")) {
        throw "CARGO_HOME was not exported to GITHUB_ENV"
    }
    if (-not $githubEnv.Contains((Join-Path $testRoot "cache\cargo"))) {
        throw "CARGO_HOME value was not exported"
    }

    Write-Host "GitHub Action PowerShell helper smoke passed"
} finally {
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
