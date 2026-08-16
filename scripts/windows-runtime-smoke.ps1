param(
    [string]$BinDir = (Join-Path $PSScriptRoot "..\target\debug")
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
if (Test-Path Variable:PSNativeCommandUseErrorActionPreference) {
    $PSNativeCommandUseErrorActionPreference = $false
}

function Assert-True {
    param(
        [bool]$Condition,
        [string]$Message
    )
    if (-not $Condition) {
        throw $Message
    }
}

function Assert-ContractOutput {
    param(
        [string]$Shell,
        [int]$ExitCode,
        [string]$StdoutPath,
        [string]$StderrPath
    )
    $stdout = (Get-Content -LiteralPath $StdoutPath -Raw).Trim()
    $stderr = (Get-Content -LiteralPath $StderrPath -Raw).Trim()
    Assert-True ($ExitCode -eq 23) "$Shell wrapper returned $ExitCode instead of 23"
    Assert-True ($stdout -eq "out:first arg:input") "$Shell stdout mismatch: $stdout"
    Assert-True ($stderr -eq "err:second arg") "$Shell stderr mismatch: $stderr"
}

function Invoke-Stage {
    param(
        [string]$Name,
        [scriptblock]$Action
    )
    $started = Get-Date
    Write-Host "::group::$Name"
    try {
        & $Action
        $elapsed = (Get-Date) - $started
        Write-Host "$Name passed in $([Math]::Round($elapsed.TotalSeconds, 2))s"
    }
    finally {
        Write-Host "::endgroup::"
    }
}

$bin = (Resolve-Path -LiteralPath $BinDir).Path
$sourceOsdk = Join-Path $bin "osdk.exe"
$sourceShim = Join-Path $bin "osdk-shim.exe"
Assert-True (Test-Path -LiteralPath $sourceOsdk -PathType Leaf) "missing $sourceOsdk"
Assert-True (Test-Path -LiteralPath $sourceShim -PathType Leaf) "missing $sourceShim"

$root = Join-Path ([IO.Path]::GetTempPath()) "one sdk 中文 runtime"
$longSegments = 1..8 | ForEach-Object { "segment-$($_)-abcdefghijklmnopqrstuvwxyz" }
$longStateRoot = $root
foreach ($segment in $longSegments) {
    $longStateRoot = Join-Path $longStateRoot $segment
}
Assert-True ($longStateRoot.Length -gt 260) "runtime state path does not exceed legacy MAX_PATH"

try {
    $programDir = Join-Path $root "program files 中文"
    $project = Join-Path $root "project with spaces 中文"
    $install = Join-Path $longStateRoot "installed SDKs"
    $data = Join-Path $root "data"
    $runtime = Join-Path $install "node\1.0.0"
    $stateDirectories = @{
        HOME = Join-Path $longStateRoot "home"
        USERPROFILE = Join-Path $longStateRoot "home"
        OSDK_DATA_DIR = $data
        OSDK_CACHE_DIR = Join-Path $longStateRoot "cache"
        OSDK_CONFIG_DIR = Join-Path $longStateRoot "config"
        OSDK_STORE_DIR = Join-Path $longStateRoot "store"
        OSDK_INSTALL_DIR = $install
        CARGO_HOME = Join-Path $longStateRoot "cargo home"
        RUSTUP_HOME = Join-Path $longStateRoot "rustup home"
        CARGO_TARGET_DIR = Join-Path $longStateRoot "build output"
        TEMP = Join-Path $longStateRoot "temp"
        TMP = Join-Path $longStateRoot "temp"
    }
    foreach ($entry in $stateDirectories.GetEnumerator()) {
        [Environment]::SetEnvironmentVariable($entry.Key, $entry.Value, "Process")
        New-Item -ItemType Directory -Force -Path $entry.Value | Out-Null
    }
    $env:PATH = [Environment]::GetEnvironmentVariable("PATH", "Machine")
    $env:OSDK_OFFLINE = "true"
    Remove-Item Env:GITHUB_TOKEN -ErrorAction SilentlyContinue

    Invoke-Stage "prepare isolated fixtures" {
        New-Item -ItemType Directory -Force -Path $programDir, $project, $runtime | Out-Null
        Copy-Item -LiteralPath $sourceOsdk -Destination (Join-Path $programDir "osdk.exe")
        Copy-Item -LiteralPath $sourceShim -Destination (Join-Path $programDir "osdk-shim.exe")
        Set-Content -LiteralPath (Join-Path $project "osdk.toml") -Encoding utf8 -Value @"
[tools]
node = "1.0.0"
"@
        Set-Content -LiteralPath (Join-Path $runtime "node.cmd") -Encoding ascii -Value @"
@echo off
set /p line=
echo out:%~1:%line%
echo err:%~2 1>&2
exit /b 23
"@
        Set-Content -LiteralPath (Join-Path $runtime ".osdk-complete") -Encoding ascii -Value ""
    }
    $osdk = Join-Path $programDir "osdk.exe"

    Push-Location $project
    try {
        Invoke-Stage "generate Windows shims" {
            & $osdk reshim | Out-Null
            Assert-True ($LASTEXITCODE -eq 0) "osdk reshim failed"
        }

        $shimCmd = Join-Path $data "shims\node.cmd"
        $shimBash = Join-Path $data "shims\node"
        Assert-True (Test-Path -LiteralPath $shimCmd -PathType Leaf) "missing cmd/PowerShell shim"
        Assert-True (Test-Path -LiteralPath $shimBash -PathType Leaf) "missing Git Bash shim"
        $inputPath = Join-Path $root "input.txt"
        Set-Content -LiteralPath $inputPath -Encoding ascii -NoNewline -Value "input"

        Invoke-Stage "PowerShell shim contract" {
            $stdout = Join-Path $root "powershell.stdout"
            $stderr = Join-Path $root "powershell.stderr"
            Get-Content -LiteralPath $inputPath -Raw |
                & $shimCmd "first arg" "second arg" 1> $stdout 2> $stderr
            $exitCode = $LASTEXITCODE
            Assert-ContractOutput "PowerShell" $exitCode $stdout $stderr
        }

        Invoke-Stage "cmd.exe shim contract" {
            $stdout = Join-Path $root "cmd.stdout"
            $stderr = Join-Path $root "cmd.stderr"
            $cmdLine = '""{0}" "first arg" "second arg" < "{1}" > "{2}" 2> "{3}""' -f `
                $shimCmd, $inputPath, $stdout, $stderr
            & $env:ComSpec /D /S /C $cmdLine
            $exitCode = $LASTEXITCODE
            Assert-ContractOutput "cmd.exe" $exitCode $stdout $stderr
        }

        Invoke-Stage "Git Bash shim contract" {
            $bashCommandInfo = Get-Command bash.exe -ErrorAction SilentlyContinue
            $bashPath = if ($bashCommandInfo) {
                $bashCommandInfo.Source
            } else {
                Join-Path $env:ProgramFiles "Git\bin\bash.exe"
            }
            Assert-True (Test-Path -LiteralPath $bashPath -PathType Leaf) "Git Bash is unavailable"
            $stdout = Join-Path $root "git-bash.stdout"
            $stderr = Join-Path $root "git-bash.stderr"
            $toBashPath = {
                param([string]$Path)
                $Path.Replace("\", "/")
            }
            $bashCommand = "'$(& $toBashPath $shimBash)' 'first arg' 'second arg' < " +
                "'$(& $toBashPath $inputPath)' > '$(& $toBashPath $stdout)' 2> '$(& $toBashPath $stderr)'"
            & $bashPath --noprofile --norc -c $bashCommand
            $exitCode = $LASTEXITCODE
            Assert-ContractOutput "Git Bash" $exitCode $stdout $stderr
        }

        $originalPath = $env:PATH
        Invoke-Stage "PowerShell activation and deactivation" {
            $activation = (& $osdk activate powershell | Out-String)
            Assert-True ($LASTEXITCODE -eq 0) "PowerShell activation rendering failed"
            Invoke-Expression $activation
            Assert-True ($env:PATH.StartsWith($runtime, [StringComparison]::OrdinalIgnoreCase)) `
                "PowerShell activation did not prepend the managed runtime"
            Assert-True ($null -ne $ExecutionContext.SessionState.InvokeCommand.PostCommandLookupAction) `
                "PowerShell activation hook was not installed"
            $deactivation = (& $osdk deactivate powershell | Out-String)
            Assert-True ($LASTEXITCODE -eq 0) "PowerShell deactivation rendering failed"
            Invoke-Expression $deactivation
            Assert-True ($env:PATH -eq $originalPath) "PowerShell deactivation did not restore PATH"
            Assert-True ($null -eq $ExecutionContext.SessionState.InvokeCommand.PostCommandLookupAction) `
                "PowerShell deactivation hook was not removed"
            Assert-True (-not (Test-Path Function:Invoke-OsdkHook)) `
                "PowerShell deactivation function was not removed"
        }
    }
    finally {
        Pop-Location
    }

    Write-Host "Windows runtime smoke passed with SDK state under $longStateRoot"
}
finally {
    if ($ExecutionContext.SessionState.InvokeCommand.PostCommandLookupAction) {
        $ExecutionContext.SessionState.InvokeCommand.PostCommandLookupAction = $null
    }
    Remove-Item Function:Invoke-OsdkHook -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}
