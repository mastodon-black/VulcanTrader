#!/usr/bin/env bash
# ---------------------------------------------------------------------------
#  VulcanTrader - one-shot installer (Linux / macOS)
#
#  Creates a virtualenv in .venv, upgrades pip, and installs every dependency
#  listed in requirements.txt.  Override the interpreter with PYTHON=pythonX.Y
#
#  Optional components - asked at the end (default: no), or preset for an
#  unattended install:
#    --with-ninjatrader / --without-ninjatrader   (or INSTALL_NINJATRADER=yes|no)
#        NinjaTrader 8 under Wine + the VulcanTraderBridge AddOn, for the
#        `ninjatrader` exchange (Linux only) - runs ./setup-ninjatrader.sh guided,
#        which installs NT8 and compiles the bridge. When NT8 is already set up
#        (and neither flag is given) there is no question: the bridge is just
#        recompiled if the repo copy changed (./setup-ninjatrader.sh addon-if-needed).
#
#  Always: sets "fleet_worker_processes" in user_data/configs_system/system.json
#  to this machine's CPU count (preset another value with FLEET_WORKER_PROCESSES=N).
# ---------------------------------------------------------------------------
set -euo pipefail
cd "$(dirname "$0")"

INSTALL_NINJATRADER="${INSTALL_NINJATRADER:-}"
for arg in "$@"; do
    case "$arg" in
        --with-ninjatrader) INSTALL_NINJATRADER=yes ;;
        --without-ninjatrader) INSTALL_NINJATRADER=no ;;
        *) echo "[install] ERROR: unknown option '$arg' (known: --with-ninjatrader, --without-ninjatrader)" >&2; exit 2 ;;
    esac
done

PYTHON_BIN="${PYTHON:-python3}"

if ! command -v "$PYTHON_BIN" >/dev/null 2>&1; then
    echo "[install] ERROR: '$PYTHON_BIN' not found. Install Python 3 first." >&2
    echo "[install]        (set PYTHON=python3.12 to pin a specific version)" >&2
    exit 1
fi

if [[ ! -d .venv ]]; then
    echo "[install] creating virtualenv .venv ($("$PYTHON_BIN" --version 2>&1)) ..."
    "$PYTHON_BIN" -m venv .venv
else
    echo "[install] reusing existing .venv"
fi

# shellcheck disable=SC1091
source .venv/bin/activate

python -m pip install --upgrade pip wheel
python -m pip install -r requirements.txt

# async-rithmic (VulcanTrader/exchange/rithmic.py - live/dry-run data feed for
# CME-style futures) ships its own SSL cert file inside the package; verify
# both the import and that file made it into the venv, so a broken/partial
# install is visible now instead of surfacing as a connect-time
# FileNotFoundError deep inside a running bot.
python -c "import async_rithmic; from pathlib import Path as _P; _c = _P(async_rithmic.__file__).parent / 'certificates' / 'rithmic_ssl_cert_auth_params'; assert _c.exists(), f'missing bundled Rithmic SSL cert: {_c}'; print('[install] async_rithmic OK (Rithmic exchange live data feed)')"

echo
echo "[install] building the Rust backtester + vulcan_rust_indicators bridge (VulcanTrader/backtester) ..."
echo "[install]   One crate: the backtest engine, and the PyO3 indicator module that"
echo "[install]   AllIndicatorsDemoStrategy.py and several user_data/strategies/*.py import."
if ! command -v cargo >/dev/null 2>&1; then
    echo "[install] ERROR: 'cargo' not found. Install the Rust toolchain from https://rustup.rs first." >&2
    exit 1
fi
# maturin builds the `vulcan_rust_indicators` extension (which also compiles the
# engine). PYO3_USE_ABI3_FORWARD_COMPATIBILITY allows building the abi3 module
# against a Python newer than PyO3 explicitly supports.
(cd VulcanTrader/backtester && PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 maturin develop --release --features extension-module)

# The #[pymodule] registration in VulcanTrader/backtester/src/python.rs has
# silently dropped functions between source revisions before (compiles fine,
# only fails at call-time for whichever strategy happens to use the missing
# one) - always print what actually got registered after a build so a
# regression is visible immediately instead of surfacing later as an
# AttributeError deep in a backtest.
python -c "import vulcan_rust_indicators as v; print('[install] vulcan_rust_indicators exports:', [f for f in dir(v) if not f.startswith('_')])"

# Split the single_process_mode fleet across one worker process per CPU on THIS
# machine (one process = one GIL). Re-run on every install, so a repo copied to
# a bigger/smaller box picks up that box's count.
echo
python VulcanTrader/util/fleet_workers.py "${FLEET_WORKER_PROCESSES:-auto}" \
    || echo "[install] WARNING: could not set fleet_worker_processes in user_data/configs_system/system.json - set it by hand (the core install is complete)." >&2

echo
echo "[install] core install done."

# ---------------------------------------------------------------------------
#  Optional components. Each is asked about (default no) unless preset by flag
#  or env var; a failure here never undoes or fails the core install above.
# ---------------------------------------------------------------------------
ask_optional() {
    # ask_optional <preset value> <question> -> exit 0 for yes
    local answer="$1"
    if [[ -z "$answer" ]]; then
        if [[ -t 0 ]]; then
            read -rp "[install] Optional: $2 [y/N] " answer || answer=""
        else
            answer="no"
        fi
    fi
    case "$(printf '%s' "$answer" | tr '[:upper:]' '[:lower:]')" in
        y|yes|1|true) return 0 ;;
        *) return 1 ;;
    esac
}

echo
if [[ -z "$INSTALL_NINJATRADER" && "$(uname -s)" == "Linux" ]] && ./setup-ninjatrader.sh installed 2>/dev/null; then
    # NinjaTrader is already set up on this box: keep its compiled bridge in step
    # with the repo copy (rebuilds + restarts NT8 only if the AddOn changed).
    echo "[install] NinjaTrader 8 is set up here - compiling the VulcanTraderBridge AddOn if the repo copy changed..."
    ./setup-ninjatrader.sh addon-if-needed \
        || echo "[install] WARNING: bridge compile did not finish - re-run ./setup-ninjatrader.sh addon (the core install is complete)." >&2
elif ask_optional "$INSTALL_NINJATRADER" "set up NinjaTrader 8 (under Wine) + the VulcanTraderBridge AddOn for the 'ninjatrader' exchange?"; then
    if [[ "$(uname -s)" != "Linux" ]]; then
        echo "[install] NinjaTrader 8 setup is supported on Linux (Wine) and Windows (install.bat) only - skipping."
    elif ./setup-ninjatrader.sh guided; then
        echo "[install] NinjaTrader 8 setup finished."
    else
        echo "[install] WARNING: NinjaTrader setup did not finish - fix the error above and re-run ./setup-ninjatrader.sh guided (the core install is complete)." >&2
    fi
else
    echo "[install] skipping NinjaTrader 8 setup (run ./setup-ninjatrader.sh guided any time)."
fi

echo
echo "[install] done. Activate later with:  source .venv/bin/activate"
echo "[install] then run:                   ./run-paper.sh   (or ./run-live.sh)"
