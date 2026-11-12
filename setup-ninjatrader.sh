#!/usr/bin/env bash
# ---------------------------------------------------------------------------
#  VulcanTrader - NinjaTrader 8 on Linux (Wine), end to end
#
#  Sets up everything the `ninjatrader` exchange needs on a Linux box: Wine,
#  a dedicated prefix with .NET 4.8, NinjaTrader 8 itself, the fixes that make
#  it work headless under Wine, the VulcanTraderBridge AddOn (compiled), and a
#  supervised, memory-capped boot service. Every step is idempotent.
#
#    ./setup-ninjatrader.sh guided          EVERYTHING below in order, asking only for
#                                           what it can't know (sudo password, the
#                                           NT8 .msi, the NT login) - what install.sh
#                                           runs when you opt in to NinjaTrader
#    sudo ./setup-ninjatrader.sh system [MEM_MAX [SWAP_MAX]]
#                                           WineHQ stable, winetricks, Xvfb/xfwm4/
#                                           xdotool, and the nt8-autostart.service
#                                           boot unit (run once, as root via sudo)
#    ./setup-ninjatrader.sh prefix          Wine prefix + .NET 4.8 + corefonts, WPF
#                                           software rendering, websocket.dll
#    ./setup-ninjatrader.sh install         silent NT8 install (NT8_INSTALLER=...msi)
#    ./setup-ninjatrader.sh credentials     NT login -> chmod-600 file (NT_USERNAME /
#                                           NT_PASSWORD env, or prompted)
#    ./setup-ninjatrader.sh addon           deploy + compile VulcanTraderBridge.cs
#    ./setup-ninjatrader.sh addon-if-needed same, only if the repo copy changed since
#                                           the last build (install.sh runs this when
#                                           NinjaTrader is already installed)
#    ./setup-ninjatrader.sh start           bring NT8 up now
#    ./setup-ninjatrader.sh stop            stop NT8
#    ./setup-ninjatrader.sh doctor          check every piece + ninjatrader configs
#    ./setup-ninjatrader.sh all             prefix install credentials addon start
#
#  Settings (prefix path, display, bridge port) live in
#  ~/.config/vulcantrader/ninjatrader.env - written on first use from these env
#  vars, read by every later step and by nt8-autostart.sh:
#    NT8_WINEPREFIX (default ~/.wine-ninjatrader)   NT8_DISPLAY (default :99)
#    BRIDGE_PORT (default 8755)                      NT8_CREDENTIALS
#  `system` caps the unit at MEM_MAX (default 3G) RAM + SWAP_MAX (default 2G)
#  swap: NT8 under Wine grows ~1 GB/h, so the kernel kills it at the cap and
#  the unit's supervisor loop relaunches it.
#
#  Why each fix exists: AGENTS.md "NinjaTrader 8 on Linux (Wine)".
#  Tested with winehq-stable 11.0, NinjaTrader 8.1.8.4, Ubuntu 24.04.
# ---------------------------------------------------------------------------
set -euo pipefail
cd "$(dirname "$0")"
REPO="$(pwd)"
ADDON_DIR="$REPO/VulcanTrader/exchange/ninjatrader_addon"
LINUX_DIR="$ADDON_DIR/linux"
AUTOSTART="$LINUX_DIR/nt8-autostart.sh"
UNIT_NAME="nt8-autostart.service"

log() { printf '\n[setup-ninjatrader] %s\n' "$1"; }
die() { printf '\n[setup-ninjatrader] ERROR: %s\n' "$1" >&2; exit 1; }

# ---------------------------------------------------------------- settings ---
target_user() { if [[ $EUID -eq 0 ]]; then echo "${SUDO_USER:-}"; else id -un; fi; }
target_home() { getent passwd "$(target_user)" | cut -d: -f6; }

SETTINGS_HOME="$(target_home)"
NT8_SETTINGS="${NT8_SETTINGS:-$SETTINGS_HOME/.config/vulcantrader/ninjatrader.env}"
# shellcheck disable=SC1090
[[ -f "$NT8_SETTINGS" ]] && source "$NT8_SETTINGS"
NT8_WINEPREFIX="${NT8_WINEPREFIX:-$SETTINGS_HOME/.wine-ninjatrader}"
NT8_DISPLAY="${NT8_DISPLAY:-:99}"
BRIDGE_PORT="${BRIDGE_PORT:-8755}"
NT8_CREDENTIALS="${NT8_CREDENTIALS:-$NT8_WINEPREFIX/nt8-credentials.env}"
export WINEPREFIX="$NT8_WINEPREFIX" WINEDEBUG="${WINEDEBUG:--all}"
NT8_BIN="$NT8_WINEPREFIX/drive_c/Program Files/NinjaTrader 8/bin"

save_settings() {
    [[ -f "$NT8_SETTINGS" ]] && return 0
    mkdir -p "$(dirname "$NT8_SETTINGS")"
    cat > "$NT8_SETTINGS" <<EOF
# VulcanTrader NinjaTrader-on-Linux settings (setup-ninjatrader.sh, nt8-autostart.sh)
NT8_WINEPREFIX="$NT8_WINEPREFIX"
NT8_DISPLAY="$NT8_DISPLAY"
BRIDGE_PORT="$BRIDGE_PORT"
NT8_CREDENTIALS="$NT8_CREDENTIALS"
EOF
    log "Saved settings to $NT8_SETTINGS"
}

require_user() {
    [[ $EUID -ne 0 ]] || die "Run '$1' as your normal user, not root (Wine refuses to run as root)."
    command -v wine >/dev/null || die "wine not installed - run: sudo ./setup-ninjatrader.sh system"
    save_settings
}

# A display for Wine/winetricks during setup: reuse NT8's Xvfb if up, else a
# throwaway one (setup never needs a desktop/VNC session).
SETUP_XVFB_PID=""
with_display() {
    local n="${NT8_DISPLAY#:}"
    if [[ -S "/tmp/.X11-unix/X$n" ]] && pgrep -f "Xvfb $NT8_DISPLAY( |$)" >/dev/null; then
        export DISPLAY="$NT8_DISPLAY"; return
    fi
    n=98; while [[ -S "/tmp/.X11-unix/X$n" ]]; do n=$((n - 1)); done
    Xvfb ":$n" -screen 0 1920x1080x24 >/dev/null 2>&1 & SETUP_XVFB_PID=$!
    for _ in $(seq 1 20); do [[ -S "/tmp/.X11-unix/X$n" ]] && break; sleep 0.5; done
    export DISPLAY=":$n"
    trap '[[ -n "$SETUP_XVFB_PID" ]] && kill "$SETUP_XVFB_PID" 2>/dev/null || true' EXIT
}

nt_docs() {
    # Wine maps C:\users\<user>\Documents to the Linux user's Documents folder.
    local d; d="$(winepath -u "C:\\users\\$(id -un)\\Documents" 2>/dev/null | tr -d '\r')"
    echo "${d%/}/NinjaTrader 8"
}

nt8_running() { pgrep -f 'NinjaTrader\.exe$' >/dev/null 2>&1; }
bridge_up() { ss -tln 2>/dev/null | grep ":${BRIDGE_PORT} " >/dev/null; }
unit_supervises() { systemctl cat "$UNIT_NAME" 2>/dev/null | grep -- '--supervise' >/dev/null && systemctl is-active --quiet "$UNIT_NAME"; }

# ------------------------------------------------------------------ system ---
step_system() {
    [[ $EUID -eq 0 ]] || die "'system' needs root: sudo ./setup-ninjatrader.sh system"
    local user home group
    user="$(target_user)"; [[ -n "$user" && "$user" != root ]] || die "Run via sudo from your normal user (SUDO_USER is empty)."
    home="$(target_home)"; group="$(id -gn "$user")"
    export DEBIAN_FRONTEND=noninteractive

    if ! dpkg -s winehq-stable >/dev/null 2>&1; then
        local codename; codename="$(. /etc/os-release && echo "$VERSION_CODENAME")"
        log "Installing WineHQ stable for $codename (+ i386 multiarch)..."
        dpkg --add-architecture i386
        mkdir -pm755 /etc/apt/keyrings
        wget -qO /etc/apt/keyrings/winehq-archive.key https://dl.winehq.org/wine-builds/winehq.key
        wget -qNP /etc/apt/sources.list.d/ "https://dl.winehq.org/wine-builds/ubuntu/dists/${codename}/winehq-${codename}.sources"
        apt-get update -y
        apt-get install -y --install-recommends winehq-stable
    else
        log "winehq-stable already installed ($(wine --version 2>/dev/null))."
    fi
    log "Installing helpers (winetricks deps, Xvfb, xfwm4, xdotool, imagemagick)..."
    apt-get install -y --no-install-recommends cabextract unzip p7zip-full wget xvfb xfwm4 xdotool imagemagick iproute2
    if [[ ! -x /usr/local/bin/winetricks ]]; then
        wget -qO /usr/local/bin/winetricks https://raw.githubusercontent.com/Winetricks/winetricks/master/src/winetricks
        chmod +x /usr/local/bin/winetricks
    fi

    local mem="${1:-${NT8_MEMORY_MAX:-3G}}" swap="${2:-${NT8_SWAP_MAX:-2G}}"
    log "Installing /etc/systemd/system/$UNIT_NAME (user $user, MemoryMax=$mem, MemorySwapMax=$swap)..."
    cat > "/etc/systemd/system/$UNIT_NAME" <<EOF
# Installed by $REPO/setup-ninjatrader.sh system - re-run that to change it.
[Unit]
Description=NinjaTrader 8 under Wine (headless) + VulcanTraderBridge, supervised
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=$user
Group=$group
Environment=HOME=$home
Environment=NT8_SETTINGS=$NT8_SETTINGS
ExecStart=$AUTOSTART --supervise
ExecStop=$AUTOSTART --stop
Restart=always
RestartSec=30
TimeoutStopSec=90
# NT8 under Wine leaks ~1 GB/h. The cap makes the kernel kill NT8 inside this
# unit instead of exhausting the server; the --supervise loop relaunches it.
MemoryMax=$mem
MemorySwapMax=$swap
OOMPolicy=continue

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
    systemctl enable "$UNIT_NAME"
    log "Done. Next, as $user: ./setup-ninjatrader.sh all   (then: sudo systemctl restart $UNIT_NAME)"
}

# ------------------------------------------------------------------ prefix ---
install_websocket_dll() {
    local sys32="$NT8_WINEPREFIX/drive_c/windows/system32" src="$LINUX_DIR/websocket/websocket.dll"
    if command -v x86_64-w64-mingw32-gcc >/dev/null; then
        local tmp; tmp="$(mktemp -d)"
        x86_64-w64-mingw32-gcc -shared -O2 -Wall -Wno-unused-parameter -static-libgcc -s \
            -o "$tmp/websocket.dll" "$LINUX_DIR/websocket/websocket.c" "$LINUX_DIR/websocket/websocket.def"
        src="$tmp/websocket.dll"
        log "Built websocket.dll from source with MinGW."
    else
        log "No MinGW - using the prebuilt websocket.dll shipped in the repo."
    fi
    [[ -f "$sys32/websocket.dll.wine-builtin" ]] || cp -p "$sys32/websocket.dll" "$sys32/websocket.dll.wine-builtin"
    cp "$src" "$sys32/websocket.dll"
    wine reg add 'HKCU\Software\Wine\DllOverrides' /v websocket /t REG_SZ /d native,builtin /f >/dev/null
}

step_prefix() {
    require_user prefix
    with_display
    if [[ ! -f "$NT8_WINEPREFIX/system.reg" ]]; then
        log "Creating 64-bit Wine prefix $NT8_WINEPREFIX ..."
        WINEARCH=win64 wineboot --init
        wineserver -w
    fi
    if [[ ! -f "$NT8_WINEPREFIX/dotnet48_installed" ]]; then
        log "Installing .NET Framework 4.8 via winetricks (10-20 min; NT8 needs real .NET, not wine-mono)..."
        winetricks -q win10
        winetricks -q dotnet48
        winetricks -q win10
        touch "$NT8_WINEPREFIX/dotnet48_installed"
        winetricks -q corefonts || log "corefonts had errors (cosmetic, continuing)."
    else
        log ".NET 4.8 already installed in the prefix."
    fi
    log "Forcing WPF software rendering (without it wined3d deadlocks when NT8's login window closes)..."
    wine reg add 'HKCU\Software\Microsoft\Avalon.Graphics' /v DisableHWAcceleration /t REG_DWORD /d 1 /f >/dev/null
    log "Installing the websocket.dll replacement (Wine's is a stub; NT8's Tradovate/Simulation connection needs it)..."
    install_websocket_dll
    wineserver -w
    log "Prefix ready."
}

# ----------------------------------------------------------------- install ---
step_install() {
    require_user install
    [[ -f "$NT8_WINEPREFIX/dotnet48_installed" ]] || die "Prefix not set up - run ./setup-ninjatrader.sh prefix first."
    if [[ -f "$NT8_BIN/NinjaTrader.exe" ]]; then
        log "NinjaTrader 8 already installed in $NT8_WINEPREFIX."
        return
    fi
    local msi="${NT8_INSTALLER:-}"
    [[ -n "$msi" ]] || msi="$(find "$REPO" "$HOME" -maxdepth 1 -iname 'NinjaTrader*.msi' 2>/dev/null | head -n1)"
    [[ -n "$msi" && -f "$msi" ]] || die "Set NT8_INSTALLER=/path/to/NinjaTrader.Install.msi (download it from your NinjaTrader account)."
    with_display
    log "Installing NinjaTrader 8 silently from $msi ..."
    wine msiexec /i "$(winepath -w "$msi")" /qn /norestart /l*v "$(winepath -w "$NT8_WINEPREFIX/nt8-install.log")"
    wineserver -w
    [[ -f "$NT8_BIN/NinjaTrader.exe" ]] || die "Install finished but NinjaTrader.exe is missing - see $NT8_WINEPREFIX/nt8-install.log"
    touch "$NT8_WINEPREFIX/ninjatrader_installed"
    log "NinjaTrader 8 installed."
}

# ------------------------------------------------------------- credentials ---
step_credentials() {
    require_user credentials
    local u="${NT_USERNAME:-}" p="${NT_PASSWORD:-}"
    if [[ -z "$u" || -z "$p" ]]; then
        if [[ -f "$NT8_CREDENTIALS" ]]; then log "Credentials file already present: $NT8_CREDENTIALS"; return; fi
        [[ -t 0 ]] || die "Set NT_USERNAME and NT_PASSWORD (or run interactively)."
        read -rp "NinjaTrader username: " u
        read -rsp "NinjaTrader password: " p; echo
    fi
    mkdir -p "$(dirname "$NT8_CREDENTIALS")"
    ( umask 077; printf 'NT_USERNAME=%q\nNT_PASSWORD=%q\n' "$u" "$p" > "$NT8_CREDENTIALS" )
    chmod 600 "$NT8_CREDENTIALS"
    log "Saved credentials to $NT8_CREDENTIALS (chmod 600)."
}

# ------------------------------------------------------------------- addon ---
# NT8 under Wine/Xvfb never recompiles NinjaScript by itself (not on restart,
# not on file change) and its NinjaScript Editor can't be opened headless, so
# NinjaTrader.Custom.dll is rebuilt here with NT8's own bundled Roslyn
# (linux/ntbuild.cs) and installed; NT8 loads it on its next start.
build_custom_dll() {
    local docs="$1" build="${XDG_CACHE_HOME:-$HOME/.cache}/vulcantrader/nt8-build"
    local fw='C:\windows\Microsoft.NET\Framework64\v4.0.30319'
    mkdir -p "$build"; cp "$LINUX_DIR/ntbuild.cs" "$build/"
    local f; for f in "$NT8_BIN"/*.dll; do
        case "$(basename "$f")" in System.*|Microsoft.CodeAnalysis*|Microsoft.Bcl.*) cp -u "$f" "$build/" ;; esac
    done
    # NT8's own assembly binding redirects (Roslyn 5 needs them to load).
    { echo '<?xml version="1.0" encoding="utf-8"?><configuration>'
      echo '<startup><supportedRuntime version="v4.0" sku=".NETFramework,Version=v4.8"/></startup>'
      sed -n '/<runtime>/,/<\/runtime>/p' "$NT8_BIN/NinjaTrader.exe.config"
      echo '</configuration>'; } > "$build/ntbuild.exe.config"
    # No DISPLAY: a .NET crash then prints to the console instead of hanging
    # on an invisible Wine "Program Error" dialog.
    ( cd "$build"; unset DISPLAY
      if [[ ! -f ntbuild.exe || ntbuild.cs -nt ntbuild.exe ]]; then
          wine "$fw\\csc.exe" /nologo /platform:x64 /nowarn:1701 /out:ntbuild.exe /r:Microsoft.CodeAnalysis.dll \
              /r:Microsoft.CodeAnalysis.CSharp.dll /r:System.Collections.Immutable.dll /r:"$fw\\System.Runtime.dll" \
              /r:"$fw\\netstandard.dll" /r:"$fw\\System.Reflection.dll" /r:"$fw\\System.Text.Encoding.dll" ntbuild.cs
      fi
      rm -rf out
      wine ntbuild.exe build "$(winepath -w "$docs/bin/Custom")" "$(winepath -w "$NT8_BIN")" \
          "$(winepath -w "$docs/bin/Custom/NinjaTrader.Custom.dll")" "$(winepath -w "$build/out")" \
          | tr -d '\r' | grep -v '^\s*$' | tee build.log
      grep -q '^BUILD OK' build.log ) || die "NinjaScript build failed - see $build/build.log"
    local bk="${XDG_DATA_HOME:-$HOME/.local/share}/vulcantrader/nt8-custom-backups/$(date +%Y%m%d-%H%M%S)"
    mkdir -p "$bk"
    cp -p "$docs/bin/Custom"/NinjaTrader.Custom.{dll,pdb,xml} "$bk/" 2>/dev/null || true
    cp -p "$build/out"/NinjaTrader.Custom.{dll,pdb,xml} "$docs/bin/Custom/"
    log "Installed the rebuilt NinjaTrader.Custom.dll (previous build backed up to $bk)."
}

step_addon() {
    require_user addon
    [[ -f "$NT8_BIN/NinjaTrader.exe" ]] || die "NinjaTrader not installed - run ./setup-ninjatrader.sh install first."
    local docs; docs="$(nt_docs)"
    if [[ ! -f "$docs/bin/Custom/NinjaTrader.Custom.dll" ]]; then
        log "First launch of NinjaTrader (creates $docs and its NinjaScript sources)..."
        NT8_REQUIRE_BRIDGE=0 NT8_SETTINGS="$NT8_SETTINGS" "$AUTOSTART" || die "First NinjaTrader launch failed."
        for _ in $(seq 1 60); do [[ -f "$docs/bin/Custom/NinjaTrader.Custom.dll" ]] && break; sleep 2; done
        [[ -f "$docs/bin/Custom/NinjaTrader.Custom.dll" ]] || die "NinjaTrader never created $docs/bin/Custom."
    fi
    local was_running=0; nt8_running && was_running=1
    mkdir -p "$docs/bin/Custom/AddOns"
    cp "$ADDON_DIR/VulcanTraderBridge.cs" "$docs/bin/Custom/AddOns/VulcanTraderBridge.cs"
    log "Deployed VulcanTraderBridge.cs; compiling NinjaTrader.Custom.dll with NT8's Roslyn..."
    build_custom_dll "$docs"
    if (( was_running )); then
        "$AUTOSTART" --stop
        if [[ "${NT8_ADDON_NO_RESTART:-0}" == 1 ]]; then
            log "NinjaTrader stopped; it loads the new build on its next start."
        elif unit_supervises; then
            log "Restarting NinjaTrader to load the new build - the $UNIT_NAME supervisor relaunches it within ~30 s."
        else
            log "Restarting NinjaTrader to load the new build..."
            "$AUTOSTART" || die "NinjaTrader did not come back up."
        fi
    fi
}

# The compiled bridge is current when the deployed .cs is the repo's copy and
# NinjaTrader.Custom.dll was built after it was deployed.
addon_current() {
    local docs; docs="$(nt_docs 2>/dev/null || true)"
    [[ -n "$docs" ]] \
        && cmp -s "$ADDON_DIR/VulcanTraderBridge.cs" "$docs/bin/Custom/AddOns/VulcanTraderBridge.cs" \
        && [[ "$docs/bin/Custom/NinjaTrader.Custom.dll" -nt "$docs/bin/Custom/AddOns/VulcanTraderBridge.cs" ]]
}

# What install.sh runs when NinjaTrader is already set up: recompile the bridge
# only if the repo copy changed (so a plain re-install never restarts NT8).
step_addon_if_needed() {
    [[ -f "$NT8_BIN/NinjaTrader.exe" ]] || { log "NinjaTrader 8 not installed - no bridge to compile."; return 0; }
    require_user addon-if-needed
    if addon_current; then
        log "VulcanTraderBridge is already compiled from the current repo copy."
        return 0
    fi
    log "VulcanTraderBridge in the repo changed (or was never compiled) - rebuilding it."
    step_addon
}

# ------------------------------------------------------------------ guided ---
# The opt-in path from install.sh: every step, asking only for what can't be
# known (sudo password, the NT8 .msi, the NT login). NT8's first real start
# happens inside the supervised, memory-capped boot unit.
step_guided() {
    [[ $EUID -ne 0 ]] || die "Run the guided setup as your normal user - it asks for sudo when it needs it."
    log "Guided NinjaTrader 8 setup: system (sudo) -> prefix -> install -> credentials -> addon -> start -> doctor"
    if ! command -v wine >/dev/null || ! command -v winetricks >/dev/null \
        || ! systemctl cat "$UNIT_NAME" 2>/dev/null | grep -- '--supervise' >/dev/null; then
        log "System packages and the boot unit need root - running: sudo $0 system"
        sudo "$0" system
    fi
    step_prefix
    if [[ ! -f "$NT8_BIN/NinjaTrader.exe" && -z "${NT8_INSTALLER:-}" ]]; then
        NT8_INSTALLER="$(find "$REPO" "$HOME" "$HOME/Downloads" -maxdepth 1 -iname 'NinjaTrader*.msi' 2>/dev/null | head -n1 || true)"
        if [[ -z "$NT8_INSTALLER" && -t 0 ]]; then
            read -rp "Path to the NinjaTrader 8 installer (.msi from your NinjaTrader account): " NT8_INSTALLER
        fi
        export NT8_INSTALLER
    fi
    step_install
    step_credentials
    NT8_ADDON_NO_RESTART=1 step_addon
    "$AUTOSTART" --stop >/dev/null 2>&1 || true   # so the unit starts NT8 inside its own cgroup
    log "Starting NinjaTrader under $UNIT_NAME (supervised, memory-capped)..."
    sudo systemctl restart "$UNIT_NAME"
    local waited=0
    until bridge_up; do
        (( waited >= 900 )) && { log "Bridge not up after 15 min - check: journalctl -u $UNIT_NAME"; break; }
        sleep 5; waited=$((waited + 5))
    done
    bridge_up && log "NinjaTrader is up and the bridge is listening on :$BRIDGE_PORT."
    ask_auto_start
    step_doctor || log "doctor reported problems - see above."
}

# system.json "ninjatrader": {"auto_start": true} makes VulcanTrader itself start
# NT8 whenever the fleet starts (util/ninjatrader_launcher.py) - a no-op while
# the supervised boot unit owns NT8, and the way NT8 comes up without it.
LAUNCHER_PY="$REPO/VulcanTrader/util/ninjatrader_launcher.py"
repo_python() { if [[ -x "$REPO/.venv/bin/python" ]]; then echo "$REPO/.venv/bin/python"; else echo python3; fi; }
ask_auto_start() {
    local ans="${NT8_AUTO_START:-}"
    if [[ -z "$ans" && -t 0 ]]; then
        read -rp "Have VulcanTrader start NinjaTrader whenever the fleet starts (system.json ninjatrader.auto_start)? [Y/n] " ans || ans=""
    fi
    case "$(printf '%s' "$ans" | tr '[:upper:]' '[:lower:]')" in
        n|no|0|false|off) "$(repo_python)" -I "$LAUNCHER_PY" --auto-start off ;;
        *) "$(repo_python)" -I "$LAUNCHER_PY" --auto-start on ;;
    esac
}

# ------------------------------------------------------------- start / stop ---
step_start() {
    require_user start
    if unit_supervises; then
        log "$UNIT_NAME is active and supervising NinjaTrader (status: systemctl status $UNIT_NAME)."
        return
    fi
    "$AUTOSTART"
}

step_stop() {
    require_user stop
    unit_supervises && log "Note: $UNIT_NAME is supervising and will relaunch NT8 - use: sudo systemctl stop $UNIT_NAME"
    "$AUTOSTART" --stop
}

# ------------------------------------------------------------------ doctor ---
step_doctor() {
    local bad=0
    ok()   { printf '  \033[32mOK\033[0m   %s\n' "$1"; }
    fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; bad=$((bad + 1)); }
    chk()  { if eval "$2" >/dev/null 2>&1; then ok "$1"; else fail "$1${3:+ - $3}"; fi; }
    echo "[setup-ninjatrader] doctor (settings: $NT8_SETTINGS, prefix: $NT8_WINEPREFIX)"
    chk "wine installed ($(wine --version 2>/dev/null))" 'command -v wine' "sudo ./setup-ninjatrader.sh system"
    chk "winetricks, Xvfb, xfwm4, xdotool installed" 'command -v winetricks && command -v Xvfb && command -v xfwm4 && command -v xdotool'
    chk "Wine prefix with .NET 4.8" '[[ -f "$NT8_WINEPREFIX/dotnet48_installed" ]]' "./setup-ninjatrader.sh prefix"
    chk "WPF software rendering (DisableHWAcceleration=1)" 'grep -q "\"DisableHWAcceleration\"=dword:00000001" "$NT8_WINEPREFIX/user.reg"' "./setup-ninjatrader.sh prefix"
    chk "websocket.dll replacement installed + DllOverride" 'cmp -s "$LINUX_DIR/websocket/websocket.dll" "$NT8_WINEPREFIX/drive_c/windows/system32/websocket.dll" && grep -q "\"websocket\"=\"native,builtin\"" "$NT8_WINEPREFIX/user.reg"' "./setup-ninjatrader.sh prefix"
    chk "NinjaTrader 8 installed" '[[ -f "$NT8_BIN/NinjaTrader.exe" ]]' "./setup-ninjatrader.sh install"
    chk "credentials file present, mode 600" '[[ "$(stat -c %a "$NT8_CREDENTIALS")" == 600 ]]' "./setup-ninjatrader.sh credentials"
    local docs; docs="$(nt_docs 2>/dev/null || true)"
    chk "deployed AddOn matches the repo copy" 'cmp -s "$ADDON_DIR/VulcanTraderBridge.cs" "$docs/bin/Custom/AddOns/VulcanTraderBridge.cs"' "./setup-ninjatrader.sh addon"
    chk "compiled NinjaTrader.Custom.dll contains the current bridge" 'strings "$docs/bin/Custom/NinjaTrader.Custom.dll" | grep -x TimeframeMs >/dev/null && [[ "$docs/bin/Custom/NinjaTrader.Custom.dll" -nt "$docs/bin/Custom/AddOns/VulcanTraderBridge.cs" ]]' "./setup-ninjatrader.sh addon"
    chk "boot unit $UNIT_NAME supervised + memory-capped" 'systemctl cat "$UNIT_NAME" | grep -- --supervise >/dev/null && systemctl cat "$UNIT_NAME" | grep "^MemoryMax=" >/dev/null' "sudo ./setup-ninjatrader.sh system"
    chk "NinjaTrader running" 'nt8_running' "./setup-ninjatrader.sh start"
    chk "VulcanTraderBridge listening on :$BRIDGE_PORT" 'bridge_up'
    local lastconn; lastconn="$(cat "$docs"/log/log.*.txt 2>/dev/null | grep -a 'Primary connection=' | tail -n1 | cut -c1-19 || true)"
    chk "last NT8 connection event is Connected (${lastconn:-none})" 'cat "$docs"/log/log.*.txt | grep -a "Primary connection=" | tail -n1 | grep -q "Primary connection=Connected"'
    printf '  info system.json ninjatrader.auto_start = %s (VulcanTrader starts NT8 with the fleet; toggle: python -m VulcanTrader.bot ninjatrader auto-start on|off)\n' \
        "$(python3 -c 'import json,sys; b=json.load(open(sys.argv[1])).get("ninjatrader") or {}; print(str(b.get("auto_start") is True).lower())' \
            "$REPO/user_data/configs_system/system.json" 2>/dev/null || echo unset)"
    echo "  -- VulcanTrader configs on the ninjatrader exchange:"
    python3 - "$REPO/user_data/configs" <<'EOF' || bad=$((bad + 1))
import glob, json, re, sys
bad = 0
for f in sorted(glob.glob(sys.argv[1] + "/*.json")):
    try:
        d = json.load(open(f, encoding="utf-8"))
    except Exception:
        continue
    ex = d.get("exchange") if isinstance(d, dict) else None
    if not isinstance(ex, dict) or ex.get("name") != "ninjatrader":
        continue
    name, problems = f.rsplit("/", 1)[1], []
    if not ex.get("ninjatrader_account"):
        problems.append("no exchange.ninjatrader_account (e.g. \"Sim101\")")
    loose = [p for p in ex.get("pair_whitelist", []) if not re.fullmatch(r"[A-Z0-9]+-\d\d-\d\d", p)]
    if loose:
        problems.append("pairs without a contract month (want ROOT-MM-YY, e.g. HG-12-26): " + ", ".join(loose))
    if problems:
        bad += 1
        print("  \033[31mFAIL\033[0m " + name + ": " + "; ".join(problems))
print("  %s  %d config(s) with problems" % ("\033[32mOK\033[0m  " if not bad else "\033[31mFAIL\033[0m", bad))
sys.exit(1 if bad else 0)
EOF
    echo; if (( bad )); then echo "[setup-ninjatrader] $bad problem(s)."; return 1; fi
    echo "[setup-ninjatrader] all good."
}

# -------------------------------------------------------------------- main ---
case "${1:-}" in
    guided) step_guided ;;
    system) step_system "${2:-}" "${3:-}" ;;
    prefix) step_prefix ;;
    install) step_install ;;
    credentials) step_credentials ;;
    addon) step_addon ;;
    addon-if-needed) step_addon_if_needed ;;
    installed) [[ -f "$NT8_BIN/NinjaTrader.exe" ]] ;;   # exit 0 if NT8 is installed (used by install.sh)
    start) step_start ;;
    stop) step_stop ;;
    doctor) step_doctor ;;
    all) step_prefix; step_install; step_credentials; step_addon; step_start; step_doctor ;;
    *) awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"; exit 2 ;;
esac
