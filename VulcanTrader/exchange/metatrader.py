"""Live/dry-run/live-trading data+order feed for retail forex/CFD/metals/
index/crypto instruments via a locally running MetaTrader 5 terminal.

Data (OHLCV/tickers) always works. Real order routing - `create_order`/
`cancel_order`/`fetch_order`/`fetch_balance`/`fetch_positions`, backed by
`mt5.order_send()` et al - is implemented but gated **off by default** behind
an explicit, separate opt-in: `exchange.metatrader_enable_live_orders: true`.
Without it (the default), `has["createOrder"]` etc. all stay `False` exactly
like the original data-only design, and order simulation goes through the
generic dry-run fill engine every other exchange in this package already
uses. This is deliberately a second gate on top of the bot's own `dry_run`
flag, not a replacement for it: `dry_run` is a general, bot-wide setting a
user might flip without specifically weighing "this will route real orders
to my actual MT5 account" the way they would for, say, a crypto exchange API
key - connecting this adapter to a real broker terminal is a materially
bigger real-world consequence than flipping `dry_run` usually is. Both gates
must agree (`dry_run: false` AND `metatrader_enable_live_orders: true`)
before a single real order can be sent.

**Order model**: every order is a synchronous market execution
(`TRADE_ACTION_DEAL`) at the current bid/ask - there are no resting/pending
limit orders (see "Known simplifications" below for why). An entry opens a
position; an exit (`reduceOnly` in `params`) looks up and closes this bot's
own open position for that symbol, matched by a magic number
(`_MAGIC_NUMBER`) stamped on every order this adapter places, so it never
touches a position a human (or another bot) placed by hand on the same
account. Closing specifically *by position ticket* (not just "send the
opposite side and hope it nets") is required because MT5 accounts can be in
hedging mode - `ACCOUNT_MARGIN_MODE_RETAIL_HEDGING` - where multiple
same-symbol positions coexist and an opposite-direction deal opens a new one
instead of reducing the old one.

Uses the official `MetaTrader5` PyPI package (MetaQuotes' own Python API),
not ccxt - MT5 is a retail broker terminal protocol ccxt doesn't cover, and
it only exists on Windows. It's imported lazily (inside the functions that
actually need it) so this module - and the rest of the bot - stays importable
for users who never configure `"exchange": {"name": "metatrader"}`, matching
this repo's existing lazy-import convention for heavy/platform-specific
optional dependencies (see `VulcanTrader/util/_ccxt_lazy.py`, `rithmic.py`).

Concurrency model
------------------
Unlike Rithmic's `async-rithmic` (a persistent websocket client that needs its
own background thread/event loop - see `rithmic.py`'s module docstring), the
`MetaTrader5` package is a thin, fully synchronous wrapper around a local IPC
channel to an already-running terminal: `mt5.initialize()`, `mt5.login()`,
`mt5.copy_rates_from_pos()`, `mt5.symbol_info_tick()`, etc. are all plain
blocking module-level function calls with no concept of a "client object" or
a push subscription - every call is a fresh pull, already backed by whatever
bars/ticks the terminal itself has buffered. That means no background
thread/loop, no buffer management and no front-month-rollover-style logic is
needed here at all: `MetatraderCCXTAdapter.fetch_ohlcv`/`fetch_ticker` just
run the matching `mt5.*` call in a thread-pool executor (so a slow call never
blocks the bot's own asyncio loop) and return whatever the terminal gives
back.

One real, inherent limitation of the vendor package (not something this
module's connection registry below can route around): **the underlying IPC
channel is one global resource per Python process**, not scoped to any
Python object. Calling `mt5.initialize(path=...)` / `mt5.login(...)` again
from the same process re-points that *same* global channel, it does not open
a second independent one. Practically: every `Metatrader` exchange object
constructed in one process - e.g. several bots sharing a process under
`single_process_mode` - MUST be configured with the same
login/server/path, or whichever one (re)connects last will silently redirect
every other bot's calls to its own account/terminal. Running different
MetaTrader accounts concurrently needs separate OS processes (the
`single_process_mode: false` default), exactly like running two independent
MT5 terminal installations side by side outside this bot entirely.

Known simplifications (mirrors the equivalent section in `rithmic.py`):
  - No leverage/liquidation modeling in the bot's own bookkeeping - the
    broker's actual margin/leverage rules still apply to any real order sent
    (MT5 enforces its own margin checks server-side regardless of what this
    adapter thinks), but this adapter never computes or reports a
    liquidation price. `trading_mode`/`margin_mode` follow the same
    `(FUTURES, NONE)` convention `Rithmic`/`Futuressim` already use, even
    though forex/CFD margin trading isn't literally "futures" - there's no
    other non-crypto trading-mode precedent in this codebase to reuse
    instead.
  - No per-symbol fee table - MT5 broker commission structures vary wildly
    (flat per-lot, spread-only, tiered), so this relies entirely on the
    config's flat `fee`, same as every ccxt exchange config in this project.
    Real orders are unaffected by this - the broker charges whatever it
    actually charges regardless of what the bot's own PnL bookkeeping
    assumes; this only means the bot's *own ledger* won't exactly match the
    account statement.
  - No `market_is_open`/trading-session gating override - forex/CFD markets
    broadly trade Sun 22:00 UTC - Fri 22:00 UTC, but exact session hours are
    broker- and symbol-specific (and MT5 already won't produce fresh
    ticks/bars, or accept orders, outside them) - the bot's ordinary
    stale-data handling plus the broker's own rejection of orders when the
    market is closed are what actually prevent trading during a closed
    session, same as documented for `Rithmic`.
  - Symbol names are **not** standardized across brokers the way CME root
    symbols are (unlike `Rithmic`, there is no `futures_specs.py`-style
    static table here) - e.g. this project's own test connection spells
    EUR/USD as `"EURUSD.i"`, not `"EURUSD"`. `pair_whitelist` entries must
    match exactly what that broker's own MT5 Market Watch calls them; verify
    there before whitelisting.
  - No stoploss-on-exchange - `trader_has["stoploss_on_exchange"]` stays
    `False` and `Metatrader.__init__` forces every `order_types` key
    (including `stoploss_on_exchange`) to a value this adapter actually
    supports, so the bot's own software-side stoploss/ROI logic is what
    protects a position, exactly as before order routing existed. A real
    order's `sl`/`tp` fields are left unset; nothing here pushes a stop to
    the broker.
  - No resting/pending limit orders - every order is sent as an immediate
    market deal (see "Order model" above). MT5 pending orders
    (`TRADE_ACTION_PENDING`) have broker-specific rules about which side of
    the current price a limit price must sit on that don't map cleanly onto
    this project's generic `entry_pricing`/`exit_pricing` config (which is
    written assuming a marketable, immediately-fillable price to begin
    with - see `price_side: "other"` in `configMetaTrader.json`), so
    `unfilledtimeout`/open-order management never has anything to do here:
    an order placed by this adapter has always already resolved (filled or
    rejected) by the time `create_order` returns.
"""

from __future__ import annotations

import asyncio
import logging
from datetime import UTC, datetime
from threading import RLock
from typing import Any

from ccxt import TICK_SIZE

from VulcanTrader.constants import Config
from VulcanTrader.enums import MarginMode, TradingMode
from VulcanTrader.exchange.exchange import Exchange
from VulcanTrader.exchange.exchange_types import TraderHas
from VulcanTrader.util.exceptions import (
    InsufficientFundsError,
    InvalidOrderException,
    OperationalException,
    TemporaryError,
)

logger = logging.getLogger(__name__)

# Stamped on every order this adapter places (`order_send`'s `magic` field) so
# a reduceOnly exit only ever closes a position THIS bot opened - never one a
# human (or another bot/EA) placed by hand on the same account. See "Order
# model" in the module docstring.
_MAGIC_NUMBER = 269_412_017

# freqtrade/VulcanTrader timeframe string -> MetaTrader5 module attribute name.
# MT5 natively supports many more (M2/M3/M4.../H2/H3/H6/H8/H12/W1/MN1) but this
# is the subset the rest of VulcanTrader already standardizes on elsewhere
# (see futures_common.TIMEFRAMES).
_TIMEFRAME_ATTRS = {
    "1m": "TIMEFRAME_M1",
    "5m": "TIMEFRAME_M5",
    "15m": "TIMEFRAME_M15",
    "30m": "TIMEFRAME_M30",
    "1h": "TIMEFRAME_H1",
    "4h": "TIMEFRAME_H4",
    "1d": "TIMEFRAME_D1",
}


def _timeframe_to_mt5(mt5module: Any, timeframe: str) -> int:
    attr = _TIMEFRAME_ATTRS.get(timeframe)
    if attr is None:
        raise OperationalException(f"MetaTrader does not support timeframe {timeframe!r}")
    return getattr(mt5module, attr)


def _build_market(info: Any) -> dict[str, Any]:
    """ccxt-like market dict for one MT5 `SymbolInfo`, mirroring the shape
    `futures_common.build_market` uses for Rithmic/Futuressim - but with real
    per-symbol specs read from the connected broker instead of a static
    table, since MT5 symbol specs are broker-specific (see module docstring).
    """
    symbol = info.name
    quote = info.currency_profit or "USD"
    return {
        "id": symbol,
        "symbol": symbol,
        "base": info.currency_base or symbol,
        "quote": quote,
        "settle": quote,
        "type": "swap",
        "swap": True,
        "spot": False,
        "linear": True,
        "inverse": False,
        "active": bool(info.visible),
        "contractSize": float(info.trade_contract_size or 1.0),
        "precision": {"amount": info.volume_step, "price": info.point},
        "limits": {
            "amount": {"min": info.volume_min, "max": info.volume_max},
            "price": {"min": None, "max": None},
            "cost": {"min": None, "max": None},
            # No margin/leverage modeling - see module docstring.
            "leverage": {"min": 1, "max": 1},
        },
        "info": {},
    }


def _filling_mode_for(mt5module: Any, info: Any) -> int:
    """Pick an `ORDER_FILLING_*` mode this symbol's broker actually accepts.
    `info.filling_mode` is a bitmask of allowed modes (`SYMBOL_FILLING_FOK=1`,
    `_IOC=2`, `_BOC=4`) - prefer IOC (fill whatever's immediately available,
    cancel the rest) over FOK (all-or-nothing) over RETURN (the fallback for
    brokers that report neither bit, though RETURN is really meant for
    pending orders, not a market deal)."""
    mask = getattr(info, "filling_mode", 0) or 0
    if mask & 2:
        return mt5module.ORDER_FILLING_IOC
    if mask & 1:
        return mt5module.ORDER_FILLING_FOK
    return mt5module.ORDER_FILLING_RETURN


def _raise_for_retcode(mt5module: Any, result: Any, *, symbol: str, side: str) -> None:
    """Translate a non-success `OrderSendResult.retcode` into the matching
    VulcanTrader exception type, so callers (`enter_positions`/
    `execute_trade_exit` et al) get the same `DependencyException` family
    they already catch for every other exchange - see
    `VulcanTrader/util/exceptions.py`'s hierarchy."""
    retcode = result.retcode
    comment = result.comment
    if retcode == mt5module.TRADE_RETCODE_NO_MONEY:
        raise InsufficientFundsError(
            f"MetaTrader: insufficient margin to {side} {symbol} (retcode={retcode}: {comment})"
        )
    if retcode in (
        mt5module.TRADE_RETCODE_CONNECTION,
        mt5module.TRADE_RETCODE_TIMEOUT,
        mt5module.TRADE_RETCODE_PRICE_CHANGED,
        mt5module.TRADE_RETCODE_PRICE_OFF,
        mt5module.TRADE_RETCODE_REQUOTE,
        mt5module.TRADE_RETCODE_MARKET_CLOSED,
    ):
        raise TemporaryError(
            f"MetaTrader: transient order failure for {side} {symbol} "
            f"(retcode={retcode}: {comment}) - may succeed on retry"
        )
    raise InvalidOrderException(
        f"MetaTrader order_send rejected for {side} {symbol} (retcode={retcode}: {comment})"
    )


# -- per-process shared-connection registry --------------------------------
#
# Mirrors Rithmic's `_get_or_create_feed`/`_release_feed` pattern (see
# rithmic.py) for a similar reason - multiple `Metatrader` exchange objects
# in this process (e.g. sync + async adapters of the same bot, or several
# bots sharing one process under single_process_mode) must not each blindly
# call `mt5.shutdown()` out from under one another. Refcounted by
# (login, server, path) - see the module docstring for why this does NOT
# make it safe to run two actually-different accounts in one process.
_conn_lock = RLock()
_conn_refcounts: dict[tuple[str, str, str], int] = {}


def _conn_key(credentials: dict[str, str]) -> tuple[str, str, str]:
    return (credentials.get("login", ""), credentials.get("server", ""), credentials.get("path", ""))


def _connect(credentials: dict[str, str]) -> Any:
    """Idempotent connect (+ login, if credentials were given) to the
    configured terminal. Returns the `MetaTrader5` module itself - every
    "connection" is a call on that same module, there is no separate client
    object (see module docstring). If `login` is empty, this just attaches to
    whatever account the terminal is already logged into (the common case:
    a human already has the terminal open)."""
    import MetaTrader5 as mt5

    key = _conn_key(credentials)
    with _conn_lock:
        count = _conn_refcounts.get(key, 0)
        if count == 0:
            path = credentials.get("path") or None
            ok = mt5.initialize(path=path) if path else mt5.initialize()
            if not ok:
                raise OperationalException(f"MetaTrader5 initialize() failed: {mt5.last_error()}")
            login = credentials.get("login")
            if login:
                ok = mt5.login(
                    int(login),
                    password=credentials.get("password", ""),
                    server=credentials.get("server", ""),
                )
                if not ok:
                    err = mt5.last_error()
                    mt5.shutdown()
                    raise OperationalException(
                        f"MetaTrader5 login() failed for account {login}: {err}"
                    )
        _conn_refcounts[key] = count + 1
    return mt5


def _disconnect(credentials: dict[str, str]) -> None:
    """Releases one reference from a prior `_connect` call. Only actually
    calls `mt5.shutdown()` once every holder of this (login, server, path)
    in this process has released it."""
    import MetaTrader5 as mt5

    key = _conn_key(credentials)
    with _conn_lock:
        count = _conn_refcounts.get(key, 1) - 1
        if count <= 0:
            _conn_refcounts.pop(key, None)
            mt5.shutdown()
        else:
            _conn_refcounts[key] = count


class MetatraderCCXTAdapter:
    """Minimal ccxt-like adapter for MetaTrader 5, following the same pattern
    as `RithmicCCXTAdapter` (`rithmic.py`): a plain Python class exposing only
    the subset of the ccxt interface the base `Exchange` code actually calls
    for live trading (`fetch_ohlcv`/`fetch_ticker`/`fetch_tickers`), backed
    here by the `MetaTrader5` package's synchronous local calls instead of
    HTTP/websockets.

    Both the sync and async adapter connect and hold the connection open for
    the life of the bot (there is no static per-symbol table to fall back on
    for `self.markets` - see module docstring - so both need a live
    connection, not just the async one that `fetch_ohlcv`/`fetch_ticker`
    use). Only the async adapter's reference ever gets explicitly released
    (via `close()`, awaited by the base `Exchange.close()` - which never
    awaits `self._api.close()`, same as any real ccxt sync REST client): the
    sync adapter's reference is simply never released and relies on normal
    process exit to tear down, which is fine - this IPC channel is a cheap
    local resource, not a remote session that needs explicit teardown to
    avoid server-side leakage.

    `create_order`/`cancel_order`/`fetch_order`/`fetch_balance`/
    `fetch_positions` are plain synchronous methods (not `async def`) because
    `Exchange.create_order`/`cancel_order`/`fetch_order`/`get_balances` all
    call `self._api.*` directly, not `self._api_async.*` - matching ccxt's
    own convention of using the sync client for order placement.
    """

    def __init__(
        self,
        parent: "Metatrader",
        *,
        is_async: bool,
        credentials: dict[str, str],
        pair_whitelist: list[str],
        live_orders_enabled: bool,
    ) -> None:
        self._parent = parent
        self._is_async = is_async
        self._credentials = credentials
        # The REAL enforcement point for the live-orders gate - `self.has`
        # below is informational only (nothing in trader_bot.py/exchange.py
        # checks `exchange_has("createOrder")` before calling
        # `exchange.create_order()`; `dry_run` is the only thing that gates
        # that call, and this adapter's order methods are reached precisely
        # when dry_run is False). Every order/balance/position method below
        # checks this first and raises rather than silently proceeding.
        self._live_orders_enabled = live_orders_enabled

        self.id = "metatrader"
        self.name = "MetaTrader5"
        self.markets: dict[str, Any] = {}
        self.symbols: list[str] = []

        self.precisionMode = TICK_SIZE
        self.timeframes = dict.fromkeys(_TIMEFRAME_ATTRS, "")
        for tf in self.timeframes:
            self.timeframes[tf] = tf
        self.options = {"timeframes": {"swap": self.timeframes, "spot": self.timeframes}}

        self.has = {
            "fetchOHLCV": True,
            "fetchTicker": True,
            "fetchTickers": True,
            "fetchL2OrderBook": False,
            "fetchTrades": False,
            # Off by default - see module docstring's "Order model" section
            # and `exchange.metatrader_enable_live_orders`.
            "createOrder": live_orders_enabled,
            "createMarketOrder": live_orders_enabled,
            "createLimitOrder": live_orders_enabled,
            "cancelOrder": live_orders_enabled,
            "fetchOrder": live_orders_enabled,
            "fetchBalance": live_orders_enabled,
        }
        self.features = {"swap": {"linear": {"fetchOHLCV": {"limit": 5000}}}}

        self._mt5 = _connect(credentials)
        self._load_symbol_markets(self._mt5, pair_whitelist)

        # Truthy for both adapters - both hold a live connection for the
        # life of the bot (see class docstring). Exchange.close() gates on
        # `self._api_async.session` before awaiting our close(), same as any
        # real ccxt-pro session check; it never looks at `self._api.session`.
        self.session = self._mt5

    def _load_symbol_markets(self, mt5module: Any, pair_whitelist: list[str]) -> None:
        for symbol in pair_whitelist:
            if not mt5module.symbol_select(symbol, True):
                logger.warning(
                    f"MetaTrader: symbol {symbol!r} could not be selected in Market "
                    "Watch - check spelling/availability on this broker's MT5 server."
                )
                continue
            info = mt5module.symbol_info(symbol)
            if info is None:
                logger.warning(f"MetaTrader: no symbol_info returned for {symbol!r}.")
                continue
            self.markets[symbol] = _build_market(info)
        self.symbols = list(self.markets)

    def load_markets(self, *args, **kwargs) -> dict[str, Any]:
        return self.markets

    async def close(self) -> None:
        if self._is_async and self._mt5 is not None:
            _disconnect(self._credentials)
            self._mt5 = None

    async def fetch_ohlcv(
        self,
        symbol: str,
        timeframe: str = "1m",
        since: int | None = None,
        limit: int | None = None,
        params: dict[str, Any] | None = None,
    ) -> list[list]:
        if self._mt5 is None:
            return []
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(None, self._fetch_ohlcv_sync, symbol, timeframe, since, limit)

    def _fetch_ohlcv_sync(
        self, symbol: str, timeframe: str, since: int | None, limit: int | None
    ) -> list[list]:
        mt5module = self._mt5
        tf = _timeframe_to_mt5(mt5module, timeframe)
        candle_limit = int(limit or 500)
        if since is not None:
            date_from = datetime.fromtimestamp(since / 1000, tz=UTC)
            rates = mt5module.copy_rates_from(symbol, tf, date_from, candle_limit)
        else:
            rates = mt5module.copy_rates_from_pos(symbol, tf, 0, candle_limit)
        if rates is None or len(rates) == 0:
            return []
        return [
            [int(t) * 1000, float(o), float(h), float(l), float(c), float(v)]
            for t, o, h, l, c, v in zip(
                rates["time"], rates["open"], rates["high"], rates["low"], rates["close"], rates["tick_volume"]
            )
        ]

    async def fetch_ticker(self, symbol: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        if self._mt5 is None:
            raise OperationalException("MetaTrader ticker requested on a disconnected adapter")
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(None, self._fetch_ticker_sync, symbol)

    def _fetch_ticker_sync(self, symbol: str) -> dict[str, Any]:
        mt5module = self._mt5
        tick = mt5module.symbol_info_tick(symbol)
        if tick is None:
            raise TemporaryError(f"MetaTrader: no live tick yet for {symbol}")
        ts_ms = int(tick.time) * 1000
        # Forex/CFD ticks rarely carry a "last trade" price (last == 0) -
        # fall back to the bid/ask midpoint, then plain bid.
        if tick.last:
            last = tick.last
        elif tick.bid and tick.ask:
            last = (tick.bid + tick.ask) / 2
        else:
            last = tick.bid
        return {
            "symbol": symbol,
            "timestamp": ts_ms,
            "datetime": datetime.fromtimestamp(ts_ms / 1000, tz=UTC).isoformat(),
            "last": last,
            "close": last,
            "bid": tick.bid,
            "ask": tick.ask,
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

    # -- order routing - gated by `self._live_orders_enabled`, the REAL
    # enforcement point for `exchange.metatrader_enable_live_orders` (see
    # the note on it in `__init__` and the module docstring's "Order
    # model") -------------------------------------------------------------

    def _require_live_orders_enabled(self, action: str) -> None:
        if not self._live_orders_enabled:
            raise OperationalException(
                f"MetaTrader: refusing to {action} - this bot's config has dry_run disabled "
                "but 'exchange.metatrader_enable_live_orders' is not set to true. Both must "
                "be set for this adapter to ever send a real order - see metatrader.py's "
                "module docstring."
            )

    def create_order(
        self,
        symbol: str,
        type: str,
        side: str,
        amount: float,
        price: float | None = None,
        params: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        """Synchronous market execution - see module docstring's "Order
        model" for why there is no resting/pending-order path. Called as
        `self._api.create_order(...)` by `Exchange.create_order` (positional:
        symbol, type, side, amount, price, params), matching ccxt's own sync
        client convention."""
        self._require_live_orders_enabled("place a real order")
        if self._mt5 is None:
            raise OperationalException("MetaTrader order requested on a disconnected adapter")
        params = params or {}
        mt5module = self._mt5

        info = mt5module.symbol_info(symbol)
        if info is None:
            raise InvalidOrderException(f"MetaTrader: no symbol_info for {symbol!r}, cannot order")

        if params.get("reduceOnly"):
            return self._close_position(symbol, side, amount, info)
        return self._open_position(symbol, side, amount, info)

    def _open_position(self, symbol: str, side: str, amount: float, info: Any) -> dict[str, Any]:
        mt5module = self._mt5
        tick = mt5module.symbol_info_tick(symbol)
        if tick is None:
            raise TemporaryError(f"MetaTrader: no live tick for {symbol}, cannot price order")
        order_price = tick.ask if side == "buy" else tick.bid

        request = {
            "action": mt5module.TRADE_ACTION_DEAL,
            "symbol": symbol,
            "volume": float(amount),
            "type": mt5module.ORDER_TYPE_BUY if side == "buy" else mt5module.ORDER_TYPE_SELL,
            "price": order_price,
            "deviation": 20,
            "magic": _MAGIC_NUMBER,
            "comment": "VulcanTrader",
            "type_time": mt5module.ORDER_TIME_GTC,
            "type_filling": _filling_mode_for(mt5module, info),
        }
        result = mt5module.order_send(request)
        return self._result_to_ccxt_order(symbol, side, amount, order_price, result)

    def _close_position(self, symbol: str, side: str, amount: float, info: Any) -> dict[str, Any]:
        """Closes THIS bot's own open position for `symbol` (matched by
        `_MAGIC_NUMBER`), closing *by position ticket* rather than just
        sending the opposite side - required for correctness under
        `ACCOUNT_MARGIN_MODE_RETAIL_HEDGING`, where an opposite-direction deal
        with no `position` field opens a new position instead of reducing
        the old one. See module docstring."""
        mt5module = self._mt5
        positions = mt5module.positions_get(symbol=symbol) or ()
        ours = [p for p in positions if p.magic == _MAGIC_NUMBER]
        if not ours:
            raise InvalidOrderException(
                f"MetaTrader: no open position found for {symbol} (magic {_MAGIC_NUMBER}) to close - "
                "already closed, or never opened by this bot."
            )
        position = ours[0]

        tick = mt5module.symbol_info_tick(symbol)
        if tick is None:
            raise TemporaryError(f"MetaTrader: no live tick for {symbol}, cannot price close")
        close_price = tick.bid if side == "sell" else tick.ask
        close_volume = float(min(amount, position.volume))

        request = {
            "action": mt5module.TRADE_ACTION_DEAL,
            "symbol": symbol,
            "position": position.ticket,
            "volume": close_volume,
            "type": mt5module.ORDER_TYPE_SELL if side == "sell" else mt5module.ORDER_TYPE_BUY,
            "price": close_price,
            "deviation": 20,
            "magic": _MAGIC_NUMBER,
            "comment": "VulcanTrader-close",
            "type_time": mt5module.ORDER_TIME_GTC,
            "type_filling": _filling_mode_for(mt5module, info),
        }
        result = mt5module.order_send(request)
        return self._result_to_ccxt_order(symbol, side, close_volume, close_price, result)

    def _result_to_ccxt_order(
        self, symbol: str, side: str, amount: float, intended_price: float, result: Any
    ) -> dict[str, Any]:
        mt5module = self._mt5
        if result is None:
            raise TemporaryError(f"MetaTrader: order_send returned None for {symbol}: {mt5module.last_error()}")
        if result.retcode not in (mt5module.TRADE_RETCODE_DONE, mt5module.TRADE_RETCODE_DONE_PARTIAL):
            _raise_for_retcode(mt5module, result, symbol=symbol, side=side)

        filled = float(result.volume)
        avg_price = float(result.price) if result.price else intended_price
        ts_ms = int(datetime.now(tz=UTC).timestamp() * 1000)
        order_id = str(result.order or result.deal or 0)
        return {
            "id": order_id,
            "symbol": symbol,
            "side": side,
            "type": "market",
            "amount": float(amount),
            "price": intended_price,
            "average": avg_price,
            "filled": filled,
            "remaining": max(float(amount) - filled, 0.0),
            "cost": avg_price * filled,
            "status": "closed" if filled >= float(amount) else "open",
            "timestamp": ts_ms,
            "datetime": datetime.fromtimestamp(ts_ms / 1000, tz=UTC).isoformat(),
            "fee": {},
            "info": {"retcode": result.retcode, "comment": result.comment, "deal": result.deal},
        }

    def fetch_order(
        self, id: str, symbol: str | None = None, params: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        """Looks up an already-resolved deal from account history. Since
        every order placed by `create_order` fills or fails synchronously
        (see module docstring), this is only ever reporting a past result,
        never polling a pending one."""
        if self._mt5 is None:
            raise OperationalException("MetaTrader order lookup requested on a disconnected adapter")
        try:
            ticket = int(id)
        except (TypeError, ValueError):
            raise InvalidOrderException(f"MetaTrader: invalid order id {id!r}") from None

        mt5module = self._mt5
        deals = mt5module.history_deals_get(ticket=ticket)
        if not deals:
            raise InvalidOrderException(f"MetaTrader: order {id} not found in deal history")
        deal = deals[0]
        side = "buy" if deal.type == 0 else "sell"
        ts_ms = int(deal.time) * 1000
        return {
            "id": str(ticket),
            "symbol": deal.symbol,
            "side": side,
            "type": "market",
            "amount": float(deal.volume),
            "price": float(deal.price),
            "average": float(deal.price),
            "filled": float(deal.volume),
            "remaining": 0.0,
            "cost": float(deal.price) * float(deal.volume),
            "status": "closed",
            "timestamp": ts_ms,
            "datetime": datetime.fromtimestamp(ts_ms / 1000, tz=UTC).isoformat(),
            "fee": {},
            "info": {"ticket": deal.ticket},
        }

    def cancel_order(
        self, id: str, symbol: str | None = None, params: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        """Every order here resolves synchronously inside `create_order` -
        there is nothing left to cancel by the time anything could call this
        (see module docstring), so this just reports the already-resolved
        state back. `Exchange.cancel_order_with_result` falls back to
        `fetch_order` anyway whenever a cancel result is missing
        fee/status/amount, which a pass-through here never will be."""
        return self.fetch_order(id, symbol, params)

    def fetch_balance(self) -> dict[str, Any]:
        if self._mt5 is None:
            raise OperationalException("MetaTrader balance requested on a disconnected adapter")
        account = self._mt5.account_info()
        if account is None:
            raise TemporaryError("MetaTrader: account_info() unavailable")
        currency = account.currency or "USD"
        bal = {
            "free": float(account.margin_free),
            "used": float(account.margin),
            "total": float(account.balance),
        }
        return {
            currency: bal,
            "free": {currency: bal["free"]},
            "used": {currency: bal["used"]},
            "total": {currency: bal["total"]},
            "info": {"login": account.login, "server": account.server, "equity": account.equity},
        }

    def fetch_positions(
        self, symbols: list[str] | None = None, params: dict[str, Any] | None = None
    ) -> list[dict[str, Any]]:
        if self._mt5 is None:
            raise OperationalException("MetaTrader positions requested on a disconnected adapter")
        mt5module = self._mt5
        if symbols:
            positions = []
            for sym in symbols:
                positions.extend(mt5module.positions_get(symbol=sym) or ())
        else:
            positions = list(mt5module.positions_get() or ())
        ours = [p for p in positions if p.magic == _MAGIC_NUMBER]
        return [
            {
                "symbol": p.symbol,
                "side": "long" if p.type == 0 else "short",
                "contracts": float(p.volume),
                "leverage": None,
                "collateral": None,
                "initialMargin": None,
                "liquidationPrice": None,
                "entryPrice": float(p.price_open),
                "markPrice": float(p.price_current),
                "unrealizedPnl": float(p.profit),
                "info": {"ticket": p.ticket, "magic": p.magic},
            }
            for p in ours
        ]


class Metatrader(Exchange):
    """See the module docstring. Config name: `"exchange": {"name": "metatrader"}`.

    Required `exchange` config fields: none - if a MetaTrader 5 terminal is
    already running on this machine and logged into an account (the common
    case for a human-operated demo/challenge account), `mt5.initialize()`
    attaches to it directly with no credentials needed. Optional: `key` (the
    MT5 account/login number), `password`, `metatrader_server` (broker server
    name, e.g. `"MetaQuotes-Demo"`), `metatrader_path` (explicit path to a
    specific installation's `terminal64.exe`, when more than one exists on
    this machine). `pair_whitelist` entries are plain broker symbol names
    (`"EURUSD.i"`, `"XAUUSD"`, `"US30"`, ...) exactly as that broker's MT5
    server spells them - see the module docstring, these are NOT standardized
    across brokers.

    `metatrader_enable_live_orders` (bool, default `False`): the second,
    MetaTrader-specific gate on real order routing - see the module
    docstring's opening paragraph. Both this AND `dry_run: false` must be set
    for a real order to ever reach `mt5.order_send()`.
    """

    # Inherited-attribute parity with Rithmic for the same reason: nothing in
    # trader_bot.py's live/dry-run path or Futuressim/rust_backtest.py reads
    # this (MetaTrader never backtests), kept for interface consistency only.
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
        "ohlcv_candle_limit": 5000,
        "ws_enabled": False,
        "needs_trading_fees": False,
        "tickers_have_price": True,
        "ccxt_futures_name": "swap",
    }
    trader_has_futures: TraderHas = {
        "uses_leverage_tiers": False,
    }

    # No MarginMode.CROSS/ISOLATED entry - margin/liquidation isn't modeled
    # (dry-run data feed, not a broker connection - see module docstring), so
    # a config for this exchange should simply omit margin_mode.
    _supported_trading_mode_margin_pairs: list[tuple[TradingMode, MarginMode]] = [
        (TradingMode.FUTURES, MarginMode.NONE),
    ]

    def __init__(self, config: Config, *args, **kwargs) -> None:
        # MetaTrader has no market-order primitive via this adapter (data-only
        # for now - see module docstring): `has["createMarketOrder"]` is
        # False. `Exchange.__init__` validates `config["order_types"]` against
        # that before a strategy ever gets a chance to override anything, and
        # the JSON-schema default for `order_types.emergency_exit` alone is
        # "market" - so any config that doesn't spell out `order_types` at all
        # (the common case, see configMetaTrader.json) would otherwise fail
        # construction outright with a ConfigurationError, in both dry-run and
        # live. Same fix `Polymarket` needed for the same reason (see
        # `polymarket.py`'s `__init__`) - `setdefault` so an explicit user
        # override of any individual key still wins.
        order_types = config.setdefault("order_types", {})
        for _key in ("entry", "exit", "force_entry", "force_exit", "emergency_exit", "stoploss"):
            order_types.setdefault(_key, "limit")
        order_types.setdefault("stoploss_on_exchange", False)
        super().__init__(config, *args, **kwargs)

    def _init_ccxt(
        self, exchange_config: dict[str, Any], sync: bool, ccxt_kwargs: dict[str, Any]
    ) -> MetatraderCCXTAdapter:
        login = exchange_config.get("key", "")
        credentials = {
            "login": str(login) if login else "",
            "password": exchange_config.get("password", ""),
            "server": exchange_config.get("metatrader_server", ""),
            "path": exchange_config.get("metatrader_path", ""),
        }
        pair_whitelist = exchange_config.get("pair_whitelist", [])
        live_orders_enabled = bool(exchange_config.get("metatrader_enable_live_orders", False))
        return MetatraderCCXTAdapter(
            self,
            is_async=not sync,
            credentials=credentials,
            pair_whitelist=pair_whitelist,
            live_orders_enabled=live_orders_enabled,
        )

    def reload_markets(self, force: bool = False, *, load_leverage_tiers: bool = True) -> None:
        """Both the sync and async adapters already loaded real per-symbol
        specs at construction time (see `MetatraderCCXTAdapter.__init__`) -
        this just mirrors whichever one is populated onto `self._markets`,
        decoupled from any live connection staying open (matches Rithmic's
        `reload_markets` override for the same reason)."""
        if self._markets and not force:
            return
        self._markets = dict(self._api_async.markets or self._api.markets)
