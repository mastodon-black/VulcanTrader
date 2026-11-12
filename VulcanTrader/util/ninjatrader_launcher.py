"""Start NinjaTrader 8 when VulcanTrader starts.

Setting: ``user_data/configs_system/system.json`` ->
``"ninjatrader": {"auto_start": true}``. When it is on, ``bot.py webserver``
(and ``bot.py autostart``) call :func:`start_for_fleet` just before the
auto_start bots launch, so ``ninjatrader``-exchange bots find the
VulcanTraderBridge AddOn listening. They retry the bridge connection every few
seconds, so nothing waits for NT8 to finish logging in.

What "start" means per platform:

* **Linux (NT8 under Wine):** runs the repo's headless launcher,
  ``exchange/ninjatrader_addon/linux/nt8-autostart.sh`` (Xvfb + login +
  Simulation pick; set up by ``setup-ninjatrader.sh``). It runs as a
  transient, memory-capped, one-shot ``systemd-run --user`` unit
  (:data:`USER_UNIT`), so NT8 is not a child of VulcanTrader and survives a
  webserver restart. Without a user systemd manager it falls back to a
  detached process (new session, so tmux closing doesn't take it down).
* **Windows (native NT8):** opens ``NinjaTrader.exe`` if it isn't running.
  NT8 on Windows has no automated login, so you log in yourself.
* Anything else: logged as unsupported.

It never starts a second NT8 and never fights another manager. It is a no-op
when:

* the bridge host isn't this machine (NT8 runs elsewhere),
* the bridge is already listening,
* the supervised boot unit (:data:`BOOT_UNIT`, installed by
  ``sudo ./setup-ninjatrader.sh system``) is active and owns NT8, or
* an NT8 process is already running (still logging in).

Every entry point logs and returns a status string instead of raising, so a
broken NT8 setup never stops the fleet from starting.

CLI: ``python -m VulcanTrader.bot ninjatrader [status|start|stop]``.
Self-test: ``python -m VulcanTrader.util.ninjatrader_launcher --self-test``.
"""

from __future__ import annotations

import logging
import os
import shutil
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

SYSTEM_KEY = "ninjatrader"
DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 8755
DEFAULT_MEMORY_MAX = "3G"
DEFAULT_MEMORY_SWAP_MAX = "2G"
USER_UNIT = "vulcantrader-ninjatrader"
BOOT_UNIT = "nt8-autostart.service"
REPO_ROOT = Path(__file__).resolve().parents[2]
LINUX_LAUNCHER = REPO_ROOT / "VulcanTrader" / "exchange" / "ninjatrader_addon" / "linux" / "nt8-autostart.sh"
_LOCAL_HOSTS = {"127.0.0.1", "localhost", "::1", "0.0.0.0", ""}
# How long the background watcher waits for the bridge after a launch before
# logging a warning (Wine NT8 login + Simulation connect is ~2-4 min).
BRIDGE_WAIT_S = 600


# ------------------------------------------------------------------ settings --

def settings(system_cfg: dict[str, Any] | None) -> dict[str, Any]:
    """The ``"ninjatrader"`` block of system.json (``{}`` if absent/invalid)."""
    block = (system_cfg or {}).get(SYSTEM_KEY)
    return block if isinstance(block, dict) else {}


def auto_start_enabled(system_cfg: dict[str, Any] | None) -> bool:
    return settings(system_cfg).get("auto_start") is True


def bridge_endpoint(system_cfg: dict[str, Any] | None) -> tuple[str, int]:
    s = settings(system_cfg)
    host = str(s.get("host") or DEFAULT_HOST).strip()
    try:
        port = int(s.get("port") or DEFAULT_PORT)
    except (TypeError, ValueError):
        port = DEFAULT_PORT
    return host, port


def is_local_host(host: str) -> bool:
    h = host.strip().lower()
    if h in _LOCAL_HOSTS or h.startswith("127."):
        return True
    return h in {socket.gethostname().lower(), socket.getfqdn().lower()}


# ------------------------------------------------------------------- probes --

def bridge_listening(host: str, port: int, timeout: float = 1.0) -> bool:
    """True if something accepts TCP connections on host:port (the AddOn)."""
    target = DEFAULT_HOST if host.strip() in ("", "0.0.0.0") else host
    try:
        with socket.create_connection((target, port), timeout=timeout):
            return True
    except OSError:
        return False


def _run(cmd: list[str], timeout: float = 15) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, check=False)


def nt8_process_running() -> bool:
    try:
        if sys.platform.startswith("win"):
            out = _run(["tasklist", "/FI", "IMAGENAME eq NinjaTrader.exe", "/NH"]).stdout
            return "NinjaTrader.exe" in out
        # Anchored: an unanchored pattern also matches any shell whose own
        # command line merely mentions NinjaTrader.exe.
        return _run(["pgrep", "-f", r"NinjaTrader\.exe$"]).returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


def _systemctl(*args: str, user: bool = False) -> subprocess.CompletedProcess | None:
    if not shutil.which("systemctl"):
        return None
    try:
        return _run(["systemctl", *(["--user"] if user else []), *args])
    except (OSError, subprocess.SubprocessError):
        return None


def boot_unit_supervising() -> bool:
    """The supervised, memory-capped boot unit from setup-ninjatrader.sh is
    installed and active - it owns NT8 (and restarts it) on this machine."""
    cat = _systemctl("cat", BOOT_UNIT)
    if cat is None or cat.returncode != 0 or "--supervise" not in cat.stdout:
        return False
    active = _systemctl("is-active", BOOT_UNIT)
    return active is not None and active.stdout.strip() in ("active", "activating", "reloading")


def user_systemd_available() -> bool:
    if not shutil.which("systemd-run") or not os.environ.get("XDG_RUNTIME_DIR"):
        return False
    state = _systemctl("is-system-running", user=True)
    return state is not None and state.stdout.strip() not in ("", "offline", "unknown")


def _user_unit_state() -> str:
    res = _systemctl("show", "-p", "ActiveState", "--value", USER_UNIT, user=True)
    return res.stdout.strip() if res is not None else ""


# -------------------------------------------------------------------- start --

def plan_start(system_cfg: dict[str, Any] | None) -> tuple[str, list[str] | None]:
    """Decide what starting NT8 would do: (reason, command or None). Pure
    decision logic - probes the machine but launches nothing."""
    host, port = bridge_endpoint(system_cfg)
    if not is_local_host(host):
        return f"NT8 bridge host is {host} (not this machine) - not starting NinjaTrader here", None
    if bridge_listening(host, port):
        return f"NinjaTrader already running (bridge listening on {host}:{port})", None
    if sys.platform.startswith("linux") and boot_unit_supervising():
        return f"NinjaTrader is managed by {BOOT_UNIT} (supervised) - leaving it to the unit", None
    if nt8_process_running():
        return "NinjaTrader is already running (bridge not listening yet - still starting?)", None

    if sys.platform.startswith("win"):
        exe = settings(system_cfg).get("exe_path") or os.path.join(
            os.environ.get("ProgramFiles", r"C:\Program Files"), "NinjaTrader 8", "bin", "NinjaTrader.exe")
        if not os.path.isfile(exe):
            return f"NinjaTrader.exe not found at {exe} - run setup-ninjatrader.bat install", None
        return "starting NinjaTrader (Windows: log in to it yourself)", [exe]

    if sys.platform.startswith("linux"):
        if not os.access(LINUX_LAUNCHER, os.X_OK):
            return f"launcher missing/not executable: {LINUX_LAUNCHER}", None
        env = {"HOME": os.path.expanduser("~"), "BRIDGE_PORT": str(port)}
        if os.environ.get("NT8_SETTINGS"):
            env["NT8_SETTINGS"] = os.environ["NT8_SETTINGS"]
        if user_systemd_available():
            s = settings(system_cfg)
            cmd = ["systemd-run", "--user", f"--unit={USER_UNIT}", "--no-block",
                   "-p", "Type=oneshot", "-p", "RemainAfterExit=yes",
                   "-p", f"MemoryMax={s.get('memory_max') or DEFAULT_MEMORY_MAX}",
                   "-p", f"MemorySwapMax={s.get('memory_swap_max') or DEFAULT_MEMORY_SWAP_MAX}",
                   "-p", "OOMPolicy=stop", "-p", "TimeoutStartSec=1500"]
            for k, v in env.items():
                cmd += ["-E", f"{k}={v}"]
            return f"starting NinjaTrader via systemd-run --user unit {USER_UNIT} (memory-capped)", cmd + [str(LINUX_LAUNCHER)]
        return "starting NinjaTrader via nt8-autostart.sh (detached; no user systemd)", [str(LINUX_LAUNCHER)]

    return f"starting NinjaTrader is not supported on {sys.platform}", None


def _launch(cmd: list[str], port: int) -> None:
    if sys.platform.startswith("win"):
        flags = getattr(subprocess, "DETACHED_PROCESS", 0x8) | getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0x200)
        subprocess.Popen(cmd, creationflags=flags, close_fds=True,
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return
    if cmd[0] == "systemd-run":
        # A previous one-shot run of the same unit stays "active (exited)" or
        # "failed" after NT8 is gone; clear it so the name can be reused.
        if _user_unit_state() in ("active", "failed", "deactivating"):
            _systemctl("stop", USER_UNIT, user=True)
        _systemctl("reset-failed", USER_UNIT, user=True)
        res = _run(cmd, timeout=30)
        if res.returncode != 0:
            raise RuntimeError(f"systemd-run failed: {(res.stderr or res.stdout).strip()}")
        return
    log_path = Path(os.environ.get("TMPDIR", "/tmp")) / "vulcantrader-ninjatrader.log"
    with open(log_path, "ab") as log_file:
        subprocess.Popen(cmd, start_new_session=True, stdin=subprocess.DEVNULL,
                         stdout=log_file, stderr=subprocess.STDOUT,
                         env={**os.environ, "BRIDGE_PORT": str(port)})


def _watch_bridge(host: str, port: int, wait_s: float) -> None:
    deadline = time.monotonic() + wait_s
    while time.monotonic() < deadline:
        if bridge_listening(host, port):
            logger.warning("ninjatrader: started - bridge is up on %s:%d", host, port)
            return
        time.sleep(5)
    logger.warning(
        "ninjatrader: bridge still not listening on %s:%d after %.0f min - check NinjaTrader "
        "(Linux: journalctl --user -u %s; ./setup-ninjatrader.sh doctor)",
        host, port, wait_s / 60, USER_UNIT)


def start(system_cfg: dict[str, Any] | None, *, watch: bool = True) -> str:
    """Start NT8 if it isn't up; returns what happened. Never raises."""
    try:
        reason, cmd = plan_start(system_cfg)
        if cmd is None:
            logger.info("ninjatrader: %s", reason)
            return reason
        host, port = bridge_endpoint(system_cfg)
        # WARNING, not INFO: the webserver/fleet logs only record WARNING and
        # up by default, and "VulcanTrader started NinjaTrader" should be visible.
        logger.warning("ninjatrader: %s", reason)
        _launch(cmd, port)
        if watch:
            threading.Thread(target=_watch_bridge, args=(host, port, BRIDGE_WAIT_S),
                             name="NinjaTraderBridgeWatch", daemon=True).start()
        return reason
    except Exception as exc:  # noqa: BLE001 - a broken NT8 setup must not stop the fleet
        logger.warning("ninjatrader: could not start NinjaTrader: %s", exc)
        return f"could not start NinjaTrader: {exc}"


def start_for_fleet(system_cfg: dict[str, Any] | None) -> str | None:
    """Fleet-start hook (bot.py webserver/autostart): start NT8 only when
    system.json sets ``ninjatrader.auto_start: true``."""
    if not auto_start_enabled(system_cfg):
        return None
    return start(system_cfg)


def stop(system_cfg: dict[str, Any] | None) -> str:
    """Stop NT8 (used by the CLI only - VulcanTrader never stops NT8 on exit)."""
    try:
        if sys.platform.startswith("win"):
            _run(["taskkill", "/IM", "NinjaTrader.exe"])
            return "asked NinjaTrader to close"
        if sys.platform.startswith("linux"):
            if boot_unit_supervising():
                return f"NinjaTrader is supervised by {BOOT_UNIT}: stop it with: sudo systemctl stop {BOOT_UNIT}"
            if os.access(LINUX_LAUNCHER, os.X_OK):
                _run([str(LINUX_LAUNCHER), "--stop"], timeout=120)
            if _user_unit_state():
                _systemctl("stop", USER_UNIT, user=True)
                _systemctl("reset-failed", USER_UNIT, user=True)
            return "NinjaTrader stopped"
        return f"stopping NinjaTrader is not supported on {sys.platform}"
    except Exception as exc:  # noqa: BLE001
        return f"could not stop NinjaTrader: {exc}"


def status(system_cfg: dict[str, Any] | None) -> dict[str, Any]:
    host, port = bridge_endpoint(system_cfg)
    info: dict[str, Any] = {
        "auto_start": auto_start_enabled(system_cfg),
        "bridge": f"{host}:{port}",
        "bridge_listening": bridge_listening(host, port),
        "nt8_process_running": nt8_process_running(),
    }
    if sys.platform.startswith("linux"):
        info["boot_unit_supervising"] = boot_unit_supervising()
        info["user_unit_state"] = _user_unit_state() or "none"
    info["start_would"] = plan_start(system_cfg)[0]
    return info


# ----------------------------------------------------------------- settings --

DEFAULT_SYSTEM_JSON = REPO_ROOT / "user_data" / "configs_system" / "system.json"


def set_auto_start(enabled: bool, system_json: Path | str = DEFAULT_SYSTEM_JSON) -> str:
    """Set ``ninjatrader.auto_start`` in system.json (creating the block, or
    the file, if missing). Keeps the file's 4-space formatting and key order
    and writes atomically - other processes read system.json on fleet scans."""
    import json

    path = Path(system_json)
    data: dict[str, Any] = {}
    if path.is_file():
        data = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(data, dict):
            raise ValueError(f"{path} is not a JSON object")
    block = data.get(SYSTEM_KEY)
    if not isinstance(block, dict):
        block = {}
    if "auto_start" in block:
        block["auto_start"] = enabled
    else:
        # New key goes right after "_comment" when there is one.
        items = list(block.items())
        at = 1 if items and items[0][0] == "_comment" else 0
        block = dict(items[:at] + [("auto_start", enabled)] + items[at:])
    data[SYSTEM_KEY] = block
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(data, indent=4, ensure_ascii=False) + "\n", encoding="utf-8")
    os.replace(tmp, path)
    return f"{path}: ninjatrader.auto_start = {str(enabled).lower()}"


# ---------------------------------------------------------------- self-test --

def _self_test() -> int:
    ok = True

    def check(name: str, cond: bool) -> None:
        nonlocal ok
        ok &= cond
        print(("PASS " if cond else "FAIL ") + name)

    check("settings: missing block -> {}", settings({}) == {} and settings(None) == {})
    check("settings: non-dict block -> {}", settings({"ninjatrader": "x"}) == {})
    check("auto_start defaults off", not auto_start_enabled({"ninjatrader": {}}))
    check("auto_start needs literal true", not auto_start_enabled({"ninjatrader": {"auto_start": "yes"}}))
    check("auto_start true", auto_start_enabled({"ninjatrader": {"auto_start": True}}))
    check("endpoint defaults", bridge_endpoint({}) == (DEFAULT_HOST, DEFAULT_PORT))
    check("endpoint from block", bridge_endpoint({"ninjatrader": {"host": "10.0.0.5", "port": 9000}}) == ("10.0.0.5", 9000))
    check("endpoint bad port -> default", bridge_endpoint({"ninjatrader": {"port": "x"}})[1] == DEFAULT_PORT)
    check("local hosts", all(is_local_host(h) for h in ("127.0.0.1", "localhost", "::1", "127.0.1.1")))
    check("remote host", not is_local_host("192.0.2.10"))
    check("start_for_fleet off -> None (no probing, no launch)", start_for_fleet({"ninjatrader": {"auto_start": False}}) is None)
    reason, cmd = plan_start({"ninjatrader": {"host": "192.0.2.10"}})
    check("remote host -> no launch", cmd is None and "not this machine" in reason)
    # A port nothing listens on, so plan_start goes past the "already up" check.
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        free_port = s.getsockname()[1]
    # Pretend NT8 isn't running and no boot unit owns it, to exercise the
    # launch planning even on a machine where NT8 is up.
    g = globals()
    saved = g["nt8_process_running"], g["boot_unit_supervising"]
    g["nt8_process_running"] = g["boot_unit_supervising"] = lambda: False
    try:
        reason, cmd = plan_start({"ninjatrader": {"port": free_port, "memory_max": "2500M"}})
    finally:
        g["nt8_process_running"], g["boot_unit_supervising"] = saved
    print(f"     plan (port {free_port}, NT8 assumed down): {reason}")
    if sys.platform.startswith("linux"):
        check("linux plan launches the repo nt8-autostart.sh", bool(cmd) and cmd[-1] == str(LINUX_LAUNCHER))
        if cmd and cmd[0] == "systemd-run":
            joined = " ".join(cmd)
            check("systemd-run plan: one-shot, BRIDGE_PORT, memory caps from settings",
                  "Type=oneshot" in joined and "RemainAfterExit=yes" in joined
                  and f"BRIDGE_PORT={free_port}" in joined and "MemoryMax=2500M" in joined
                  and f"MemorySwapMax={DEFAULT_MEMORY_SWAP_MAX}" in joined)
    import json
    import tempfile
    with tempfile.TemporaryDirectory() as td:
        p = Path(td) / "system.json"
        original = {"ninjatrader": {"_comment": "c", "host": "127.0.0.1"}, "x": 1}
        p.write_text(json.dumps(original, indent=4) + "\n", encoding="utf-8")
        set_auto_start(True, p)
        d = json.loads(p.read_text(encoding="utf-8"))
        check("set_auto_start inserts after _comment, keeps other keys",
              list(d["ninjatrader"]) == ["_comment", "auto_start", "host"] and d["x"] == 1
              and d["ninjatrader"]["auto_start"] is True)
        set_auto_start(False, p)
        check("set_auto_start toggles in place", json.loads(p.read_text())["ninjatrader"]["auto_start"] is False)
        q = Path(td) / "new" / "system.json"
        set_auto_start(True, q)
        check("set_auto_start creates a missing file", json.loads(q.read_text()) == {"ninjatrader": {"auto_start": True}})
    print("ALL PASSED" if ok else "SOME CHECKS FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    # Runnable as a plain script (stdlib only) - the installers use
    # `python <this file> --auto-start on|off` before/without the package deps.
    if "--self-test" in sys.argv:
        logging.basicConfig(level=logging.INFO)
        sys.exit(_self_test())
    if len(sys.argv) >= 3 and sys.argv[1] == "--auto-start" and sys.argv[2] in ("on", "off"):
        print(set_auto_start(sys.argv[2] == "on", sys.argv[3] if len(sys.argv) > 3 else DEFAULT_SYSTEM_JSON))
        sys.exit(0)
    print(__doc__)
    print("Script use: ninjatrader_launcher.py --self-test | --auto-start on|off [system.json]")
