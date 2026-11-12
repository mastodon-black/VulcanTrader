"""Size ``fleet_worker_processes`` in system.json to this machine's CPUs.

Setting: ``user_data/configs_system/system.json`` -> ``"fleet_worker_processes": N``.
With ``single_process_mode: true`` the webserver splits the autostart fleet across N
``bot.py fleet-worker`` processes so it can use N cores (one interpreter = one GIL);
see AGENTS.md -> "fleet_worker_processes". Shards with no bots are dropped, so N above
the bot count starts no idle workers.

The installers run this as a plain script (stdlib only, no package import):

    python VulcanTrader/util/fleet_workers.py auto      # = CPUs available to this process
    python VulcanTrader/util/fleet_workers.py 6         # explicit value
    python VulcanTrader/util/fleet_workers.py auto path/to/system.json
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any

KEY = "fleet_worker_processes"
REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_SYSTEM_JSON = REPO_ROOT / "user_data" / "configs_system" / "system.json"


def available_cpus() -> int:
    """CPUs this process may run on: respects affinity masks / cgroup-pinned
    containers where the platform reports them, not just the host's total."""
    process_cpu_count = getattr(os, "process_cpu_count", None)  # Python 3.13+
    if process_cpu_count is not None and process_cpu_count():
        return int(process_cpu_count())
    if hasattr(os, "sched_getaffinity"):  # Linux
        try:
            return max(1, len(os.sched_getaffinity(0)))
        except OSError:
            pass
    return max(1, os.cpu_count() or 1)


def set_fleet_worker_processes(
    workers: int, system_json: Path | str = DEFAULT_SYSTEM_JSON
) -> tuple[Any, dict[str, Any]]:
    """Write ``fleet_worker_processes = workers`` into system.json (creating the file if
    missing). Keeps the file's 4-space formatting and key order - a new key goes right
    after ``single_process_mode`` - and writes atomically, as other processes read
    system.json on fleet scans. Returns (previous value or None, the written config)."""
    if workers < 1:
        raise ValueError(f"{KEY} must be >= 1, got {workers}")
    path = Path(system_json)
    data: dict[str, Any] = {}
    if path.is_file():
        data = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(data, dict):
            raise ValueError(f"{path} is not a JSON object")
    previous = data.get(KEY)
    if KEY in data or "single_process_mode" not in data:
        data[KEY] = workers
    else:
        items = list(data.items())
        at = [k for k, _ in items].index("single_process_mode") + 1
        data = dict(items[:at] + [(KEY, workers)] + items[at:])
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(data, indent=4, ensure_ascii=False) + "\n", encoding="utf-8")
    os.replace(tmp, path)
    return previous, data


def main(argv: list[str]) -> int:
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        return 0 if argv else 2
    choice = argv[0].strip().lower()
    system_json = Path(argv[1]) if len(argv) > 1 else DEFAULT_SYSTEM_JSON
    if choice in ("", "auto"):
        workers, source = available_cpus(), "CPUs on this machine"
    else:
        try:
            workers, source = int(choice), "explicit value"
        except ValueError:
            print(f"[fleet-workers] ERROR: expected 'auto' or a number, got {argv[0]!r}",
                  file=sys.stderr)
            return 2
        if workers < 1:
            print(f"[fleet-workers] ERROR: {KEY} must be >= 1, got {workers}", file=sys.stderr)
            return 2
    try:
        previous, data = set_fleet_worker_processes(workers, system_json)
    except (OSError, ValueError) as e:  # json.JSONDecodeError is a ValueError
        print(f"[fleet-workers] ERROR: could not update {system_json}: {e}", file=sys.stderr)
        return 1
    change = f"{previous} -> {workers}" if previous not in (None, workers) else str(workers)
    print(f"[fleet-workers] {system_json}: {KEY} = {change} ({source})")
    if not data.get("single_process_mode"):
        print(f"[fleet-workers] note: {KEY} only takes effect with "
              f"\"single_process_mode\": true in system.json (it is off here).")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
