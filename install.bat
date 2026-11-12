@echo off
REM ---------------------------------------------------------------------------
REM  VulcanTrader - one-shot installer (Windows)
REM
REM  Creates a Python 3.12 virtualenv in .venv, upgrades pip, and installs
REM  every dependency listed in requirements.txt.
REM
REM  Optional components - asked at the end (default: no), or preset for an
REM  unattended install:
REM    --with-ninjatrader / --without-ninjatrader   (or INSTALL_NINJATRADER=yes|no)
REM        NinjaTrader 8 + the VulcanTraderBridge AddOn for the `ninjatrader`
REM        exchange - runs setup-ninjatrader.bat guided, which installs NT8 and
REM        compiles the bridge. When NT8 is already installed (and neither flag
REM        is given) there is no question: the bridge is just recompiled if the
REM        repo copy changed (setup-ninjatrader.bat addon-if-needed).
REM ---------------------------------------------------------------------------
setlocal enableextensions
cd /d "%~dp0"

set "VT_NT8=%INSTALL_NINJATRADER%"
for %%A in (%*) do (
    if /i "%%~A"=="--with-ninjatrader" set "VT_NT8=yes"
    if /i "%%~A"=="--without-ninjatrader" set "VT_NT8=no"
)

if not exist ".venv" (
    echo [install] creating virtualenv .venv with Python 3.12 ...
    python -m venv .venv
    if errorlevel 1 (
        echo [install] ERROR: "py -3.12" failed. Install Python 3.12 from python.org first.
        exit /b 1
    )
) else (
    echo [install] reusing existing .venv
)

call ".venv\Scripts\activate.bat"
if errorlevel 1 (
    echo [install] ERROR: failed to activate .venv
    exit /b 1
)

python -m pip install --upgrade pip wheel
if errorlevel 1 exit /b 1

python -m pip install -r requirements.txt
if errorlevel 1 exit /b 1

REM async-rithmic (VulcanTrader\exchange\rithmic.py - live/dry-run data feed for
REM CME-style futures) ships its own SSL cert file inside the package; verify
REM both the import and that file made it into the venv, so a broken/partial
REM install is visible now instead of surfacing as a connect-time
REM FileNotFoundError deep inside a running bot.
".venv\Scripts\python.exe" -c "import async_rithmic; from pathlib import Path as _P; _c = _P(async_rithmic.__file__).parent / 'certificates' / 'rithmic_ssl_cert_auth_params'; assert _c.exists(), f'missing bundled Rithmic SSL cert: {_c}'; print('[install] async_rithmic OK (Rithmic exchange live data feed)')"
if errorlevel 1 exit /b 1

echo.
echo [install] building the Rust backtester + vulcan_rust_indicators bridge (VulcanTrader\backtester) ...
echo [install]   One crate: the backtest engine, and the PyO3 indicator module that
echo [install]   AllIndicatorsDemoStrategy.py and several user_data\strategies\*.py import.
where cargo >nul 2>nul
if errorlevel 1 (
    echo [install] ERROR: "cargo" not found. Install the Rust toolchain from https://rustup.rs first.
    exit /b 1
)
REM maturin builds the vulcan_rust_indicators extension (which also compiles the
REM engine). PYO3_USE_ABI3_FORWARD_COMPATIBILITY allows building the abi3 module
REM against a Python newer than PyO3 explicitly supports.
pushd "VulcanTrader\backtester"
set PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1
maturin develop --release --features extension-module
if errorlevel 1 (
    popd
    echo [install] ERROR: vulcan_rust_indicators build failed.
    exit /b 1
)
popd

REM The #[pymodule] registration in VulcanTrader/backtester/src/python.rs has
REM silently dropped functions between source revisions before (compiles fine,
REM only fails at call-time for whichever strategy happens to use the missing
REM one) - always print what actually got registered after a build so a
REM regression is visible immediately instead of surfacing later as an
REM AttributeError deep in a backtest.
".venv\Scripts\python.exe" -c "import vulcan_rust_indicators as v; print('[install] vulcan_rust_indicators exports:', [f for f in dir(v) if not f.startswith('_')])"

echo.
echo [install] core install done.
echo.

REM ---------------------------------------------------------------------------
REM  Optional components. Each is asked about (default no) unless preset by
REM  flag or env var; a failure here never undoes or fails the core install.
REM ---------------------------------------------------------------------------
REM NinjaTrader already installed (and no flag/env given): no question - just keep
REM its compiled bridge in step with the repo copy (rebuilds only if it changed).
set "VT_NT8_PRESENT="
if not defined VT_NT8 (
    call "%~dp0setup-ninjatrader.bat" installed >nul 2>nul
    if not errorlevel 1 set "VT_NT8_PRESENT=1"
)
if defined VT_NT8_PRESENT (
    echo [install] NinjaTrader 8 is installed - compiling the VulcanTraderBridge AddOn if the repo copy changed...
    call "%~dp0setup-ninjatrader.bat" addon-if-needed
    if errorlevel 1 echo [install] WARNING: bridge compile did not finish - re-run setup-ninjatrader.bat addon ^(the core install is complete^).
    set "VT_NT8=done"
)
if not defined VT_NT8 set /p "VT_NT8=[install] Optional: set up NinjaTrader 8 + the VulcanTraderBridge AddOn for the 'ninjatrader' exchange? [y/N] "
if /i "%VT_NT8%"=="y" set "VT_NT8=yes"
if /i "%VT_NT8%"=="1" set "VT_NT8=yes"
if /i "%VT_NT8%"=="true" set "VT_NT8=yes"
if /i "%VT_NT8%"=="yes" (
    call "%~dp0setup-ninjatrader.bat" guided
    if errorlevel 1 (
        echo [install] WARNING: NinjaTrader setup did not finish - fix the error above and re-run setup-ninjatrader.bat ^(the core install is complete^).
    ) else (
        echo [install] NinjaTrader 8 setup finished.
    )
) else if /i not "%VT_NT8%"=="done" (
    echo [install] skipping NinjaTrader 8 setup ^(run setup-ninjatrader.bat any time^).
)

echo.
echo [install] done. Activate later with:  .\.venv\Scripts\Activate.ps1
echo [install] then run:                   run-paper.bat   (or run-live.bat)
endlocal
