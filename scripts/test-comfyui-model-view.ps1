#!/usr/bin/env pwsh
# ComfyUI model-view end-to-end integration test.
#
# Drives the REAL osdk CLI against a REAL (headless) source-tree ComfyUI through
# the whole chain:
#   pull content-hashed snapshot -> declarative [models] render ->
#   idempotent extra_model_paths.yaml export -> folder_paths discovery,
# plus the two-model same-view-root collision contract.
#
# Everything lives under a temp OSDK_* root; nothing under E:\Comfy-Desktop,
# E:\osdk-data, or the user's config is touched.
#
# Usage: pwsh -NoProfile -File scripts/test-comfyui-model-view.ps1 -BinDir target/debug -ComfyDir <clone>
#        [-Keep]
# Requires: python (3.11 ok) with PyYAML (folder_paths only needs yaml).
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$BinDir,
    [Parameter(Mandatory = $true)][string]$ComfyDir,
    [switch]$Keep
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Fail([string]$msg) { Write-Error "FAIL: $msg"; exit 1 }
function Step([string]$msg) { Write-Host "==> $msg" }

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$fixtureServer = Join-Path $repoRoot "scripts\comfy-view-fixture-server.py"
$probeScript = Join-Path $repoRoot "scripts\comfy-view-probe.py"

$osdk = Join-Path $BinDir "osdk.exe"
if (-not (Test-Path $osdk)) { $osdk = Join-Path $BinDir "osdk" }
if (-not (Test-Path $osdk)) { Fail "osdk binary not found under $BinDir" }
$osdk = (Resolve-Path $osdk).Path
if (-not (Test-Path (Join-Path $ComfyDir "folder_paths.py"))) { Fail "ComfyDir ($ComfyDir) has no folder_paths.py" }

$python = "C:\Python311\python.exe"
if (-not (Test-Path $python)) { $python = (Get-Command python).Source }

$work = Join-Path ([IO.Path]::GetTempPath()) ("comfy-view-" + [guid]::NewGuid().ToString("").Substring(0, 8))
New-Item -ItemType Directory -Path $work | Out-Null
$sp = $null
try {
    $data = Join-Path $work "data"; $cache = Join-Path $work "cache"; $config = Join-Path $work "config"
    $store = Join-Path $work "store"; $installs = Join-Path $work "installs"; $home2 = Join-Path $work "home"
    New-Item -ItemType Directory -Path $data, $cache, $config, $store, $installs, $home2 | Out-Null
    $project = Join-Path $work "project"; New-Item -ItemType Directory -Path $project | Out-Null

    $env:OSDK_DATA_DIR = $data; $env:OSDK_CACHE_DIR = $cache; $env:OSDK_CONFIG_DIR = $config
    $env:OSDK_STORE_DIR = $store; $env:OSDK_INSTALL_DIR = $installs
    $env:HOME = $home2; $env:USERPROFILE = $home2
    $env:NO_PROXY = "127.0.0.1,localhost"; $env:no_proxy = "127.0.0.1,localhost"
    foreach ($k in @("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY", "all_proxy")) {
        Remove-Item "env:$k" -ErrorAction SilentlyContinue
    }

    Step "start loopback fixture server"
    $pick = Join-Path $work "pick_port.py"
    'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()' |
        Set-Content -Path $pick -Encoding ASCII
    $port = (& $python $pick).Trim()
    $sp = Start-Process -FilePath $python -ArgumentList @("`"$fixtureServer`"", "$port") -PassThru -WindowStyle Hidden
    Start-Sleep -Milliseconds 600
    $endpoint = "http://127.0.0.1:$port"

        Step "write [models] declarations"
        $lines = @(
            '[models.a]',
            'source = "hf:owner/a@main"',
            '[models.a.views.comfyui]',
            'profile = "default"',
            '',
            '# c declares a source but no view: its snapshot is fetched, then the',
            '# collision is exercised below via explicit `view add`.',
            '[models.c]',
            'source = "hf:owner/c@main"',
            '',
            '[models.b]',
            'source = "hf:owner/b@main"',
            '[models.b.views.comfyui]',
            'profile = "second"'
        )
        Set-Content -Path (Join-Path $project "osdk.toml") -Value ($lines -join "`n") -Encoding UTF8

        foreach ($m in @("a", "b", "c")) {
            Step "pull $m (declaration must auto-render)"
            Push-Location $project
            $pull = & $osdk model pull $m "hf:owner/$m@main" --endpoint $endpoint 2>&1
            $code = $LASTEXITCODE
            Pop-Location
            if ($code -ne 0) { Fail "pull $m failed:`n$($pull -join "`n")" }
        }

        Step "verify declarative render artifacts"
        $lock = Get-Content (Join-Path $project "osdk.lock") -Raw
        if ($lock -notmatch '(?s)\[models\.a\.views\.comfyui\]') { Fail "lock missing a views`n$lock" }
        $state = Get-Content (Join-Path $data "views\.osdk-views.json") -Raw
        if ($state -notmatch [regex]::Escape("`"a`"")) { Fail "view state missing a`n$state" }
        if ($state -notmatch [regex]::Escape("`"b`"")) { Fail "view state missing b`n$state" }
        if ($state -match [regex]::Escape("`"c`"")) { Fail "c has no view declaration; it must not be in view state yet`n$state" }

        # Second view root coexists: b renders into profile "second" without
        # colliding with a in "default" (different roots).
        Push-Location $project
        $secondRootOut = & $osdk model view path comfyui --profile second 2>&1
        $secondCode = $LASTEXITCODE
        Pop-Location
        if ($secondCode -ne 0) { Fail "second view path failed: $secondRootOut" }
        $secondRoot = ($secondRootOut | Select-Object -Last 1).ToString().Trim()
        $secondVae = Join-Path $secondRoot "vae/model.safetensors"
        $secondLora = Join-Path $secondRoot "loras/x.safetensors"
        if (-not (Test-Path $secondVae)) { Fail "second root missing b vae" }
        if ([Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($secondVae)) -ne "BBB-VAE") { Fail "second root vae bytes wrong" }
        if (-not (Test-Path $secondLora)) { Fail "second root missing b lora" }

        Push-Location $project
        $pathOut = & $osdk model view path comfyui 2>&1
        $pathCode = $LASTEXITCODE
        Pop-Location
        if ($pathCode -ne 0) { Fail "view path failed: $pathOut" }
        $viewRoot = ($pathOut | Select-Object -Last 1).ToString().Trim()
        $expect = @{
            "diffusion_models/model.safetensors" = "AAA-UNET"
            "vae/model.safetensors" = "AAA-VAE"
            "text_encoders/model.safetensors" = "AAA-TE"
        }
        foreach ($k in $expect.Keys) {
            $f = Join-Path $viewRoot $k
            if (-not (Test-Path $f)) { Fail "missing rendered $k" }
            $text = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($f))
            if ($text -ne $expect[$k]) { Fail "$k content mismatch: $text" }
        }

        Step "export yaml: no is_default, idempotent, user block preserved"
        $yamlPath = Join-Path $work "extra_model_paths.yaml"
        Set-Content -Path $yamlPath -Value "user_block:`n  base_path: C:/keep-me`n  checkpoints: mine`n" -Encoding UTF8
        Push-Location $project
        & $osdk model view export comfyui --to $yamlPath | Out-Null
        $exp1 = $LASTEXITCODE
        & $osdk model view export comfyui --to $yamlPath | Out-Null
        $exp2 = $LASTEXITCODE
        Pop-Location
        if ($exp1 -ne 0) { Fail "first export failed" }
        if ($exp2 -ne 0) { Fail "second (idempotent) export failed" }
        $yamlText = Get-Content $yamlPath -Raw
        if ($yamlText -match "is_default") { Fail "export emitted is_default" }
        if ($yamlText -notmatch "user_block:" -or $yamlText -notmatch "C:/keep-me") { Fail "user block not preserved" }
        $managed = ([regex]::Matches($yamlText, "osdk-comfyui-default:")).Count
        if ($managed -ne 1) { Fail "managed block not idempotent (count=$managed)" }

        Step "two models same consumer path -> collision rejected, incumbent intact"
        $collide = Join-Path $viewRoot "diffusion_models/model.safetensors"
        Push-Location $project
        $add = & $osdk model view add comfyui c 2>&1
        $addCode = $LASTEXITCODE
        Pop-Location
        if ($addCode -eq 0) { Fail "expected collision error adding c, got success`n$($add -join ' ')" }
        if (($add -join " ") -notmatch "already provided by model") { Fail "wrong collision error: $($add -join ' ')" }
        $text = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($collide))
        if ($text -ne "AAA-UNET") { Fail "incumbent a link overwritten after collision: $text" }

        Push-Location $project
        & $osdk model view remove comfyui --model c | Out-Null
        $rmCode = $LASTEXITCODE
        Pop-Location
        if ($rmCode -ne 0) { Fail "remove c failed" }
        if (-not (Test-Path $collide)) { Fail "removing c broke a's link" }
        $text = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($collide))
        if ($text -ne "AAA-UNET") { Fail "a link changed after removing c: $text" }

        Push-Location $project
        & $osdk model view add comfyui a | Out-Null
        $reAddCode = $LASTEXITCODE
        Pop-Location
        if ($reAddCode -ne 0) { Fail "re-add a failed" }
        $text = [Text.Encoding]::UTF8.GetString([IO.File]::ReadAllBytes($collide))
        if ($text -ne "AAA-UNET") { Fail "a bytes changed after rebuild: $text" }

        Step "real ComfyUI folder_paths discovery + reverse controls"
        function Invoke-Probe {
            $out = & $python $probeScript $ComfyDir $yamlPath 2>&1
            $line = ($out | Where-Object { "$_" -match "^RESULT=" } | Select-Object -Last 1)
            if (-not $line) { Fail "ComfyUI probe produced no RESULT:`n$($out -join "`n")" }
            return (($line -replace "^RESULT=", "") | ConvertFrom-Json)
        }
        $json = Invoke-Probe
        foreach ($cat in @("diffusion_models", "vae", "text_encoders")) {
            if ($json.$cat -notcontains "model.safetensors") { Fail "$cat missing model.safetensors: $($json.$cat -join ',')" }
        }
        if ($json.checkpoints.Count -ne 0) {
            Fail "reverse control failed: checkpoints must be empty, got $($json.checkpoints -join ',')"
        }

        # Second reverse control: removing the rendered link must make the
        # category empty again -- proves the non-empty result really came from
        # the rendered file, distinguishing missing path / wrong category /
        # not-rescanned (the section 5.12.3 three causes).
        Remove-Item $collide -Force
        $json2 = Invoke-Probe
        if ($json2.diffusion_models -contains "model.safetensors") {
            Fail "after removing the link, diffusion_models still lists it"
        }

        Write-Host ""
        Write-Host "ComfyUI model-view end-to-end PASSED" -ForegroundColor Green
    }
    finally {
        if ($null -ne $sp -and -not $sp.HasExited) {
            Stop-Process -Id $sp.Id -Force -ErrorAction SilentlyContinue
        }
        if ($Keep) { Write-Host "kept workdir: $work" }
        else { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }
    }
