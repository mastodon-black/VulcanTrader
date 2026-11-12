@echo off
REM ---------------------------------------------------------------------------
REM  VulcanTrader - NinjaTrader 8 on Windows (native)
REM
REM  Installs NinjaTrader 8 and deploys + compiles the VulcanTraderBridge AddOn
REM  for the `ninjatrader` exchange. install.bat offers to run this.
REM
REM    setup-ninjatrader.bat            guided: install, addon, start, doctor
REM    setup-ninjatrader.bat install    NinjaTrader 8 from its .msi (NT8_INSTALLER=...)
REM    setup-ninjatrader.bat addon      deploy + compile VulcanTraderBridge.cs
REM    setup-ninjatrader.bat addon-if-needed   same, only if the repo copy changed
REM    setup-ninjatrader.bat doctor     check everything
REM
REM  Logic: VulcanTrader\exchange\ninjatrader_addon\windows\setup-ninjatrader.ps1
REM  (Linux equivalent: setup-ninjatrader.sh)
REM ---------------------------------------------------------------------------
setlocal
set "STEP=%~1"
if "%STEP%"=="" set "STEP=guided"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0VulcanTrader\exchange\ninjatrader_addon\windows\setup-ninjatrader.ps1" -Step %STEP%
set "RC=%errorlevel%"
endlocal & exit /b %RC%
