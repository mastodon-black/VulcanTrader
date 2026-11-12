// =============================================================================
// MULTI-PAIR BACKTEST ENGINE — native replacement for `_joint_multi_pair_
// backtest` (VulcanTrader/rust_backtest.py)
// =============================================================================
//
// `_joint_multi_pair_backtest` is a ~1400-line, bar-by-bar, cross-pair,
// shared-capital simulator that (per its own docstring/comments) invokes up
// to 13 freqtrade-style Python strategy callbacks. Profiling showed it at 27%
// of a real backtest's wall time — `do_exit_check` and `funding_fee_for` each
// called 33,222 times, `funding_fee_for` dominated by `pd.Timestamp`
// construction overhead rather than algorithmic cost (it's already O(log n)
// via a precomputed cumsum).
//
// This module is a faithful, line-traceable Rust port of that bar-level state
// machine, with every strategy callback routed through the `StrategyCallbacks`
// trait below rather than hardcoded — `python.rs`'s `PyStrategyCallbacks`
// calls into the REAL Python method for whichever hooks a given strategy
// overrides, degrading to `NativeDefaults`'s hardcoded value (verified against
// each base method's actual body, not assumed) for the rest:
//   - leverage()            -> `_resolve_leverage` clamps `min(max(1.0,1.0),
//                               max_leverage)` = 1.0 exactly (base leverage()
//                               returns 1.0, and 1.0 is already >= the clamp
//                               floor).
//   - custom_stake_amount() -> returns `proposed_stake` unchanged.
//   - custom_entry_price()/custom_exit_price() -> returns the proposed rate
//                               unchanged (order_types defaults to "limit",
//                               so these ARE called by default, but are a
//                               no-op when unoverridden). custom_exit_price()
//                               is not bridged (see below) — only its default
//                               is relevant here.
//   - confirm_trade_entry()/custom_exit() -> both gate on
//                               `_market_ending_within_buffer`, which reads
//                               `market.get("expiry")` — populated ONLY by
//                               VulcanTrader/exchange/polymarket.py. For any
//                               non-Polymarket exchange this is always False,
//                               making confirm_trade_entry always True and
//                               custom_exit always None when NOT overridden.
//   - confirm_trade_exit()  -> always True, unconditionally, when NOT
//                               overridden — verified against `IStrategy.
//                               confirm_trade_exit`'s actual body (no
//                               market-expiry conditional here, unlike
//                               confirm_trade_entry/custom_exit).
//   - custom_stoploss()     -> never called at all unless the strategy sets
//                               `use_custom_stoploss = True`.
//
// Bridged when overridden: `leverage`/`custom_stake_amount`/
// `confirm_trade_entry`/`custom_entry_price` (entry-time; none take a live
// `trade`), `custom_stoploss`/`custom_exit`/`confirm_trade_exit` (per-bar or
// per-exit-candidate; need a real `LocalTrade` — see `on_trade_opened`/
// `adjust_min_max_rates`/`on_trade_closed`). `confirm_trade_exit` was the
// single largest source of native-engine exclusions in the real deployed
// fleet (64/160 strategies, 40%, ALL via the same `ChampionPositionSizerMixin`
// pattern — see project memory) before being bridged 2026-09-27; bridging it
// also surfaced a real, separate bug in `_joint_multi_pair_backtest`'s
// `fill_entry` (missing `trade.recalc_open_trade_value()` after the fill),
// fixed there rather than replicated here. NOT bridged, ever — a strategy
// overriding either of these still falls all the way back to
// `_joint_multi_pair_backtest`: `custom_exit_price` (needs the resting-order/
// Order-notification apparatus this engine doesn't model) and `custom_sell`
// (freqtrade's pre-3.0 name; `IStrategy.custom_exit()`'s own base
// implementation falls through to it, which would silently defeat a
// per-hook override check). The Python-side eligibility gate
// (`rust_backtest.py`) verifies all of this before
// dispatching here — this module does not re-check it.
//
// Exit-priority state machine mirrors `do_exit_check` exactly, including four
// documented historical correctness fixes preserved here on purpose:
//   1. Funding fees are baked into `bound_profit` BEFORE the trailing/ROI
//      trigger comparison, not after (see `funding_so_far` below).
//   2. Every stop-loss ratchet candidate is rounded to tick size BEFORE
//      comparing against the current stop (see `round_stop`).
//   3. The ROI exit price uses a fee-consistent solve with a correction for
//      "a new tighter tier just took effect and the candle gapped through
//      it" (see the `time_in_position == roi_entry_period` branch).
//   4. A trailing-stop exit on the entry candle uses one of TWO different
//      pessimistic-pricing formulas depending on whether custom_stoploss is
//      in play for this run (`cfg.use_custom_stoploss`) — both are
//      implemented (see the `TrailingStop && time_in_position == 0` branch in
//      `do_exit_check`), since custom_stoploss is a real, active path now.
// =============================================================================

/// Opaque handle to a live "trade" the callback implementation owns — this
/// module never inspects it, just passes it back to whichever
/// `StrategyCallbacks` impl issued it (a real Python `LocalTrade` object id,
/// for `PyStrategyCallbacks`; unused for `NativeDefaults`).
pub type TradeHandle = u64;

/// The freqtrade-style strategy callback surface `_joint_multi_pair_backtest`
/// invokes, abstracted so this module stays pure Rust (no `pyo3` dependency,
/// still testable with plain `cargo test`). `NativeDefaults` below reproduces
/// exactly the hardcoded behavior this module used before the callback
/// bridge existed; `python.rs`'s `PyStrategyCallbacks` (pyo3-gated) calls
/// into real Python only for the hooks a given strategy actually overrides,
/// falling back to these same native defaults for the rest.
///
/// Split into entry-time methods (called once per candidate entry — cheap
/// even when bridged to Python, since entries are rare relative to bars) and
/// per-bar methods (`custom_stoploss`/`custom_exit`/`adjust_min_max_rates`,
/// called once per open position per bar — the ones that actually cost
/// something when bridged).
pub trait StrategyCallbacks {
    /// `strategy.leverage(...)`. Caller still applies the
    /// `min(max(lev, 1.0), max_leverage)` clamp — this returns the raw value.
    fn leverage(&mut self, pair: &str, bar_ns: i64, rate: f64, max_leverage: f64, direction: &str) -> f64;
    /// `strategy.custom_stake_amount(...)`. Returns `proposed` unchanged by default.
    fn custom_stake_amount(&mut self, pair: &str, bar_ns: i64, rate: f64, proposed: f64,
        max_stake: f64, leverage: f64, direction: &str) -> f64;
    /// `strategy.confirm_trade_entry(...)`.
    fn confirm_trade_entry(&mut self, pair: &str, bar_ns: i64, amount: f64, rate: f64, direction: &str) -> bool;
    /// `strategy.custom_entry_price(...)`. Returns `proposed_rate` unchanged by default.
    fn custom_entry_price(&mut self, pair: &str, bar_ns: i64, proposed_rate: f64, direction: &str) -> f64;

    /// A position just opened. Implementations that need a live `LocalTrade`
    /// (for `custom_stoploss`/`custom_exit`, which take `trade=...`) construct
    /// it here and return a handle; `NativeDefaults` returns a dummy handle.
    #[allow(clippy::too_many_arguments)]
    fn on_trade_opened(&mut self, pair: &str, bar_ns: i64, is_short: bool, amount: f64,
        rate: f64, stake: f64, leverage: f64, fee_open: f64) -> TradeHandle;
    /// `trade.adjust_min_max_rates(high, low)` — called every bar a position
    /// with a live handle is open, before evaluating exit conditions (fixing
    /// the historical bug where a strategy's own `custom_stoploss`/
    /// `custom_exit` reading `trade.max_rate`/`min_rate` saw `None` forever).
    fn adjust_min_max_rates(&mut self, handle: TradeHandle, high: f64, low: f64);
    /// `strategy.custom_stoploss(...)`. Returns `None` (no ratchet) by default.
    fn custom_stoploss(&mut self, handle: TradeHandle, pair: &str, bar_ns: i64,
        current_rate: f64, current_profit: f64) -> Option<f64>;
    /// `strategy.custom_exit(...)`, which in Python returns `str | bool |
    /// None`. Contract here: `None` = not triggered (Python returned
    /// `None`/`False`/falsy); `Some(tag)` = triggered, where `tag` is the
    /// returned string, or an EMPTY string if Python returned bare `True`
    /// (triggered with no specific tag — `run_rust_backtest` already treats
    /// an empty/falsy tag as "use the default `custom_exit` label", so this
    /// is not a special case downstream, just a way to keep "triggered" and
    /// "has a string tag" distinguishable here). Do not conflate "no tag"
    /// with "not triggered" — those are different Python return values.
    fn custom_exit(&mut self, handle: TradeHandle, pair: &str, bar_ns: i64,
        current_rate: f64, current_profit: f64) -> Option<String>;
    /// The position closed — implementations holding a live `LocalTrade`
    /// finalize and drop it here.
    fn on_trade_closed(&mut self, handle: TradeHandle, close_bar_ns: i64, profit_ratio: f64, profit_abs: f64);

    /// Mirrors `_set_dataframe_max_index(pair, required_startup + bar_idx)` —
    /// called once per (pair, bar), for EVERY pair with data this bar
    /// (regardless of position state), before any other callback for that
    /// pair/bar. A strategy's entry-time hooks can read `self.dp.
    /// get_analyzed_dataframe(pair)` just as legitimately as `custom_stoploss`/
    /// `custom_exit` can, so this runs unconditionally per bar for any bridged
    /// pair — a no-op for `NativeDefaults` and for `PyStrategyCallbacks` when
    /// no DataProvider wiring is needed this run.
    fn set_bar_cursor(&mut self, pair: &str, bar_idx: i64);
    /// Keeps a live `self.wallets` proxy (if any) in sync with this loop's own
    /// running balance / capital tied up in open positions — called whenever
    /// either changes (entry accepted, position closed, force-close). A no-op
    /// for `NativeDefaults`.
    fn on_balance_changed(&mut self, running_balance: f64, tied_up_capital: f64);
    /// `strategy.confirm_trade_exit(...)`. Called once per exit CANDIDATE
    /// (Signal/CustomExit/Stoploss/RoiTarget/TrailingStop/MaxHoldPeriod), right
    /// before it would close the position — a `false` here means "try the
    /// next candidate this same bar" (mirrors `do_exit_check`'s `for exit_
    /// reason, ... in candidates: if not confirm_exit(...): continue`), not
    /// "the position stays open forever". Base default: always true,
    /// unconditionally (verified against `IStrategy.confirm_trade_exit`'s
    /// actual body — no market-expiry conditional here, unlike
    /// `confirm_trade_entry`/`custom_exit`). NOT called for the end-of-data
    /// force-close (`handle_left_open` doesn't ask the strategy either).
    fn confirm_trade_exit(&mut self, handle: TradeHandle, pair: &str, bar_ns: i64,
        amount: f64, rate: f64, exit_reason: &str) -> bool;
}

/// Reproduces exactly the hardcoded defaults this module used before the
/// callback bridge existed — see the module doc comment for why each of
/// these is the correct unoverridden-`IStrategy` behavior, verified against
/// the real base-class method bodies. Used by every existing test (so this
/// refactor is behavior-preserving) and as the fallback inside
/// `PyStrategyCallbacks` for whichever hooks a given strategy does NOT
/// override.
pub struct NativeDefaults;

impl StrategyCallbacks for NativeDefaults {
    fn leverage(&mut self, _pair: &str, _bar_ns: i64, _rate: f64, _max_leverage: f64, _direction: &str) -> f64 { 1.0 }
    fn custom_stake_amount(&mut self, _pair: &str, _bar_ns: i64, _rate: f64, proposed: f64,
        _max_stake: f64, _leverage: f64, _direction: &str) -> f64 { proposed }
    fn confirm_trade_entry(&mut self, _pair: &str, _bar_ns: i64, _amount: f64, _rate: f64, _direction: &str) -> bool { true }
    fn custom_entry_price(&mut self, _pair: &str, _bar_ns: i64, proposed_rate: f64, _direction: &str) -> f64 { proposed_rate }
    fn on_trade_opened(&mut self, _pair: &str, _bar_ns: i64, _is_short: bool, _amount: f64,
        _rate: f64, _stake: f64, _leverage: f64, _fee_open: f64) -> TradeHandle { 0 }
    fn adjust_min_max_rates(&mut self, _handle: TradeHandle, _high: f64, _low: f64) {}
    fn custom_stoploss(&mut self, _handle: TradeHandle, _pair: &str, _bar_ns: i64,
        _current_rate: f64, _current_profit: f64) -> Option<f64> { None }
    fn custom_exit(&mut self, _handle: TradeHandle, _pair: &str, _bar_ns: i64,
        _current_rate: f64, _current_profit: f64) -> Option<String> { None }
    fn on_trade_closed(&mut self, _handle: TradeHandle, _close_bar_ns: i64, _profit_ratio: f64, _profit_abs: f64) {}
    fn set_bar_cursor(&mut self, _pair: &str, _bar_idx: i64) {}
    fn on_balance_changed(&mut self, _running_balance: f64, _tied_up_capital: f64) {}
    fn confirm_trade_exit(&mut self, _handle: TradeHandle, _pair: &str, _bar_ns: i64,
        _amount: f64, _rate: f64, _exit_reason: &str) -> bool { true }
}

/// Per-pair precomputed inputs, mirroring what `_joint_multi_pair_backtest`
/// already builds per pair before its own loop (dates/OHLC/shifted signals/
/// precision/exchange max leverage) — computed identically in Python either
/// way, just handed to Rust instead of a Python closure.
#[derive(Clone)]
pub struct PairInput {
    pub pair: String,
    pub dates_ns: Vec<i64>,
    // f64, not f32 like this crate's bulk indicator arrays: this module
    // produces trade-level, decision-facing output (not a hot per-bar
    // indicator pass), and f32 here measurably drifted stake/profit values
    // over a compounding multi-hundred-trade run relative to the Python
    // engine's own f64 arithmetic — same reasoning `compute_result_metrics`
    // (python.rs) already applies: "stats are user-facing numbers; matching
    // pandas/numpy bit-for-bit matters more here than the few ns f32 would
    // save."
    pub open: Vec<f64>,
    pub high: Vec<f64>,
    pub low: Vec<f64>,
    /// Already `.shift(1)`-ed, exactly like `_PairState.el/xl/es/xs`.
    pub enter_long: Vec<u8>,
    pub exit_long: Vec<u8>,
    pub enter_short: Vec<u8>,
    pub exit_short: Vec<u8>,
    /// Per-bar tick size; `NaN` means "unknown, don't round" (see `round_stop`).
    pub precision: Vec<f64>,
    pub exchange_max_leverage: f64,
    /// Exchange's lot-size tick for the ORDER AMOUNT (not price) — ccxt
    /// `market['precision']['amount']`, TICK_SIZE mode. `<= 0` or `NaN` means
    /// "unknown, don't round" (mirrors `do_enter`'s `if exchange is not None`
    /// guard: no exchange, no rounding).
    pub amount_precision: f64,
    /// ccxt `contract_size` for this pair (1.0 for most spot/linear pairs).
    pub contract_size: f64,
    /// CME-style flat $/contract/side commission (Futuressim/Rithmic/Ninjatrader
    /// only). `<= 0.0` means "no override" - every other exchange's `fill_fee`
    /// is completely unaffected and uses `EngineConfig.fee_taker`/`fee_maker`
    /// exactly as before. When set, `fill_fee` divides it by `contract_size *
    /// <this fill's own price>` instead - CME commissions are flat per
    /// contract, not a percentage, and the equivalent percentage drifts with
    /// price (see `futures_common.get_futures_fee_pct` in Python, which this
    /// mirrors natively so the native engine doesn't need one precomputed,
    /// already-stale-by-the-next-fill percentage from the Python side).
    pub fee_per_contract: f64,
    /// Funding-fee lookup: `funding_dates_ns[k]` paired with
    /// `funding_cumsum[k+1] - funding_cumsum[0..]`, i.e. `funding_cumsum` has
    /// one more element than `funding_dates_ns` (a leading 0.0), exactly
    /// mirroring `_funding_cumsum`'s `np.concatenate(([0.0], np.cumsum(...)))`.
    /// Empty when this pair has no funding/mark data (funding fee is then 0).
    pub funding_dates_ns: Vec<i64>,
    pub funding_cumsum: Vec<f64>,
}

#[derive(Clone, serde::Deserialize)]
pub struct EngineConfig {
    pub can_short: bool,
    /// `<= 0` means unlimited (matches `max_open_trades = int(config.get(...))`).
    pub max_open_trades: i64,
    // f64, not f32: these are config SCALARS (a fee rate, a stoploss ratio),
    // not bulk per-bar arrays — storing e.g. 0.00045 as f32 doesn't round-trip
    // exactly (came back as 0.00044999999227...), which is a real, measured,
    // compounding-visible drift from the Python engine's f64 arithmetic, not
    // just float noise. Same reasoning as `PairInput.open/high/low`.
    pub fee_taker: f64,
    /// A resting order that fills is a maker fill (see `fill_fee`). With no
    /// explicit maker rate configured this equals `fee_taker`.
    pub fee_maker: f64,
    pub base_stoploss: f64,
    pub trailing_enabled: bool,
    pub trailing_trigger: f64,
    pub trailing_offset: f64,
    pub trailing_offset_set: bool,
    pub trailing_only_offset: bool,
    /// Legacy order (pre 2026-11-06): `custom_exit` saw this bar's high/low
    /// through `trade.min_rate/max_rate` and exited at its open. See
    /// `do_exit_check`.
    #[serde(default)]
    pub custom_exit_lookahead: bool,
    /// Legacy entry-candle trailing price (pre 2026-11-06): the arming
    /// offset, a leveraged profit ratio, used as a price move.
    #[serde(default)]
    pub trailing_offset_unlevered: bool,
    /// Isolated Hyperliquid liquidation (`liquidation_price_isolated` in
    /// rust_backtest.py, the formula dry-run bots use). Off = never
    /// liquidates (the only behaviour before 2026-11-06).
    #[serde(default)]
    pub liquidation_enabled: bool,
    #[serde(default)]
    pub liquidation_buffer: f64,
    /// Legacy intrabar order (pre 2026-11-06): the trailing worst case only
    /// on the entry candle, and ROI checked before the trailing stop.
    #[serde(default)]
    pub intrabar_optimistic: bool,
    /// freqtrade exit settings live trading obeys (ignored by this engine
    /// before 2026-11-07): no exit signals / custom_exit when false.
    #[serde(default = "default_true")]
    pub use_exit_signal: bool,
    /// Take an exit signal only when profit at the open > exit_profit_offset.
    #[serde(default)]
    pub exit_profit_only: bool,
    #[serde(default)]
    pub exit_profit_offset: f64,
    /// Skip ROI on a bar with a same-side entry signal.
    #[serde(default)]
    pub ignore_roi_if_entry_signal: bool,
    /// Legacy: ignore the three settings above.
    #[serde(default)]
    pub exit_settings_ignored: bool,
    pub roi_enabled: bool,
    /// ROI targets per tier (any number of tiers, sorted by period; live
    /// freqtrade uses them all - this used to hold only the first four).
    pub roi_vals: Vec<f64>,
    /// Bar thresholds per tier, matching `roi_vals`.
    pub roi_periods: Vec<i64>,
    pub max_hold_period: i64,
    pub is_futures: bool,
    /// Whether `custom_stoploss()` is ever consulted (freqtrade only calls it
    /// at all when the strategy sets `use_custom_stoploss = True`). Also
    /// selects which of the two entry-candle trailing-stop pessimistic-price
    /// formulas applies — see the `TrailingStop && time_in_position == 0`
    /// branch in `do_exit_check`.
    pub use_custom_stoploss: bool,
    pub compounding: bool,
    pub starting_balance: f64,
    pub tradable_ratio: f64,
    pub max_trade_amount: f64,
}

fn default_true() -> bool { true }

/// One closed trade, field-for-field matching the dict `close_position`/the
/// end-of-run force-close block append to `trades` in the Python engine —
/// `run_rust_backtest` consumes this exact shape (`_dates` is attached by the
/// Python caller afterward, from its own already-known per-pair dates array,
/// same as it does today for `_joint_multi_pair_backtest`'s output).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TradeOut {
    pub pair: String,
    pub is_short: bool,
    pub entry_row: i64,
    pub exit_row: i64,
    pub entry_price: f64,
    pub base_exit_price: f64,
    pub exit_price: f64,
    pub base_entry_price: f64,
    pub fee_open: f64,
    pub fee_close: f64,
    pub profit_ratio: f64,
    pub profit_abs: f64,
    pub leverage: f64,
    pub exit_reason: &'static str,
    pub exit_reason_tag: Option<String>,
    pub stake: f64,
    pub funding_fees: f64,
}

pub struct EngineOutput {
    pub trades: Vec<TradeOut>,
    pub rejected: i64,
}

/// `_round_stop`: ccxt `TICK_SIZE` rounding, ROUND_DOWN for shorts / ROUND_UP
/// for longs. `NaN` precision means "unknown, skip rounding" (see
/// `_precision_per_bar`'s doc comment on why NaN must not become "round to
/// zero digits").
fn round_stop(price: f64, precision: f64, is_short: bool) -> f64 {
    if precision.is_nan() || precision <= 0.0 {
        return price;
    }
    let n = price / precision;
    let rounded = if is_short { n.floor() } else { n.ceil() };
    rounded * precision
}

/// `exchange.amount_to_contract_precision`: ccxt TICK_SIZE + TRUNCATE, via
/// contracts. Traced exactly against `ccxt.base.decimal_to_precision`'s
/// TICK_SIZE/TRUNCATE branch (`missing = amount % tick; amount -= missing`
/// for amount >= 0, which is precisely `floor(amount/tick)*tick`) and
/// `exchange_utils.amount_to_contract_precision`'s contracts round-trip
/// (`amount -> contracts = amount/contract_size -> truncate -> amount =
/// contracts*contract_size`). Hyperliquid (this repo's primary exchange) uses
/// TICK_SIZE — confirmed directly against ccxt's own hyperliquid.py. Exchanges
/// using DECIMAL_PLACES mode instead (bitunix/drift) are excluded by the
/// Python-side eligibility gate rather than guessed at here.
///
/// `amount_precision <= 0` or `NaN` means "unknown, don't round" (mirrors
/// `do_enter`'s `if exchange is not None` guard — no exchange, no rounding).
/// Returns `None` when the resulting amount would be `<= 0` (mirrors `if
/// amount_rounded > 0: ...` — Python keeps the UNROUNDED amount/stake in that
/// case rather than rejecting the trade).
fn truncate_amount(amount: f64, amount_precision: f64, contract_size: f64) -> Option<f64> {
    if amount_precision.is_nan() || amount_precision <= 0.0 {
        return None;
    }
    let cs = if contract_size.is_finite() && contract_size > 0.0 && contract_size != 1.0 {
        contract_size
    } else {
        1.0
    };
    let contracts = amount / cs;
    let truncated_contracts = (contracts / amount_precision).floor() * amount_precision;
    let result = truncated_contracts * cs;
    if result > 0.0 { Some(result) } else { None }
}

/// Binary-search cumsum lookup, replacing `funding_fee_for`'s `pd.Timestamp`
/// `.ceil()`/`.floor()`/`.as_unit()` construction with native `i64` ns
/// arithmetic — this Python-object overhead, not the (already O(log n))
/// algorithm, was the actual cost `funding_fee_for` paid 33,222 times.
///
/// `unit_ns` mirrors `dates.dtype.unit`'s ceil/floor snapping (typically
/// nanoseconds already, so this is usually a no-op) — pass `1` when the
/// funding-mark dates are at their native (already-integral) resolution.
fn funding_fee_for(
    funding_dates_ns: &[i64],
    funding_cumsum: &[f64],
    amount: f64,
    is_short: bool,
    open_date_ns: i64,
    close_date_ns: i64,
) -> f64 {
    if funding_dates_ns.is_empty() {
        return 0.0;
    }
    let lo_idx = funding_dates_ns.partition_point(|&d| d < open_date_ns);
    let hi_idx = funding_dates_ns.partition_point(|&d| d <= close_date_ns);
    let fees = (funding_cumsum[hi_idx] - funding_cumsum[lo_idx]) * amount;
    let fees = if fees.is_nan() { 0.0 } else { fees };
    if is_short { fees } else { -fees }
}

struct PairState<'a> {
    input: &'a PairInput,
    n: usize,
    row_idx: usize,

    in_position: bool,
    is_short: bool,
    entry_price: f64,
    base_price: f64,
    entry_row: i64,
    trade_leverage: f64,
    stop_loss_price: f64,
    /// 0.0 = no liquidation modelled for this trade.
    liq_price: f64,
    stop_is_trailing: bool,
    stop_loss_dist: f64,
    trailing_activated: bool,
    trade_stake: f64,
    trade_amount: f64,
    entry_fee: f64,
    /// Set by `on_trade_opened` while a position is open; passed back to
    /// `adjust_min_max_rates`/`custom_stoploss`/`custom_exit`/`on_trade_closed`.
    trade_handle: TradeHandle,
}

impl<'a> PairState<'a> {
    fn new(input: &'a PairInput) -> Self {
        let n = input.dates_ns.len();
        PairState {
            input,
            n,
            row_idx: 0,
            in_position: false,
            is_short: false,
            entry_price: 0.0,
            base_price: 0.0,
            entry_row: -1,
            trade_leverage: 1.0,
            stop_loss_price: 0.0,
            liq_price: 0.0,
            stop_is_trailing: false,
            stop_loss_dist: 0.0,
            trailing_activated: false,
            trade_stake: 0.0,
            trade_amount: 0.0,
            entry_fee: 0.0,
            trade_handle: 0,
        }
    }

    fn funding_fee(&self, amount: f64, open_row: i64, close_row: usize) -> f64 {
        funding_fee_for(
            &self.input.funding_dates_ns,
            &self.input.funding_cumsum,
            amount,
            self.is_short,
            self.input.dates_ns[open_row as usize],
            self.input.dates_ns[close_row],
        )
    }
}

/// `entry_dir`: which direction (if any) this bar's shifted signals ask for.
fn entry_dir(input: &PairInput, i: usize, can_short: bool) -> Option<bool> {
    let enter_long = input.enter_long[i] == 1;
    let exit_long = input.exit_long[i] == 1;
    let enter_short = can_short && input.enter_short[i] == 1;
    let exit_short = can_short && input.exit_short[i] == 1;
    if enter_long && !(exit_long || enter_short) {
        return Some(false); // false = long
    }
    if enter_short && !(exit_short || enter_long) {
        return Some(true); // true = short
    }
    None
}

/// `_EXIT_REASON_MAP` (rust_backtest.py), ported so `confirm_trade_exit()`'s
/// `exit_reason`/`sell_reason` kwargs and the final stored `exit_reason_tag`
/// see the exact same freqtrade-style string a strategy already expects
/// (`['roi','stop_loss','trailing_stop_loss','exit_signal','force_exit']` —
/// the subset this engine's candidates can ever produce). For `CustomExit`,
/// a non-empty tag from `custom_exit()` wins over the generic mapped label,
/// matching `if exit_reason == "CustomExit" and custom_tag: reason_tag =
/// custom_tag` exactly (Python truthiness: an empty tag string doesn't count).
fn exit_reason_tag_str(exit_reason: &str, custom_tag: &Option<String>) -> String {
    if exit_reason == "CustomExit" {
        if let Some(t) = custom_tag {
            if !t.is_empty() {
                return t.clone();
            }
        }
        return "custom_exit".to_string();
    }
    match exit_reason {
        "Signal" => "exit_signal",
        "RoiTarget" => "roi",
        "Stoploss" => "stop_loss",
        "TrailingStop" => "trailing_stop_loss",
        "Liquidation" => "liquidation",
        "MaxHoldPeriod" | "ForceExit" => "force_exit",
        _ => "exit_signal",
    }.to_string()
}

/// `fill_fee`: a limit order priced on the favourable side of the market
/// rests (maker); one priced at/through the market takes (taker). Every
/// order on this native-default path fills at the bar's own open (no resting
/// simulation — see the module doc comment), so this always resolves to the
/// taker rate in practice, but is kept general for traceability against the
/// CME-style per-pair fee override, or `None` to use the caller's flat rate.
/// `fee_per_contract`/`contract_size` are a specific pair's own `PairInput`
/// fields (`<= 0.0` for anything but Futuressim/Rithmic/Ninjatrader, so every
/// other exchange always gets `None` here and is completely unaffected).
/// Shared by `fill_fee` and every other spot that needs a %-of-notional fee
/// assumption at some `price` (`open_profit`, `eval_custom_exit`'s
/// `current_profit`, ROI-tier price solving, the end-of-data force exit) -
/// a futures pair's unrealized profit (what strategies like `custom_exit`
/// branch on) must use the same real CME economics as the fee actually
/// charged at a fill, not the flat `EngineConfig` rate every other exchange
/// uses. Mirrors `futures_common.get_futures_fee_pct` in Python, which notes
/// the real exchange fee applies the same whether a fill is passive or
/// aggressive - unlike the flat-rate maker/taker split, there is no `rests`
/// branch here.
fn futures_fee_override(fee_per_contract: f64, contract_size: f64, price: f64) -> Option<f64> {
    if fee_per_contract > 0.0 && contract_size > 0.0 && price > 0.0 {
        Some(fee_per_contract / (price * contract_size))
    } else {
        None
    }
}

/// `fill_fee`: a limit order priced on the favourable side of the market
/// rests (maker); one priced at/through the market takes (taker). Every
/// order on this native-default path fills at the bar's own open (no resting
/// simulation — see the module doc comment), so this always resolves to the
/// taker rate in practice, but is kept general for traceability against the
/// Python source.
fn fill_fee(
    price: f64, reference: f64, is_buy: bool, fee_taker: f64, fee_maker: f64,
    fee_per_contract: f64, contract_size: f64,
) -> f64 {
    if let Some(r) = futures_fee_override(fee_per_contract, contract_size, price) {
        return r;
    }
    let rests = if is_buy { price < reference } else { price > reference };
    if rests { fee_maker } else { fee_taker }
}

/// `do_enter`: resolve entry price/leverage/stake via the callback bridge
/// (each falling back to its `NativeDefaults` value when unoverridden), then
/// open the position — always fills same-bar at `open[i]` since no
/// resting-order simulation applies on this path (see module doc comment;
/// the eligibility gate already verified no limit-order offsets are
/// configured). Mirrors `do_enter`'s exact call order: custom_entry_price()
/// first (clamped into this bar's own range), then leverage(), then
/// custom_stake_amount() (given that leverage), then the capital/lot-size
/// clamps, then confirm_trade_entry() last.
#[allow(clippy::too_many_arguments)]
fn do_enter(
    cfg: &EngineConfig,
    p: &mut PairState,
    i: usize,
    is_short: bool,
    proposed_stake: f64,
    running_balance: f64,
    tied_up_capital: f64,
    callbacks: &mut dyn StrategyCallbacks,
) -> Option<(f64, f64)> {
    let pair = p.input.pair.as_str();
    let bar_ns = p.input.dates_ns[i];
    let direction = if is_short { "short" } else { "long" };
    let open_i = p.input.open[i] as f64;

    // custom_entry_price(): base default returns proposed_rate (open[i])
    // unchanged. "We can't place orders higher/lower than this candle's own
    // high/low" clamp applies regardless of whether it was overridden.
    let resolved_price = callbacks.custom_entry_price(pair, bar_ns, open_i, direction);
    let base_price = if is_short {
        resolved_price.max(p.input.low[i])
    } else {
        resolved_price.min(p.input.high[i])
    };

    // leverage(): clamp exactly like `_resolve_leverage`.
    let raw_lev = callbacks.leverage(pair, bar_ns, base_price, p.input.exchange_max_leverage, direction);
    let lev = raw_lev.max(1.0).min(p.input.exchange_max_leverage);

    // custom_stake_amount(): base default returns proposed_stake unchanged.
    let max_stake_for_callback = (running_balance * cfg.tradable_ratio - tied_up_capital).max(0.0);
    let stake = callbacks.custom_stake_amount(pair, bar_ns, base_price, proposed_stake, max_stake_for_callback, lev, direction);
    let lev = lev.max(0.1);

    // Mirrors `wallets.validate_stake_amount`: clamp whatever custom_stake_amount
    // proposed to what capital is actually still available.
    let available = (running_balance * cfg.tradable_ratio - tied_up_capital).max(0.0);
    let stake = stake.min(available);
    if stake <= 0.0 {
        return None;
    }

    let mut actual_stake = stake;
    let mut amount = stake * lev / base_price;

    // freqtrade's `_enter_trade`: the amount is rounded to the exchange's lot
    // size, and the stake actually tied up is BACK-CALCULATED from that
    // rounded amount — not the nominal proposed stake (see `truncate_amount`'s
    // doc comment; this is what `do_enter`'s own Python comment calls "a
    // small but real amount each trade, compounding over thousands of trades
    // into a wallet-balance drift").
    if let Some(amount_rounded) = truncate_amount(amount, p.input.amount_precision, p.input.contract_size) {
        amount = amount_rounded;
        actual_stake = amount * base_price / lev;
    }

    // confirm_trade_entry(): called LAST, with the final rounded amount/rate
    // — base default True (verified dead outside Polymarket, which the
    // eligibility gate excludes for strategies that don't override it).
    if !callbacks.confirm_trade_entry(pair, bar_ns, amount, base_price, direction) {
        return None;
    }

    p.entry_fee = fill_fee(
        base_price, p.input.open[i] as f64, !is_short, cfg.fee_taker, cfg.fee_maker,
        p.input.fee_per_contract, p.input.contract_size,
    );
    p.in_position = true;
    p.is_short = is_short;
    p.base_price = base_price;
    p.entry_price = base_price * (1.0 + if !is_short { p.entry_fee } else { -p.entry_fee });
    p.entry_row = i as i64;
    p.trade_leverage = lev;
    p.trade_stake = actual_stake;
    p.trade_amount = amount;
    p.trade_handle = callbacks.on_trade_opened(pair, bar_ns, is_short, amount, base_price, actual_stake, lev, p.entry_fee);
    p.trailing_activated = false;
    p.stop_is_trailing = false;

    let raw_stop = base_price * (1.0 + if !is_short { cfg.base_stoploss as f64 / lev } else { -(cfg.base_stoploss as f64) / lev });
    p.stop_loss_price = round_stop(raw_stop, p.input.precision[i], is_short);
    p.liq_price = if cfg.liquidation_enabled {
        liquidation_price_isolated(base_price, lev, p.input.exchange_max_leverage, is_short, cfg.liquidation_buffer)
    } else {
        0.0
    };
    p.stop_loss_dist = (cfg.base_stoploss as f64).abs() / lev;

    Some((actual_stake, lev))
}

/// One closed-trade candidate this bar, mirroring the Python `candidates`
/// list entries `(reason, tag, resting_price)` — `resting_price` is always
/// `None` here (no resting-order simulation on this path).
struct ExitCandidate {
    reason: &'static str,
    tag: Option<String>,
}

/// Hyperliquid's isolated-margin liquidation price plus the exchange buffer;
/// mirrors `liquidation_price_isolated` in rust_backtest.py. 0.0 = none.
pub fn liquidation_price_isolated(price: f64, leverage: f64, max_leverage: f64, is_short: bool, buffer: f64) -> f64 {
    if price <= 0.0 || leverage <= 1.0 || max_leverage <= 0.0 {
        return 0.0;
    }
    let ll = 1.0 / (2.0 * max_leverage);
    let side = if is_short { -1.0 } else { 1.0 };
    let dist = (1.0 / leverage - ll) / (1.0 - ll * side);
    if dist <= 0.0 {
        return 0.0;
    }
    let mut liq = price * (1.0 - side * dist);
    liq += side * (price - liq).abs() * buffer;
    liq.max(0.0)
}

/// Leveraged profit at this bar's open (freqtrade's `current_profit` at `rate`).
fn open_profit(cfg: &EngineConfig, p: &PairState, open_i: f64, funding_so_far: f64, is_long: bool) -> f64 {
    let (base_price, lev, amount, fee_open) = (p.base_price, p.trade_leverage, p.trade_amount, p.entry_fee);
    let fee_exit = futures_fee_override(p.input.fee_per_contract, p.input.contract_size, open_i)
        .unwrap_or(cfg.fee_taker);
    let (ov, cv) = if is_long {
        (amount * base_price * (1.0 + fee_open), amount * open_i * (1.0 - fee_exit) + funding_so_far)
    } else {
        (amount * base_price * (1.0 - fee_open), amount * open_i * (1.0 + fee_exit) - funding_so_far)
    };
    if ov == 0.0 { return 0.0; }
    if is_long { ((cv / ov) - 1.0) * lev } else { (1.0 - (cv / ov)) * lev }
}

/// `custom_exit()` at this bar's open (`current_rate = open_i`, profit at
/// the open incl. funding so far). `None` = no custom exit.
#[allow(clippy::too_many_arguments)]
fn eval_custom_exit(cfg: &EngineConfig, p: &PairState, callbacks: &mut dyn StrategyCallbacks, pair: &str,
                    bar_ns: i64, open_i: f64, funding_so_far: f64, is_long: bool) -> Option<String> {
    let (base_price, lev, amount, fee_open) = (p.base_price, p.trade_leverage, p.trade_amount, p.entry_fee);
    let fee_exit = futures_fee_override(p.input.fee_per_contract, p.input.contract_size, open_i)
        .unwrap_or(cfg.fee_taker);
    let (ov_ce, cv_ce) = if is_long {
        (amount * base_price * (1.0 + fee_open), amount * open_i * (1.0 - fee_exit) + funding_so_far)
    } else {
        (amount * base_price * (1.0 - fee_open), amount * open_i * (1.0 + fee_exit) - funding_so_far)
    };
    let leveraged_open_return = if ov_ce != 0.0 {
        if is_long { ((cv_ce / ov_ce) - 1.0) * lev } else { (1.0 - (cv_ce / ov_ce)) * lev }
    } else {
        0.0
    };
    callbacks.custom_exit(p.trade_handle, pair, bar_ns, open_i, leveraged_open_return)
}

/// `do_exit_check`: the per-bar exit-priority state machine. Returns
/// `Some(TradeOut)` if the position closed this bar.
#[allow(clippy::too_many_arguments)]
fn do_exit_check(cfg: &EngineConfig, p: &mut PairState, i: usize, callbacks: &mut dyn StrategyCallbacks) -> Option<TradeOut> {
    let is_long = !p.is_short;
    let time_in_position = i as i64 - p.entry_row;
    let (high_i, low_i, open_i) = (p.input.high[i], p.input.low[i], p.input.open[i]);
    let (base_price, entry_price, lev, amount) = (p.base_price, p.entry_price, p.trade_leverage, p.trade_amount);
    let fee_open = p.entry_fee;
    let bound = if is_long { high_i } else { low_i };
    let pair = p.input.pair.clone();
    let bar_ns = p.input.dates_ns[i];

    // freqtrade's `should_exit` calls `trade.adjust_min_max_rates(high, low)`
    // unconditionally, BEFORE any stoploss/ROI/exit-signal/custom_exit logic
    // — every bar a position is open. A no-op for `NativeDefaults`.
    //
    // custom_exit() is asked at this bar's OPEN and exits at that open, so
    // with this bar's high/low already in min/max it could see a dip coming
    // and leave ahead of it (AdaptiveMAEMFE's mae_cut did, on every bar that
    // later fell through its threshold). Default: min/max through the open
    // for custom_exit, this bar's range added right after it.
    if cfg.custom_exit_lookahead {
        callbacks.adjust_min_max_rates(p.trade_handle, high_i, low_i);
    } else {
        callbacks.adjust_min_max_rates(p.trade_handle, open_i, open_i);
    }

    // Fix #1: funding fees are baked into bound_profit BEFORE the
    // trailing/ROI trigger comparison below, not applied only at close.
    let funding_so_far = if cfg.is_futures {
        p.funding_fee(amount, p.entry_row, i)
    } else {
        0.0
    };

    let fee_bound = futures_fee_override(p.input.fee_per_contract, p.input.contract_size, bound)
        .unwrap_or(cfg.fee_taker as f64);
    let (open_value, close_value_bound, leveraged_bound_return) = if is_long {
        let ov = amount * base_price * (1.0 + fee_open);
        let cv = amount * bound * (1.0 - fee_bound) + funding_so_far;
        (ov, cv, if ov != 0.0 { ((cv / ov) - 1.0) * lev } else { 0.0 })
    } else {
        let ov = amount * base_price * (1.0 - fee_open);
        let cv = amount * bound * (1.0 + fee_bound) - funding_so_far;
        (ov, cv, if ov != 0.0 { (1.0 - (cv / ov)) * lev } else { 0.0 })
    };
    let _ = (open_value, close_value_bound);

    let raw_exit_signal = if is_long {
        p.input.exit_long[i] > 0 && !(p.input.enter_long[i] > 0)
    } else {
        p.input.exit_short[i] > 0 && !(p.input.enter_short[i] > 0)
    };
    let same_side_entry = if is_long { p.input.enter_long[i] > 0 } else { p.input.enter_short[i] > 0 };
    let use_exit_signal = cfg.use_exit_signal || cfg.exit_settings_ignored;
    // freqtrade's should_exit: exit signals and custom_exit exist only with
    // use_exit_signal; a raw signal blocked by exit_profit_only (profit at
    // the open not above the offset) also skips custom_exit.
    let profit_ok = !(cfg.exit_profit_only && !cfg.exit_settings_ignored)
        || open_profit(cfg, p, open_i, funding_so_far, is_long) > cfg.exit_profit_offset;
    let signal_exit_triggered = use_exit_signal && raw_exit_signal && profit_ok;
    let ask_custom_exit = use_exit_signal && !raw_exit_signal;
    let mut custom_exit_tag: Option<String> = None;
    if !cfg.custom_exit_lookahead {
        if ask_custom_exit {
            custom_exit_tag = eval_custom_exit(cfg, p, callbacks, &pair, bar_ns, open_i, funding_so_far, is_long);
        }
        callbacks.adjust_min_max_rates(p.trade_handle, high_i, low_i);
    }

    let (mut stop_loss_price, mut stop_is_trailing, mut stop_loss_dist) =
        (p.stop_loss_price, p.stop_is_trailing, p.stop_loss_dist);
    let mut trailing_activated = p.trailing_activated;
    let armed_at_open = p.trailing_activated;
    // freqtrade's pure native-trailing setup (the one its entry-candle worst case covers).
    let pess_trailing = !cfg.use_custom_stoploss && cfg.trailing_enabled && cfg.trailing_only_offset
        && cfg.trailing_offset_set && cfg.trailing_offset != 0.0;
    let dir_correct = if is_long { stop_loss_price < low_i } else { stop_loss_price > high_i };
    let precision_i = p.input.precision[i];

    if dir_correct {
        // custom_stoploss(): base default returns None (no ratchet) — see
        // module doc comment. Ratchets FIRST, before trailing, using the
        // exact same round-then-compare-tighter mechanism and also setting
        // `stop_is_trailing = true` (freqtrade's `adjust_stop_loss` marks
        // EVERY ratchet as trailing, including this one — a previously
        // mislabelled exit reason bug when this was hardcoded off).
        if let Some(sl_ratio) = callbacks.custom_stoploss(p.trade_handle, &pair, bar_ns, bound, leveraged_bound_return) {
            if sl_ratio.is_finite() {
                let dist = sl_ratio.abs() / lev;
                let candidate = if is_long { bound * (1.0 - dist) } else { bound * (1.0 + dist) };
                let candidate = round_stop(candidate, precision_i, !is_long);
                let tighter = if is_long { candidate > stop_loss_price } else { candidate < stop_loss_price };
                if tighter {
                    stop_loss_price = candidate;
                    stop_is_trailing = true;
                    stop_loss_dist = dist;
                }
            }
        }
    }
    if dir_correct && cfg.trailing_enabled {
        // Fix #2: round every ratchet candidate to tick size BEFORE the
        // "is it tighter" comparison, not after.
        if !(cfg.trailing_only_offset && leveraged_bound_return < cfg.trailing_trigger as f64) {
            let trail_ratio = if cfg.trailing_offset_set && leveraged_bound_return > cfg.trailing_trigger as f64 {
                trailing_activated = true;
                cfg.trailing_offset as f64
            } else {
                cfg.base_stoploss as f64
            };
            let trail_dist = trail_ratio.abs() / lev;
            let candidate = if is_long { bound * (1.0 - trail_dist) } else { bound * (1.0 + trail_dist) };
            let candidate = round_stop(candidate, precision_i, !is_long);
            let tighter = if is_long { candidate > stop_loss_price } else { candidate < stop_loss_price };
            if tighter {
                stop_loss_price = candidate;
                stop_is_trailing = true;
                stop_loss_dist = trail_dist;
            }
        }
    }

    let stop_triggered = if is_long { low_i <= stop_loss_price } else { high_i >= stop_loss_price };
    let skip_roi = cfg.ignore_roi_if_entry_signal && !cfg.exit_settings_ignored && same_side_entry;
    let roi_triggered = cfg.roi_enabled && !skip_roi && ((0..cfg.roi_vals.len()).any(|k| {
        time_in_position >= cfg.roi_periods[k] && leveraged_bound_return >= cfg.roi_vals[k] as f64
    }));
    // No guard on max_hold_period == 0: the Python config's "disabled"
    // sentinel is 10**9 (`_build_engine_config`), not 0 — 0 is a legitimate
    // configured value that should trigger immediately, same as Python.
    let max_hold_triggered = time_in_position >= cfg.max_hold_period;

    // custom_exit(): base default returns None — see module doc comment.
    // Only evaluated when there's no raw signal exit this bar (freqtrade's
    // `should_exit`), and — unlike the stoploss/ROI/trailing checks above,
    // which all use `bound` (this bar's high/low) — priced off the bar's
    // OPEN, matching `_get_exit_for_signal`'s own `current_rate=open_i`.
    if cfg.custom_exit_lookahead && ask_custom_exit {
        custom_exit_tag = eval_custom_exit(cfg, p, callbacks, &pair, bar_ns, open_i, funding_so_far, is_long);
    }
    let custom_exit_triggered = custom_exit_tag.is_some();

    let mut candidates: Vec<ExitCandidate> = Vec::with_capacity(4);
    if signal_exit_triggered {
        candidates.push(ExitCandidate { reason: "Signal", tag: None });
    } else if custom_exit_triggered {
        candidates.push(ExitCandidate { reason: "CustomExit", tag: custom_exit_tag });
    }
    // freqtrade's `trader_stoploss_reached`: liquidation wins only when the
    // liquidation price sits inside the stop.
    let liq = p.liq_price;
    let liq_triggered = liq > 0.0 && if is_long {
        liq >= stop_loss_price && low_i <= liq
    } else {
        liq <= stop_loss_price && high_i >= liq
    };
    if liq_triggered {
        candidates.push(ExitCandidate { reason: "Liquidation", tag: None });
    } else if stop_triggered && !stop_is_trailing {
        candidates.push(ExitCandidate { reason: "Stoploss", tag: None });
    }
    // A candle has no order for its high and low: ROI that needs the price
    // past the trail's arming level (or a trail already armed at the open)
    // can't come first in the worst case, since the trail fires at the first
    // pullback.
    let mut roi_in_force = cfg.roi_vals[0];
    for k in 0..cfg.roi_vals.len() {
        if time_in_position >= cfg.roi_periods[k] { roi_in_force = cfg.roi_vals[k]; }
    }
    let roi_preempted = !cfg.intrabar_optimistic && pess_trailing && stop_triggered && stop_is_trailing
        && (armed_at_open || roi_in_force > cfg.trailing_trigger);
    if roi_triggered && !roi_preempted { candidates.push(ExitCandidate { reason: "RoiTarget", tag: None }); }
    if stop_triggered && stop_is_trailing && !liq_triggered { candidates.push(ExitCandidate { reason: "TrailingStop", tag: None }); }
    if max_hold_triggered { candidates.push(ExitCandidate { reason: "MaxHoldPeriod", tag: None }); }

    p.stop_loss_price = stop_loss_price;
    p.stop_is_trailing = stop_is_trailing;
    p.trailing_activated = trailing_activated;
    p.stop_loss_dist = stop_loss_dist;

    if candidates.is_empty() {
        return None;
    }

    for c in &candidates {
        let exit_reason = c.reason;

        let arms_here = pess_trailing && !cfg.intrabar_optimistic && !armed_at_open;
        let base_exit_price = if exit_reason == "TrailingStop" && (time_in_position == 0 || arms_here) {
            // Fix #4: entry-candle worst case has TWO formulas, matching
            // `_get_close_rate_for_stoploss`'s exact branch condition. The
            // offset-anchored formula ONLY applies when custom_stoploss is
            // NOT in play at all (`use_custom_stoploss=False`) AND the
            // trailing config is specifically "only after offset, with an
            // explicit offset set" — every other case (including any
            // custom_stoploss-active strategy, unconditionally) uses the
            // open-anchored formula.
            let pess = if !cfg.use_custom_stoploss && cfg.trailing_enabled && cfg.trailing_only_offset
                && cfg.trailing_offset_set && cfg.trailing_offset != 0.0 {
                let trail_dist = cfg.trailing_offset.abs() / lev;
                // The arming offset is a leveraged profit ratio: its price
                // move is offset / leverage (freqtrade divides only the trail
                // distance; at 5x a 1.5% offset was booked as a +1.5% move).
                let arm = if cfg.trailing_offset_unlevered {
                    cfg.trailing_trigger.abs()
                } else {
                    cfg.trailing_trigger.abs() / lev
                };
                // Entry candle: from its open. A later candle that arms the
                // trail: from the entry price, where the arming level sits.
                if time_in_position == 0 {
                    if is_long { open_i * (1.0 + arm - trail_dist) } else { open_i * (1.0 - arm + trail_dist) }
                } else if is_long {
                    // an open gapped past the arming level arms right there
                    (base_price * (1.0 + arm)).max(open_i) * (1.0 - trail_dist)
                } else {
                    (base_price * (1.0 - arm)).min(open_i) * (1.0 + trail_dist)
                }
            } else if is_long {
                open_i * (1.0 - stop_loss_dist.abs())
            } else {
                open_i * (1.0 + stop_loss_dist.abs())
            };
            let fills = if is_long { pess <= high_i } else { pess >= low_i };
            if !fills { continue; }
            if is_long { pess.max(low_i) } else { pess.min(high_i) }
        } else if exit_reason == "Liquidation" {
            if is_long {
                if liq > high_i { open_i } else { liq }
            } else if liq < low_i { open_i } else { liq }
        } else if exit_reason == "TrailingStop" || exit_reason == "Stoploss" {
            if is_long {
                if stop_loss_price > high_i { open_i } else { stop_loss_price }
            } else if stop_loss_price < low_i { open_i } else { stop_loss_price }
        } else if exit_reason == "RoiTarget" {
            // the latest tier whose period has started and whose target this bar reached
            let mut tier = (cfg.roi_vals[0] / lev, cfg.roi_periods[0]);
            for k in (1..cfg.roi_vals.len()).rev() {
                if time_in_position >= cfg.roi_periods[k] && leveraged_bound_return >= cfg.roi_vals[k] {
                    tier = (cfg.roi_vals[k] / lev, cfg.roi_periods[k]);
                    break;
                }
            }
            let (roi_pct, roi_entry_period) = tier;
            let side_1 = if is_long { 1.0 } else { -1.0 };
            let roi_rate = base_price * roi_pct;
            let open_fee_rate = side_1 * base_price * (1.0 + side_1 * fee_open);
            // Solving exactly for a futures pair would need the fee AT the
            // still-unknown solved price; `open_i` (this bar's own open) is a
            // close-enough proxy - the real fee charged at whatever price the
            // fill actually clamps to below still goes through `fill_fee`.
            let fee_roi_solve = futures_fee_override(p.input.fee_per_contract, p.input.contract_size, open_i)
                .unwrap_or(cfg.fee_taker as f64);
            let mut raw_close = -(roi_rate + open_fee_rate) / (fee_roi_solve - side_1);
            // Fix #3: if this is exactly the bar a new tighter ROI tier took
            // effect AND the open already gapped through the solved close
            // rate, use the open instead.
            if time_in_position > 0 && time_in_position == roi_entry_period {
                let is_new_roi = if is_long { open_i > raw_close } else { open_i < raw_close };
                if is_new_roi { raw_close = open_i; }
            }
            let (lo, hi) = (low_i.min(high_i), low_i.max(high_i));
            raw_close.max(lo).min(hi)
        } else {
            // MaxHoldPeriod / Signal / CustomExit -> candle's own open.
            open_i
        };

        // custom_exit_price(): a no-op on this path (proposed rate unchanged)
        // — strategies overriding it are excluded by the Python-side
        // eligibility gate (see module doc comment); no resting-order branch
        // applies here either. confirm_trade_exit() IS bridged: a `false`
        // means "try the next candidate this same bar", matching
        // `do_exit_check`'s own `for ... in candidates: if not confirm_exit
        // (...): continue` exactly.
        let reason_tag = exit_reason_tag_str(exit_reason, &c.tag);
        if exit_reason != "Liquidation"
            && !callbacks.confirm_trade_exit(p.trade_handle, &pair, bar_ns, amount, base_exit_price, &reason_tag) {
            continue;
        }
        let fee_close = fill_fee(
            base_exit_price, open_i, !is_long, cfg.fee_taker, cfg.fee_maker,
            p.input.fee_per_contract, p.input.contract_size,
        );
        return Some(close_position(
            p, i, base_exit_price, exit_reason, Some(reason_tag), bar_ns,
            funding_so_far, fee_close, entry_price, base_price, callbacks,
        ));
    }
    None
}

/// `close_position`: book the close at an already-resolved fill price.
#[allow(clippy::too_many_arguments)]
fn close_position(
    p: &mut PairState, i: usize, base_exit_price: f64, exit_reason: &'static str,
    exit_reason_tag: Option<String>, bar_ns: i64, funding_fees: f64, fee_close: f64,
    entry_price: f64, base_price: f64, callbacks: &mut dyn StrategyCallbacks,
) -> TradeOut {
    let is_long = !p.is_short;
    let (lev, amount) = (p.trade_leverage, p.trade_amount);
    let fee_open = p.entry_fee;
    let exit_price = if is_long { base_exit_price * (1.0 - fee_close) } else { base_exit_price * (1.0 + fee_close) };
    let stake = p.trade_stake;

    let (pnl_amount, leveraged_profit) = if is_long {
        let open_value = amount * base_price * (1.0 + fee_open);
        let close_value = amount * base_exit_price * (1.0 - fee_close) + funding_fees;
        (close_value - open_value, if open_value != 0.0 { ((close_value / open_value) - 1.0) * lev } else { 0.0 })
    } else {
        let open_value = amount * base_price * (1.0 - fee_open);
        let close_value = amount * base_exit_price * (1.0 + fee_close) - funding_fees;
        (open_value - close_value, if open_value != 0.0 { (1.0 - (close_value / open_value)) * lev } else { 0.0 })
    };

    let out = TradeOut {
        pair: p.input.pair.clone(),
        is_short: p.is_short,
        entry_row: p.entry_row,
        exit_row: i as i64,
        entry_price,
        base_exit_price,
        exit_price,
        base_entry_price: base_price,
        fee_open,
        fee_close,
        profit_ratio: leveraged_profit,
        profit_abs: pnl_amount,
        leverage: lev,
        exit_reason,
        exit_reason_tag,
        stake,
        funding_fees,
    };

    callbacks.on_trade_closed(p.trade_handle, bar_ns, leveraged_profit, pnl_amount);
    p.in_position = false;
    out
}

/// Top-level entry point: runs the full chronological, cross-pair,
/// shared-capital simulation across every pair in `inputs`, in the exact
/// candle order `_joint_multi_pair_backtest` uses (open pairs first, in
/// entry order, so a slot freed this candle is available to a later pair the
/// SAME candle — matching `ordered = list(dict.fromkeys(open_order +
/// pair_whitelist))`).
pub fn multi_pair_backtest(inputs: &[PairInput], cfg: &EngineConfig, callbacks: &mut dyn StrategyCallbacks) -> EngineOutput {
    let mut states: Vec<PairState> = inputs.iter().map(PairState::new).collect();
    let n_pairs = states.len();
    if n_pairs == 0 {
        return EngineOutput { trades: Vec::new(), rejected: 0 };
    }

    // Union of all distinct timestamps across every pair, sorted ascending —
    // mirrors `all_dates = np.unique(np.concatenate(...)); all_dates.sort()`.
    let mut all_dates: Vec<i64> = Vec::new();
    for s in &states { all_dates.extend_from_slice(&s.input.dates_ns); }
    all_dates.sort_unstable();
    all_dates.dedup();
    let last_ts = all_dates.last().copied();

    let mut trades: Vec<TradeOut> = Vec::new();
    let mut rejected: i64 = 0;
    let mut open_count: i64 = 0;
    let mut running_balance = cfg.starting_balance;
    let mut tied_up_capital = 0.0f64;
    // Pairs currently open, in entry order (drives the open-pairs-first
    // candle ordering, exactly like the Python `open_order` list).
    let mut open_order: Vec<usize> = Vec::new();
    let fixed_stake = (cfg.starting_balance * cfg.tradable_ratio).min(cfg.max_trade_amount);

    for &ts in &all_dates {
        let can_enter_bar = Some(ts) != last_ts;

        // Open pairs first (in entry order), then flat pairs in whitelist
        // order, deduped — mirrors `dict.fromkeys(open_order + whitelist)`.
        let mut ordered: Vec<usize> = Vec::with_capacity(n_pairs);
        let mut seen = vec![false; n_pairs];
        for &pi in &open_order { if !seen[pi] { seen[pi] = true; ordered.push(pi); } }
        for pi in 0..n_pairs { if !seen[pi] { seen[pi] = true; ordered.push(pi); } }

        for pi in ordered {
            let idx = states[pi].row_idx;
            if idx >= states[pi].n || states[pi].input.dates_ns[idx] != ts {
                continue;
            }
            states[pi].row_idx = idx + 1;
            callbacks.set_bar_cursor(&states[pi].input.pair, idx as i64);
            let direction = entry_dir(states[pi].input, idx, cfg.can_short);

            // Two-pass reversal-retry loop, mirroring `for _ in range(2):
            // prior_dir = process_pass(...); if not can_short or prior_dir is
            // None or prior_dir == direction: break`.
            for _pass in 0..2 {
                let prior_dir = if states[pi].in_position { Some(states[pi].is_short) } else { None };

                // Entry FIRST, then exit-check — matches Python's
                // `process_pass` exactly (not the other way around): a
                // same-candle entry can immediately trip an exit condition
                // (e.g. Fix #4's entry-candle trailing stop, or an
                // entry-candle ROI hit), and that only gets caught if the
                // exit-check runs on this same pass, after the entry.
                if can_enter_bar && !states[pi].in_position && direction.is_some() {
                    let want_short = direction.unwrap();
                    let slot_free = cfg.max_open_trades <= 0 || open_count < cfg.max_open_trades;
                    if slot_free {
                        let stake = if cfg.compounding {
                            // freqtrade's unlimited stake: balance*ratio split
                            // across the slots, limited to what is still free.
                            // `max_trade_amount` is only the first trade's
                            // stake, not a cap (same as Python `current_stake`).
                            let total_stake_amount = running_balance * cfg.tradable_ratio;
                            let slots = if cfg.max_open_trades > 0 { cfg.max_open_trades as f64 } else { 1.0 };
                            (total_stake_amount / slots).min(total_stake_amount - tied_up_capital)
                        } else {
                            fixed_stake
                        };
                        let capital_ok = (running_balance * cfg.tradable_ratio - tied_up_capital) >= stake;
                        if stake > 0.0 && capital_ok {
                            if let Some((actual_stake, _lev)) = do_enter(
                                cfg, &mut states[pi], idx, want_short, stake,
                                running_balance, tied_up_capital, callbacks,
                            ) {
                                open_count += 1;
                                tied_up_capital += actual_stake;
                                open_order.push(pi);
                                callbacks.on_balance_changed(running_balance, tied_up_capital);
                            } else {
                                rejected += 1;
                            }
                        } else {
                            rejected += 1;
                        }
                    } else {
                        rejected += 1;
                    }
                }

                if states[pi].in_position {
                    if let Some(closed) = do_exit_check(cfg, &mut states[pi], idx, callbacks) {
                        open_count -= 1;
                        tied_up_capital -= closed.stake;
                        running_balance += closed.profit_abs;
                        open_order.retain(|&x| x != pi);
                        trades.push(closed);
                        callbacks.on_balance_changed(running_balance, tied_up_capital);
                    }
                }

                // freqtrade only runs the second pass after this pass CLOSED
                // the position (a same-candle reversal). Re-running the exit
                // check on a still-open position let custom_exit see this
                // bar's high/low; the legacy order keeps the old double pass.
                let still_open = states[pi].in_position && !cfg.custom_exit_lookahead;
                if !cfg.can_short || prior_dir.is_none() || still_open || prior_dir == direction {
                    break;
                }
            }
        }
    }

    // Force-close anything still open at the end (`handle_left_open`).
    for pi in 0..n_pairs {
        let s = &mut states[pi];
        if !s.in_position { continue; }
        let idx = s.n - 1;
        let is_long = !s.is_short;
        let open_i = s.input.open[idx] as f64;
        let fee_force_exit = futures_fee_override(s.input.fee_per_contract, s.input.contract_size, open_i)
            .unwrap_or(cfg.fee_taker as f64);
        let exit_price = if is_long { open_i * (1.0 - fee_force_exit) } else { open_i * (1.0 + fee_force_exit) };
        let (lev, amount, stake) = (s.trade_leverage, s.trade_amount, s.trade_stake);

        let funding_fees = if cfg.is_futures { s.funding_fee(amount, s.entry_row, idx) } else { 0.0 };
        let (pnl_amount, leveraged_profit) = if is_long {
            let open_value = amount * s.base_price * (1.0 + s.entry_fee);
            let close_value = amount * open_i * (1.0 - fee_force_exit) + funding_fees;
            (close_value - open_value, if open_value != 0.0 { ((close_value / open_value) - 1.0) * lev } else { 0.0 })
        } else {
            let open_value = amount * s.base_price * (1.0 - s.entry_fee);
            let close_value = amount * open_i * (1.0 + fee_force_exit) - funding_fees;
            (open_value - close_value, if open_value != 0.0 { (1.0 - (close_value / open_value)) * lev } else { 0.0 })
        };
        running_balance += pnl_amount;
        tied_up_capital -= stake;
        trades.push(TradeOut {
            pair: s.input.pair.clone(),
            is_short: s.is_short,
            entry_row: s.entry_row,
            exit_row: idx as i64,
            entry_price: s.entry_price,
            base_exit_price: open_i,
            exit_price,
            base_entry_price: s.base_price,
            fee_open: s.entry_fee,
            fee_close: fee_force_exit,
            profit_ratio: leveraged_profit,
            profit_abs: pnl_amount,
            leverage: lev,
            exit_reason: "ForceExit",
            exit_reason_tag: None,
            stake,
            funding_fees,
        });
        callbacks.on_trade_closed(s.trade_handle, s.input.dates_ns[idx], leveraged_profit, pnl_amount);
        s.in_position = false;
    }

    EngineOutput { trades, rejected }
}

#[cfg(test)]
#[path = "multi_pair_tests.rs"]
mod tests;
