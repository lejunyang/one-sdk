param(
    [Parameter(Mandatory)][string]$Osdk
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'PowerShell 7 required' }
$osdkPath = (Resolve-Path -LiteralPath $Osdk).Path
$pwsh = (Get-Process -Id $PID).Path
$root = Join-Path ([IO.Path]::GetTempPath()) ('osdk-go-proxy-' + [Guid]::NewGuid().ToString('N'))
$original = @{}
$names = @('HOME', 'USERPROFILE', 'GOENV', 'GOPROXY', 'GOPRIVATE', 'GONOPROXY', 'GOINSECURE',
    'OSDK_TASK', 'OSDK_TASK_NAME', 'OSDK_TASK_ROOT', 'OSDK_OFFLINE', 'OSDK_MANAGED_ENV',
    'OSDK_APPLIED_GOPROXY', 'OSDK_TRUSTED_CONFIG_PATHS', 'OSDK_DATA_DIR', 'OSDK_CONFIG_DIR',
    'OSDK_CACHE_DIR', 'OSDK_STORE_DIR', 'OSDK_INSTALL_DIR', 'OSDK_ORIGINAL_PATH',
    'OSDK_ORIGINAL_PATH_SET', 'OSDK_ADDED_PATH')
$names += @(Get-ChildItem Env: | Where-Object Name -Like 'OSDK_ORIG_*' | ForEach-Object Name)
foreach ($name in ($names | Select-Object -Unique)) {
    $original[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    [Environment]::SetEnvironmentVariable($name, $null, 'Process')
}
try {
    foreach ($kind in @('data', 'config', 'cache', 'store', 'install')) {
        $path = Join-Path $root $kind
        New-Item -ItemType Directory -Path $path -Force | Out-Null
        [Environment]::SetEnvironmentVariable("OSDK_$($kind.ToUpper())_DIR", $path, 'Process')
    }
    $env:HOME = Join-Path $root 'home'
    $env:USERPROFILE = $env:HOME
    New-Item -ItemType Directory -Path $env:HOME -Force | Out-Null
    $env:GOENV = 'off'
    $env:OSDK_OFFLINE = 'true'
    $env:OSDK_TRUSTED_CONFIG_PATHS = $root
    $project = Join-Path $root 'project with spaces 中文'
    $runtime = Join-Path $env:OSDK_INSTALL_DIR 'go/1.24.0'
    foreach ($relative in @('bin', 'pkg/tool', 'src/runtime')) {
        New-Item -ItemType Directory -Path (Join-Path $runtime $relative) -Force | Out-Null
    }
    New-Item -ItemType Directory -Path $project -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $project 'osdk.toml') -Encoding utf8 -Value "[tools]`ngo = `"1.24.0`""
    # Runtime structure is a fixture; Go is never executed by this shell contract.
    $suffix = if ($IsWindows) { '.exe' } else { '' }
    foreach ($relative in @("bin/go$suffix", "bin/gofmt$suffix", "pkg/tool/compile$suffix")) {
        Copy-Item -LiteralPath $osdkPath -Destination (Join-Path $runtime $relative)
    }
    Set-Content -LiteralPath (Join-Path $runtime 'src/runtime/runtime.go') -Encoding utf8 -Value 'package runtime'
    Set-Content -LiteralPath (Join-Path $runtime 'VERSION') -Encoding ascii -Value 'go1.24.0'
    Set-Content -LiteralPath (Join-Path $runtime 'go.env') -Encoding ascii -Value 'GOTOOLCHAIN=local'
    Set-Content -LiteralPath (Join-Path $runtime '.osdk-complete') -Encoding ascii -Value ''
    $child = Join-Path $root 'check.ps1'
    Set-Content -LiteralPath $child -Encoding utf8 -Value @'
param([string]$Osdk, [string]$Entry)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
function Check([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Apply([string[]]$Arguments) {
    $text = (& $Osdk @Arguments | Out-String)
    Check ($LASTEXITCODE -eq 0) "render failed: $Arguments"
    Check (-not [string]::IsNullOrWhiteSpace($text)) "empty rendering: $Arguments"
    Invoke-Expression $text
}
if ($Entry -eq 'env') { Apply @('env', '--format', 'powershell') }
Check (-not (Test-Path Env:OSDK_ORIG_GOPROXY_SET)) 'standalone output captured a hook snapshot'
Check ((Test-Path Env:GOPROXY) -and ($env:GOPROXY -ceq $env:OSDK_APPLIED_GOPROXY)) 'missing managed source marker'
$managed = $env:GOPROXY
Apply @('hook-env', '--shell', 'powershell')
Check ($env:OSDK_ORIG_GOPROXY_PRESENT -eq '0') 'first hook captured managed output as user source'
Apply @('hook-env', '--shell', 'powershell')
Check ($env:OSDK_ORIG_GOPROXY_PRESENT -eq '0') 'repeated hook replaced absence snapshot'
Check ($env:GOPROXY -ceq $managed) 'repeated hook changed source'
Apply @('deactivate', 'powershell')
Check (-not (Test-Path Env:GOPROXY)) 'deactivate did not restore absence'
Check (-not (Test-Path Env:OSDK_APPLIED_GOPROXY)) 'deactivate leaked marker'
Apply @('env', '--format', 'powershell')
$env:GOPROXY = 'https://user.example,direct'
Apply @('hook-env', '--shell', 'powershell')
Apply @('deactivate', 'powershell')
Check ($env:GOPROXY -ceq 'https://user.example,direct') 'pre-hook override lost'
Remove-Item Env:GOPROXY
Apply @('env', '--format', 'powershell')
Apply @('hook-env', '--shell', 'powershell')
$env:GOPROXY = 'off'
Apply @('deactivate', 'powershell')
Check ($env:GOPROXY -ceq 'off') 'post-hook override lost'
Remove-Item Env:GOPROXY
Apply @('env', '--format', 'powershell')
Apply @('hook-env', '--shell', 'powershell')
Remove-Item Env:GOPROXY
Apply @('deactivate', 'powershell')
Check (-not (Test-Path Env:GOPROXY)) 'user unset lost'
Write-Output "GO_PROXY_OK entry=$Entry os=$($PSVersionTable.OS) pwsh=$($PSVersionTable.PSVersion)"
'@
    Push-Location $project
    try {
        & $pwsh -NoProfile -NonInteractive -File $child -Osdk $osdkPath -Entry env
        if ($LASTEXITCODE -ne 0) { throw 'env -> hook-env PowerShell regression failed' }
        & $osdkPath exec -- $pwsh -NoProfile -NonInteractive -File $child -Osdk $osdkPath -Entry exec
        if ($LASTEXITCODE -ne 0) { throw 'exec -> hook-env PowerShell regression failed' }
    }
    finally { Pop-Location }
}
finally {
    foreach ($name in $original.Keys) {
        [Environment]::SetEnvironmentVariable($name, $original[$name], 'Process')
    }
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force }
}
