"""CME futures contract economics shared by the backtest-only `Futuressim`
exchange (`futures_sim.py`) and the live/dry-run `Rithmic` exchange
(`rithmic.py`).

Kept in one place so the contract multiplier/fee/session-hours logic that
`Futuressim`'s PnL math was cross-checked against a NautilusTrader port for
(see `nautilus_compare/`) cannot silently drift between the backtest and live
code paths.

`get_futures_fee_pct` (fees) IS now live, both backtest and live/dry-run, as of
2026-11-11 - see "CME futures fees" under the Rust engine section of the
repo-root AGENTS.md for the full wiring (`Futuressim`/`Rithmic`/`Ninjatrader`'s
`get_fee()` overrides call it; the native Rust multi-pair engine gets the raw
`get_fee_per_contract()` figure instead and re-divides by its own fill price
per trade, via `PairInput.fee_per_contract`). Before that date every configNT_*
backtest/paper account used one flat, generic %-of-notional `fee` from its
config, which for a CME contract's real flat $/contract/side commission is
wrong in BOTH directions depending on contract notional size - documented
there with real numbers (NQ overcharged ~135x, a small micro-notional contract
like MET undercharged ~6x).

`market_is_open` is NOT called anywhere outside `nautilus_compare/`'s own
reference harness - the mainline Rust backtester and the live trading loop
don't consume it. Kept available here (as it was on `Futuressim`) for parity
between the two exchanges and for whatever wires it up next, but nothing
should assume it's already gating anything live today. In practice, the
Globex closed/maintenance window shows up on the live `Rithmic` feed simply as
"no new bars arrive" - the bot's ordinary stale-data handling is what actually
prevents trading during it right now, not `market_is_open`.
"""

from __future__ import annotations

from datetime import UTC, datetime
from typing import Any

from VulcanTrader.exchange.futures_specs import CFD_CONTRACT_SPECS, FUTURES_CONTRACT_SPECS

# Matches the timeframes actually present in user_data/data/futures/.
TIMEFRAMES = {"1m": "1m", "5m": "5m", "15m": "15m", "30m": "30m", "1h": "1h", "4h": "4h", "1d": "1d"}


def build_market(symbol: str, spec: dict[str, float]) -> dict[str, Any]:
    return {
        "id": symbol,
        "symbol": symbol,
        "base": symbol,
        "quote": "USD",
        "settle": "USD",
        "type": "swap",
        "swap": True,
        "spot": False,
        "linear": True,
        "inverse": False,
        "active": True,
        "contractSize": spec["multiplier"],
        "precision": {"amount": 1, "price": spec.get("tick_size") or 0.01},
        "limits": {
            "amount": {"min": 1, "max": None},
            "price": {"min": None, "max": None},
            "cost": {"min": None, "max": None},
            "leverage": {"min": 1, "max": 1},
        },
    }


def all_markets() -> dict[str, Any]:
    """Every market this fake exchange knows about (futures + CFD placeholders)."""
    markets = {sym: build_market(sym, spec) for sym, spec in FUTURES_CONTRACT_SPECS.items()}
    markets.update({sym: build_market(sym, spec) for sym, spec in CFD_CONTRACT_SPECS.items()})
    return markets


def get_futures_fee_pct(pair: str, price: float, *, is_maker: bool = False) -> float | None:
    """Convert this symbol's flat $/contract commission into an
    equivalent percent-of-notional rate at `price`. CME commissions are flat
    per contract, not a percentage, and the equivalent percentage drifts with
    price - so this must be recomputed per trade, not baked into a single
    config value. `is_maker` is accepted for interface symmetry with
    `fill_fee`'s maker/taker split but unused: CME's per-side exchange fee
    applies regardless of whether the fill was passive or aggressive (matches
    the `Futures.txt` reference's own `maker == taker` default).
    Returns `None` for symbols with no per-contract fee on file (the CFD
    placeholders) - the caller should fall back to the config's global
    `fee`/`fee_maker`.
    """
    spec = FUTURES_CONTRACT_SPECS.get(pair)
    if spec is None or price <= 0:
        return None
    notional_per_contract = price * spec["multiplier"]
    if notional_per_contract <= 0:
        return None
    return spec["fee_per_contract"] / notional_per_contract


def get_fee_per_contract(pair: str) -> float | None:
    """The flat $/contract/side commission itself, price-independent - unlike
    `get_futures_fee_pct`, meant for a caller (the native Rust multi-pair
    engine) that re-divides by its own fill price per trade rather than
    taking one already-converted percentage. `None` for symbols with no
    per-contract fee on file (the CFD placeholders)."""
    spec = FUTURES_CONTRACT_SPECS.get(pair)
    return spec["fee_per_contract"] if spec else None


def market_is_open(pair: str, timestamp: Any) -> bool:
    """CME Globex session gate: Sun 23:00 UTC - Fri 22:00 UTC, minus the
    daily 21:00-22:00 UTC maintenance break. Ported from the `"globex"`
    branch of `is_within_market_hours()` in the user's `Futures.txt`
    reference (its `chrono` weekday numbering, Mon=0...Sun=6, matches
    Python's `datetime.weekday()` exactly - no offset needed). Only
    applies to real CME-family symbols; CFD placeholders (no session data
    supplied) are always open.
    """
    if pair not in FUTURES_CONTRACT_SPECS:
        return True
    ts = timestamp
    if not isinstance(ts, datetime):
        ts = datetime.fromtimestamp(float(ts), tz=UTC)
    elif ts.tzinfo is None:
        ts = ts.replace(tzinfo=UTC)
    dow = ts.weekday()  # Mon=0 ... Sun=6
    time_mins = ts.hour * 60 + ts.minute
    is_weekend_closed = (
        dow == 5  # Saturday - fully closed
        or (dow == 6 and time_mins < 23 * 60)  # Sunday before 23:00 UTC
        or (dow == 4 and time_mins >= 22 * 60)  # Friday after 22:00 UTC
    )
    is_maintenance = 21 * 60 <= time_mins < 22 * 60
    return not is_weekend_closed and not is_maintenance
