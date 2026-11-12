"""
Timing: would the strategy survive its entries and exits happening a few minutes off?

Every trade is re-priced with its entry, its exit, or both moved by OFFSETS minutes (-10 .. +10): the
fill moves with the market's own 1m path over that gap, everything else about the trade stays (same
trades, same exit decisions, same leverage, fees and funding). The scenario's net is compared with the
trade list as it was. Used by the backtester page's Analysis panel (web_portal) and the top-strategies
report.

Fill instants
-------------
- Paper / live trades carry real fill times (exact_times=True): those are used as they are.
- Backtest trades are stamped with their candle's open. A fill at that candle's open price happened
  at the open; any other (stop, ROI, trailing, a limit) is placed in the first 1m candle of its
  strategy candle that reached the fill price, at mid-minute. When no 1m candle quite touches it
  (another venue's 1m prices), the closest one is used. A stop-type exit (stop, trailing, liquidation)
  priced past the candle's open on the profit side - a trail tightened inside the candle - was hit on
  the way back from the candle's extreme, so only minutes from that extreme (the high for a long, the
  low for a short) on count: the first touch would be the price passing it on the way up.
- Price at an instant: the containing 1m candle's open, moved linearly toward its close.

1m data comes from the bot's own exchange first and Binance (USDT futures, then spot) after it,
per trade; only relative moves are used, so a second venue's minutes are a fair stand-in. A trade
without 1m data around every point it needs (down to +-10 minutes) is left out of every scenario.

Shifted price, for each moved fill:  rate * P(t + offset) / P(t)
New trade P&L:                       pnl + stake * leverage * (move(entry', exit') - move(entry, exit))
where move = exit/entry - 1 (long) or 1 - exit/entry (short), so the trades count by their real size
(the report ranks accounts by P&L). Without profit_abs + stake_amount on every trade the per-trade
returns are used instead (return + leverage * the same difference). A shift that puts the entry at
or after the exit makes the trade flat: its fees remain.

Accuracy (measured 2026-11-04 against the engine itself, every entry signal really delayed by one or
two candles, 2025-08..2026-10, all signals taken): Romulus (5m, ROI + stop exits) +5m kept -84% in
the engine vs -79% here, +10m -80% vs -80%; SocratesModel (15m, mostly trailing stops and mae cuts)
+15m kept 90% vs 78% here. Exits stay put in this model, but a trailing stop, an ROI target or a cut
priced off the entry partly moves with a later entry in a real run, so for such exits the Entries
row is on the pessimistic side.

Rating (per scenario; retained = scenario net / net as traded)
--------------------------------------------------------------
  Great     retained >= 90%
  Good      >= 50%
  Bad       >= 0 (still profitable, most of the edge gone)
  Terrible  < 0 (now loses money)
  n/a       the trades as traded do not make money, or fewer than MIN_TRADES of them
Verdict: "Robust to +-10m" when every scenario up to 10 minutes either way is Good or Great;
"Holds to +-Nm" for the largest N that is; "Breaks within +-1m" when +-1m already fails.
"""
from __future__ import annotations

import math
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

import numpy as np
import pandas as pd

OFFSETS: list[int] = [-10, -5, -2, -1, 1, 2, 5, 10]      # minutes
LEGS: list[str] = ["entry", "exit", "both"]
RATINGS = ["Great", "Good", "Bad", "Terrible", "n/a"]
RETAIN_GREAT, RETAIN_GOOD = 0.9, 0.5
MIN_TRADES = 20
AT_OPEN_TOLERANCE = 2e-4   # a fill this close to its candle's open price happened at the open
MINUTE = 60_000_000_000    # ns


@dataclass
class Minutes:
    """One source's 1m candles for one pair, as numpy arrays (start in ns, UTC)."""
    source: str
    start: np.ndarray
    open: np.ndarray
    high: np.ndarray
    low: np.ndarray
    close: np.ndarray

    def index(self, t: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
        """(candle index containing t, valid) - invalid where no 1m candle covers t (gap, out of range)."""
        i = np.searchsorted(self.start, t, side="right") - 1
        ok = i >= 0
        ic = np.clip(i, 0, len(self.start) - 1)
        ok &= (t - self.start[ic]) < MINUTE
        return ic, ok

    def price_at(self, t: np.ndarray) -> np.ndarray:
        """Price at instants t (ns): the 1m candle's open moved linearly toward its close. NaN where
        no candle covers t."""
        i, ok = self.index(t)
        frac = np.clip((t - self.start[i]) / MINUTE, 0.0, 1.0)
        p = self.open[i] + (self.close[i] - self.open[i]) * frac
        return np.where(ok, p, np.nan)

    def fill_instants(self, stamp: np.ndarray, rate: np.ndarray, tf_minutes: int,
                      stop_long: np.ndarray | None = None, stop_short: np.ndarray | None = None) -> np.ndarray:
        """Fill instants (ns) for candle-stamped fills - see the module docstring. `stop_long` /
        `stop_short` flag stop-type exits of long / short trades. NaN-free: falls back to the stamp
        itself when the strategy candle has no 1m data."""
        out = stamp.astype(np.float64).copy()
        i0 = np.searchsorted(self.start, stamp, side="left")
        i0c = np.clip(i0, 0, len(self.start) - 1)
        at_open = (self.start[i0c] == stamp) & (np.abs(rate / self.open[i0c] - 1.0) < AT_OPEN_TOLERANCE)
        todo = np.flatnonzero(~at_open)
        if not len(todo):
            return out
        w = max(1, int(tf_minutes))
        idx = np.clip(i0c[todo, None] + np.arange(w)[None, :], 0, len(self.start) - 1)   # (n, w)
        inside = (self.start[idx] >= stamp[todo, None]) & (self.start[idx] < stamp[todo, None] + w * MINUTE)
        r = rate[todo, None]
        gap = np.maximum.reduce([self.low[idx] - r, r - self.high[idx], np.zeros(idx.shape)])
        gap = np.where(inside, gap, np.inf)
        # Stop exits priced past the open on the profit side were hit coming back from the extreme.
        first_open = self.open[idx[:, 0]]
        for flags, past_open, extreme in ((stop_long, rate[todo] > first_open, np.where(inside, self.high[idx], -np.inf)),
                                          (stop_short, rate[todo] < first_open, np.where(inside, -self.low[idx], -np.inf))):
            if flags is None:
                continue
            rows = flags[todo] & past_open
            if rows.any():
                peak = np.argmax(extreme, axis=1)
                before = np.arange(w)[None, :] < peak[:, None]
                gap = np.where(rows[:, None] & before, np.inf, gap)
        best = np.argmin(gap, axis=1)                       # first of the closest (touching = 0)
        found = np.isfinite(gap[np.arange(len(todo)), best])
        tau = self.start[idx[np.arange(len(todo)), best]] + MINUTE // 2
        out[todo] = np.where(found, tau, stamp[todo])
        return out


def _load(datadir: Path, pair: str, candle_type, label: str, lo: pd.Timestamp, hi: pd.Timestamp) -> Minutes | None:
    from VulcanTrader.data.history.datahandlers.featherdatahandler import FeatherDataHandler
    h = FeatherDataHandler(datadir)
    path = h._pair_data_filename(datadir, pair, "1m", candle_type)
    if not path.is_file():
        return None
    try:
        df = pd.read_feather(path, columns=["date", "open", "high", "low", "close"])
    except Exception:
        return None
    dates = pd.to_datetime(df["date"], utc=True)
    keep = (dates >= lo) & (dates <= hi)
    if not keep.any():
        return None
    df, dates = df[keep], dates[keep]
    start = dates.dt.as_unit("ns").astype("int64").to_numpy()   # feather dates are often ms
    order = np.argsort(start, kind="stable")
    return Minutes(label, start[order], *(df[c].to_numpy(np.float64)[order] for c in ("open", "high", "low", "close")))


def default_sources(datadirs: list[Path], binance_dir: Path | None, trading_mode: str) -> Callable:
    """pair -> [(datadir, pair name, candle type, label)]: the bot's own data first, then Binance
    USDT futures and spot for crypto pairs (`k`-prefixed Hyperliquid names map to Binance's 1000x)."""
    from VulcanTrader.enums import CandleType
    own = CandleType.FUTURES if trading_mode == "futures" else CandleType.SPOT

    def sources(pair: str) -> list[tuple]:
        out = [(d, pair, own, d.name) for d in datadirs]
        if binance_dir is not None and "/" in pair:
            base = pair.split("/")[0]
            fut = "1000" + base[1:] if base.startswith("k") and base[1:].isupper() else base
            spot = base[1:] if base.startswith("k") and base[1:].isupper() else base
            out += [(binance_dir, f"{fut}/USDT:USDT", CandleType.FUTURES, "binance"),
                    (binance_dir, f"{spot}/USDT", CandleType.SPOT, "binance")]
        return out
    return sources


def is_stop_exit(reason) -> bool:
    """Stop-type exit reasons (ExitType values: stop_loss, trailing_stop_loss, stoploss_on_exchange,
    liquidation)."""
    r = str(reason or "").lower()
    return "stop" in r or r == "liquidation"


def _to_ns(values) -> np.ndarray:
    return pd.to_datetime(pd.Series(values), utc=True, format="mixed").dt.as_unit("ns").astype("int64").to_numpy()


def _move(entry: np.ndarray, exit_: np.ndarray, short: np.ndarray) -> np.ndarray:
    return np.where(short, 1.0 - exit_ / entry, exit_ / entry - 1.0)


def _summary(values: np.ndarray) -> dict:
    gain, loss = values[values > 0].sum(), -values[values < 0].sum()
    pf = gain / loss if loss > 0 else (math.inf if gain > 0 else 1.0)
    return {"net": round(float(values.sum()), 6), "pf": None if math.isinf(pf) else round(float(pf), 3),
            "win_rate": round(float((values > 0).mean()) * 100, 2) if len(values) else 0.0}


def _first(*values):
    for v in values:
        if v is not None:
            return v
    return None


def rate_retained(retained: float | None) -> str:
    if retained is None:
        return "n/a"
    if retained >= RETAIN_GREAT:
        return "Great"
    if retained >= RETAIN_GOOD:
        return "Good"
    return "Bad" if retained >= 0 else "Terrible"


def verdict(cells: dict, rated: bool) -> tuple[str, int | None]:
    """(label, largest +-minutes that still holds) - see the module docstring."""
    if not rated:
        return "Not rated", None
    holds = None
    for m in sorted({abs(o) for o in OFFSETS}):
        ok = all(c["rating"] in ("Great", "Good") for leg in LEGS for c in cells[leg] if abs(c["offset"]) <= m)
        if not ok:
            break
        holds = m
    biggest = max(abs(o) for o in OFFSETS)
    if holds == biggest:
        return f"Robust to ±{biggest}m", holds
    if holds is None:
        return "Breaks within ±1m", None
    return f"Holds to ±{holds}m", holds


def analyze(trades: list[dict], sources: Callable, timeframe: str | None, exact_times: bool) -> dict:
    """Timing analysis of closed trades (dicts with pair, open_date, close_date, open_rate, close_rate,
    profit_ratio or close_profit, is_short, leverage, exit_reason). `sources(pair)` lists the 1m candidates (default_sources);
    `timeframe` is the strategy's (for placing intrabar backtest fills); `exact_times` for paper/live."""
    from VulcanTrader.exchange.exchange_utils_timeframe import timeframe_to_minutes
    tf_min = timeframe_to_minutes(timeframe) if timeframe else 1
    rows = [t for t in trades if t.get("pair") and t.get("open_date") and t.get("close_date")
            and float(t.get("open_rate") or 0) > 0 and float(t.get("close_rate") or 0) > 0]
    n = len(rows)
    points = np.array([0] + OFFSETS) * MINUTE           # 0 first: the fill itself
    pe = np.full((n, len(points)), np.nan)
    px = np.full((n, len(points)), np.nan)
    te = np.zeros(n)
    tx = np.zeros(n)
    used_by: dict[str, int] = {}
    by_pair: dict[str, list[int]] = {}
    for i, t in enumerate(rows):
        by_pair.setdefault(t["pair"], []).append(i)

    for pair, idx in by_pair.items():
        idx = np.array(idx)
        o_ns = _to_ns([rows[i]["open_date"] for i in idx])
        c_ns = _to_ns([rows[i]["close_date"] for i in idx])
        o_rate = np.array([float(rows[i]["open_rate"]) for i in idx])
        c_rate = np.array([float(rows[i]["close_rate"]) for i in idx])
        stop = np.array([is_stop_exit(rows[i].get("exit_reason")) for i in idx], dtype=bool)
        is_short = np.array([bool(rows[i].get("is_short")) for i in idx], dtype=bool)
        lo = pd.Timestamp(int(o_ns.min()), tz="UTC") - pd.Timedelta(minutes=15)
        hi = pd.Timestamp(int(c_ns.max()), tz="UTC") + pd.Timedelta(minutes=15 + tf_min)
        todo = np.ones(len(idx), dtype=bool)
        for datadir, name, candle, label in sources(pair):
            if not todo.any():
                break
            m = _load(datadir, name, candle, label, lo, hi)
            if m is None:
                continue
            if exact_times:
                e_t, x_t = o_ns.astype(np.float64), c_ns.astype(np.float64)
            else:
                e_t = m.fill_instants(o_ns, o_rate, tf_min)
                x_t = m.fill_instants(c_ns, c_rate, tf_min, stop & ~is_short, stop & is_short)
            e_p = m.price_at((e_t[:, None] + points[None, :]).astype(np.int64))
            x_p = m.price_at((x_t[:, None] + points[None, :]).astype(np.int64))
            ok = todo & np.isfinite(e_p).all(axis=1) & np.isfinite(x_p).all(axis=1) & (e_p[:, 0] > 0) & (x_p[:, 0] > 0)
            sel = idx[ok]
            pe[sel], px[sel], te[sel], tx[sel] = e_p[ok], x_p[ok], e_t[ok], x_t[ok]
            if ok.any():
                used_by[label] = used_by.get(label, 0) + int(ok.sum())
            todo &= ~ok

    have = np.isfinite(pe[:, 0])
    rows_ok = [t for t, h in zip(rows, have) if h]
    pe, px, te, tx = pe[have], px[have], te[have], tx[have]
    used = len(rows_ok)
    # backtest results say profit_ratio / profit_abs, account snapshots close_profit / close_profit_abs
    ret = np.array([float(_first(t.get("profit_ratio"), t.get("close_profit"), 0.0)) for t in rows_ok])
    pnl = np.array([float(_first(t.get("profit_abs"), t.get("close_profit_abs"), np.nan)) for t in rows_ok])
    stake = np.array([float(t.get("stake_amount") or np.nan) for t in rows_ok])
    in_money = bool(used) and bool(np.isfinite(pnl).all() and (stake > 0).all())
    lev = np.array([float(t.get("leverage") or 1.0) for t in rows_ok])
    short = np.array([bool(t.get("is_short")) for t in rows_ok])
    e0 = np.array([float(t["open_rate"]) for t in rows_ok])
    x0 = np.array([float(t["close_rate"]) for t in rows_ok])
    base_move = _move(e0, x0, short) if used else np.zeros(0)

    def value(move: np.ndarray) -> np.ndarray:
        """Each trade's P&L (or return) with its price move replaced by `move`."""
        if in_money:
            return pnl + stake * lev * (move - base_move)
        return ret + lev * (move - base_move)

    base = {"trades": used, **_summary(pnl if in_money else ret), "net_pct": round(float(ret.sum()) * 100, 3)}
    rated = used >= MIN_TRADES and base["net"] > 0

    cells: dict[str, list[dict]] = {leg: [] for leg in LEGS}
    for j, off in enumerate(OFFSETS, start=1):
        shift = off * MINUTE
        for leg in LEGS:
            e = e0 * pe[:, j] / pe[:, 0] if leg in ("entry", "both") else e0
            x = x0 * px[:, j] / px[:, 0] if leg in ("exit", "both") else x0
            new_te = te + (shift if leg in ("entry", "both") else 0)
            new_tx = tx + (shift if leg in ("exit", "both") else 0)
            flat = new_te >= new_tx if leg != "both" else np.zeros(used, dtype=bool)
            move = np.where(flat, 0.0, _move(e, x, short)) if used else np.zeros(0)
            s = _summary(value(move))
            retained = s["net"] / base["net"] if base["net"] > 0 else None
            cells[leg].append({"offset": off, **s, "retained": None if retained is None else round(retained, 4),
                               "flat": int(flat.sum()), "rating": rate_retained(retained) if rated else "n/a"})
    label, holds = verdict(cells, rated)
    if not rated:
        label = "Not enough trades" if used < MIN_TRADES else "Not profitable as traded"
    return {"offsets": OFFSETS, "legs": LEGS, "base": base, "cells": cells, "verdict": label, "holds_to": holds,
            "unit": "abs" if in_money else "ratio",
            "trades": len(trades), "used": used, "skipped": len(trades) - used, "sources": used_by,
            "timeframe": timeframe, "exact_times": exact_times, "min_trades": MIN_TRADES}
