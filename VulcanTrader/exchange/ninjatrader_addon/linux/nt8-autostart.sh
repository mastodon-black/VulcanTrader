#!/usr/bin/env bash
#
# nt8-autostart.sh - bring NinjaTrader 8 up headless under Wine for VulcanTrader.
#
# Part of the VulcanTrader NinjaTrader-on-Linux setup (see setup-ninjatrader.sh
# at the repo root and AGENTS.md "NinjaTrader 8 on Linux"). Starts a private
# Xvfb display + xfwm4 (NT8's WPF windows need _NET_ACTIVE_WINDOW support,
# which bare Xvfb lacks), launches NT8, logs in via xdotool (NT8 has no
# headless/API login), clicks "Start Simulated Trading" on the trading-mode
# picker, and waits for the VulcanTraderBridge AddOn's TCP listener.
#
#   nt8-autostart.sh              one-shot: bring NT8 up (no-op if healthy), exit
#   nt8-autostart.sh --supervise  keep it up: start it, then health-check every
#                                 HEALTH_INTERVAL s and relaunch when NT8 dies or
#                                 the bridge stops listening (what the systemd
#                                 unit installed by `setup-ninjatrader.sh system`
#                                 runs; the unit's MemoryMax caps NT8's growth and
#                                 this loop restarts it after an OOM kill)
#   nt8-autostart.sh --stop       stop NinjaTrader (wineserver -k + wait)
#
# Settings come from $NT8_SETTINGS (default ~/.config/vulcantrader/ninjatrader.env,
# written by setup-ninjatrader.sh); every NT8_* variable below can also be set in
# the environment. Credentials come from $NT8_CREDENTIALS (chmod 600 file setting
# NT_USERNAME / NT_PASSWORD) and are never logged.
#
# Lessons baked in (all seen live, 2026-11):
#   - Wine/WPF can create and destroy the "Welcome" HWND during startup: only act
#     on a visible window id that survives a settle delay.
#   - The "Select a Trading Mode" picker is the same "Welcome" window resized
#     from 378 to 902 px wide, not a separately titled window.
#   - After `wineserver -k`, wait for the server to exit (`wineserver -w`) or the
#     next launch attaches to the dying server and NT8 exits at once.
#   - `pgrep -f 'NinjaTrader\.exe'` matches any shell whose argv mentions it;
#     the pattern is anchored to the end of the command line.

set -uo pipefail

NT8_SETTINGS="${NT8_SETTINGS:-$HOME/.config/vulcantrader/ninjatrader.env}"
# shellcheck disable=SC1090
[[ -f "$NT8_SETTINGS" ]] && source "$NT8_SETTINGS"

WINEPREFIX="${NT8_WINEPREFIX:-$HOME/.wine-ninjatrader}"
DISPLAY_NUM="${NT8_DISPLAY:-:99}"
NT8_EXE="${WINEPREFIX}/drive_c/Program Files/NinjaTrader 8/bin/NinjaTrader.exe"
CREDS_FILE="${NT8_CREDENTIALS:-${WINEPREFIX}/nt8-credentials.env}"
LOG_DIR="${NT8_LOG_DIR:-/tmp}"
DISP_ID="${DISPLAY_NUM#:}"
WINE_LOG="${LOG_DIR}/ninjatrader-wine-xvfb${DISP_ID}.log"
SCREENSHOT_DIR="${LOG_DIR}/nt8-autostart-shots"
LOGIN_TIMEOUT="${LOGIN_TIMEOUT:-120}"
BRIDGE_PORT="${BRIDGE_PORT:-8755}"
BRIDGE_TIMEOUT="${BRIDGE_TIMEOUT:-180}"
MAX_ATTEMPTS="${MAX_ATTEMPTS:-3}"
HEALTH_INTERVAL="${HEALTH_INTERVAL:-30}"
# NT8_REQUIRE_BRIDGE=0: success once logged in + Simulation picked (used for
# NT8's very first launch, before the AddOn has been compiled).
NT8_REQUIRE_BRIDGE="${NT8_REQUIRE_BRIDGE:-1}"
# Memory preflight. NT8 is ~1.4 GB RSS once connected with data.
NT8_EXPECTED_MB="${NT8_EXPECTED_MB:-1500}"     # warn if MemAvailable below this
NT8_MIN_TOTAL_MB="${NT8_MIN_TOTAL_MB:-2000}"   # refuse if MemAvailable+SwapFree below this
MAX_MEM_PSI_FULL="${MAX_MEM_PSI_FULL:-10}"     # refuse if memory 'full' PSI avg60 (%) above this
LOGIN_DIALOG_WIDTH=378
MODE_PICKER_WIDTH=902

log() { printf '[nt8-autostart] %s\n' "$1"; }
die() { printf '[nt8-autostart] ERROR: %s\n' "$1" >&2; exit 1; }
xdo() { DISPLAY="$DISPLAY_NUM" timeout 10 xdotool "$@"; }

load_prereqs() {
    [[ -x "$(command -v wine)" ]] || die "wine not found - run: sudo ./setup-ninjatrader.sh system"
    [[ -f "$NT8_EXE" ]] || die "NinjaTrader.exe not found under ${WINEPREFIX} - run ./setup-ninjatrader.sh install"
    [[ -f "$CREDS_FILE" ]] || die "Credentials file not found: ${CREDS_FILE} - run ./setup-ninjatrader.sh credentials"
    # shellcheck disable=SC1090
    source "$CREDS_FILE"
    [[ -n "${NT_USERNAME:-}" && -n "${NT_PASSWORD:-}" ]] \
        || die "${CREDS_FILE} must set NT_USERNAME and NT_PASSWORD."
}

nt8_running() { pgrep -f 'NinjaTrader\.exe$' >/dev/null 2>&1; }
bridge_up() { ss -tln 2>/dev/null | grep ":${BRIDGE_PORT} " >/dev/null; }

x_socket_live() {
    # Process table + socket file only - never an X round trip (xdotool/xdpyinfo
    # can hang forever against a wedged server instead of failing fast).
    pgrep -af "Xvfb ${DISPLAY_NUM}( |$)" >/dev/null 2>&1 && [[ -S "/tmp/.X11-unix/X${DISP_ID}" ]]
}

check_memory() {
    local avail swapfree psi_full
    avail=$(awk '/^MemAvailable:/{print int($2/1024)}' /proc/meminfo)
    swapfree=$(awk '/^SwapFree:/{print int($2/1024)}' /proc/meminfo)
    psi_full=$(awk '/^full/{split($3,a,"="); print int(a[2])}' /proc/pressure/memory 2>/dev/null || echo 0)
    log "Memory preflight: ${avail} MB available, ${swapfree} MB swap free, PSI full avg60=${psi_full}% (NT8 needs ~${NT8_EXPECTED_MB} MB)."
    if (( avail + swapfree < NT8_MIN_TOTAL_MB )); then
        log "Refusing to launch: available RAM + free swap (${avail}+${swapfree} MB) is below ${NT8_MIN_TOTAL_MB} MB - OOM risk."
        return 1
    fi
    if (( psi_full > MAX_MEM_PSI_FULL )); then
        log "Refusing to launch: system is already under heavy memory pressure (PSI full ${psi_full}% > ${MAX_MEM_PSI_FULL}%)."
        return 1
    fi
    if (( avail < NT8_EXPECTED_MB )); then
        log "WARNING: only ${avail} MB RAM available; NT8 will push ~$((NT8_EXPECTED_MB - avail)) MB of other processes into swap."
    fi
    return 0
}

win_width() {
    xdo getwindowgeometry "$1" 2>/dev/null | awk '/Geometry:/{split($2,a,"x"); print a[1]}'
}

screenshot() {
    command -v import >/dev/null 2>&1 || return 0
    mkdir -p "$SCREENSHOT_DIR"
    local out
    out="${SCREENSHOT_DIR}/$(date +%Y%m%d-%H%M%S)-$1.png"
    DISPLAY="$DISPLAY_NUM" timeout 10 import -window root "$out" 2>/dev/null \
        && log "Screenshot: ${out}"
}

ensure_xvfb() {
    if x_socket_live; then
        log "Xvfb ${DISPLAY_NUM} already up."
        return 0
    fi
    log "Starting Xvfb ${DISPLAY_NUM}..."
    rm -f "/tmp/.X${DISP_ID}-lock" "/tmp/.X11-unix/X${DISP_ID}"   # stale after an unclean shutdown
    Xvfb "$DISPLAY_NUM" -screen 0 1920x1080x24 > "${LOG_DIR}/xvfb${DISP_ID}.log" 2>&1 &
    disown
    for _ in $(seq 1 20); do
        x_socket_live && return 0
        sleep 1
    done
    log "Xvfb ${DISPLAY_NUM} did not come up."
    return 1
}

ensure_wm() {
    local pidfile="${LOG_DIR}/xfwm4-${DISP_ID}.pid"
    if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
        log "Window manager already running, skipping."
        return 0
    fi
    log "Starting window manager on ${DISPLAY_NUM}..."
    DISPLAY="$DISPLAY_NUM" xfwm4 --compositor=off > "${LOG_DIR}/xfwm4-${DISP_ID}.log" 2>&1 &
    echo $! > "$pidfile"
    disown
    sleep 2
}

kill_nt8() {
    log "Stopping NinjaTrader / Wine prefix..."
    WINEPREFIX="$WINEPREFIX" timeout 20 wineserver -k 2>/dev/null || true
    pkill -9 -f 'NinjaTrader\.exe$' 2>/dev/null || true
    WINEPREFIX="$WINEPREFIX" timeout 30 wineserver -w 2>/dev/null || true
    for _ in $(seq 1 15); do
        nt8_running || return 0
        sleep 1
    done
}

launch_nt8() {
    log "Launching NinjaTrader 8 under Wine..."
    WINEPREFIX="$WINEPREFIX" DISPLAY="$DISPLAY_NUM" wine "$NT8_EXE" > "$WINE_LOG" 2>&1 &
    disown
}

find_window() {
    xdo search --onlyvisible --name "$1" 2>/dev/null | tail -n1 || true
}

wait_for_stable_window() {
    # Up to $2 s for a visible window titled $1 whose id survives a settle delay.
    local name="$1" wtimeout="$2" waited=0 winid again
    while (( waited < wtimeout )); do
        winid="$(find_window "$name")"
        if [[ -n "$winid" ]]; then
            sleep 3
            again="$(find_window "$name")"
            if [[ "$again" == "$winid" ]] && xdo getwindowgeometry "$winid" >/dev/null 2>&1; then
                echo "$winid"
                return 0
            fi
            waited=$((waited + 3))
            continue
        fi
        nt8_running || return 1
        x_socket_live || return 1
        sleep 2
        waited=$((waited + 2))
    done
    return 1
}

click_at() {
    # click_at <winid> <x> <y>  (window-relative)
    xdo mousemove --window "$1" "$2" "$3" && xdo click --window "$1" 1
}

do_login() {
    local winid
    winid="$(wait_for_stable_window "Welcome" "$LOGIN_TIMEOUT")" || {
        log "Login window never appeared (or NT8 died) within ${LOGIN_TIMEOUT}s."
        return 1
    }
    log "Logging in (window ${winid})..."
    xdo windowactivate "$winid" >/dev/null 2>&1 || true
    sleep 1
    # Window-relative coordinates, calibrated against the 378x550 "Welcome" dialog.
    click_at "$winid" 189 130 \
        && xdo key --window "$winid" ctrl+a \
        && xdo type --window "$winid" --delay 80 -- "$NT_USERNAME" \
        && click_at "$winid" 150 200 \
        && xdo key --window "$winid" ctrl+a \
        && xdo type --window "$winid" --delay 80 -- "$NT_PASSWORD" \
        && click_at "$winid" 189 283 \
        || { log "xdotool failed while typing into the login window."; return 1; }
    log "Credentials submitted."

    local waited=0 width
    while (( waited < 90 )); do
        width="$(win_width "$winid")"
        if [[ -z "$width" ]]; then
            log "Login accepted (Welcome closed)."
            return 0
        fi
        if [[ "$width" != "$LOGIN_DIALOG_WIDTH" ]] \
            || [[ -n "$(find_window "Control Center")" ]] \
            || bridge_up; then
            log "Login accepted (window now ${width} wide)."
            return 0
        fi
        nt8_running || { log "NT8 exited during login."; return 1; }
        sleep 2
        waited=$((waited + 2))
    done
    log "No post-login window 90s after submitting credentials."
    screenshot login-stuck
    return 1
}

maybe_pick_sim_mode() {
    # The picker is the "Welcome" window resized to 902x502; "Start Simulated
    # Trading" button centre is window-relative (669, 436).
    local winid="" try waited=0
    while (( waited < 30 )); do
        bridge_up && return 0
        winid="$(find_window "Welcome")"
        [[ -n "$winid" && "$(win_width "$winid")" == "$MODE_PICKER_WIDTH" ]] && break
        winid=""
        sleep 2
        waited=$((waited + 2))
    done
    [[ -n "$winid" ]] || { log "No trading-mode picker shown; continuing."; return 0; }
    for try in 1 2 3; do
        log "Selecting Simulation mode (click ${try})..."
        xdo windowactivate "$winid" >/dev/null 2>&1 || true
        sleep 1
        click_at "$winid" 669 436 || log "Click on trading-mode dialog failed."
        sleep 5
        [[ "$(win_width "$winid")" != "$MODE_PICKER_WIDTH" ]] && return 0
        bridge_up && return 0
    done
    log "Trading-mode dialog still open after 3 clicks."
    screenshot trading-mode-stuck
    return 1
}

wait_for_bridge() {
    if [[ "$NT8_REQUIRE_BRIDGE" == 0 ]]; then
        log "NT8_REQUIRE_BRIDGE=0 - not waiting for the bridge."
        return 0
    fi
    local waited=0
    log "Waiting for VulcanTraderBridge on 127.0.0.1:${BRIDGE_PORT}..."
    while (( waited < BRIDGE_TIMEOUT )); do
        bridge_up && { log "Bridge is up."; return 0; }
        nt8_running || { log "NinjaTrader.exe exited before the bridge came up."; return 1; }
        sleep 2
        waited=$((waited + 2))
    done
    log "Bridge did not come up on port ${BRIDGE_PORT} within ${BRIDGE_TIMEOUT}s."
    screenshot no-bridge
    return 1
}

bring_up() {
    # Launch + log in + pick Simulation + wait for the bridge, with retries.
    ensure_xvfb || return 1
    ensure_wm
    local attempt
    for attempt in $(seq 1 "$MAX_ATTEMPTS"); do
        log "Attempt ${attempt}/${MAX_ATTEMPTS}"
        check_memory || return 1
        launch_nt8
        if do_login && maybe_pick_sim_mode && wait_for_bridge; then
            log "Done. NinjaTrader 8 is up$([[ "$NT8_REQUIRE_BRIDGE" == 0 ]] || echo ' and the bridge is listening')."
            return 0
        fi
        screenshot "attempt${attempt}-failed"
        kill_nt8
        sleep 5
    done
    log "NinjaTrader failed to come up after ${MAX_ATTEMPTS} attempts. Check ${WINE_LOG} and ${SCREENSHOT_DIR}."
    return 1
}

supervise() {
    log "Supervising NinjaTrader (prefix ${WINEPREFIX}, display ${DISPLAY_NUM}, bridge :${BRIDGE_PORT}, check every ${HEALTH_INTERVAL}s)."
    local fails=0 backoff
    while true; do
        if nt8_running && { [[ "$NT8_REQUIRE_BRIDGE" == 0 ]] || bridge_up; }; then
            fails=0
            sleep "$HEALTH_INTERVAL"
            continue
        fi
        if nt8_running; then
            # Running but no listener: it may still be starting - give it the
            # normal bridge timeout before declaring it broken.
            wait_for_bridge && continue
            log "NinjaTrader is running without the bridge - restarting it."
            kill_nt8
        else
            log "NinjaTrader is not running - starting it."
        fi
        if bring_up; then
            fails=0
        else
            fails=$((fails + 1))
            backoff=$(( fails * 60 > 900 ? 900 : fails * 60 ))
            log "Start failed (${fails} in a row); retrying in ${backoff}s."
            sleep "$backoff"
        fi
    done
}

main() {
    case "${1:-}" in
        --stop) kill_nt8; exit 0 ;;
        --supervise) load_prereqs; supervise ;;
    esac
    load_prereqs
    if nt8_running; then
        log "NinjaTrader.exe already running; skipping launch/login."
        ensure_xvfb >/dev/null || true
        wait_for_bridge && { log "Done. NinjaTrader 8 is up and the bridge is listening."; exit 0; }
        die "Running instance has no bridge listener. Stop it (WINEPREFIX=${WINEPREFIX} wineserver -k) and re-run."
    fi
    bring_up || die "NinjaTrader did not come up."
}

main "$@"
