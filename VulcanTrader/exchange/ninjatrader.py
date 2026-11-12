"""Live order-routing + data exchange for NinjaTrader 8, for its three native
connection types: Sim101 (paper), a real live brokerage connection, and
Market Replay - all exposed identically here as "whichever NT8 `Account` this
config points at", exactly like a human choosing a connection in NT8's
Control Center. Unlike `rithmic.py`/`drift.py` (price data only - orders are
always paper-filled in software, `has["createOrder"] = False`), this is the
first exchange in this package that places **real** orders on an external
venue: `has["createOrder"]/["cancelOrder"]` are `True`, and `dry_run: false`
in a VulcanTrader config actually submits a live order into NT8.

Architecture
------------
NT8 is a Windows desktop .NET app with no public socket API of its own for
either market data or order routing from an external process. This module
talks to a small companion **NinjaScript AddOn** (plain C#, shipped alongside
this file at `ninjatrader_addon/VulcanTraderBridge.cs`) compiled into NT8's
NinjaTrader.Custom.dll - on Windows by pasting it into the NinjaScript Editor
and pressing F5; on Linux `setup-ninjatrader.sh` (repo root) installs NT8 under
Wine and deploys + compiles it headless (see AGENTS.md "NinjaTrader 8 on Linux
(Wine)"). The AddOn opens a local TCP
listener inside the running NT8 process and speaks a bespoke newline-delimited
JSON protocol (documented in full at the top of that .cs file and mirrored by
the message handling below): bars/ticks pushed out for subscribed
instruments, orders/cancels pushed in, order and execution updates and
periodic account-state (cash/positions) pushed out. There is no vendor
library here the way `async-rithmic` is for `rithmic.py` - the protocol and
both ends of it are hand-rolled for this project.

Status (2026-11-11): compiled and running against NT8 8.1.8.4 (Simulation
connection, Sim101) under Wine; market data, historical backfill and the
account-state push are verified live. Order placement has not yet been
exercised by a non-dry-run bot. Two AddOn bugs found live and fixed: a
subscribe for an instrument NT8 can't resolve used to throw out of the read
loop and drop the whole shared connection, and backfill sized
`BarsRequest(instrument, int)` as days back when that int is BARS back (so
every backfill returned 1-4 bars).

Front-month / contract resolution: deliberately **not** attempted here - a
`pair_whitelist` entry is the exact, fully-qualified NT8 contract month, as a
**dash-separated alias** of NT8's own instrument string: NT8 names an
instrument "SIL 12-26" (root, space, MM-YY), but a literal space in a pair
string gets silently dropped by `pairlist_helpers.py::expand_pairlist`'s
wildcard-match sanity filter (`^[A-Za-z0-9:/-]+$`, no space) - confirmed live
2026-10-12: the exchange/market side was entirely correct and the active
whitelist was still always empty. `pair_whitelist` entries therefore use a
dash where NT8 uses a space - "SIL-12-26", not "SIL 12-26" - and
`_to_nt_instrument`/`_to_alias` translate at the `_NinjaTraderBridge`
boundary; `NinjatraderCCXTAdapter` and the AddOn's own wire protocol never
see the alias, only the real NT8 name. Updating the contract month as it
rolls is the user's responsibility (config edit), same documented
simplification `rithmic.py` makes for its own front-month handling. Contract
economics (point multiplier, tick size, flat per-contract commission) are
looked up by the **root** (text before the first dash in the alias) in the
same `futures_specs.py` table `Rithmic`/`Futuressim` use, so adding a new
instrument to a whitelist only works if its root is already in
`FUTURES_CONTRACT_SPECS`/`CFD_CONTRACT_SPECS`.

Concurrency model: identical in shape to `rithmic.py`'s `_RithmicFeed` - a
dedicated background `Thread` runs its own `asyncio` event loop holding the
persistent TCP connection to the AddOn, reconnecting with backoff on drop.
Public **data** methods (`fetch_ohlcv`/`fetch_ticker`/`fetch_tickers`) are
awaited from the bot's own loop via `_api_async`, bridging onto the feed loop
with `asyncio.run_coroutine_threadsafe` + `asyncio.wrap_future`, exactly like
Rithmic. The new part is **order routing**: the base `Exchange` class calls
`create_order`/`cancel_order`/`fetch_order`/`fetch_balance`/`fetch_positions`
*synchronously* on `self._api` (never `self._api_async` - confirmed by
reading `exchange.py`), so those methods block the bot's calling thread with
a plain `concurrent.futures.Future.result(timeout=...)` bridge into the same
feed loop/thread rather than an `await` (there is no running loop to await
from on that side). Both `self._api` and `self._api_async` share one
`_NinjaTraderBridge` instance per (host, port, account) - see the
`_get_or_create_feed`/`_release_feed` registry below, copied from Rithmic's
same per-process sharing rationale (multiple bots under `single_process_mode`
pointed at the same NT8/account share one TCP connection).

Known gaps (documented, not silently assumed away):
  - No on-exchange stoploss (`stoploss_on_exchange = False`, matching every
    other non-ccxt exchange in this package) - the bot manages exits in
    software via its own exit pipeline. NT8 natively supports stop orders,
    but wiring that up is extra bracket/OCO-management complexity not
    attempted in this first pass.
  - `reduceOnly` is passed through to the AddOn as metadata only - NT8 orders
    have no native reduce-only flag the way perpetual-swap exchanges do;
    correctness there still rests on the bot's own position tracking, same
    as it always has for every exchange here without native support.
  - Account balance/positions depend entirely on the AddOn's periodic
    `account_state` push; `fetch_balance`/`fetch_positions` raise
    `TemporaryError` if none has arrived yet rather than guessing.
  - A config for this exchange must set `"data_server": {"enabled": false}`
    (confirmed live 2026-10-12, not theoretical) - the shared data_server
    master is a separate OS subprocess that only gets a minimal auto-written
    config with no per-exchange credentials (`trader_bot.py::
    _write_data_server_config`), so it has no way to learn
    `ninjatrader_account`/host/port and crash-loops trying to build its own
    `Ninjatrader` exchange instance. This module's own `_bridge_registry`
    already provides the in-process connection sharing data_server would
    otherwise add, and the AddOn connection is local-machine-only anyway, so
    nothing is lost by disabling it here.
  - Two INDEPENDENT connections (separate bot processes, or any process not
    sharing this module's `_bridge_registry`) requesting historical bars for
    the SAME instrument at the same time can collide - confirmed live
    2026-10-12: with one bot already running a live `BarsSubscription` on
    "SIL 12-26", a second, separate process's fresh `fetch_ohlcv` backfill
    for that same instrument consistently returned 0 rows, while an
    instrument with no concurrent subscriber (same process, same moment)
    backfilled normally. The running bot's own data was unaffected throughout
    (confirmed via its own analyzed dataframe) - this only hits a SECOND,
    independent connection's one-shot historical request against an
    instrument someone else already has open, not normal single-bot
    operation. Root cause not isolated further (likely NT8-side contention
    on simultaneous `BarsRequest` objects for the same instrument); avoid
    running two separate NinjaTrader bot processes against the same
    instrument against this same NT8/AddOn.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
import uuid
from collections import deque
from concurrent.futures import TimeoutError as FuturesTimeoutError
from datetime import UTC, datetime
from threading import Event, RLock, Thread
from typing import TYPE_CHECKING, Any

from ccxt import TICK_SIZE

from VulcanTrader.enums import MarginMode, TradingMode
from VulcanTrader.exchange.exchange import Exchange
from VulcanTrader.exchange.exchange_types import TraderHas
from VulcanTrader.exchange.exchange_utils_timeframe import timeframe_to_msecs
from VulcanTrader.exchange.futures_common import TIMEFRAMES, build_market
from VulcanTrader.exchange.futures_common import get_fee_per_contract as _get_fee_per_contract
from VulcanTrader.exchange.futures_common import get_futures_fee_pct as _get_futures_fee_pct
from VulcanTrader.exchange.futures_common import market_is_open as _market_is_open
from VulcanTrader.exchange.futures_specs import CFD_CONTRACT_SPECS, FUTURES_CONTRACT_SPECS
from VulcanTrader.util.exceptions import InvalidOrderException, OperationalException, TemporaryError

if TYPE_CHECKING:
    import asyncio as _asyncio_t

logger = logging.getLogger(__name__)

_BUFFER_LEN = 5000

# How long to wait for the AddOn's first order-status push (Working/Filled/
# Rejected) before `create_order`/`cancel_order` give up waiting and return
# whatever's known so far - the order isn't abandoned, just no longer
# blocking the caller; later state still arrives via push and is picked up by
# the next `fetch_order`.
_ORDER_ACK_TIMEOUT_S = 8.0
# A cold startup backfill (35 bots x several timeframes, each ~1000 candles that
# NT8 assembles from 1m history) routinely needs more than 15s - at 15s every
# first attempt timed out at fleet start (114 failures in the first minute,
# 2026-11-11) and bots waited out the retry backoff with no data. Safe to wait
# longer now that backfill_needed() stops the duplicate-request flood.
_BACKFILL_TIMEOUT_S = 60.0
# After a failed or empty backfill, wait this long (doubling per consecutive
# failure, capped) before asking NT8 for that instrument/timeframe again -
# retrying on every fetch is what buried NT8 in duplicate history requests.
_BACKFILL_RETRY_BASE_S = 30.0
_BACKFILL_RETRY_MAX_S = 600.0
# How long the sync adapter's data methods block waiting on the bridge loop
# (covers ensure_ready's 10s plus one backfill).
_SYNC_DATA_TIMEOUT_S = _BACKFILL_TIMEOUT_S + 15.0
# The AddOn pushes account_state every ~2s once connected (see
# PushAccountStates in VulcanTraderBridge.cs) - 15s gives a cold connection
# several pushes' worth of margin before fetch_balance/fetch_positions give
# up and raise.
_ACCOUNT_STATE_TIMEOUT_S = 15.0

_TERMINAL_ORDER_STATUSES = {"Filled", "Cancelled", "Rejected"}

_NT_STATUS_TO_CCXT = {
    "Submitted": "open",
    "Accepted": "open",
    "Working": "open",
    "PartFilled": "open",
    "Filled": "closed",
    "Cancelled": "canceled",
    "Rejected": "canceled",
    "Unknown": "open",
}


def _root_from_instrument(alias: str) -> str:
    """"SIL-12-26" -> "SIL". Splits on the bot-facing alias's dash, not NT8's
    own space - see `_to_nt_instrument`'s docstring for why pair_whitelist
    entries use a dash instead of the space NT8 itself uses. A bare root with
    no dash (continuous/backtest-style symbol) is returned unchanged."""
    return alias.split("-", 1)[0]


def _to_nt_instrument(alias: str) -> str:
    """Bot-facing alias ("SIL-12-26") -> the real NT8 instrument string
    ("SIL 12-26") sent over the wire to the AddOn. NT8 instrument names are
    "ROOT MM-YY" (a literal space between root and contract month), but
    `pairlist_helpers.py::expand_pairlist`'s wildcard-match sanity filter
    only keeps strings matching `^[A-Za-z0-9:/-]+$` - confirmed live
    2026-10-12 that a literal space silently empties the active pair
    whitelist on every cycle even though the market/exchange side is
    otherwise entirely correct (`exchange.markets` has the right entry,
    `market_is_tradable`/`get_pair_quote_currency` all pass - the pair is
    just invisible to anything downstream of `PairListManager.
    verify_whitelist`). `pair_whitelist` entries therefore use a dash in
    NT8's space's place, and this module translates at the bridge boundary
    only - `NinjatraderCCXTAdapter` and the AddOn's own wire protocol are
    untouched; every (instrument, timeframe) key/cache inside
    `_NinjaTraderBridge` stays keyed by the alias, translating to NT8's real
    name only in the literal message sent out, and back again (`_to_alias`)
    for whatever the AddOn echoes in incoming messages.
    """
    return alias.replace("-", " ", 1)


def _to_alias(nt_instrument: str) -> str:
    """Inverse of `_to_nt_instrument` - applied to every "instrument" field
    read off an incoming AddOn message before it's used as a cache key or
    handed back to the ccxt-adapter layer."""
    return nt_instrument.replace(" ", "-", 1)


class _NinjaTraderBridge:
    """Owns one TCP connection to the VulcanTraderBridge NinjaScript AddOn on
    a dedicated background thread/event loop. See the module docstring for
    why this can't just be an `async def` method awaited on the bot's own
    loop, and for the sync-vs-async split in how callers reach it.
    """

    def __init__(self, host: str, port: int, account: str) -> None:
        self._host = host
        self._port = port
        self._account = account

        self._lock = RLock()
        self._bars: dict[tuple[str, str], deque] = {}
        self._last_price: dict[str, tuple[float, int]] = {}
        self._subscribed_bars: set[tuple[str, str]] = set()
        self._subscribed_ticks: set[str] = set()
        self._orders: dict[str, dict[str, Any]] = {}
        self._account_state: dict[str, Any] | None = None

        self._pending_acks: dict[str, "_asyncio_t.Future"] = {}
        self._pending_backfills: dict[str, "_asyncio_t.Future"] = {}
        # Per (instrument, timeframe): earliest `since_ms` a backfill has
        # successfully covered, and failure backoff - see backfill_needed().
        self._backfill_covered_from: dict[tuple[str, str], int] = {}
        self._backfill_retry_at: dict[tuple[str, str], float] = {}
        self._backfill_failures: dict[tuple[str, str], int] = {}
        # (instrument, timeframe) -> the backfill currently in flight, which
        # concurrent callers share instead of sending duplicates.
        self._backfill_inflight: dict[tuple[str, str], Any] = {}

        self._reader: Any = None
        self._writer: Any = None
        self._loop: Any = None
        self._stopped = False
        self._ready = Event()
        self._connected = Event()
        self._account_state_ready = Event()

        self._thread = Thread(name="ninjatrader_bridge", target=self._run, daemon=True)
        self._thread.start()

    # -- background thread --------------------------------------------------

    def _run(self) -> None:
        self._loop = asyncio.new_event_loop()
        asyncio.set_event_loop(self._loop)
        try:
            self._loop.run_until_complete(self._main())
        except Exception:
            # aclose() sets self._stopped then calls loop.stop() from outside
            # this loop, which always makes run_until_complete raise "Event
            # loop stopped before Future completed" here since _main() never
            # finishes on its own (infinite reconnect loop) - that's the
            # expected shape of a clean shutdown, not a real crash, so it's
            # only worth logging when shutdown wasn't requested.
            if not self._stopped:
                logger.exception("Ninjatrader bridge loop crashed")
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
        backoff = 5.0
        while not self._stopped:
            try:
                # Default StreamReader limit is 64KiB/line - confirmed live
                # 2026-10-12 that a single backfill_result line can exceed
                # that (NT8's real BarsRequest(instrument, from, to)
                # constructor rounds the range up to full trading days
                # regardless of how narrow a window was asked for, so even a
                # few-minute request can come back as ~1,380 1m bars on one
                # line) and raises LimitOverrunError, which desyncs the
                # newline-delimited stream and forces a reconnect. The C#
                # side is also fixed to bound payload size to the actual
                # requested window (see HandleBackfillBars), but this is kept
                # too as a second line of defense for any future large
                # response (e.g. a big startup_candle_count backfill).
                self._reader, self._writer = await asyncio.open_connection(
                    self._host, self._port, limit=16 * 1024 * 1024
                )
                await self._send_raw(
                    {"type": "hello", "account": self._account, "client": "VulcanTrader"}
                )
                with self._lock:
                    # NT8-side subscriptions belong to a connection, so a fresh
                    # one has none. Forget ours: the next fetch re-subscribes and,
                    # being first use again, backfills whatever was missed while
                    # disconnected. (Before this, bars silently stopped after any
                    # reconnect - the "already subscribed" sets never cleared.)
                    self._subscribed_bars.clear()
                    self._subscribed_ticks.clear()
                    self._backfill_retry_at.clear()
                    self._backfill_failures.clear()
                self._connected.set()
                self._ready.set()
                backoff = 5.0
                await self._read_loop()
            except Exception:
                # aclose() closing the writer makes _read_loop() raise, same
                # expected-shutdown shape as _run()'s own guard above - only
                # worth the scary log (and the "is NT8 running?" prompt) when
                # this wasn't a deliberate stop.
                if not self._stopped:
                    logger.exception(
                        f"Ninjatrader bridge: connection to {self._host}:{self._port} failed/dropped "
                        f"- retrying in {backoff:.0f}s. Is NT8 running with the VulcanTraderBridge "
                        "AddOn compiled and enabled?"
                    )
            self._connected.clear()
            # Any order awaiting its first ack on a connection that just died
            # should not hang forever - fail them so callers stop blocking.
            with self._lock:
                pending = list(self._pending_acks.values()) + list(self._pending_backfills.values())
                self._pending_acks.clear()
                self._pending_backfills.clear()
            for fut in pending:
                if not fut.done():
                    fut.set_exception(TemporaryError("Ninjatrader bridge disconnected"))
            if self._stopped:
                break
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, 300.0)

    async def _read_loop(self) -> None:
        assert self._reader is not None
        while not self._stopped:
            line = await self._reader.readline()
            if not line:
                raise TemporaryError("Ninjatrader bridge closed the connection")
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except (ValueError, UnicodeDecodeError):
                logger.debug(f"Ninjatrader bridge: malformed line ignored: {line!r}")
                continue
            try:
                await self._dispatch(msg)
            except Exception:
                logger.exception(f"Ninjatrader bridge: error handling message {msg!r}")

    async def _dispatch(self, msg: dict[str, Any]) -> None:
        mtype = msg.get("type")
        if mtype == "bar":
            self._on_bar(msg)
        elif mtype == "tick":
            self._on_tick(msg)
        elif mtype == "order_update":
            self._on_order_update(msg)
        elif mtype == "account_state":
            self._on_account_state(msg)
        elif mtype == "backfill_result":
            self._on_backfill_result(msg)
        elif mtype == "hello_ack":
            if not msg.get("account_exists", True):
                logger.warning(
                    f"Ninjatrader bridge: account {self._account!r} was not found by NT8 - "
                    "check the account name matches exactly (case-sensitive), e.g. 'Sim101'."
                )
        elif mtype == "error":
            logger.warning(f"Ninjatrader bridge reported an error: {msg.get('message')}")
        else:
            logger.debug(f"Ninjatrader bridge: unhandled message type {mtype!r}")

    # -- inbound handlers (run on the feed's own loop/thread) ---------------

    def _on_bar(self, msg: dict[str, Any]) -> None:
        instrument = msg.get("instrument")
        timeframe = msg.get("timeframe")
        if not instrument or not timeframe:
            return
        instrument = _to_alias(instrument)
        try:
            row = [
                int(msg["ts_ms"]),
                float(msg["open"]),
                float(msg["high"]),
                float(msg["low"]),
                float(msg["close"]),
                float(msg.get("volume") or 0.0),
            ]
        except (KeyError, TypeError, ValueError):
            logger.debug(f"Ninjatrader bridge: malformed bar payload: {msg}")
            return
        with self._lock:
            buf = self._bars.setdefault((instrument, timeframe), deque(maxlen=_BUFFER_LEN))
            if buf and buf[-1][0] == row[0]:
                buf[-1] = row
            else:
                buf.append(row)

    def _on_tick(self, msg: dict[str, Any]) -> None:
        instrument = msg.get("instrument")
        price = msg.get("price")
        if not instrument or price is None:
            return
        instrument = _to_alias(instrument)
        ts_ms = int(msg.get("ts_ms") or datetime.now(tz=UTC).timestamp() * 1000)
        with self._lock:
            self._last_price[instrument] = (float(price), ts_ms)

    def _on_order_update(self, msg: dict[str, Any]) -> None:
        order_id = msg.get("order_id")
        if not order_id:
            return
        status = msg.get("status", "Unknown")
        raw_instrument = msg.get("instrument")
        with self._lock:
            self._orders[order_id] = {
                "status": status,
                "instrument": _to_alias(raw_instrument) if raw_instrument else raw_instrument,
                "filled_qty": float(msg.get("filled_qty") or 0.0),
                "avg_fill_price": msg.get("avg_fill_price"),
                "remaining_qty": msg.get("remaining_qty"),
                "reject_reason": msg.get("reject_reason"),
                "nt_order_id": msg.get("nt_order_id"),
            }
            # NT8 (like any real venue) sends intermediate updates
            # (Submitted/Accepted/Working) before a terminal one - resolving
            # the pending ack on the *first* update would make create_order/
            # cancel_order return "open"/stale status almost every time, even
            # when the order actually fills/cancels moments later. Only a
            # terminal status (or the ack timeout elapsing) lets a blocked
            # caller stop waiting; every update still updates self._orders
            # above regardless, so fetch_order always sees the latest state.
            fut = self._pending_acks.pop(order_id, None) if status in _TERMINAL_ORDER_STATUSES else None
        if fut is not None and not fut.done():
            fut.set_result(True)

    def _on_account_state(self, msg: dict[str, Any]) -> None:
        # Each position's "instrument" is whatever the AddOn's own
        # instrumentCanonicalToRequested mapping resolved to - best case the
        # exact NT8 wire-format string the bot last used for that instrument
        # (see VulcanTraderBridge.cs), which still needs the same NT8->alias
        # translation _on_bar/_on_tick/_on_order_update already apply. Missed
        # on the first pass (confirmed live 2026-10-12): fetch_positions()
        # was reporting "SIL 12-26" (NT8 wire format) instead of "SIL-12-26"
        # (the alias the rest of the bot/ccxt-adapter layer expects).
        positions = []
        for p in msg.get("positions") or []:
            p = dict(p)
            instrument = p.get("instrument")
            if instrument:
                p["instrument"] = _to_alias(instrument)
            positions.append(p)
        with self._lock:
            self._account_state = {
                "cash_value": msg.get("cash_value"),
                "net_liquidation": msg.get("net_liquidation"),
                "buying_power": msg.get("buying_power"),
                "positions": positions,
            }
        self._account_state_ready.set()

    def _on_backfill_result(self, msg: dict[str, Any]) -> None:
        req_id = msg.get("req_id")
        with self._lock:
            fut = self._pending_backfills.pop(req_id, None)
        if fut is not None and not fut.done():
            fut.set_result(msg.get("bars") or [])

    # -- outbound (must run on the feed's own loop) --------------------------

    async def _send_raw(self, msg: dict[str, Any]) -> None:
        if self._writer is None:
            raise TemporaryError("Ninjatrader bridge: not connected")
        data = (json.dumps(msg) + "\n").encode("utf-8")
        self._writer.write(data)
        await self._writer.drain()

    async def _send(self, msg: dict[str, Any]) -> None:
        if not self._connected.is_set():
            # Give a just-starting/reconnecting connection a brief window
            # rather than failing instantly on a cold start.
            await asyncio.sleep(0)
            if not self._connected.is_set():
                raise TemporaryError("Ninjatrader bridge: not connected to NT8 AddOn")
        await self._send_raw(msg)

    # -- called from the bot's own loop/thread (data path, awaited) ---------

    async def ensure_ready(self, timeout: float = 10.0) -> None:
        loop = asyncio.get_running_loop()
        await loop.run_in_executor(None, self._ready.wait, timeout)

    async def ensure_subscribed_bars(self, instrument: str, timeframe: str) -> bool:
        key = (instrument, timeframe)
        with self._lock:
            if key in self._subscribed_bars:
                return False
            self._subscribed_bars.add(key)
        try:
            fut = asyncio.run_coroutine_threadsafe(
                self._send(
                    {"type": "subscribe_bars", "instrument": _to_nt_instrument(instrument), "timeframe": timeframe}
                ),
                self._loop,
            )
            await asyncio.wrap_future(fut)
        except Exception:
            # Not subscribed after all - let the next fetch try again.
            with self._lock:
                self._subscribed_bars.discard(key)
            logger.exception(f"Ninjatrader bridge: failed to subscribe bars {instrument}/{timeframe}")
        return True

    async def ensure_subscribed_ticks(self, instrument: str) -> None:
        with self._lock:
            if instrument in self._subscribed_ticks:
                return
            self._subscribed_ticks.add(instrument)
        try:
            fut = asyncio.run_coroutine_threadsafe(
                self._send({"type": "subscribe_ticks", "instrument": _to_nt_instrument(instrument)}), self._loop
            )
            await asyncio.wrap_future(fut)
        except Exception:
            with self._lock:
                self._subscribed_ticks.discard(instrument)
            logger.exception(f"Ninjatrader bridge: failed to subscribe ticks {instrument}")

    def get_buffered_ohlcv(self, instrument: str, timeframe: str) -> list[list[float]]:
        with self._lock:
            buf = self._bars.get((instrument, timeframe))
            return list(buf) if buf else []

    def get_last_price(self, instrument: str) -> tuple[float, int] | None:
        with self._lock:
            last = self._last_price.get(instrument)
            if last is not None:
                return last
            best: tuple[float, int] | None = None
            for (sym, _tf), buf in self._bars.items():
                if sym != instrument or not buf:
                    continue
                ts_ms, _o, _h, _l, close, _v = buf[-1]
                if best is None or ts_ms > best[1]:
                    best = (close, ts_ms)
            return best

    def backfill_needed(
        self, instrument: str, timeframe: str, start_ms: int, first_use: bool, have_bars: bool
    ) -> bool:
        """Whether fetch_ohlcv should ask NT8 for history.

        The old rule - backfill whenever the oldest buffered bar is more than
        one bar after the window start - is always true for a market with
        session breaks, because the window start routinely lands in closed
        hours. Every fetch then re-requested the same history: ZC/ZL/ZW/ZS were
        each requested 600-1,500 times in one afternoon (2026-11-11), drowning
        NT8's history requests for every other instrument. Now: on first use,
        when nothing is buffered, or when the window reaches further back than
        any backfill has covered - and never sooner than the backoff after a
        failed or empty attempt. Live bars keep the buffer current in between."""
        key = (instrument, timeframe)
        with self._lock:
            if time.monotonic() < self._backfill_retry_at.get(key, 0.0):
                return False
            covered_from = self._backfill_covered_from.get(key)
        if first_use or not have_bars or covered_from is None:
            return True
        return start_ms < covered_from

    def _record_backfill(self, key: tuple[str, str], since_ms: int, got_rows: bool) -> None:
        with self._lock:
            if got_rows:
                prev = self._backfill_covered_from.get(key)
                self._backfill_covered_from[key] = since_ms if prev is None else min(prev, since_ms)
                self._backfill_failures.pop(key, None)
                self._backfill_retry_at.pop(key, None)
            else:
                n = self._backfill_failures.get(key, 0) + 1
                self._backfill_failures[key] = n
                delay = min(_BACKFILL_RETRY_BASE_S * 2 ** (n - 1), _BACKFILL_RETRY_MAX_S)
                self._backfill_retry_at[key] = time.monotonic() + delay

    async def backfill_ohlcv(
        self, instrument: str, timeframe: str, since_ms: int, until_ms: int
    ) -> list[list[float]]:
        """Bridges onto the feed's own loop/thread exactly like
        `ensure_subscribed_bars` - the pending future keyed by `req_id` is
        created, awaited, and resolved (by `_on_backfill_result`) entirely
        inside `_do()`, which runs on `self._loop`. Only the *outer*
        `concurrent.futures.Future` crosses threads via `wrap_future`, same
        as `_RithmicFeed.backfill_ohlcv`. Creating the pending future
        directly on the caller's (bot) loop and awaiting it there would be
        wrong - `_on_backfill_result` resolves it from the feed's loop, and
        an asyncio.Future can only be awaited from the loop it belongs to.
        """
        key = (instrument, timeframe)
        req_id = str(uuid.uuid4())

        async def _do() -> list[list[float]]:
            fut = self._loop.create_future()
            with self._lock:
                self._pending_backfills[req_id] = fut
            try:
                await self._send(
                    {
                        "type": "backfill_bars",
                        "req_id": req_id,
                        "instrument": _to_nt_instrument(instrument),
                        "timeframe": timeframe,
                        "since_ms": since_ms,
                        "until_ms": until_ms,
                    }
                )
                bars = await asyncio.wait_for(fut, timeout=_BACKFILL_TIMEOUT_S)
            except BaseException:
                self._record_backfill(key, since_ms, False)
                raise
            finally:
                with self._lock:
                    self._pending_backfills.pop(req_id, None)

            rows: list[list[float]] = []
            for bar in bars:
                try:
                    rows.append(
                        [int(bar[0]), float(bar[1]), float(bar[2]), float(bar[3]), float(bar[4]), float(bar[5] or 0.0)]
                    )
                except (IndexError, TypeError, ValueError):
                    continue
            self._record_backfill(key, since_ms, bool(rows))
            # Merged here, on the feed's loop, so every caller sharing this
            # request (see below) gets the merged buffer back.
            with self._lock:
                existing = list(self._bars.get(key, ()))
                known_ts = {r[0] for r in existing}
                combined = existing + [r for r in rows if r[0] not in known_ts]
                combined.sort(key=lambda r: r[0])
                merged = deque(combined[-_BUFFER_LEN:], maxlen=_BUFFER_LEN)
                self._bars[key] = merged
                return list(merged)

        # One request per instrument/timeframe at a time. Several bots trade the
        # same contract and all asked for the same history at fleet start; now
        # later callers wait on the request already in flight.
        with self._lock:
            outer_fut = self._backfill_inflight.get(key)
            shared = outer_fut is not None
            if not shared:
                covered = self._backfill_covered_from.get(key)
                if covered is not None and covered <= since_ms and self._bars.get(key):
                    # A request that finished since this caller decided to
                    # backfill already covers its window.
                    return list(self._bars[key])
                outer_fut = asyncio.run_coroutine_threadsafe(_do(), self._loop)
                self._backfill_inflight[key] = outer_fut
                outer_fut.add_done_callback(lambda f: self._drop_inflight(key, f))
        try:
            return await asyncio.wrap_future(outer_fut)
        except Exception:
            if not shared:
                logger.exception(f"Ninjatrader bridge: historical backfill failed for {instrument}/{timeframe}")
            return self.get_buffered_ohlcv(instrument, timeframe)

    def _drop_inflight(self, key: tuple[str, str], fut: Any) -> None:
        with self._lock:
            if self._backfill_inflight.get(key) is fut:
                del self._backfill_inflight[key]

    # -- called from the bot's own (sync) thread, blocking -------------------
    # These back `self._api.create_order`/`cancel_order`, which the base
    # Exchange class calls synchronously, never via `await` - see the module
    # docstring. `fut.result(timeout=...)` blocks the calling thread only,
    # not the feed's own loop.

    def place_order_blocking(
        self,
        *,
        order_id: str,
        instrument: str,
        action: str,
        order_type: str,
        quantity: float,
        limit_price: float | None,
        stop_price: float | None,
        tif: str,
        reduce_only: bool,
        timeout: float = _ORDER_ACK_TIMEOUT_S,
    ) -> dict[str, Any]:
        async def _do() -> dict[str, Any]:
            fut = self._loop.create_future()
            with self._lock:
                self._pending_acks[order_id] = fut
                self._orders[order_id] = {
                    "status": "Submitted",
                    "instrument": instrument,
                    "filled_qty": 0.0,
                    "avg_fill_price": None,
                    "remaining_qty": quantity,
                    "reject_reason": None,
                }
            await self._send(
                {
                    "type": "place_order",
                    "order_id": order_id,
                    "instrument": _to_nt_instrument(instrument),
                    "action": action,
                    "order_type": order_type,
                    "quantity": quantity,
                    "limit_price": limit_price,
                    "stop_price": stop_price,
                    "tif": tif,
                    "reduce_only": reduce_only,
                    "account": self._account,
                }
            )
            try:
                await asyncio.wait_for(fut, timeout=timeout)
            except asyncio.TimeoutError:
                # Not an error - the order is live, just no terminal status
                # yet. The next fetch_order() picks up whatever arrives.
                logger.debug(f"Ninjatrader bridge: no ack yet for order {order_id} after {timeout}s")
            finally:
                # wait_for() cancels `fut` on timeout - if we didn't also pop
                # it here, a later terminal order_update for this same
                # order_id would try to resolve an already-cancelled Future
                # (fut.done() is True for a cancelled future too, so
                # _on_order_update's guard prevents a crash, but the stale
                # entry would otherwise sit in _pending_acks forever).
                with self._lock:
                    self._pending_acks.pop(order_id, None)
            with self._lock:
                return dict(self._orders.get(order_id, {"status": "Unknown", "instrument": instrument}))

        if self._loop is None:
            raise OperationalException("Ninjatrader bridge not started")
        fut = asyncio.run_coroutine_threadsafe(_do(), self._loop)
        try:
            return fut.result(timeout=timeout + 5.0)
        except FuturesTimeoutError as e:
            raise TemporaryError(f"Ninjatrader: place_order timed out for {instrument}") from e

    def cancel_order_blocking(self, order_id: str, timeout: float = _ORDER_ACK_TIMEOUT_S) -> None:
        """Waits for the terminal (Cancelled/Rejected/Filled-before-the-cancel-
        landed) order_update, same ack mechanism as place_order_blocking -
        without this, cancel_order() could read get_order_status() before
        NT8's own cancel confirmation arrives and report stale ("Working")
        state instead of "canceled"."""

        async def _do() -> None:
            fut = self._loop.create_future()
            with self._lock:
                existing = self._pending_acks.get(order_id)
                if existing is not None and not existing.done():
                    existing.cancel()
                self._pending_acks[order_id] = fut
            await self._send({"type": "cancel_order", "order_id": order_id})
            try:
                await asyncio.wait_for(fut, timeout=timeout)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                logger.debug(f"Ninjatrader bridge: no cancel ack yet for order {order_id} after {timeout}s")
            finally:
                with self._lock:
                    self._pending_acks.pop(order_id, None)

        if self._loop is None:
            raise OperationalException("Ninjatrader bridge not started")
        fut = asyncio.run_coroutine_threadsafe(_do(), self._loop)
        try:
            fut.result(timeout=timeout + 5.0)
        except FuturesTimeoutError as e:
            raise TemporaryError(f"Ninjatrader: cancel_order timed out for {order_id}") from e

    def get_order_status(self, order_id: str) -> dict[str, Any] | None:
        with self._lock:
            snap = self._orders.get(order_id)
            return dict(snap) if snap is not None else None

    def get_account_state(self) -> dict[str, Any] | None:
        with self._lock:
            return dict(self._account_state) if self._account_state is not None else None

    def wait_for_account_state(self, timeout: float = _ACCOUNT_STATE_TIMEOUT_S) -> bool:
        """Blocks the CALLING thread (plain `Event.wait`, safe from either a
        sync or async context, no loop-bridging needed) until the AddOn's
        first periodic `account_state` push arrives. Confirmed live
        2026-10-12: `Wallets.__init__()` calls `fetch_balance()` immediately
        at bot startup, before the bridge has had any chance to receive its
        first push (sent every ~2s, only after `hello`/`hello_ack`
        completes) - the base `Exchange.get_balances()` retrier gave up
        almost instantly, well short of that window, so every fresh bot
        start failed at init. `fetch_balance`/`fetch_positions` call this
        first so they only raise `TemporaryError` for a genuinely stuck
        connection, not an ordinary cold start."""
        return self._account_state_ready.wait(timeout)

    async def aclose(self) -> None:
        self._stopped = True
        writer = self._writer
        if writer is not None and self._loop is not None and not self._loop.is_closed():
            async def _do() -> None:
                try:
                    writer.close()
                except Exception:
                    pass

            try:
                fut = asyncio.run_coroutine_threadsafe(_do(), self._loop)
                await asyncio.wrap_future(fut)
            except Exception:
                pass

        if self._loop is not None and not self._loop.is_closed():
            self._loop.call_soon_threadsafe(self._loop.stop)
        self._thread.join(timeout=5.0)


# -- per-process shared-connection registry ---------------------------------
# Same rationale as rithmic.py's registry: multiple bots in this *process*
# (e.g. under single_process_mode) pointed at the same NT8 AddOn + account
# share one TCP connection/bridge instead of each opening their own.
_bridge_registry_lock = RLock()
_bridge_registry: dict[tuple[str, int, str], _NinjaTraderBridge] = {}
_bridge_refcounts: dict[tuple[str, int, str], int] = {}


def _bridge_key(credentials: dict[str, Any]) -> tuple[str, int, str]:
    return (credentials["host"], credentials["port"], credentials["account"])


def _get_or_create_feed(credentials: dict[str, Any]) -> _NinjaTraderBridge:
    key = _bridge_key(credentials)
    with _bridge_registry_lock:
        bridge = _bridge_registry.get(key)
        if bridge is None:
            bridge = _NinjaTraderBridge(credentials["host"], credentials["port"], credentials["account"])
            bridge._registry_key = key  # type: ignore[attr-defined]
            _bridge_registry[key] = bridge
            _bridge_refcounts[key] = 0
        _bridge_refcounts[key] += 1
        return bridge


async def _release_feed(bridge: _NinjaTraderBridge) -> None:
    key = getattr(bridge, "_registry_key", None)
    should_close = True
    if key is not None:
        with _bridge_registry_lock:
            _bridge_refcounts[key] = _bridge_refcounts.get(key, 1) - 1
            should_close = _bridge_refcounts[key] <= 0
            if should_close:
                _bridge_registry.pop(key, None)
                _bridge_refcounts.pop(key, None)
    if should_close:
        await bridge.aclose()


def _require_bridge(bridge: _NinjaTraderBridge | None) -> None:
    if bridge is None:
        raise OperationalException(
            "Ninjatrader order routing requested on an adapter with no bridge configured"
        )


def _order_snapshot_to_ccxt(
    snapshot: dict[str, Any], order_id: str, symbol: str, type_: str, side: str, amount: float, price: float | None
) -> dict[str, Any]:
    now = datetime.now(tz=UTC)
    nt_status = snapshot.get("status", "Unknown")
    status = _NT_STATUS_TO_CCXT.get(nt_status, "open")
    filled = float(snapshot.get("filled_qty") or 0.0)
    average = snapshot.get("avg_fill_price")
    return {
        "id": order_id,
        "symbol": symbol,
        "type": type_,
        "side": side,
        "amount": amount,
        "price": price,
        "average": average if average is not None else price,
        "filled": filled,
        "remaining": max(amount - filled, 0.0),
        "status": status,
        "timestamp": int(now.timestamp() * 1000),
        "datetime": now.isoformat(),
        "fee": None,
        "info": dict(snapshot),
    }


class NinjatraderCCXTAdapter:
    """Minimal ccxt-like adapter for NinjaTrader, following the same shape as
    `RithmicCCXTAdapter`/`DriftCCXTAdapter` - but unlike those, this one
    implements real order routing (`createOrder`/`cancelOrder`/`fetchOrder`/
    `fetchBalance`/`fetchPositions` are all live), backed by
    `_NinjaTraderBridge` instead of HTTP/websocket. Both the sync adapter
    (`self._api`, used for order routing - see the module docstring on why
    that path is synchronous) and the async adapter (`self._api_async`, used
    for OHLCV/ticker data) share the *same* bridge instance.
    """

    def __init__(
        self,
        parent: "Ninjatrader",
        *,
        is_async: bool,
        bridge: _NinjaTraderBridge | None,
        pair_whitelist: list[str],
    ) -> None:
        self._parent = parent
        self._is_async = is_async
        self._bridge = bridge

        self.id = "ninjatrader"
        self.name = "NinjaTrader"
        self.precisionMode = TICK_SIZE
        self.timeframes = dict(TIMEFRAMES)
        self.options = {"timeframes": {"swap": self.timeframes}}

        self.markets: dict[str, Any] = self._build_markets(pair_whitelist)
        self.symbols: list[str] = list(self.markets)

        self.has = {
            "fetchOHLCV": True,
            "fetchTicker": True,
            "fetchTickers": True,
            "fetchL2OrderBook": False,
            "fetchTrades": False,
            "createOrder": True,
            "createMarketOrder": True,
            "createLimitOrder": True,
            "cancelOrder": True,
            "fetchOrder": True,
            "fetchBalance": True,
            "fetchPositions": True,
        }
        self.features = {"swap": {"linear": {"fetchOHLCV": {"limit": 1000}}}}

        # Truthy whenever this adapter shares a real bridge - Exchange.close()
        # gates on `self._api_async.session` before awaiting our close().
        self.session = bridge

        if not is_async:
            # The base Exchange calls these on the SYNC adapter (self._api)
            # without awaiting - e.g. Exchange.fetch_ticker, which trader_bot
            # uses to price every entry. Defined only as coroutines, they
            # returned an un-awaited coroutine there: "TypeError: 'coroutine'
            # object is not subscriptable" on every entry attempt (4,402 times
            # on HG alone, 2026-11-11). Shadow them on this instance with
            # blocking versions that run the same coroutine on the bridge's
            # own loop - the async adapter keeps the coroutines.
            fetch_ticker_async = self.fetch_ticker
            fetch_tickers_async = self.fetch_tickers
            fetch_ohlcv_async = self.fetch_ohlcv
            self.fetch_ticker = lambda symbol, params=None: self._run_blocking(  # type: ignore[method-assign]
                fetch_ticker_async(symbol, params)
            )
            self.fetch_tickers = lambda symbols=None, params=None: self._run_blocking(  # type: ignore[method-assign]
                fetch_tickers_async(symbols, params)
            )
            self.fetch_ohlcv = lambda symbol, timeframe="1m", since=None, limit=None, params=None: (  # type: ignore[method-assign]
                self._run_blocking(fetch_ohlcv_async(symbol, timeframe, since, limit, params))
            )

    def _run_blocking(self, coro: Any, timeout: float = _SYNC_DATA_TIMEOUT_S) -> Any:
        """Run `coro` on the bridge's own event loop and block for its result -
        the sync adapter's way into the async data path (never called from the
        bridge thread itself, which would deadlock)."""
        bridge = self._bridge
        loop = getattr(bridge, "_loop", None) if bridge is not None else None
        if loop is None or loop.is_closed():
            coro.close()
            raise TemporaryError("Ninjatrader bridge is not running")
        fut = asyncio.run_coroutine_threadsafe(coro, loop)
        try:
            return fut.result(timeout=timeout)
        except FuturesTimeoutError:
            fut.cancel()
            raise TemporaryError(f"Ninjatrader bridge did not answer within {timeout:.0f}s") from None

    @staticmethod
    def _build_markets(pair_whitelist: list[str]) -> dict[str, Any]:
        """Markets keyed by the *exact* dash-separated alias string in
        `pair_whitelist` (e.g. "SIL-12-26", not NT8's own "SIL 12-26" - see
        `_to_nt_instrument`'s docstring), not by root - unlike
        `Rithmic`/`Futuressim`, whose whitelist entries already are the bare
        root. Economics (multiplier/tick size) still come from the root's
        entry in `futures_specs.py`."""
        out: dict[str, Any] = {}
        for instrument in pair_whitelist:
            root = _root_from_instrument(instrument)
            spec = FUTURES_CONTRACT_SPECS.get(root) or CFD_CONTRACT_SPECS.get(root)
            if spec is None:
                logger.warning(
                    f"Ninjatrader: no contract spec for root {root!r} (from pair_whitelist entry "
                    f"{instrument!r}) in futures_specs.py - skipping, this instrument will not trade."
                )
                continue
            out[instrument] = build_market(instrument, spec)
        return out

    def load_markets(self, *args, **kwargs) -> dict[str, Any]:
        return self.markets

    async def close(self) -> None:
        # Real teardown (bridge release) happens in Ninjatrader.close(),
        # which runs exactly once per exchange instance regardless of how
        # many adapters (sync/async) were handed the same bridge.
        pass

    # -- async: data path (awaited from the bot's own loop) -----------------

    async def fetch_ohlcv(
        self,
        symbol: str,
        timeframe: str = "1m",
        since: int | None = None,
        limit: int | None = None,
        params: dict[str, Any] | None = None,
    ) -> list[list]:
        bridge = self._bridge
        if bridge is None:
            return []
        await bridge.ensure_ready()

        candle_limit = int(limit or 500)
        tf_ms = timeframe_to_msecs(timeframe)
        now_ms = int(datetime.now(tz=UTC).timestamp() * 1000)
        start_ms = since if since is not None else now_ms - candle_limit * tf_ms

        first_use = await bridge.ensure_subscribed_bars(symbol, timeframe)
        buffered = bridge.get_buffered_ohlcv(symbol, timeframe)
        if bridge.backfill_needed(symbol, timeframe, start_ms, first_use, bool(buffered)):
            buffered = await bridge.backfill_ohlcv(symbol, timeframe, start_ms, now_ms)

        rows = [row for row in buffered if row[0] >= start_ms]
        if limit is not None:
            rows = rows[-candle_limit:]
        return rows

    async def fetch_ticker(self, symbol: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        bridge = self._bridge
        if bridge is None:
            raise OperationalException("Ninjatrader ticker requested on the sync (no-bridge) adapter")
        await bridge.ensure_ready()
        await bridge.ensure_subscribed_ticks(symbol)

        last = bridge.get_last_price(symbol)
        if last is None:
            raise TemporaryError(f"Ninjatrader: no live price yet for {symbol}")

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
                # The class's coroutine, not self.fetch_ticker: on the sync
                # adapter that attribute is the blocking wrapper (see __init__),
                # and calling it from here - on the bridge loop - would deadlock.
                out[sym] = await NinjatraderCCXTAdapter.fetch_ticker(self, sym)
            except Exception:
                continue
        return out

    # -- sync: order-routing path (blocking, called from the bot's own thread) -

    def create_order(
        self,
        symbol: str,
        type: str,
        side: str,
        amount: float,
        price: float | None = None,
        params: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        _require_bridge(self._bridge)
        bridge = self._bridge
        params = params or {}
        order_id = str(uuid.uuid4())
        action = "BUY" if side == "buy" else "SELL"
        order_type = "LIMIT" if type == "limit" else "MARKET"
        tif = str(params.get("timeInForce", "GTC"))
        reduce_only = bool(params.get("reduceOnly", False))

        snapshot = bridge.place_order_blocking(
            order_id=order_id,
            instrument=symbol,
            action=action,
            order_type=order_type,
            quantity=amount,
            limit_price=price if order_type == "LIMIT" else None,
            stop_price=None,
            tif=tif,
            reduce_only=reduce_only,
        )
        if snapshot.get("status") == "Rejected":
            raise InvalidOrderException(
                f"Ninjatrader rejected {side} {order_type} order on {symbol} "
                f"(amount={amount}, price={price}): {snapshot.get('reject_reason')}"
            )
        return _order_snapshot_to_ccxt(snapshot, order_id, symbol, type, side, amount, price)

    def fetch_order(
        self, order_id: str, pair: str | None = None, params: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        _require_bridge(self._bridge)
        snapshot = self._bridge.get_order_status(order_id)
        if snapshot is None:
            raise InvalidOrderException(f"Ninjatrader: unknown order id {order_id!r}")
        symbol = snapshot.get("instrument") or pair or ""
        # side/type/amount/price aren't recoverable from the bridge's own
        # snapshot (NT8 doesn't echo them back on every update) - the base
        # Exchange only reads status/filled/remaining/average off this
        # result for an already-placed order, so blank-but-consistent
        # placeholders here are harmless.
        return _order_snapshot_to_ccxt(snapshot, order_id, symbol, "", "", snapshot.get("remaining_qty") or 0.0, None)

    def cancel_order(
        self, order_id: str, pair: str | None = None, params: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        _require_bridge(self._bridge)
        self._bridge.cancel_order_blocking(order_id)
        snapshot = self._bridge.get_order_status(order_id) or {"status": "Cancelled"}
        symbol = snapshot.get("instrument") or pair or ""
        return _order_snapshot_to_ccxt(snapshot, order_id, symbol, "", "", snapshot.get("remaining_qty") or 0.0, None)

    def fetch_balance(self, params: dict[str, Any] | None = None) -> dict[str, Any]:
        _require_bridge(self._bridge)
        self._bridge.wait_for_account_state()
        state = self._bridge.get_account_state()
        if state is None:
            raise TemporaryError(
                "Ninjatrader: no account_state received yet from NT8 - the bridge pushes this "
                "periodically once connected; this is transient on a fresh connection."
            )
        cash = float(state.get("cash_value") or 0.0)
        stake_currency = self._parent._config.get("stake_currency", "USD")
        return {
            stake_currency: {"free": cash, "used": 0.0, "total": cash},
            "free": {},
            "used": {},
            "total": {},
            "info": state,
        }

    def fetch_positions(
        self, symbols: list[str] | None = None, params: dict[str, Any] | None = None
    ) -> list[dict[str, Any]]:
        _require_bridge(self._bridge)
        self._bridge.wait_for_account_state()
        state = self._bridge.get_account_state() or {}
        out: list[dict[str, Any]] = []
        for p in state.get("positions", []):
            instrument = p.get("instrument")
            if symbols and instrument not in symbols:
                continue
            market_position = p.get("market_position")
            quantity = p.get("quantity") or 0
            if market_position not in ("Long", "Short") or quantity == 0:
                continue
            out.append(
                {
                    "symbol": instrument,
                    "side": "long" if market_position == "Long" else "short",
                    "contracts": abs(quantity),
                    "entryPrice": p.get("avg_price"),
                    "info": p,
                }
            )
        return out


class Ninjatrader(Exchange):
    """See the module docstring. Config name: `"exchange": {"name": "ninjatrader"}`.

    Required `exchange` config fields: `ninjatrader_account` (the exact NT8
    account name - "Sim101" for paper, your live account's name, or whatever
    account a Market Replay connection is driving; this one field is the only
    thing distinguishing sim/live/replay). Optional: `ninjatrader_host`
    (default "127.0.0.1"), `ninjatrader_port` (default 8755, must match the
    port the VulcanTraderBridge AddOn is listening on). `pair_whitelist`
    entries are dash-separated aliases of the exact NT8 contract (e.g.
    "SIL-12-26" for NT8's "SIL 12-26"), not bare roots - see the module
    docstring for why a literal space can't be used here.
    """

    # Entries/exits fill at the price the AddOn reports back for the actual
    # NT8 fill, not a replayed bar's open/close - "which price within a bar"
    # is a backtest-only concept that doesn't apply to a real order routed
    # through a real (or NT8-simulated) matching engine.
    use_close_price_fills: bool = False

    trader_has: TraderHas = {
        "stoploss_on_exchange": False,
        # No FOK - confirmed 2026-10-12 against a real NT8 compile that
        # Cbi.TimeInForce has no Fok member (Ioc does). See MapTif() in
        # ninjatrader_addon/VulcanTraderBridge.cs.
        "order_time_in_force": ["GTC", "DAY", "IOC"],
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
        "order_props_in_contracts": ["amount", "cost", "filled", "remaining"],
    }
    trader_has_futures: TraderHas = {
        "uses_leverage_tiers": False,
    }

    # No margin/liquidation modeling - a config for this exchange should
    # simply omit `margin_mode`, same as Rithmic/Futuressim.
    _supported_trading_mode_margin_pairs: list[tuple[TradingMode, MarginMode]] = [
        (TradingMode.FUTURES, MarginMode.NONE),
    ]

    def _init_ccxt(
        self, exchange_config: dict[str, Any], sync: bool, ccxt_kwargs: dict[str, Any]
    ) -> NinjatraderCCXTAdapter:
        if getattr(self, "_bridge", None) is None:
            host = exchange_config.get("ninjatrader_host") or "127.0.0.1"
            port = int(exchange_config.get("ninjatrader_port") or 8755)
            account = exchange_config.get("ninjatrader_account", "")
            if not account:
                raise OperationalException(
                    "Ninjatrader exchange config is missing required field 'ninjatrader_account' "
                    "(the exact NT8 account name, e.g. 'Sim101') under the 'exchange' config block."
                )
            self._bridge = _get_or_create_feed({"host": host, "port": port, "account": account})
        pair_whitelist = list(exchange_config.get("pair_whitelist", []))
        return NinjatraderCCXTAdapter(
            self, is_async=not sync, bridge=self._bridge, pair_whitelist=pair_whitelist
        )

    def reload_markets(self, force: bool = False, *, load_leverage_tiers: bool = True) -> None:
        """Static market metadata built from `pair_whitelist` at adapter
        construction time - decoupled from the live bridge becoming ready so
        startup never blocks on NT8 responding."""
        if self._markets and not force:
            return
        self._markets = dict(self._api.markets)

    def get_futures_fee_pct(self, pair: str, price: float, *, is_maker: bool = False) -> float | None:
        """See `futures_common.get_futures_fee_pct`, keyed by root (`pair` here
        is the dash-separated alias, e.g. "SIL-12-26")."""
        return _get_futures_fee_pct(_root_from_instrument(pair), price, is_maker=is_maker)

    def get_fee_per_contract(self, pair: str) -> float | None:
        """See `futures_common.get_fee_per_contract` (shared with `Futuressim`/`Rithmic`),
        keyed by root - consumed by the native Rust multi-pair engine, which re-divides
        by its own fill price per trade."""
        return _get_fee_per_contract(_root_from_instrument(pair))

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
        pct = _get_futures_fee_pct(_root_from_instrument(symbol), price, is_maker=(taker_or_maker == "maker"))
        if pct is not None:
            return pct
        return super().get_fee(
            symbol, order_type=order_type, side=side, amount=amount, price=price,
            taker_or_maker=taker_or_maker,
        )

    def market_is_open(self, pair: str, timestamp: Any) -> bool:
        """See `futures_common.market_is_open`, keyed by root."""
        return _market_is_open(_root_from_instrument(pair), timestamp)

    def close(self) -> None:
        # Release the shared bridge *before* the base class closes/clears
        # `self.loop` - we still need a live loop to bridge the async
        # `_release_feed` call onto the feed's own thread.
        bridge = getattr(self, "_bridge", None)
        if bridge is not None:
            try:
                if self.loop and not self.loop.is_closed():
                    self.loop.run_until_complete(_release_feed(bridge))
            except Exception:
                logger.exception("Ninjatrader: error releasing bridge during close()")
            self._bridge = None
        super().close()
