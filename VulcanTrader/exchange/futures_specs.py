"""Per-symbol contract specs for the `Futuressim` backtest-only exchange.

`FUTURES_CONTRACT_SPECS` holds real CME contract multipliers ($ notional per
1.00 of price movement), ported from a reference `FeeSchedule`/
`contract_multiplier` table the user supplied from a different project of
theirs (pasted into `Futures.txt` at the repo root). The original 34 were
cross-checked against every symbol actually present in `user_data/data/futures/`.
12 more (RTY, M2K, YM, MYM, M6A, M6B, MICD, MCL, MHG, FDAX, ZN, ZW) were added
2026-10-11 from the user's local NinjaTrader 8 historical data
(`db/minute/<ROOT> */`) once `user_data/data/futuressim/futures/` was built
for them the same way - see the per-symbol comments below for which of these
have a verified vs. best-effort spec (FDAX and MICD both carry real caveats).

`fee_per_contract` mirrors that reference's flat commission model
(`COMMISSION_PER_CONTRACT_PER_SIDE` + `EXCHANGE_FEE_PER_CONTRACT_PER_SIDE` =
$2.25/side for minis; that file's own comment notes micros run cheaper,
~$0.75/side, so the explicit micro contracts here (M-prefixed) use that
instead of the mini default).

`tick_size` is the minimum price increment, filled from standard published
CME contract specs. It only feeds `markets[pair]["precision"]["price"]`
(cosmetic rounding for the limit-order-offset simulation) — it is NOT used
anywhere in the PnL math (that only depends on `multiplier`), so an
imprecise tick size here cannot skew backtest profit/loss. A few of these
(UB, ZC, ZL, ZS treasury/ag ticks depend on whether the price series is
quoted in dollars or cents, which wasn't verified against the actual feather
files) are best-effort rather than confirmed.

`rithmic_exchange` is the venue-routing code the live `Rithmic` exchange
(`VulcanTrader/exchange/rithmic.py`) passes as Rithmic's `exchange` parameter
to `get_front_month_contract`/`subscribe_to_time_bar_data` — CME Group trades
different product families on different matching engines (CME/CBOT/NYMEX/
COMEX), so this is NOT uniformly `"CME"`. Seeded from standard published CME
Group product groupings, not yet verified against a live
`RithmicClient.search_symbols()`/`list_exchanges()` call - worth a sanity
check the first time a real connection is made. Symbols with no
`rithmic_exchange` entry (the ETF/CFD placeholders below) are backtest-only:
Rithmic is a futures-focused venue and does not carry NYSE/Nasdaq equity or
ETF data, so `Rithmic` skips subscribing to them (with a warning) rather than
guessing a routing code.
"""

from __future__ import annotations

# {symbol: (multiplier $/point, tick_size, fee_per_contract_per_side $, rithmic venue)}
FUTURES_CONTRACT_SPECS: dict[str, dict[str, float]] = {
    # FX futures (CME)
    "6A": {"multiplier": 100_000.0, "tick_size": 0.0001, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "6B": {"multiplier": 62_500.0, "tick_size": 0.0001, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "6C": {"multiplier": 100_000.0, "tick_size": 0.00005, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "6E": {"multiplier": 125_000.0, "tick_size": 0.00005, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "6J": {"multiplier": 12_500_000.0, "tick_size": 0.0000005, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "6M": {"multiplier": 500_000.0, "tick_size": 0.00001, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "6N": {"multiplier": 100_000.0, "tick_size": 0.0001, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "6S": {"multiplier": 125_000.0, "tick_size": 0.0001, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "M6E": {"multiplier": 12_500.0, "tick_size": 0.0001, "fee_per_contract": 0.75, "rithmic_exchange": "CME"},
    # Energy (NYMEX)
    "CL": {"multiplier": 1_000.0, "tick_size": 0.01, "fee_per_contract": 2.25, "rithmic_exchange": "NYMEX"},
    # Equity index futures (CME)
    "ES": {"multiplier": 50.0, "tick_size": 0.25, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "NQ": {"multiplier": 20.0, "tick_size": 0.25, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "MES": {"multiplier": 5.0, "tick_size": 0.25, "fee_per_contract": 0.75, "rithmic_exchange": "CME"},
    "MNQ": {"multiplier": 2.0, "tick_size": 0.25, "fee_per_contract": 0.75, "rithmic_exchange": "CME"},
    # Metals (COMEX)
    "GC": {"multiplier": 100.0, "tick_size": 0.10, "fee_per_contract": 2.25, "rithmic_exchange": "COMEX"},
    "MGC": {"multiplier": 10.0, "tick_size": 0.10, "fee_per_contract": 0.75, "rithmic_exchange": "COMEX"},
    "SI": {"multiplier": 5_000.0, "tick_size": 0.005, "fee_per_contract": 2.25, "rithmic_exchange": "COMEX"},
    "SIL": {"multiplier": 1_000.0, "tick_size": 0.005, "fee_per_contract": 2.25, "rithmic_exchange": "COMEX"},
    "HG": {"multiplier": 25_000.0, "tick_size": 0.0005, "fee_per_contract": 2.25, "rithmic_exchange": "COMEX"},
    "PL": {"multiplier": 50.0, "tick_size": 0.10, "fee_per_contract": 2.25, "rithmic_exchange": "NYMEX"},
    # Treasury futures (CBOT)
    "ZB": {"multiplier": 1_000.0, "tick_size": 0.03125, "fee_per_contract": 2.25, "rithmic_exchange": "CBOT"},
    "UB": {"multiplier": 1_000.0, "tick_size": 0.03125, "fee_per_contract": 2.25, "rithmic_exchange": "CBOT"},
    "ZT": {"multiplier": 2_000.0, "tick_size": 0.0078125, "fee_per_contract": 2.25, "rithmic_exchange": "CBOT"},
    # Crypto futures (CME)
    "MBT": {"multiplier": 0.1, "tick_size": 5.0, "fee_per_contract": 0.75, "rithmic_exchange": "CME"},
    "ETH": {"multiplier": 50.0, "tick_size": 0.50, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "MET": {"multiplier": 0.1, "tick_size": 0.50, "fee_per_contract": 0.75, "rithmic_exchange": "CME"},
    # Agricultural futures (CBOT) — price series units (dollars vs cents) unverified
    "ZC": {"multiplier": 50.0, "tick_size": 0.0025, "fee_per_contract": 2.25, "rithmic_exchange": "CBOT"},
    "ZS": {"multiplier": 50.0, "tick_size": 0.0025, "fee_per_contract": 2.25, "rithmic_exchange": "CBOT"},
    "ZL": {"multiplier": 600.0, "tick_size": 0.0001, "fee_per_contract": 2.25, "rithmic_exchange": "CBOT"},
    "ZW": {"multiplier": 50.0, "tick_size": 0.0025, "fee_per_contract": 2.25, "rithmic_exchange": "CBOT"},
    # Treasury note (CBOT)
    "ZN": {"multiplier": 1_000.0, "tick_size": 0.015625, "fee_per_contract": 2.25, "rithmic_exchange": "CBOT"},
    # Equity index futures, round 2 (CME/CBOT) - Dow products (YM/MYM) trade on
    # CBOT's matching engine within CME Group, same as the treasury/ag group above.
    "RTY": {"multiplier": 50.0, "tick_size": 0.10, "fee_per_contract": 2.25, "rithmic_exchange": "CME"},
    "M2K": {"multiplier": 5.0, "tick_size": 0.10, "fee_per_contract": 0.75, "rithmic_exchange": "CME"},
    "YM": {"multiplier": 5.0, "tick_size": 1.0, "fee_per_contract": 2.25, "rithmic_exchange": "CBOT"},
    "MYM": {"multiplier": 0.50, "tick_size": 1.0, "fee_per_contract": 0.75, "rithmic_exchange": "CBOT"},
    # More FX micros (CME)
    "M6A": {"multiplier": 10_000.0, "tick_size": 0.0001, "fee_per_contract": 0.75, "rithmic_exchange": "CME"},
    "M6B": {"multiplier": 6_250.0, "tick_size": 0.0001, "fee_per_contract": 0.75, "rithmic_exchange": "CME"},
    # Micro CAD/USD. CME's own Globex code for this product is "MCD" - no
    # public CME ticker "MICD" exists. The user's local NinjaTrader 8 data is
    # filed under "MICD" though, and its decoded price range (0.67-0.74,
    # matching real CAD/USD) confirms the underlying is right even if the
    # exact official ticker mapping is unverified - same "best effort, not
    # confirmed" status this file already gives UB/ZC/ZL/ZS's tick units.
    "MICD": {"multiplier": 10_000.0, "tick_size": 0.0001, "fee_per_contract": 0.75, "rithmic_exchange": "CME"},
    # Energy / metals micros (NYMEX/COMEX)
    "MCL": {"multiplier": 100.0, "tick_size": 0.01, "fee_per_contract": 0.75, "rithmic_exchange": "NYMEX"},
    "MHG": {"multiplier": 2_500.0, "tick_size": 0.0005, "fee_per_contract": 0.75, "rithmic_exchange": "COMEX"},
    # DAX futures (Eurex, Deutsche Börse) - EUR-denominated, NOT USD like
    # every other symbol here. `build_market()` hardcodes `settle: "USD"` and
    # nothing in this codebase does FX conversion, so this multiplier (the
    # real EUR25/point) gets treated as USD25/point - a known, flagged
    # approximation, not a silent bug: PnL will drift from real economics by
    # whatever EURUSD is at the time. No `rithmic_exchange` - Eurex routing
    # through Rithmic has no precedent in this file (unlike the CME-family
    # venues above) and is unverified, so this is backtest-only like the
    # ETF/CFD placeholders below.
    "FDAX": {"multiplier": 25.0, "tick_size": 0.5, "fee_per_contract": 2.25},
    # ETFs / leveraged ETFs (multiplier=1, price is per-share notional directly).
    # No `rithmic_exchange` - these are NYSE Arca/Nasdaq equities, not CME-family
    # futures, and Rithmic (a futures-focused venue) doesn't carry them. Backtest-only.
    "SPY": {"multiplier": 1.0, "tick_size": 0.01, "fee_per_contract": 2.25},
    "QQQ": {"multiplier": 1.0, "tick_size": 0.01, "fee_per_contract": 2.25},
    "TQQQ": {"multiplier": 1.0, "tick_size": 0.01, "fee_per_contract": 2.25},
    "SQQQ": {"multiplier": 1.0, "tick_size": 0.01, "fee_per_contract": 2.25},
    "TLT": {"multiplier": 1.0, "tick_size": 0.01, "fee_per_contract": 2.25},
}

# CFD symbols (from user_data/data/futures/, previously nested under CFDs/).
# No authoritative per-instrument spec was supplied for these, so they get a
# flat placeholder: multiplier=1 means the fake exchange behaves exactly like
# a normal crypto pair for them (no contract rounding, global % fee from
# config, no Globex session gating) until real CFD contract terms are known.
CFD_CONTRACT_SPECS: dict[str, dict[str, float]] = {
    sym: {"multiplier": 1.0, "tick_size": 0.01}
    for sym in (
        "SPX500", "UK100", "UK250", "UKOIL", "US2000", "US30", "USDX", "USOIL",
        "VTI", "XAGUSD", "XAUUSD", "XLE", "XLF",
    )
}


def all_symbols() -> list[str]:
    """Every symbol this fake exchange knows about (futures + CFD placeholders)."""
    return list(FUTURES_CONTRACT_SPECS) + list(CFD_CONTRACT_SPECS)
