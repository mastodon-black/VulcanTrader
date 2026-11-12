<#
  VulcanTrader - NinjaTrader 8 on Windows (native): install NT8 and deploy + compile
  the VulcanTraderBridge AddOn for the `ninjatrader` exchange.

  Run through setup-ninjatrader.bat at the repo root (install.bat offers it):
    setup-ninjatrader.bat [guided|install|addon|doctor]     (default: guided)

    guided   install -> addon -> (offer to start NinjaTrader) -> doctor
    install  NinjaTrader 8 from its .msi (NT8_INSTALLER env var, a NinjaTrader*.msi in
             the repo or Downloads, or asked for); Windows asks for administrator approval
    addon    copy VulcanTraderBridge.cs into Documents\NinjaTrader 8\bin\Custom\AddOns and
             compile NinjaTrader.Custom.dll with NinjaTrader's own bundled Roslyn
             (..\linux\ntbuild.cs - same driver the Linux/Wine setup uses), so no manual
             NinjaScript Editor / F5 step is needed
    addon-if-needed   addon, only if the repo copy changed since the last build
             (install.bat runs this when NinjaTrader is already installed)
    doctor   check every piece + the configs that use the ninjatrader exchange

  Windows runs NT8 natively, so none of the Wine-only fixes (websocket.dll, WPF software
  rendering, Xvfb login automation, memory cap) apply here - see AGENTS.md
  "NinjaTrader 8 on Linux (Wine)" for those.
#>
param([string]$Step = "guided")
$ErrorActionPreference = "Stop"

$Repo     = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..\..")).Path
$AddonDir = Join-Path $Repo "VulcanTrader\exchange\ninjatrader_addon"
$AddonSrc = Join-Path $AddonDir "VulcanTraderBridge.cs"
$NtBin    = Join-Path $env:ProgramFiles "NinjaTrader 8\bin"
$NtExe    = Join-Path $NtBin "NinjaTrader.exe"
$NtDocs   = Join-Path ([Environment]::GetFolderPath("MyDocuments")) "NinjaTrader 8"
$Custom   = Join-Path $NtDocs "bin\Custom"
$CustomDll = Join-Path $Custom "NinjaTrader.Custom.dll"
$Fw       = Join-Path $env:WINDIR "Microsoft.NET\Framework64\v4.0.30319"
$BridgePort = 8755
if ($env:BRIDGE_PORT) { $BridgePort = [int]$env:BRIDGE_PORT }

function Log([string]$m) { Write-Host "[setup-ninjatrader] $m" }
function Nt8Running { return [bool](Get-Process -Name "NinjaTrader" -ErrorAction SilentlyContinue) }
function BridgeUp {
    return [bool](Get-NetTCPConnection -LocalPort $BridgePort -State Listen -ErrorAction SilentlyContinue)
}

# --------------------------------------------------------------------- install ---
function Step-Install {
    if (Test-Path $NtExe) { Log "NinjaTrader 8 already installed ($NtExe)."; return }
    $msi = $env:NT8_INSTALLER
    if (-not $msi) {
        $msi = Get-ChildItem -Path $Repo, (Join-Path $env:USERPROFILE "Downloads") -Filter "NinjaTrader*.msi" `
                   -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName
    }
    if (-not $msi) {
        $msi = Read-Host "Path to the NinjaTrader 8 installer (.msi from your NinjaTrader account; Enter to skip)"
    }
    if (-not $msi) { throw "NinjaTrader 8 is not installed. Install it (ninjatrader.com), then re-run setup-ninjatrader.bat." }
    if (-not (Test-Path $msi)) { throw "Installer not found: $msi" }
    Log "Installing NinjaTrader 8 from $msi (Windows will ask for administrator approval)..."
    $p = Start-Process -FilePath "msiexec.exe" -ArgumentList "/i `"$msi`" /passive /norestart" -Verb RunAs -Wait -PassThru
    if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) { throw "msiexec failed with exit code $($p.ExitCode)." }
    if (-not (Test-Path $NtExe)) { throw "Install finished but $NtExe is missing." }
    Log "NinjaTrader 8 installed."
}

# ----------------------------------------------------------------------- addon ---
function Wait-FirstRun {
    if (Test-Path $CustomDll) { return }
    Log "NinjaTrader has not been run on this Windows account yet - it creates $NtDocs on first start."
    if (-not (Nt8Running)) { Start-Process -FilePath $NtExe }
    Log "Log in to NinjaTrader (and pick a trading mode if asked); waiting up to 15 min..."
    $deadline = (Get-Date).AddMinutes(15)
    while (-not (Test-Path $CustomDll)) {
        if ((Get-Date) -gt $deadline) { throw "NinjaTrader never created $Custom - log in to NinjaTrader once, then re-run." }
        Start-Sleep -Seconds 5
    }
    Start-Sleep -Seconds 10   # let NinjaTrader finish writing its first-run files
}

function Wait-Nt8Closed {
    if (-not (Nt8Running)) { return }
    Log "Close NinjaTrader (File > Exit) so its compiled NinjaScript can be replaced - waiting..."
    $deadline = (Get-Date).AddMinutes(10)
    while (Nt8Running) {
        if ((Get-Date) -gt $deadline) { throw "NinjaTrader is still running - close it and re-run setup-ninjatrader.bat addon." }
        Start-Sleep -Seconds 3
    }
}

function Build-CustomDll {
    # Native tools (csc, ntbuild): with "Stop", Windows PowerShell 5.1 turns any
    # redirected stderr line into a terminating error - check results explicitly.
    $ErrorActionPreference = "Continue"
    $build = Join-Path $env:LOCALAPPDATA "VulcanTrader\nt8-build"
    New-Item -ItemType Directory -Force -Path $build | Out-Null
    Copy-Item (Join-Path $AddonDir "linux\ntbuild.cs") $build -Force
    Get-ChildItem $NtBin -Filter "*.dll" | Where-Object {
        $_.Name -like "System.*" -or $_.Name -like "Microsoft.CodeAnalysis*" -or $_.Name -like "Microsoft.Bcl.*"
    } | Copy-Item -Destination $build -Force
    # NinjaTrader's own assembly binding redirects - Roslyn needs them to load.
    $ntConfig = Get-Content (Join-Path $NtBin "NinjaTrader.exe.config") -Raw
    $runtime = [regex]::Match($ntConfig, "(?s)<runtime>.*?</runtime>").Value
    if (-not $runtime) { throw "No <runtime> section in NinjaTrader.exe.config." }
    $cfg = '<?xml version="1.0" encoding="utf-8"?><configuration>' +
           '<startup><supportedRuntime version="v4.0" sku=".NETFramework,Version=v4.8"/></startup>' +
           $runtime + '</configuration>'
    Set-Content -Path (Join-Path $build "ntbuild.exe.config") -Value $cfg -Encoding UTF8

    Push-Location $build
    try {
        $exe = Join-Path $build "ntbuild.exe"
        $src = Join-Path $build "ntbuild.cs"
        if (-not (Test-Path $exe) -or (Get-Item $src).LastWriteTime -gt (Get-Item $exe).LastWriteTime) {
            & (Join-Path $Fw "csc.exe") /nologo /platform:x64 /nowarn:1701 /out:ntbuild.exe `
                /r:Microsoft.CodeAnalysis.dll /r:Microsoft.CodeAnalysis.CSharp.dll /r:System.Collections.Immutable.dll `
                "/r:$Fw\System.Runtime.dll" "/r:$Fw\netstandard.dll" "/r:$Fw\System.Reflection.dll" `
                "/r:$Fw\System.Text.Encoding.dll" ntbuild.cs
            if ($LASTEXITCODE -ne 0) { throw "Could not compile the build driver (ntbuild.cs)." }
        }
        $out = Join-Path $build "out"
        if (Test-Path $out) { Remove-Item $out -Recurse -Force }
        $lines = & $exe build $Custom $NtBin $CustomDll $out 2>&1 | ForEach-Object { "$_" }
        $lines | Where-Object { $_.Trim() } | ForEach-Object { Write-Host "  $_" }
        $lines | Set-Content (Join-Path $build "build.log")
        if (-not ($lines | Where-Object { $_ -match "^BUILD OK" })) { throw "NinjaScript build failed - see $build\build.log" }
    } finally { Pop-Location }

    $bk = Join-Path $env:LOCALAPPDATA ("VulcanTrader\nt8-custom-backups\" + (Get-Date -Format "yyyyMMdd-HHmmss"))
    New-Item -ItemType Directory -Force -Path $bk | Out-Null
    foreach ($ext in "dll", "pdb", "xml") {
        $f = Join-Path $Custom "NinjaTrader.Custom.$ext"
        if (Test-Path $f) { Copy-Item $f $bk -Force -ErrorAction Stop }
        Copy-Item (Join-Path $build "out\NinjaTrader.Custom.$ext") $Custom -Force -ErrorAction Stop
    }
    Log "Installed the rebuilt NinjaTrader.Custom.dll (previous build backed up to $bk)."
}

function Step-Addon {
    if (-not (Test-Path $NtExe)) { throw "NinjaTrader 8 is not installed - run setup-ninjatrader.bat install first." }
    Wait-FirstRun
    Wait-Nt8Closed
    $addons = Join-Path $Custom "AddOns"
    New-Item -ItemType Directory -Force -Path $addons | Out-Null
    Copy-Item $AddonSrc (Join-Path $addons "VulcanTraderBridge.cs") -Force
    Log "Deployed VulcanTraderBridge.cs; compiling NinjaTrader.Custom.dll with NinjaTrader's Roslyn..."
    Build-CustomDll
    Log "Done. Start NinjaTrader and connect; its Log tab shows 'VulcanTraderBridge listening on port $BridgePort'."
}

# The compiled bridge is current when the deployed .cs is the repo's copy and
# NinjaTrader.Custom.dll was built after it was deployed.
function Addon-Current {
    $deployed = Join-Path $Custom "AddOns\VulcanTraderBridge.cs"
    if (-not (Test-Path $deployed) -or -not (Test-Path $CustomDll)) { return $false }
    if ((Get-FileHash $deployed).Hash -ne (Get-FileHash $AddonSrc).Hash) { return $false }
    return ((Get-Item $CustomDll).LastWriteTime -gt (Get-Item $deployed).LastWriteTime)
}

# What install.bat runs when NinjaTrader is already installed: recompile the
# bridge only if the repo copy changed.
function Step-AddonIfNeeded {
    if (-not (Test-Path $NtExe)) { Log "NinjaTrader 8 not installed - no bridge to compile."; return }
    if (Addon-Current) { Log "VulcanTraderBridge is already compiled from the current repo copy."; return }
    Log "VulcanTraderBridge in the repo changed (or was never compiled) - rebuilding it."
    Step-Addon
}

# system.json "ninjatrader": {"auto_start": true} makes VulcanTrader open NinjaTrader
# whenever the fleet starts (VulcanTrader\util\ninjatrader_launcher.py).
function Set-AutoStart {
    $answer = $env:NT8_AUTO_START
    if (-not $answer) {
        $answer = Read-Host "Have VulcanTrader open NinjaTrader whenever the fleet starts (system.json ninjatrader.auto_start)? [Y/n]"
    }
    $value = "on"
    if ($answer -match '^(n|no|0|false|off)$') { $value = "off" }
    $py = Join-Path $Repo ".venv\Scripts\python.exe"
    if (-not (Test-Path $py)) { $py = "python" }
    & $py -I (Join-Path $Repo "VulcanTrader\util\ninjatrader_launcher.py") --auto-start $value
}

# ---------------------------------------------------------------------- doctor ---
function Step-Doctor {
    $script:bad = 0
    function Check([string]$name, [bool]$ok, [string]$fix) {
        if ($ok) { Write-Host "  OK    $name" -ForegroundColor Green }
        else { Write-Host "  FAIL  $name$(if ($fix) { " - $fix" })" -ForegroundColor Red; $script:bad++ }
    }
    Log "doctor"
    Check "NinjaTrader 8 installed" (Test-Path $NtExe) "setup-ninjatrader.bat install"
    Check "NinjaTrader has been run once ($Custom)" (Test-Path $CustomDll) "start NinjaTrader and log in"
    $deployed = Join-Path $Custom "AddOns\VulcanTraderBridge.cs"
    $same = (Test-Path $deployed) -and ((Get-FileHash $deployed).Hash -eq (Get-FileHash $AddonSrc).Hash)
    Check "deployed AddOn matches the repo copy" $same "setup-ninjatrader.bat addon"
    $built = (Test-Path $CustomDll) -and (Select-String -Path $CustomDll -Pattern "TimeframeMs" -SimpleMatch -Quiet) `
             -and (Test-Path $deployed) -and ((Get-Item $CustomDll).LastWriteTime -ge (Get-Item $deployed).LastWriteTime)
    Check "compiled NinjaTrader.Custom.dll contains the current bridge" $built "setup-ninjatrader.bat addon"
    Check "NinjaTrader running" (Nt8Running) "start NinjaTrader"
    Check "VulcanTraderBridge listening on :$BridgePort" (BridgeUp) "start NinjaTrader and connect"
    $auto = "unset"
    try {
        $sys = Get-Content (Join-Path $Repo "user_data\configs_system\system.json") -Raw | ConvertFrom-Json
        if ($sys.ninjatrader -and $sys.ninjatrader.auto_start -eq $true) { $auto = "true" } else { $auto = "false" }
    } catch { }
    Write-Host "  info  system.json ninjatrader.auto_start = $auto (toggle: python -m VulcanTrader.bot ninjatrader auto-start on|off)"
    Write-Host "  -- configs on the ninjatrader exchange:"
    $cfgBad = 0
    Get-ChildItem (Join-Path $Repo "user_data\configs") -Filter "*.json" -ErrorAction SilentlyContinue | ForEach-Object {
        try { $c = Get-Content $_.FullName -Raw | ConvertFrom-Json } catch { return }
        if (-not $c.exchange -or $c.exchange.name -ne "ninjatrader") { return }
        $problems = @()
        if (-not $c.exchange.ninjatrader_account) { $problems += 'no exchange.ninjatrader_account (e.g. "Sim101")' }
        $loose = @($c.exchange.pair_whitelist | Where-Object { $_ -notmatch '^[A-Z0-9]+-\d\d-\d\d$' })
        if ($loose.Count) { $problems += "pairs without a contract month (want ROOT-MM-YY, e.g. HG-12-26): $($loose -join ', ')" }
        if ($problems.Count) { $cfgBad++; Write-Host "  FAIL  $($_.Name): $($problems -join '; ')" -ForegroundColor Red }
    }
    Check "$cfgBad config(s) with problems" ($cfgBad -eq 0) ""
    if ($script:bad) { Log "$($script:bad) problem(s)."; return $false }
    Log "all good."
    return $true
}

# ------------------------------------------------------------------------ main ---
switch ($Step.ToLower()) {
    "install" { Step-Install }
    "addon"   { Step-Addon }
    "addon-if-needed" { Step-AddonIfNeeded }
    "installed" { if (Test-Path $NtExe) { exit 0 } else { exit 1 } }   # used by install.bat
    "doctor"  { if (-not (Step-Doctor)) { exit 1 } }
    "guided"  {
        Log "Guided NinjaTrader 8 setup (Windows): install -> addon -> start -> doctor"
        Step-Install
        Step-Addon
        $start = Read-Host "Start NinjaTrader now? [Y/n]"
        if ($start -notmatch '^(n|no)$') {
            Start-Process -FilePath $NtExe
            Log "Log in and connect (e.g. Simulation); the bridge starts with NinjaTrader."
            $deadline = (Get-Date).AddMinutes(10)
            while (-not (BridgeUp) -and (Get-Date) -lt $deadline) { Start-Sleep -Seconds 5 }
        }
        Set-AutoStart
        [void](Step-Doctor)
    }
    default {
        $lines = @(Get-Content $PSCommandPath)
        $end = [array]::IndexOf($lines, "#>")
        $lines[1..($end - 1)]
        exit 2
    }
}
