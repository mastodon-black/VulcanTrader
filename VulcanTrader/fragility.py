"""
Fragility: how a strategy held up through the big BTC runs and crashes since 2017.

The strategy is backtested on BTC alone over Binance's spot BTC/USDT history - the only local series
that reaches back to 2017 (the archive's first USDT candle is 2017-08-17, see
scripts/download_binance_archive.py) - and every event window in EVENTS is rated from the trades
that were open inside it. Used by the backtester page's Analysis panel (web_portal) and the
top-strategies report.

The run
-------
- Candles: Binance spot BTC/USDT, written into ``user_data/data/fragility_btc/<exchange>/`` under the
  bot's own BTC pair name and candle type (``BTC/USDC:USDC`` futures for a Hyperliquid bot). The bot
  therefore runs with its own config - exchange model, fees, leverage, shorts - over Binance's prices.
  Timeframes the download lacks (2h, 4h, 1d, ...) are resampled from 1m; 1m itself is only copied
  when the strategy or its timeframe_detail needs it. Derived data: rebuilt whenever the source
  file is newer.
- Funding: Binance's BTCUSDT perpetual funding (from 2020-01; `download_binance_archive.py --funding`),
  each 8h rate spread evenly over its 8 hours as the bot exchange's 1h funding candles, plus 1h mark
  candles from the same prices. Strategies that read funding through the data provider see it (a
  funding filter would otherwise stand aside for the whole run - AresMomentum took 822 trades on BTC
  in 2025-08..2026-10 without it against 501 on Hyperliquid's own data), and the rust engine charges
  it. Before 2020 there is none (BTC perps started in September 2019).
- Engine rust.
- Config: the bot's config plus OVERLAY - BTC only on a StaticPairList, and a $1e9 wallet so no
  balance floor or blow-up can stop it trading before a later event. Stake, max_open_trades,
  leverage, protections and timeframe_detail stay as the bot has them.
- Timerange 20170817- (to the newest candle).

Rating (per event; sizing-free, from per-trade returns on stake; net = their sum, in % of one stake)
---------------------------------------------------------------------------------------------------
  Terrible  profit factor < 0.67 and net <= -10%, or one trade lost >= 90% of its stake (liquidation level)
  Bad       net < -1%
  Great     PF >= 1.5 and net >= +5%
  Good      anything else: made money, or about flat
  No trades nothing was open in the window
Profit factor alone misreads a window with one or two trades (a single -0.01% trade has PF 0, a
single +0.01% one PF infinity); the net floors keep those flat outcomes at Good, while long windows,
whose net is large either way, are decided by PF.
A trade counts for an event when it was open at any time inside the window, so a position caught by
a crash counts even if it was entered before it. Overall: Fragile if any event is Terrible, Mixed if
any is Bad, Robust if every traded event is Good or Great (at least MIN_EVENTS of them), else
Untested.

Runs are cached under ``user_data/backtest_results/fragility_btc/`` and reused while the strategy's
source file(s), the bot config and OVERLAY are unchanged (code it imports from other modules is not
hashed: re-run after changing that).
"""
from __future__ import annotations

import hashlib
import json
import math
import re
import shutil
import sys
from datetime import UTC, datetime
from pathlib import Path

import pandas as pd

SOURCE_EXCHANGE = "binance"
SOURCE_PAIR = "BTC/USDT"
SOURCE_LABEL = "Binance spot BTC/USDT"
TIMERANGE = "20170817-"
DATA_DIRNAME = "fragility_btc"           # under user_data/data
RESULTS_DIRNAME = "fragility_btc"        # under user_data/backtest_results

# (key, label, kind, start, end) - windows are [start, end) in UTC. Crash and run windows run from
# the turning point before to the one after, read off the Binance (2017-2020) and Hyperliquid daily
# series; the 2025 correction is taken to its trough in the data (2026-07-01, -54%), not to the $84k
# of late 2025. Mt. Gox (2011, 2014) predates any data here.
EVENTS: list[tuple[str, str, str, str, str]] = [
    ("ico_run", "2017 ICO mania", "run", "2017-08-17", "2017-12-17"),
    ("ico_bust", "2018 ICO bust", "crash", "2017-12-17", "2018-12-16"),
    ("covid", "Black Thursday", "shock", "2020-03-12", "2020-03-14"),
    ("inst_run", "2020-21 institutional run", "run", "2020-03-13", "2021-11-11"),
    ("bear_2022", "2021-22 bear", "crash", "2021-11-10", "2022-11-22"),
    ("luna", "Terra/LUNA collapse", "shock", "2022-05-05", "2022-05-13"),
    ("celsius", "Celsius freeze", "shock", "2022-06-10", "2022-06-19"),
    ("ftx", "FTX collapse", "shock", "2022-11-06", "2022-11-10"),
    ("etf_run", "2024-25 ETF run", "run", "2024-01-11", "2025-10-07"),
    ("correction_2025", "2025-26 correction", "crash", "2025-10-06", "2026-07-02"),
]
RATINGS = ["Great", "Good", "Bad", "Terrible", "No trades"]
PF_GREAT, PF_TERRIBLE = 1.5, 0.67
NET_GREAT, NET_BAD, NET_TERRIBLE = 5.0, -1.0, -10.0     # % of one stake
LIQUIDATION_LOSS = -0.9
MIN_EVENTS = 3

# Every standard timeframe a strategy or its informative calls are likely to ask for.
TIMEFRAMES = ["5m", "15m", "30m", "1h", "2h", "4h", "6h", "8h", "12h", "1d"]
# Bumped whenever the derived data changes meaning, so cached runs made on the old data go stale.
# 2: funding + mark candles added.
DATA_VERSION = 2
FUNDING_SOURCE_TF, FUNDING_SOURCE_HOURS = "8h", 8    # Binance's funding interval for BTCUSDT
# Detail mode: the python engine resolves each strategy candle with its 1m candles; funding the data
# doesn't cover (before 2020) is 0 rather than a reason to drop the pair.
DETAIL_OVERLAY = {"timeframe_detail": "1m", "futures_funding_rate": 0}
SPAN_LEAD_DAYS = 14

OVERLAY = {
    "pairlists": [{"method": "StaticPairList"}],
    "dry_run_wallet": 1_000_000_000,
}


# ---------------------------------------------------------------------------
#  Config, pair and strategy lookups
# ---------------------------------------------------------------------------

def read_config(user_data: Path, config: str) -> dict:
    path = user_data / "configs" / f"{config.removesuffix('.json')}.json"
    return json.loads(path.read_text(encoding="utf-8"))


def btc_pair(cfg: dict) -> str:
    """The bot's own name for BTC: BTC/<stake>:<stake> on futures, BTC/<stake> on spot."""
    stake = cfg.get("stake_currency") or "USDT"
    return f"BTC/{stake}:{stake}" if cfg.get("trading_mode") == "futures" else f"BTC/{stake}"


def configs_for_strategy(user_data: Path, strategy: str) -> list[str]:
    """Configs (bots) that run `strategy`, the bot's own configBot<Strategy> first."""
    out = []
    for path in sorted((user_data / "configs").glob("*.json")):
        try:
            if json.loads(path.read_text(encoding="utf-8")).get("strategy") == strategy:
                out.append(path.stem)
        except (OSError, ValueError):
            continue
    return sorted(out, key=lambda s: (s != f"configBot{strategy}", s))


def strategy_files(user_data: Path, strategy: str) -> list[Path]:
    """Every .py under user_data/strategies (and user_data itself) that defines class `strategy`."""
    pat = re.compile(rf"^class\s+{re.escape(strategy)}\s*[(:]", re.M)
    found = []
    roots = [user_data / "strategies", user_data]
    for root in roots:
        files = root.rglob("*.py") if root.name == "strategies" else root.glob("*.py")
        for path in files:
            try:
                if pat.search(path.read_text(encoding="utf-8", errors="replace")):
                    found.append(path)
            except OSError:
                continue
    return sorted(set(found))


def fingerprint(user_data: Path, config: str, strategy: str, detail: bool = False) -> str:
    """Hash of what decides the run: strategy source, bot config, the overlay, the mode."""
    h = hashlib.sha256()
    for path in strategy_files(user_data, strategy):
        h.update(path.read_bytes())
    h.update((user_data / "configs" / f"{config}.json").read_bytes())
    h.update(json.dumps(OVERLAY, sort_keys=True).encode())
    h.update(json.dumps(EVENTS).encode())
    h.update(f"data-v{DATA_VERSION}".encode())
    if detail:
        h.update(json.dumps(DETAIL_OVERLAY, sort_keys=True).encode() + json.dumps(spans()).encode())
    return h.hexdigest()[:16]


# ---------------------------------------------------------------------------
#  Derived candles
# ---------------------------------------------------------------------------

def _resample(df: pd.DataFrame, tf: str) -> pd.DataFrame:
    from VulcanTrader.exchange.exchange_utils_timeframe import timeframe_to_minutes
    # Minutes keep every rule tick-like, so origin="epoch" aligns 4h/1d to 00:00 UTC as exchanges do.
    rule = f"{timeframe_to_minutes(tf)}min"
    out = (df.resample(rule, on="date", label="left", closed="left", origin="epoch")
           .agg({"open": "first", "high": "max", "low": "min", "close": "last", "volume": "sum"})
           .dropna(subset=["open"]))
    return out.reset_index()


def prepare_data(user_data: Path, cfg: dict, timeframes: list[str]) -> Path:
    """Fill user_data/data/fragility_btc/<exchange>/ with Binance BTC/USDT candles under the bot's
    pair name, for `timeframes`; returns that datadir. Raises FileNotFoundError without the source."""
    from VulcanTrader.data.history.datahandlers.featherdatahandler import FeatherDataHandler
    from VulcanTrader.enums import CandleType

    src_dir = user_data / "data" / SOURCE_EXCHANGE
    src = FeatherDataHandler(src_dir)
    src_1m = src._pair_data_filename(src_dir, SOURCE_PAIR, "1m", CandleType.SPOT)
    if not src_1m.is_file():
        raise FileNotFoundError(f"{src_1m} missing: run scripts/download_binance_archive.py "
                                f"--market spot --pairs BTC/USDT")
    exchange = (cfg.get("exchange") or {}).get("name") or "exchange"
    datadir = user_data / "data" / DATA_DIRNAME / exchange
    (datadir / "futures").mkdir(parents=True, exist_ok=True)
    dst = FeatherDataHandler(datadir)
    pair = btc_pair(cfg)
    candle = CandleType.FUTURES if cfg.get("trading_mode") == "futures" else CandleType.SPOT
    newest_source = src_1m.stat().st_mtime

    one_minute = None
    for tf in dict.fromkeys(timeframes):
        out = dst._pair_data_filename(datadir, pair, tf, candle)
        if out.is_file() and out.stat().st_mtime >= newest_source:
            continue
        own = src._pair_data_filename(src_dir, SOURCE_PAIR, tf, CandleType.SPOT)
        if own.is_file():
            df = src.ohlcv_load(SOURCE_PAIR, tf, CandleType.SPOT, fill_missing=False)
        else:
            if one_minute is None:
                one_minute = src.ohlcv_load(SOURCE_PAIR, "1m", CandleType.SPOT, fill_missing=False)
            df = one_minute if tf == "1m" else _resample(one_minute, tf)
        dst.ohlcv_store(pair, tf, df, candle)

    if candle == CandleType.FUTURES:
        _write_funding_and_mark(src, src_dir, dst, datadir, pair, exchange, newest_source)

    # Markets for the bot's exchange, so the run needs no network (see Exchange._markets_cache_path).
    mode = cfg.get("trading_mode") or "spot"
    cache = user_data / "data" / exchange / ".exchange_cache" / f"{exchange}_{mode}_markets.pkl"
    if cache.is_file():
        (datadir / ".exchange_cache").mkdir(exist_ok=True)
        shutil.copy2(cache, datadir / ".exchange_cache" / cache.name)
    return datadir


def _exchange_option(exchange: str, key: str) -> str:
    """The bot exchange model's own option (funding_fee_timeframe, mark_ohlcv_timeframe)."""
    import VulcanTrader.exchange as ex
    cls = next((getattr(ex, n) for n in dir(ex) if n.lower() == exchange.lower()), ex.Exchange)
    return {**ex.Exchange.trader_has_default, **(getattr(cls, "trader_has", None) or {})}[key]


def _write_funding_and_mark(src, src_dir: Path, dst, datadir: Path, pair: str, exchange: str, newest_source: float):
    """Funding (Binance BTCUSDT, each 8h rate spread over its hours) and mark candles (the price
    candles) under the bot's pair, at its exchange's own funding / mark timeframes."""
    from VulcanTrader.enums import CandleType
    from VulcanTrader.exchange.exchange_utils_timeframe import timeframe_to_minutes
    fund_tf = _exchange_option(exchange, "funding_fee_timeframe")
    mark_tf = _exchange_option(exchange, "mark_ohlcv_timeframe")
    src_fund = src._pair_data_filename(src_dir, "BTC/USDT:USDT", FUNDING_SOURCE_TF, CandleType.FUNDING_RATE)
    out_fund = dst._pair_data_filename(datadir, pair, fund_tf, CandleType.FUNDING_RATE)
    if src_fund.is_file() and not (out_fund.is_file() and out_fund.stat().st_mtime >= src_fund.stat().st_mtime):
        f = src.ohlcv_load("BTC/USDT:USDT", FUNDING_SOURCE_TF, CandleType.FUNDING_RATE, fill_missing=False)
        step = timeframe_to_minutes(fund_tf)
        per = FUNDING_SOURCE_HOURS * 60 // step                  # target candles per source rate
        rate = f.set_index("date")["open"] / per
        idx = pd.date_range(rate.index[0], rate.index[-1] + pd.Timedelta(minutes=step * (per - 1)), freq=f"{step}min")
        r = rate.reindex(idx).ffill(limit=per - 1).dropna()
        dst.ohlcv_store(pair, fund_tf, pd.DataFrame({"date": r.index, "open": r.values, "high": r.values,
                                                     "low": r.values, "close": r.values, "volume": 0.0}),
                        CandleType.FUNDING_RATE)
    out_mark = dst._pair_data_filename(datadir, pair, mark_tf, CandleType.MARK)
    price = dst._pair_data_filename(datadir, pair, mark_tf, CandleType.FUTURES)
    if price.is_file() and not (out_mark.is_file() and out_mark.stat().st_mtime >= newest_source):
        dst.ohlcv_store(pair, mark_tf, dst.ohlcv_load(pair, mark_tf, CandleType.FUTURES, fill_missing=False),
                        CandleType.MARK)


def timeframes_for(cfg: dict, strategy_timeframe: str | None) -> list[str]:
    tfs = list(TIMEFRAMES)
    for tf in (strategy_timeframe, cfg.get("timeframe"), cfg.get("timeframe_detail")):
        if tf and tf not in tfs:
            tfs.append(tf)
    return tfs


TIMEFRAME_RE = re.compile(r"^\s+timeframe\s*(?::\s*\w+\s*)?=\s*['\"](\w+)['\"]", re.M)


def strategy_timeframe(user_data: Path, strategy: str) -> str | None:
    """The `timeframe = "15m"` class attribute, read from source (no import): the first one after
    `class <strategy>`, else the nearest above it (a base class in the same file)."""
    for path in strategy_files(user_data, strategy):
        text = path.read_text(encoding="utf-8", errors="replace")
        start = re.search(rf"^class\s+{re.escape(strategy)}\b", text, re.M)
        at = start.start() if start else 0
        after = TIMEFRAME_RE.search(text, at)
        before = [m for m in TIMEFRAME_RE.finditer(text, 0, at)]
        if after or before:
            return (after or before[-1]).group(1)
    return None


# ---------------------------------------------------------------------------
#  Run (subprocess) + cache
# ---------------------------------------------------------------------------

def results_dir(user_data: Path) -> Path:
    return user_data / "backtest_results" / RESULTS_DIRNAME


def cached_run(user_data: Path, config: str, strategy: str, detail: bool = False) -> Path | None:
    """Newest finished run of (config, strategy) in this mode whose fingerprint still matches."""
    d = results_dir(user_data)
    if not d.is_dir():
        return None
    try:
        fp = fingerprint(user_data, config, strategy, detail)
    except OSError:
        return None
    for side in sorted(d.glob(f"{strategy}-*.fragility.json"), reverse=True):
        try:
            meta = json.loads(side.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        result = d / meta.get("result", "")
        if (meta.get("strategy") == strategy and meta.get("config") == config and bool(meta.get("detail")) == detail
                and meta.get("fingerprint") == fp and result.is_file()):
            return result
    return None


def spans(lead_days: int = SPAN_LEAD_DAYS) -> list[str]:
    """Timeranges a detail run backtests separately, in parallel: the EVENTS windows merged where they
    touch, each started `lead_days` early so a position held into an event is there. No trade can
    carry from one span to the next, and every event lies inside one span."""
    first = pd.Timestamp(TIMERANGE[:8])
    merged: list[list[pd.Timestamp]] = []
    for start, end in sorted((pd.Timestamp(e[3]), pd.Timestamp(e[4])) for e in EVENTS):
        if merged and start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    return [f"{max(first, a - pd.Timedelta(days=lead_days)):%Y%m%d}-{b:%Y%m%d}" for a, b in merged]


def plan_run(user_data: Path, config: str, strategy: str, python: str | None = None,
             timeframe: str | None = None, detail: bool = False) -> dict:
    """Prepare the data and overlay for one run; returns {cmd, result, sidecar, meta} - the caller
    runs `cmd` (cwd = repo root) and, on exit code 0, calls finish_run(plan). `timeframe`: the
    strategy's, when the caller knows it (else read from its source). `detail`: python engine with
    1m candles inside the strategy's own (see the module docstring); `cmd` is then this module's
    runner, which backtests the spans in parallel."""
    cfg = read_config(user_data, config)
    tf = timeframe or strategy_timeframe(user_data, strategy)
    detail = bool(detail) and tf != "1m"           # nothing finer than 1m to resolve a 1m candle with
    datadir = prepare_data(user_data, cfg, timeframes_for(cfg, tf) + (["1m"] if detail else []))
    pair = btc_pair(cfg)
    overlay = {**OVERLAY, "exchange": {"pair_whitelist": [pair], "pair_blacklist": []}}
    if detail:
        overlay.update(DETAIL_OVERLAY)
    d = results_dir(user_data)
    d.mkdir(parents=True, exist_ok=True)
    mode = "-1m" if detail else ""
    fp = fingerprint(user_data, config, strategy, detail)
    # <Strategy>-<config>[-1m]-<UTC time>: two configs of one strategy planned in the same second differ.
    stem = (detail and _resumable_stem(d, config, strategy, fp)
            or f"{strategy}-{config}{mode}-{datetime.now(UTC).strftime('%Y-%m-%d_%H-%M-%S')}")
    overlay_path = d / f".overlay-{config}{mode}.json"
    overlay_path.write_text(json.dumps(overlay, indent=1))
    result = d / f"{stem}.json"
    py = python or sys.executable

    def backtest(timerange: str, out: Path, engine: str) -> list[str]:
        return [py, "-m", "VulcanTrader.bot", "backtest", "-c", config, "-c", str(overlay_path), "-s", strategy,
                "--engine", engine, "--timerange", timerange, "--datadir", str(datadir),
                "--exportfilename", str(out), "--user-data-dir", str(user_data)]

    meta = {"strategy": strategy, "config": config, "pair": pair, "source": SOURCE_LABEL,
            "datadir": str(datadir), "trading_mode": cfg.get("trading_mode") or "spot", "detail": detail,
            "fingerprint": fp, "result": result.name,
            "created": datetime.now(UTC).isoformat(timespec="seconds")}
    plan = {"result": result, "sidecar": d / f"{stem}.fragility.json", "meta": meta}
    if not detail:
        return {**plan, "cmd": backtest(TIMERANGE, result, "rust")}
    chunks = [(tr, d / f".{stem}-{tr}.json") for tr in spans()]
    plan_file = d / f".{stem}.plan.json"
    plan_file.write_text(json.dumps({"strategy": strategy, "config": config, "fingerprint": fp, "spans": spans(),
                                     "cmds": [backtest(tr, out, "python") for tr, out in chunks]}, indent=1))
    return {**plan, "cmd": [py, "-m", "VulcanTrader.fragility", str(plan_file)], "chunks": [out for _, out in chunks],
            "plan_file": plan_file}


def _resumable_stem(d: Path, config: str, strategy: str, fp: str) -> str | None:
    """Stem of an unfinished detail run of this strategy + config with the same fingerprint, so a run
    cut off (reboot, killed job) keeps the spans it finished: a 5m strategy's span takes ~40 minutes.
    Unfinished plans that no longer match are deleted with their spans."""
    keep = None
    for plan_file in sorted(d.glob(f".{strategy}-{config}-1m-*.plan.json"), reverse=True):
        try:
            info = json.loads(plan_file.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            info = {}
        stem = plan_file.name[1:-len(".plan.json")]
        if info.get("strategy") != strategy or info.get("config") != config:
            continue                                    # another config whose name starts the same
        if keep is None and info.get("fingerprint") == fp and info.get("spans") == spans():
            keep = stem
        else:
            _remove_plan(plan_file, [Path(c[c.index("--exportfilename") + 1]) for c in info.get("cmds", [])])
    return keep


def _remove_plan(plan_file: Path | None, chunks: list[Path]) -> None:
    for path in chunks:
        for p in (path, path.with_suffix(".meta.json")):
            p.unlink(missing_ok=True)
    if plan_file:
        plan_file.unlink(missing_ok=True)


def _readable(path: Path) -> bool:
    try:
        load_result(path)
        return True
    except (OSError, ValueError, KeyError, StopIteration, AttributeError):
        return False


def finish_run(plan: dict) -> Path:
    """Record a successful run so cached_run() finds it (a detail run's spans merged into one result
    first). Raises when a backtest wrote no result."""
    if plan.get("chunks"):
        name, merged, trades = None, None, []
        for path in plan["chunks"]:
            if not Path(path).is_file():
                raise FileNotFoundError(f"backtest wrote no result at {path}")
            name, entry = load_result(Path(path))
            trades += entry.get("trades") or []
            if merged is None:
                merged = dict(entry)
            merged["backtest_end"] = entry.get("backtest_end")
        merged["trades"] = trades
        plan["result"].write_text(json.dumps({"strategy": {name: merged}}))
    if not plan["result"].is_file():
        raise FileNotFoundError(f"backtest wrote no result at {plan['result']}")
    plan["sidecar"].write_text(json.dumps(plan["meta"], indent=1))
    if plan.get("chunks"):
        _remove_plan(plan.get("plan_file"), [Path(p) for p in plan["chunks"]])
    return plan["result"]


def _run_plan(path: str) -> int:
    """Backtest a detail plan's spans in parallel: the one command a portal job or the report runs."""
    import subprocess
    from concurrent.futures import ThreadPoolExecutor
    cmds = json.loads(Path(path).read_text(encoding="utf-8"))["cmds"]

    def one(cmd: list[str]) -> int:
        span = cmd[cmd.index("--timerange") + 1]
        if _readable(Path(cmd[cmd.index("--exportfilename") + 1])):
            print(f"span {span}: kept from an earlier, unfinished run", flush=True)
            return 0
        print(f"span {span}: backtesting with 1m detail", flush=True)
        proc = subprocess.run(cmd, capture_output=True, text=True)
        print(f"span {span}: exit {proc.returncode}", flush=True)
        if proc.returncode:
            print("\n".join((proc.stdout + proc.stderr).splitlines()[-15:]), flush=True)
        return proc.returncode

    with ThreadPoolExecutor(max(1, len(cmds))) as pool:
        codes = list(pool.map(one, cmds))
    return max(codes) if codes else 1


# ---------------------------------------------------------------------------
#  Analysis
# ---------------------------------------------------------------------------

def _ts(value) -> pd.Timestamp:
    t = pd.Timestamp(value)
    return t.tz_localize("UTC") if t.tzinfo is None else t.tz_convert("UTC")


def rate(trades: list[dict]) -> dict:
    """Rating + numbers for the trades of one event (per-trade returns on stake)."""
    rets = [float(t.get("profit_ratio") or 0.0) for t in trades]
    if not rets:
        return {"trades": 0, "wins": 0, "net_pct": None, "pf": None, "worst_pct": None,
                "liquidations": 0, "rating": "No trades"}
    gain = sum(r for r in rets if r > 0)
    loss = -sum(r for r in rets if r < 0)
    pf = gain / loss if loss > 0 else (math.inf if gain > 0 else 1.0)
    worst = min(rets)
    net = sum(rets) * 100
    liq = sum(1 for t, r in zip(trades, rets) if r <= LIQUIDATION_LOSS or t.get("exit_reason") == "liquidation")
    if liq or (pf < PF_TERRIBLE and net <= NET_TERRIBLE):
        rating = "Terrible"
    elif net < NET_BAD:
        rating = "Bad"
    elif pf >= PF_GREAT and net >= NET_GREAT:
        rating = "Great"
    else:
        rating = "Good"
    return {"trades": len(rets), "wins": sum(1 for r in rets if r > 0),
            "net_pct": round(net, 2), "pf": None if math.isinf(pf) else round(pf, 3),
            "worst_pct": round(worst * 100, 2), "liquidations": liq, "rating": rating}


def verdict(events: list[dict]) -> str:
    traded = [e for e in events if e["rating"] != "No trades"]
    if any(e["rating"] == "Terrible" for e in traded):
        return "Fragile"
    if any(e["rating"] == "Bad" for e in traded):
        return "Mixed"
    return "Robust" if len(traded) >= MIN_EVENTS else "Untested"


def analyze(trades: list[dict], daily: pd.DataFrame | None = None) -> dict:
    """Per-event ratings from a fragility run's trades. `daily` (date/open/close) adds BTC's own
    move in each window."""
    spans = []
    for t in trades:
        if t.get("open_date") is None:
            continue
        opened = _ts(t["open_date"])
        closed = _ts(t["close_date"]) if t.get("close_date") else opened
        spans.append((opened, closed, t))
    events = []
    for key, label, kind, start, end in EVENTS:
        a, b = _ts(start), _ts(end)
        inside = [t for opened, closed, t in spans if opened < b and closed >= a]
        row = {"key": key, "label": label, "kind": kind, "start": start, "end": end, "btc_pct": None,
               **rate(inside)}
        if daily is not None and len(daily):
            win = daily[(daily["date"] >= a) & (daily["date"] < b)]
            if len(win):
                row["btc_pct"] = round((float(win["close"].iloc[-1]) / float(win["open"].iloc[0]) - 1) * 100, 1)
        events.append(row)
    counts = {r: sum(1 for e in events if e["rating"] == r) for r in RATINGS}
    return {"events": events, "verdict": verdict(events), "counts": counts, "trades": len(spans)}


def load_result(path: Path) -> tuple[str, dict]:
    data = json.loads(path.read_text(encoding="utf-8"))
    name, entry = next(iter((data.get("strategy") or {}).items()))
    return name, entry


def btc_daily(user_data: Path, meta: dict | None = None) -> pd.DataFrame | None:
    """BTC/USDT daily candles: the 1d file a run's prepare_data built (`meta` = its sidecar), else
    the Binance source (resampled from 1m when it has no 1d file)."""
    from VulcanTrader.data.history.datahandlers.featherdatahandler import FeatherDataHandler
    from VulcanTrader.enums import CandleType
    if meta and meta.get("datadir") and meta.get("pair"):
        built = FeatherDataHandler(Path(meta["datadir"]))
        candle = CandleType.FUTURES if meta.get("trading_mode") == "futures" else CandleType.SPOT
        if built._pair_data_filename(Path(meta["datadir"]), meta["pair"], "1d", candle).is_file():
            return built.ohlcv_load(meta["pair"], "1d", candle, fill_missing=False)
    src_dir = user_data / "data" / SOURCE_EXCHANGE
    src = FeatherDataHandler(src_dir)
    for tf in ("1d", "1h", "1m"):
        if src._pair_data_filename(src_dir, SOURCE_PAIR, tf, CandleType.SPOT).is_file():
            df = src.ohlcv_load(SOURCE_PAIR, tf, CandleType.SPOT, fill_missing=False)
            return df if tf == "1d" else _resample(df, "1d")
    return None


def analyze_run(user_data: Path, result: Path) -> dict:
    """analyze() for a finished run, with the run's own facts attached."""
    meta_path = result.with_name(result.stem + ".fragility.json")
    meta = json.loads(meta_path.read_text(encoding="utf-8")) if meta_path.is_file() else {}
    _, entry = load_result(result)
    trades = [t for t in entry.get("trades") or [] if not t.get("is_open")]
    out = analyze(trades, btc_daily(user_data, meta))
    out.update({
        "strategy": meta.get("strategy"), "config": meta.get("config"), "pair": meta.get("pair"),
        "datadir": meta.get("datadir"), "trading_mode": meta.get("trading_mode"), "detail": bool(meta.get("detail")),
        "source": SOURCE_LABEL, "created": meta.get("created"), "result": result.name,
        "timeframe": entry.get("timeframe"), "start": str(entry.get("backtest_start") or "")[:10],
        "end": str(entry.get("backtest_end") or "")[:10],
    })
    return out


if __name__ == "__main__":
    sys.exit(_run_plan(sys.argv[1]))
