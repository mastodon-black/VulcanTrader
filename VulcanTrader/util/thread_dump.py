"""Safe all-thread stack dumps for live fleet processes.

py-spy can't attach here (ptrace_scope=1), and faulthandler's signal-time
all-thread dump segfaulted busy fleet workers. sys._current_frames() runs on
a normal thread holding the GIL, so it is safe to call at any time.
"""

from __future__ import annotations

import sys
import threading
import time
import traceback
from pathlib import Path


def dump_thread_stacks(directory: Path, prefix: str) -> Path:
    """Write every thread's current stack, labelled by thread name, to
    `<directory>/<prefix>-stacks-<timestamp>.txt` and return that path."""
    names = {thread.ident: thread.name for thread in threading.enumerate()}
    out = directory / f"{prefix}-stacks-{time.strftime('%Y%m%d-%H%M%S')}.txt"
    with open(out, "w", encoding="utf-8") as fh:
        for ident, frame in sys._current_frames().items():
            fh.write(f"--- {names.get(ident, '?')} ({ident})\n")
            fh.write("".join(traceback.format_stack(frame)))
            fh.write("\n")
    return out


def dump_if_requested(trigger: Path, prefix: str) -> Path | None:
    """If `trigger` exists, remove it and write a dump next to it."""
    if not trigger.exists():
        return None
    try:
        trigger.unlink()
    except OSError:
        pass
    return dump_thread_stacks(trigger.parent, prefix)
