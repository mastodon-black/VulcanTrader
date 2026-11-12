"""Live/dry-run data feed for CME-style futures via Rithmic.

Rithmic is NOT wired for real order routing here - `has["createOrder"]` etc.
are all `False`. This is a **data-only** exchange: it streams real-time bars
and last-trade ticks for the same CME contracts `Futuressim`
(`futures_sim.py`) backtests, so `dry_run` trading can paper-trade them
against live prices. Order simulation goes through the generic dry-run fill
engine every other exchange in this package already uses; the contract
multiplier/PnL math is the same `contractSize`/`amount_to_contract_precision`
machinery `Futuressim` was cross-checked against a NautilusTrader port for
(see `nautilus_compare/`) - see `futures_common.py` for the shared economics.

Uses the `async-rithmic` package (Rithmic's Protocol Buffer API over
websockets), not ccxt - Rithmic is a futures/CME-focused venue ccxt doesn't
cover. `async-rithmic` is imported lazily (inside the functions/methods that
actually need it) so this module - and the rest of the bot - stays importable
for users who never configure `"exchange": {"name": "rithmic"}`, matching
this repo's existing lazy-import convention for heavy optional dependencies
(see `VulcanTrader/util/_ccxt_lazy.py`).

Concurrency model
------------------
The base `Exchange` object drives its own asyncio loop with
`self.loop.run_until_complete(...)` once per bot tick - that loop is not
continuously running between ticks. A persistent websocket client with its
own background tasks (heartbeats, recv loop, auto-reconnect), which is
exactly what `async_rithmic.RithmicClient` is, cannot live on that loop.
`_RithmicFeed` solves this the same way `VulcanTrader.exchange.exchange_ws.
ExchangeWS` does for ccxt-pro: a dedicated background `Thread` runs its own
`asyncio` event loop (`run_forever`), and the `RithmicClient` connection,
subscriptions, and per-(pair, timeframe) OHLCV buffer all live there.
`RithmicCCXTAdapter.fetch_ohlcv`/`fetch_ticker` (awaited on the *bot's* loop)
read that buffer through an `RLock`, and bridge to the feed's loop for
historical backfill via `asyncio.run_coroutine_threadsafe` +
`asyncio.wrap_future` so the bot's own loop is never blocked waiting on it.

Known simplifications (see the implementation plan for the full rationale):
  - Per-symbol Rithmic venue routing codes in `futures_specs.py`
    (`rithmic_exchange`) are seeded from standard published CME Group
    product groupings, not yet verified against a live `search_symbols()`/
    `list_exchanges()` call.
  - Front-month rollover resubscribes to the new contract but does not stitch
    price continuity across the roll; an open dry-run position keeps
    tracking whatever contract it entered on until closed.
  - ETF/CFD placeholder symbols in `futures_specs.py` have no
    `rithmic_exchange` mapping (Rithmic doesn't carry NYSE/Nasdaq equity
    data) and are silently excluded from live subscription with a warning.
  - **Fleet sharing is per-process, not per-machine.** `Exchange.__init__`
    unconditionally constructs this class's `_api_async` adapter regardless
    of `data_server.py` (which only intercepts *DataProvider*-level candle/
    trade/funding fetches for other exchanges, via `trader_bot.py`'s
    `_setup_data_server_client` - it never plugs into Rithmic at all). Left
    unaddressed, every bot process would open its own independent Rithmic
    session, which both wastes redundant connections and risks Rithmic's own
    per-login session limits (many retail-tier accounts only allow one
    active login per system at a time). The module-level
    `_get_or_create_feed`/`_release_feed` registry (just above
    `RithmicCCXTAdapter`) fixes this *within one process*: every `Rithmic`
    exchange object constructed with the same (user, system_name, gateway)
    - e.g. multiple bots run together under `single_process_mode` - shares
    one `_RithmicFeed`/one real login, reference-counted so it's only
    disconnected once the last bot using it releases it. It does NOT cover
    multiple separate OS processes each running their own bot against the
    same login - that would need a data_server.py-style cross-process
    service, out of scope here.
"""

from __future__ import annotations

import logging
from collections import deque
from datetime import UTC, datetime
from threading import Event, RLock, Thread
from typing import TYPE_CHECKING, Any

from ccxt import TICK_SIZE

from VulcanTrader.enums import MarginMode, TradingMode
from VulcanTrader.exchange.exchange import Exchange
from VulcanTrader.exchange.exchange_types import TraderHas
from VulcanTrader.exchange.exchange_utils_timeframe import timeframe_to_msecs
from VulcanTrader.exchange.futures_common import TIMEFRAMES, all_markets
from VulcanTrader.exchange.futures_common import get_fee_per_contract as _get_fee_per_contract
from VulcanTrader.exchange.futures_common import get_futures_fee_pct as _get_futures_fee_pct
from VulcanTrader.exchange.futures_common import market_is_open as _market_is_open
from VulcanTrader.exchange.futures_specs import CFD_CONTRACT_SPECS, FUTURES_CONTRACT_SPECS
from VulcanTrader.util.exceptions import OperationalException, TemporaryError

if TYPE_CHECKING:
    import asyncio

logger = logging.getLogger(__name__)

# How many bars/ticks to keep per (root, timeframe) - generous enough to
# cover a fresh backfill + a long-running bot's worth of live bars without
# unbounded growth.
_BUFFER_LEN = 5000

# freqtrade/VulcanTrader timeframe string -> minutes, for the subset Rithmic's
# MINUTE_BAR service can produce directly (no client-side resampling).
_MINUTE_TIMEFRAMES = {"1m": 1, "5m": 5, "15m": 15, "30m": 30, "1h": 60, "4h": 240}


def _timeframe_to_bar_type(timeframe: str) -> tuple[Any, int]:
    """`(TimeBarType, periods)` for a freqtrade-style timeframe string.
    Imports `async_rithmic` lazily - see the module docstring."""
    from async_rithmic import TimeBarType

    if timeframe == "1d":
        return TimeBarType.DAILY_BAR, 1
    minutes = _MINUTE_TIMEFRAMES.get(timeframe)
    if minutes is None:
        raise OperationalException(f"Rithmic does not support timeframe {timeframe!r}")
    return TimeBarType.MINUTE_BAR, minutes


def _venue_for(root: str) -> str | None:
    spec = FUTURES_CONTRACT_SPECS.get(root)
    return spec.get("rithmic_exchange") if spec else None


class _RithmicFeed:
    """Owns one `async_rithmic.RithmicClient` on a dedicated background
    thread/event loop. See the module docstring for why this can't just be
    an `async def` method awaited on the bot's own loop.

    One instance is shared by every `Rithmic` exchange object in this
    *process* that was configured with the same login (see the module-level
    `_get_or_create_feed`/`_release_feed` registry below) - root resolution
    (front-month contract + last-trade subscription) is therefore lazy and
    per-root (`ensure_root_ready`), not done once up front for a fixed list,
    since different bots sharing one connection can each bring their own,
    possibly-non-overlapping set of root symbols.
    """

    def __init__(self, credentials: dict[str, str]) -> None:
        self._credentials = credentials

        self._lock = RLock()
        self._bars: dict[tuple[str, str], deque] = {}
        self._last_price: dict[str, tuple[float, int]] = {}
        self._front_month: dict[str, str] = {}
        self._contract_to_root: dict[str, str] = {}
        self._unresolvable_roots: set[str] = set()  # warned-about, no rithmic_exchange mapping
        self._subscribed: set[tuple[str, str]] = set()
        self._sub_key_by_type_period: dict[tuple[int, int], str] = {}

        self._client: Any = None
        self._loop: asyncio.AbstractEventLoop | None = None
        self._stopped = False
        self._ready = Event()

        self._thread = Thread(name="rithmic_feed", target=self._run, daemon=True)
        self._thread.start()

    # -- background thread -------------------------------------------------

    def _run(self) -> None:
        import asyncio

        self._loop = asyncio.new_event_loop()
        asyncio.set_event_loop(self._loop)
        try:
            self._loop.run_until_complete(self._main())
        except Exception:
            logger.exception("Rithmic feed loop crashed")
        finally:
            try:
                pending = asyncio.all_tasks(self._loop)
                for task in pending:
                    task.cancel()
                if pending:
                    self._loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
                self._loop.run_until_complete(self._loop.shutdown_asyncgens())
            except Exception:
                pass
            self._loop.close()

    async def _main(self) -> None:
        """Once `RithmicClient.connect()` succeeds once, `async_rithmic`'s own
        per-plant reconnect loop (`ReconnectionSettings`, infinite retries by
        default) transparently handles later drops - including
        re-subscribing everything from its own `_subscriptions` bookkeeping
        on reconnect. That machinery only starts after a first successful
        connect, though; a failed *first* attempt (transient network hiccup,
        DNS blip) would otherwise kill this thread permanently for the rest
        of the bot's runtime, so that specific case gets its own bounded-
        backoff retry loop here."""
        import asyncio

        from async_rithmic import RithmicClient

        backoff = 5.0
        while not self._stopped:
            try:
                self._client = RithmicClient(
                    user=self._credentials["user"],
                    password=self._credentials["password"],
                    system_name=self._credentials["system_name"],
                    app_name=self._credentials["app_name"],
                    app_version=self._credentials["app_version"],
                    url=self._credentials["gateway"],
                )
                self._client.on_time_bar += self._on_time_bar
                self._client.on_tick += self._on_tick
                await self._client.connect()
                self._ready.set()
                break
            except Exception:
                logger.exception(f"Rithmic: initial connect/subscribe failed - retrying in {backoff:.0f}s")
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 300.0)

        while not self._stopped:
            await asyncio.sleep(3600)
            try:
                await self._check_rollover()
            except Exception:
                logger.exception("Rithmic: front-month rollover check failed")

    async def ensure_root_ready(self, root: str) -> None:
        """Resolves the front-month contract and subscribes to last-trade
        ticks for `root` the first time ANY bot sharing this connection asks
        for it. Idempotent and safe to call every time a bot touches `root`
        - cheap no-op once resolved. Bridges onto this feed's own loop/thread
        since it's called from the bot's own loop via the adapter."""
        import asyncio

        with self._lock:
            if root in self._front_month or root in self._unresolvable_roots:
                return

        venue = _venue_for(root)
        if venue is None:
            with self._lock:
                self._unresolvable_roots.add(root)
            logger.warning(
                f"Rithmic: {root!r} has no CME venue routing (ETF/CFD placeholder or "
                "unmapped symbol) and cannot be live-subscribed - backtest-only via 'futuressim'."
            )
            return

        async def _do() -> None:
            from async_rithmic import DataType

            contract = await self._client.get_front_month_contract(root, venue)
            with self._lock:
                self._front_month[root] = contract
                self._contract_to_root[contract] = root
            await self._client.subscribe_to_market_data(contract, venue, DataType.LAST_TRADE)

        try:
            fut = asyncio.run_coroutine_threadsafe(_do(), self._loop)
            await asyncio.wrap_future(fut)
        except Exception:
            logger.exception(f"Rithmic: could not resolve/subscribe front-month contract for {root}/{venue}")

    async def _check_rollover(self) -> None:
        from async_rithmic import DataType

        with self._lock:
            roots = list(self._front_month.keys())
        for root in roots:
            venue = _venue_for(root)
            if venue is None:
                continue
            old_contract = self._front_month.get(root)
            try:
                new_contract = await self._client.get_front_month_contract(root, venue)
            except Exception:
                logger.exception(f"Rithmic: front-month re-check failed for {root}/{venue}")
                continue
            if not old_contract or new_contract == old_contract:
                continue

            logger.warning(
                f"Rithmic: front-month contract for {root} rolled {old_contract} -> "
                f"{new_contract}; resubscribing. Any open dry-run position keeps tracking "
                f"{old_contract} until closed - no price-continuity stitching is performed."
            )
            with self._lock:
                tfs_for_root = [tf for (r, tf) in self._subscribed if r == root]
            for tf in tfs_for_root:
                bar_type, periods = _timeframe_to_bar_type(tf)
                try:
                    await self._client.unsubscribe_from_time_bar_data(old_contract, venue, bar_type, periods)
                except Exception:
                    pass
                try:
                    await self._client.subscribe_to_time_bar_data(new_contract, venue, bar_type, periods)
                except Exception:
                    logger.exception(f"Rithmic: failed to subscribe {tf} bars for rolled contract {new_contract}")
            try:
                await self._client.unsubscribe_from_market_data(old_contract, venue, DataType.LAST_TRADE)
            except Exception:
                pass
            try:
                await self._client.subscribe_to_market_data(new_contract, venue, DataType.LAST_TRADE)
            except Exception:
                pass
            with self._lock:
                self._contract_to_root.pop(old_contract, None)
                self._contract_to_root[new_contract] = root
                self._front_month[root] = new_contract

    # -- event handlers (run on the feed's own loop/thread) ----------------

    async def _on_time_bar(self, data: dict[str, Any]) -> None:
        contract = data.get("symbol")
        with self._lock:
            root = self._contract_to_root.get(contract)
            tf = self._sub_key_by_type_period.get((int(data.get("type", -1)), int(data.get("period", -1))))
        if root is None or tf is None:
            return
        try:
            ts_ms = int(float(data["marker"]) * 1000)
            row = [
                ts_ms,
                float(data["open_price"]),
                float(data["high_price"]),
                float(data["low_price"]),
                float(data["close_price"]),
                float(data.get("volume") or 0.0),
            ]
        except (KeyError, TypeError, ValueError):
            logger.debug(f"Rithmic: malformed time bar payload for {contract}: {data}")
            return
        with self._lock:
            buf = self._bars.setdefault((root, tf), deque(maxlen=_BUFFER_LEN))
            if buf and buf[-1][0] == ts_ms:
                buf[-1] = row
            else:
                buf.append(row)

    async def _on_tick(self, data: dict[str, Any]) -> None:
        from async_rithmic import DataType

        if data.get("data_type") != DataType.LAST_TRADE:
            return
        contract = data.get("symbol")
        price = data.get("trade_price")
        if price is None:
            return
        with self._lock:
            root = self._contract_to_root.get(contract)
        if root is None:
            return
        ts = data.get("datetime")
        ts_ms = int(ts.timestamp() * 1000) if isinstance(ts, datetime) else int(datetime.now(tz=UTC).timestamp() * 1000)
        with self._lock:
            self._last_price[root] = (float(price), ts_ms)

    # -- called from the bot's own loop/thread ------------------------------

    async def ensure_ready(self, timeout: float = 10.0) -> None:
        """Waits (off-loop, via an executor thread) for the initial
        connect + front-month resolution to finish. Safe to call repeatedly;
        returns immediately once ready."""
        import asyncio

        loop = asyncio.get_running_loop()
        await loop.run_in_executor(None, self._ready.wait, timeout)

    async def ensure_timeframe_subscribed(self, root: str, timeframe: str) -> bool:
        """Live-subscribes to `timeframe` bars for `root` the first time it's
        requested. Returns True the first time (caller should backfill),
        False on subsequent calls."""
        import asyncio

        key = (root, timeframe)
        with self._lock:
            if key in self._subscribed:
                return False
            self._subscribed.add(key)

        venue = _venue_for(root)
        contract = self._front_month.get(root)
        if venue is None or contract is None or self._client is None:
            return True

        bar_type, periods = _timeframe_to_bar_type(timeframe)
        with self._lock:
            self._sub_key_by_type_period[(int(bar_type), periods)] = timeframe

        async def _do() -> None:
            await self._client.subscribe_to_time_bar_data(contract, venue, bar_type, periods)

        try:
            fut = asyncio.run_coroutine_threadsafe(_do(), self._loop)
            await asyncio.wrap_future(fut)
        except Exception:
            logger.exception(f"Rithmic: failed to subscribe to {timeframe} bars for {root} ({contract})")
        return True

    def get_buffered_ohlcv(self, root: str, timeframe: str) -> list[list[float]]:
        with self._lock:
            buf = self._bars.get((root, timeframe))
            return list(buf) if buf else []

    def get_last_price(self, root: str) -> tuple[float, int] | None:
        """Most recent last-trade tick price for `root`, if any has arrived
        yet. Falls back to the most recent bar close across *any*
        timeframe currently buffered for `root` - not just whichever
        timeframe a strategy happens to call `fetch_ohlcv` with - since
        last-trade ticks are subscribed per-root independently of which
        OHLCV timeframes get requested."""
        with self._lock:
            last = self._last_price.get(root)
            if last is not None:
                return last
            best: tuple[float, int] | None = None
            for (r, _tf), buf in self._bars.items():
                if r != root or not buf:
                    continue
                ts_ms, _o, _h, _l, close, _v = buf[-1]
                if best is None or ts_ms > best[1]:
                    best = (close, ts_ms)
            return best

    async def backfill_ohlcv(
        self, root: str, timeframe: str, since_ms: int, until_ms: int
    ) -> list[list[float]]:
        """Historical replay for `root`/`timeframe` between the given bounds,
        bridged onto the feed's own loop/thread. Merges the result into the
        live buffer (so a later call in the same window is served from
        memory) and returns the merged, time-sorted buffer."""
        import asyncio

        venue = _venue_for(root)
        contract = self._front_month.get(root)
        if self._client is None or venue is None or contract is None:
            return self.get_buffered_ohlcv(root, timeframe)

        bar_type, periods = _timeframe_to_bar_type(timeframe)
        start = datetime.fromtimestamp(since_ms / 1000, tz=UTC)
        end = datetime.fromtimestamp(until_ms / 1000, tz=UTC)

        async def _do() -> list[dict[str, Any]]:
            return await self._client.get_historical_time_bars(
                contract, venue, start, end, bar_type, periods, idle_timeout=10.0,
            )

        try:
            fut = asyncio.run_coroutine_threadsafe(_do(), self._loop)
            bars = await asyncio.wrap_future(fut)
        except Exception:
            logger.exception(f"Rithmic: historical backfill failed for {root}/{timeframe}")
            bars = []

        rows: list[list[float]] = []
        for bar in bars:
            try:
                ts_ms = int(float(bar["marker"]) * 1000)
                rows.append([
                    ts_ms,
                    float(bar["open_price"]),
                    float(bar["high_price"]),
                    float(bar["low_price"]),
                    float(bar["close_price"]),
                    float(bar.get("volume") or 0.0),
                ])
            except (KeyError, TypeError, ValueError):
                continue

        with self._lock:
            existing = list(self._bars.get((root, timeframe), ()))
            known_ts = {r[0] for r in existing}
            combined = existing + [r for r in rows if r[0] not in known_ts]
            combined.sort(key=lambda r: r[0])
            merged = deque(combined[-_BUFFER_LEN:], maxlen=_BUFFER_LEN)
            self._bars[(root, timeframe)] = merged
            return list(merged)

    async def aclose(self) -> None:
        import asyncio

        self._stopped = True
        if self._client is not None and self._loop is not None and not self._loop.is_closed():
            async def _do() -> None:
                try:
                    await self._client.disconnect()
                except Exception:
                    logger.exception("Rithmic: error during disconnect")

            try:
                fut = asyncio.run_coroutine_threadsafe(_do(), self._loop)
                await asyncio.wrap_future(fut)
            except Exception:
                logger.exception("Rithmic: error scheduling disconnect")

        if self._loop is not None and not self._loop.is_closed():
            self._loop.call_soon_threadsafe(self._loop.stop)
        self._thread.join(timeout=5.0)


# -- per-process shared-connection registry --------------------------------
#
# Every `Rithmic` exchange object constructed in this *process* with the
# same (user, system_name, gateway) shares one `_RithmicFeed` - one real
# Rithmic login - instead of each bot opening its own. This covers the
# common case this project actually runs (multiple bots sharing one process
# via `single_process_mode`); it does NOT cover multiple separate OS
# processes each running their own bot against the same login (that would
# need a data_server.py-style cross-process service - not attempted here,
# see the module docstring).
_feed_registry_lock = RLock()
_feed_registry: dict[tuple[str, str, str], _RithmicFeed] = {}
_feed_refcounts: dict[tuple[str, str, str], int] = {}


def _feed_key(credentials: dict[str, str]) -> tuple[str, str, str]:
    return (credentials["user"], credentials["system_name"], credentials["gateway"])


def _get_or_create_feed(credentials: dict[str, str]) -> _RithmicFeed:
    """Returns this process's shared `_RithmicFeed` for this login, creating
    it (and starting its background thread) on first use. Each call must be
    paired with exactly one `_release_feed` call when that caller is done
    with it."""
    key = _feed_key(credentials)
    with _feed_registry_lock:
        feed = _feed_registry.get(key)
        if feed is None:
            feed = _RithmicFeed(credentials)
            feed._registry_key = key  # type: ignore[attr-defined]
            _feed_registry[key] = feed
            _feed_refcounts[key] = 0
        _feed_refcounts[key] += 1
        return feed


async def _release_feed(feed: _RithmicFeed) -> None:
    """Releases one reference to `feed` (from a prior `_get_or_create_feed`
    call). Only actually disconnects and tears down the background thread
    once every bot sharing it in this process has released it."""
    key = getattr(feed, "_registry_key", None)
    should_close = True
    if key is not None:
        with _feed_registry_lock:
            _feed_refcounts[key] = _feed_refcounts.get(key, 1) - 1
            should_close = _feed_refcounts[key] <= 0
            if should_close:
                _feed_registry.pop(key, None)
                _feed_refcounts.pop(key, None)
    if should_close:
        await feed.aclose()


class RithmicCCXTAdapter:
    """Minimal ccxt-like adapter for Rithmic, following the same pattern as
    `DriftCCXTAdapter` (`drift.py`): a plain Python class exposing only the
    subset of the ccxt interface the base `Exchange` code actually calls for
    live trading (`fetch_ohlcv`/`fetch_ticker`/`fetch_tickers`), backed here
    by a `_RithmicFeed` instead of HTTP.
    """

    def __init__(self, parent: "Rithmic", *, is_async: bool, feed: _RithmicFeed | None) -> None:
        self._parent = parent
        self._is_async = is_async
        self._feed = feed  # only the async adapter (self._api_async) owns a live feed

        self.id = "rithmic"
        self.name = "Rithmic"
        self.markets: dict[str, Any] = all_markets()
        self.symbols: list[str] = list(self.markets)

        self.precisionMode = TICK_SIZE
        self.timeframes = dict(TIMEFRAMES)
        self.options = {"timeframes": {"swap": self.timeframes}}

        self.has = {
            "fetchOHLCV": True,
            "fetchTicker": True,
            "fetchTickers": True,
            "fetchL2OrderBook": False,
            "fetchTrades": False,
            # Data-only for now - see the module docstring.
            "createOrder": False,
            "createMarketOrder": False,
            "createLimitOrder": False,
            "cancelOrder": False,
            "fetchOrder": False,
            "fetchBalance": False,
        }
        self.features = {"swap": {"linear": {"fetchOHLCV": {"limit": 1000}}}}

        # Truthy only when we own a real feed - Exchange.close() gates on
        # `self._api_async.session` before awaiting our close(), same as any
        # real ccxt-pro session check.
        self.session = feed

    def load_markets(self, *args, **kwargs) -> dict[str, Any]:
        return self.markets

    async def close(self) -> None:
        if self._feed is not None:
            await _release_feed(self._feed)

    async def fetch_ohlcv(
        self,
        symbol: str,
        timeframe: str = "1m",
        since: int | None = None,
        limit: int | None = None,
        params: dict[str, Any] | None = None,
    ) -> list[list]:
        feed = self._feed
        if feed is None:
            return []
        await feed.ensure_ready()
        await feed.ensure_root_ready(symbol)

        candle_limit = int(limit or 500)
        tf_ms = timeframe_to_msecs(timeframe)
        now_ms = int(datetime.now(tz=UTC).timestamp() * 1000)
        start_ms = since if since is not None else now_ms - candle_limit * tf_ms

        first_use = await feed.ensure_timeframe_subscribed(symbol, timeframe)
        buffered = feed.get_buffered_ohlcv(symbol, timeframe)
        # `start_ms` is (now - N*tf_ms) and keeps creeping forward every call,
        # so comparing it against the oldest buffered row with a strict `>`
        # would false-positive by a fraction of a bar on almost every tick,
        # triggering a historical-replay network call each time instead of
        # trusting the live-fed buffer. Only treat it as a real gap - worth a
        # backfill - once it's more than one full bar behind.
        gap_ms = (buffered[0][0] - start_ms) if buffered else None
        if first_use or gap_ms is None or gap_ms > tf_ms:
            buffered = await feed.backfill_ohlcv(symbol, timeframe, start_ms, now_ms)

        rows = [row for row in buffered if row[0] >= start_ms]
        if limit is not None:
            rows = rows[-candle_limit:]
        return rows

    async def fetch_ticker(self, symbol: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        feed = self._feed
        if feed is None:
            raise OperationalException("Rithmic ticker requested on the metadata-only (sync) adapter")
        await feed.ensure_ready()
        await feed.ensure_root_ready(symbol)

        last = feed.get_last_price(symbol)
        if last is None:
            raise TemporaryError(f"Rithmic: no live price yet for {symbol}")

        price, ts_ms = last
        return {
            "symbol": symbol,
            "timestamp": ts_ms,
            "datetime": datetime.fromtimestamp(ts_ms / 1000, tz=UTC).isoformat(),
            "last": price,
            "close": price,
            "bid": price,
            "ask": price,
            "info": {},
        }

    async def fetch_tickers(
        self, symbols: list[str] | None = None, params: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        wanted = symbols if symbols is not None else self.symbols
        out: dict[str, Any] = {}
        for sym in wanted:
            try:
                out[sym] = await self.fetch_ticker(sym)
            except Exception:
                continue
        return out


class Rithmic(Exchange):
    """See the module docstring. Config name: `"exchange": {"name": "rithmic"}`.

    Required `exchange` config fields: `key` (Rithmic user id), `password`,
    `rithmic_system_name`, `rithmic_gateway` (`host:port`). Optional:
    `rithmic_app_name`/`rithmic_app_version`. `pair_whitelist` entries are
    plain CME root symbols (`"ES"`, `"NQ"`, ...), same as `futuressim`.
    """

    # Inherited from Futuressim for interface consistency, but inert either
    # way: `create_dry_run_order` fills at whatever `rate` the caller passes
    # in (the current live ticker/orderbook price at signal time - see
    # `entry_pricing`/`exit_pricing`), not a historical bar's open or close.
    # "Which price within a replayed bar" is a backtest-only concept: nothing
    # in trader_bot.py's live/dry-run path reads this attribute, and neither
    # does Futuressim's own consumer (rust_backtest.py never references it
    # either, despite its docstring's claim - a pre-existing, separate gap in
    # Futuressim, not something Rithmic needs to (or can) fix, since Rithmic
    # never backtests).
    use_close_price_fills: bool = True

    trader_has: TraderHas = {
        "stoploss_on_exchange": False,
        "order_time_in_force": ["GTC"],
        "trades_pagination": "time",
        "trades_pagination_arg": "since",
        "trades_has_history": False,
        "ohlcv_has_history": True,
        "ohlcv_partial_candle": False,
        "ohlcv_require_since": False,
        "ohlcv_candle_limit": 1000,
        "ws_enabled": False,
        "needs_trading_fees": False,
        "tickers_have_price": True,
        "ccxt_futures_name": "swap",
    }
    trader_has_futures: TraderHas = {
        "uses_leverage_tiers": False,
    }

    # No MarginMode.CROSS/ISOLATED entry - margin/liquidation isn't modeled
    # (this is a dry-run data feed, not a broker connection), so a config for
    # this exchange should simply omit `margin_mode`.
    _supported_trading_mode_margin_pairs: list[tuple[TradingMode, MarginMode]] = [
        (TradingMode.FUTURES, MarginMode.NONE),
    ]

    def _init_ccxt(
        self, exchange_config: dict[str, Any], sync: bool, ccxt_kwargs: dict[str, Any]
    ) -> RithmicCCXTAdapter:
        feed = None
        if not sync:
            skipped_cfd = [
                p for p in exchange_config.get("pair_whitelist", [])
                if p in CFD_CONTRACT_SPECS
            ]
            if skipped_cfd:
                logger.warning(
                    f"Rithmic: {skipped_cfd} are CFD/ETF placeholders (no CME contract) - "
                    "backtest-only via 'futuressim', not tradable live here."
                )
            credentials = {
                "user": exchange_config.get("key", ""),
                "password": exchange_config.get("password", ""),
                "system_name": exchange_config.get("rithmic_system_name", ""),
                "app_name": exchange_config.get("rithmic_app_name", "VulcanTrader"),
                "app_version": exchange_config.get("rithmic_app_version", "1.0"),
                "gateway": exchange_config.get("rithmic_gateway", ""),
            }
            missing = [k for k in ("user", "password", "system_name", "gateway") if not credentials[k]]
            if missing:
                raise OperationalException(
                    f"Rithmic exchange config is missing required field(s): {missing}. Set "
                    "'key'/'password' plus 'rithmic_system_name'/'rithmic_gateway' (and "
                    "optionally 'rithmic_app_name'/'rithmic_app_version') under the "
                    "'exchange' config block."
                )
            # Shared per-process, keyed by login identity - see the registry
            # right above RithmicCCXTAdapter. Multiple bots (e.g. under
            # single_process_mode) configured with the same credentials get
            # the same _RithmicFeed instance instead of each opening their
            # own Rithmic session.
            feed = _get_or_create_feed(credentials)
        return RithmicCCXTAdapter(self, is_async=not sync, feed=feed)

    def reload_markets(self, force: bool = False, *, load_leverage_tiers: bool = True) -> None:
        """Static market metadata (mirrors `Futuressim`) - decoupled from the
        live feed becoming ready so startup never blocks on it."""
        if self._markets and not force:
            return
        self._markets = dict(self._api.markets)

    def get_futures_fee_pct(self, pair: str, price: float, *, is_maker: bool = False) -> float | None:
        """See `futures_common.get_futures_fee_pct` (shared with `Futuressim`)."""
        return _get_futures_fee_pct(pair, price, is_maker=is_maker)

    def get_fee_per_contract(self, pair: str) -> float | None:
        """See `futures_common.get_fee_per_contract` (shared with `Futuressim`/`Ninjatrader`) -
        consumed by the native Rust multi-pair engine, which re-divides by its own fill
        price per trade."""
        return _get_fee_per_contract(pair)

    def get_fee(
        self,
        symbol: str,
        order_type: str = "",
        side: str = "",
        amount: float = 1,
        price: float = 1,
        taker_or_maker: str = "maker",
    ) -> float:
        """CME-style flat per-contract commission, converted to a %-of-notional rate at
        `price` - see `Futuressim.get_fee`'s docstring (identical, shared economics)."""
        pct = _get_futures_fee_pct(symbol, price, is_maker=(taker_or_maker == "maker"))
        if pct is not None:
            return pct
        return super().get_fee(
            symbol, order_type=order_type, side=side, amount=amount, price=price,
            taker_or_maker=taker_or_maker,
        )

    def market_is_open(self, pair: str, timestamp: Any) -> bool:
        """See `futures_common.market_is_open` (shared with `Futuressim`)."""
        return _market_is_open(pair, timestamp)
