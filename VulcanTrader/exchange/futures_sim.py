"""A fake, backtest-only exchange that supplies CME-style futures contract
economics to the bot's backtester: per-symbol point-value multipliers, a flat
per-contract commission, and CME Globex session hours. It is NOT a real
CME/broker connection - there is no order routing, no live/dry-run trading
support, and no margin/liquidation modeling.

It exists purely to plug the real contract multipliers from `Futures.txt`
(a reference the user ported from a different project of theirs) into
`rust_backtest.py`'s existing ccxt-style `contractSize`/
`amount_to_contract_precision` machinery. That machinery already computes the
right PnL once `contractSize` is set correctly - `do_enter` rounds `amount`
via `exchange.amount_to_contract_precision(pair, amount)`, which internally
divides by `contract_size`, rounds to whole contracts, and multiplies back -
so `close_position`'s existing `amount * price` terms come out as real dollar
PnL with zero formula changes. See the `futures_specs` module for the actual
per-symbol numbers and their provenance.

The contract economics (market building, session-hours gate, flat-fee
conversion) live in `futures_common.py`, shared with the live/dry-run
`Rithmic` exchange so the two can't drift apart.

No leverage is modeled: `_leverage_tiers` is intentionally left empty, so the
base `Exchange.get_max_leverage` already returns 1.0 for every pair -
contracts are sized directly from `stake_amount`.
"""

from __future__ import annotations

import logging
from typing import Any

from ccxt import TICK_SIZE

from VulcanTrader.enums import MarginMode, TradingMode
from VulcanTrader.exchange.exchange import Exchange
from VulcanTrader.exchange.exchange_types import TraderHas
from VulcanTrader.exchange.futures_common import (
    TIMEFRAMES as _TIMEFRAMES,
)
from VulcanTrader.exchange.futures_common import (
    build_market as _build_market,
)
from VulcanTrader.exchange.futures_common import (
    get_fee_per_contract as _get_fee_per_contract,
)
from VulcanTrader.exchange.futures_common import (
    get_futures_fee_pct as _get_futures_fee_pct,
)
from VulcanTrader.exchange.futures_common import (
    market_is_open as _market_is_open,
)
from VulcanTrader.exchange.futures_specs import CFD_CONTRACT_SPECS, FUTURES_CONTRACT_SPECS

logger = logging.getLogger(__name__)


class FuturesSimCCXTAdapter:
    """Minimal static ccxt-like adapter - markets never change and nothing
    here ever makes a network call, unlike every other exchange adapter in
    this package."""

    def __init__(self, parent: "Futuressim", *, is_async: bool) -> None:
        self._parent = parent
        self._is_async = is_async

        self.id = "futuressim"
        self.name = "FuturesSim"
        self.precisionMode = TICK_SIZE
        self.timeframes = dict(_TIMEFRAMES)
        self.options = {"timeframes": {"swap": self.timeframes}}
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
        }
        self.markets: dict[str, Any] = {
            sym: _build_market(sym, spec) for sym, spec in FUTURES_CONTRACT_SPECS.items()
        }
        self.markets.update(
            {sym: _build_market(sym, spec) for sym, spec in CFD_CONTRACT_SPECS.items()}
        )
        self.symbols = list(self.markets)
        self.features = {"swap": {"linear": {"fetchOHLCV": {"limit": 1000}}}}
        self.session = None

    async def close(self) -> None:
        pass


class Futuressim(Exchange):
    """See module docstring. Config name: `"exchange": {"name": "futuressim"}`."""

    #: Read by `rust_backtest.py`'s `close_fill_mode` (hasattr-gated, checked
    #: only for this exchange). CME futures backtests were cross-validated
    #: against a NautilusTrader port of the same strategies, which fills a
    #: reactive order at the bar it's submitted in's CLOSE (it can only act
    #: once that bar is fully known). freqtrade's own convention — fill a
    #: signal using ITS OWN bar's OPEN — is real, intentional, and correct
    #: for every other exchange here; it just isn't what an independent
    #: reference engine agrees with, so this exchange specifically switches
    #: entries and Signal/MaxHoldPeriod/CustomExit exits to close-price fills
    #: to match. ROI/Stoploss/TrailingStop already resolve to an exact
    #: bound-crossing price under both conventions and are unaffected.
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

    # No MarginMode.CROSS/ISOLATED entry - margin/liquidation isn't modeled,
    # so a config for this exchange should simply omit `margin_mode`.
    _supported_trading_mode_margin_pairs: list[tuple[TradingMode, MarginMode]] = [
        (TradingMode.FUTURES, MarginMode.NONE),
    ]

    def _init_ccxt(
        self, exchange_config: dict[str, Any], sync: bool, ccxt_kwargs: dict[str, Any]
    ) -> FuturesSimCCXTAdapter:
        return FuturesSimCCXTAdapter(self, is_async=not sync)

    def reload_markets(self, force: bool = False, *, load_leverage_tiers: bool = True) -> None:
        """Base `Exchange.reload_markets()` is ccxt-specific (pickle cache,
        live `fetch_markets()` calls); this exchange's markets are a static
        table built once in `FuturesSimCCXTAdapter.__init__`, so there is
        nothing to fetch or throttle."""
        if self._markets and not force:
            return
        self._markets = dict(self._api.markets)

    def get_futures_fee_pct(self, pair: str, price: float, *, is_maker: bool = False) -> float | None:
        """See `futures_common.get_futures_fee_pct` (shared with `Rithmic`)."""
        return _get_futures_fee_pct(pair, price, is_maker=is_maker)

    def get_fee_per_contract(self, pair: str) -> float | None:
        """See `futures_common.get_fee_per_contract` (shared with `Rithmic`/`Ninjatrader`) -
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
        `price` - overrides the base class's flat config-`fee`/ccxt-`calculate_fee` lookup,
        which has no notion of this. Falls back to the base behaviour (the config's `fee`,
        or ccxt's `calculate_fee`) for symbols with no per-contract fee on file (the CFD
        placeholders)."""
        pct = _get_futures_fee_pct(symbol, price, is_maker=(taker_or_maker == "maker"))
        if pct is not None:
            return pct
        return super().get_fee(
            symbol, order_type=order_type, side=side, amount=amount, price=price,
            taker_or_maker=taker_or_maker,
        )

    def market_is_open(self, pair: str, timestamp: Any) -> bool:
        """See `futures_common.market_is_open` (shared with `Rithmic`)."""
        return _market_is_open(pair, timestamp)
