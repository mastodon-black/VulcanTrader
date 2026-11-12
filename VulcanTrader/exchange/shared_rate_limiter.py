"""Per-IP request-weight budget shared by every ccxt client on this machine.

ccxt throttles per *instance*. A fleet in single_process_mode creates two
clients (sync + async) per bot, and the data_server master creates its own -
each one pacing itself to the exchange's full per-IP limit while all of them
share that one IP. With ~70 Hyperliquid bots that is ~140 independent buckets
spending a single 1200 weight/min budget, which is where the constant 429s
came from.

`install()` replaces `throttle()` on a ccxt client with one backed by a GCRA
bucket whose state lives in a small file under the system temp dir, guarded by
an OS file lock, so the fleet process and the data_server process draw from
the same budget. If the file can't be used, it degrades to a per-process
bucket (still a large improvement over per-instance).
"""

from __future__ import annotations

import asyncio
import logging
import os
import struct
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

import filelock


logger = logging.getLogger(__name__)

# Documented per-IP REST weight limit, per minute.
_IP_WEIGHT_PER_MIN: dict[str, int] = {
    "hyperliquid": 1200,
}
# Fraction of the documented limit we allow ourselves - margin for weights
# ccxt doesn't model exactly and for anything else on this IP.
_BUDGET_FRACTION = 0.85
# Weight that may be spent back-to-back before pacing starts.
_BURST_WEIGHT = 100
# History fetches (data_server backfill, per-trade funding-fee updates) are
# large and not latency-sensitive. They may only use this fraction of the
# budget, so tickers/order books never queue behind a backfill - a ticker
# stuck in the queue past the data_server client's 30s timeout drops the
# fleet's connection to the master.
_BULK_LABELS = frozenset({"candleSnapshot", "fundingHistory"})
_BULK_FRACTION = 0.6
# A theoretical-arrival-time this far ahead can only be a clock jump.
_MAX_SANE_BACKLOG_S = 600.0
_STATS_INTERVAL_S = 600.0
_LONG_WAIT_WARN_S = 30.0

_limiters: dict[str, "SharedRateLimiter"] = {}
_limiters_guard = threading.Lock()


_MAIN, _BULK = 0, 1


class SharedRateLimiter:
    def __init__(self, exchange_id: str, weight_per_min: float, burst_weight: float) -> None:
        self.exchange_id = exchange_id
        main_rate = weight_per_min / 60.0
        # (rate in weight/s, burst tolerance in s) per bucket
        self._buckets = {
            _MAIN: (main_rate, burst_weight / main_rate),
            _BULK: (main_rate * _BULK_FRACTION, burst_weight / (main_rate * _BULK_FRACTION)),
        }
        self._lock = threading.Lock()
        self._local_tats = [0.0, 0.0]
        self._state_file: Any = None
        self._file_lock: filelock.FileLock | None = None
        # Fixed /tmp on POSIX rather than $TMPDIR (per-session on macOS) so
        # processes launched from different environments still share it.
        state_dir = Path("/tmp") if os.name == "posix" else Path(tempfile.gettempdir())
        state_path = state_dir / f"vulcantrader-ratelimit-{exchange_id}.state"
        try:
            state_path.touch(exist_ok=True)
            self._state_file = open(state_path, "r+b", buffering=0)  # noqa: SIM115
            self._file_lock = filelock.FileLock(str(state_path) + ".lock", thread_local=False)
        except OSError as e:
            logger.warning(
                "[ratelimit] %s: shared state file %s unusable (%s) - limiting this process only",
                exchange_id, state_path, e,
            )
            self._state_file = None
        self._stats: dict[str, list[float]] = {}
        self._stats_since = time.time()
        self._max_wait = 0.0
        self._max_bulk_wait = 0.0
        self._total_wait = 0.0
        self._interactive_count = 0
        self._last_long_wait_warn = 0.0

    def _read_tat(self, bucket: int) -> float:
        if self._state_file is None:
            return self._local_tats[bucket]
        self._state_file.seek(bucket * 8)
        raw = self._state_file.read(8)
        return struct.unpack("d", raw)[0] if len(raw) == 8 else 0.0

    def _write_tat(self, bucket: int, tat: float) -> None:
        if self._state_file is None:
            self._local_tats[bucket] = tat
            return
        self._state_file.seek(bucket * 8)
        self._state_file.write(struct.pack("d", tat))

    def _reserve_locked(self, bucket: int, cost: float) -> float:
        rate, tolerance_s = self._buckets[bucket]
        now = time.time()
        tat = self._read_tat(bucket)
        if tat - now > _MAX_SANE_BACKLOG_S:
            logger.warning(
                "[ratelimit] %s: shared bucket was %.0fs ahead of the clock - resetting it",
                self.exchange_id, tat - now,
            )
            tat = now
        tat = max(tat, now) + cost / rate
        self._write_tat(bucket, tat)
        return max(0.0, tat - now - tolerance_s)

    def _reserve(self, bucket: int, cost: float) -> float:
        with self._lock:
            if self._file_lock is not None:
                try:
                    with self._file_lock:
                        return self._reserve_locked(bucket, cost)
                except (OSError, filelock.Timeout) as e:
                    logger.warning(
                        "[ratelimit] %s: shared lock failed (%s) - limiting this process only",
                        self.exchange_id, e,
                    )
                    self._file_lock = None
                    self._state_file = None
            return self._reserve_locked(bucket, cost)

    def bulk_delay(self, cost: float, label: str) -> float:
        """First stage for history fetches: wait this long BEFORE reserve(),
        so bulk traffic enters the main bucket at no more than its share."""
        return self._reserve(_BULK, cost) if label in _BULK_LABELS else 0.0

    def reserve(self, cost: float, label: str, waited: float = 0.0) -> float:
        """Book `cost` weight and return how long the caller must wait first."""
        delay = self._reserve(_MAIN, cost)
        with self._lock:
            self._record(label, cost, waited + delay)
        return delay

    def _record(self, label: str, cost: float, delay: float) -> None:
        entry = self._stats.setdefault(label, [0, 0.0])
        entry[0] += 1
        entry[1] += cost
        now = time.time()
        if label in _BULK_LABELS:
            self._max_bulk_wait = max(self._max_bulk_wait, delay)
        else:
            self._interactive_count += 1
            self._total_wait += delay
            self._max_wait = max(self._max_wait, delay)
            if delay >= _LONG_WAIT_WARN_S and now - self._last_long_wait_warn > 60:
                self._last_long_wait_warn = now
                logger.warning(
                    "[ratelimit] %s: %s request queued %.0fs - demand is above the "
                    "shared per-IP budget", self.exchange_id, label, delay,
                )
        elapsed = now - self._stats_since
        if elapsed >= _STATS_INTERVAL_S:
            total_req = sum(int(v[0]) for v in self._stats.values())
            total_weight = sum(v[1] for v in self._stats.values())
            top = sorted(self._stats.items(), key=lambda kv: kv[1][1], reverse=True)[:6]
            logger.warning(
                "[ratelimit] %s (this process, last %.0fs): %d requests, %.0f weight "
                "(%.0f/min); interactive wait avg %.2fs max %.1fs; bulk wait max %.1fs; "
                "by type: %s",
                self.exchange_id, elapsed, total_req, total_weight,
                total_weight * 60.0 / elapsed,
                self._total_wait / max(self._interactive_count, 1), self._max_wait,
                self._max_bulk_wait,
                ", ".join(f"{k}={int(v[0])}x/{v[1]:.0f}w" for k, v in top),
            )
            self._stats = {}
            self._stats_since = now
            self._max_wait = 0.0
            self._max_bulk_wait = 0.0
            self._total_wait = 0.0
            self._interactive_count = 0


def _get_limiter(exchange_id: str) -> SharedRateLimiter | None:
    weight_per_min = _IP_WEIGHT_PER_MIN.get(exchange_id)
    if weight_per_min is None:
        return None
    with _limiters_guard:
        limiter = _limiters.get(exchange_id)
        if limiter is None:
            limiter = SharedRateLimiter(
                exchange_id, weight_per_min * _BUDGET_FRACTION, _BURST_WEIGHT
            )
            _limiters[exchange_id] = limiter
        return limiter


def _hyperliquid_candle_cost(api: Any, params: dict, fallback: float) -> float:
    """Hyperliquid charges candleSnapshot 20 + 1 per 60 candles returned;
    ccxt charges a flat 4."""
    req = params.get("req") or {}
    try:
        span_ms = int(req["endTime"]) - int(req["startTime"])
        interval_ms = api.parse_timeframe(req["interval"]) * 1000
        candles = min(5000, max(0, span_ms // interval_ms))
    except (KeyError, TypeError, ValueError, ZeroDivisionError):
        return max(fallback, 20 + 500 // 60)
    return 20 + candles // 60


def install(api: Any, sync: bool) -> None:
    """Route `api`'s rate limiting through the shared per-IP bucket, if one is
    defined for its exchange. No-op otherwise."""
    limiter = _get_limiter(getattr(api, "id", ""))
    if limiter is None or not getattr(api, "enableRateLimit", True):
        return

    original_cost = api.calculate_rate_limiter_cost
    is_hyperliquid = api.id == "hyperliquid"

    def calculate_rate_limiter_cost(api_, method, path, params, config={}):  # noqa: B006
        cost = original_cost(api_, method, path, params, config)
        label = path
        if isinstance(params, dict) and params.get("type"):
            label = str(params["type"])
            if is_hyperliquid and label == "candleSnapshot":
                cost = _hyperliquid_candle_cost(api, params, cost)
        # ccxt passes the result straight to throttle(); carry the label with it.
        return (cost, label)

    def _unpack(cost: Any) -> tuple[float, str]:
        if isinstance(cost, tuple):
            return float(cost[0] if cost[0] is not None else 1), cost[1]
        return float(cost if cost is not None else 1), "other"

    if sync:
        def throttle(cost=None):
            weight, label = _unpack(cost)
            waited = limiter.bulk_delay(weight, label)
            if waited > 0:
                time.sleep(waited)
            delay = limiter.reserve(weight, label, waited)
            if delay > 0:
                time.sleep(delay)
    else:
        async def throttle(cost=None):
            weight, label = _unpack(cost)
            waited = limiter.bulk_delay(weight, label)
            if waited > 0:
                await asyncio.sleep(waited)
            delay = limiter.reserve(weight, label, waited)
            if delay > 0:
                await asyncio.sleep(delay)

    api.calculate_rate_limiter_cost = calculate_rate_limiter_cost
    api.throttle = throttle
