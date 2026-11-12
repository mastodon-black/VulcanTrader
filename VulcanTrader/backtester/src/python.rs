// =============================================================================
// vulcan_rust_indicators — PyO3 bridge into this crate's own indicators/engine
// =============================================================================
//
// Compiled only under the `python` / `extension-module` features (what maturin
// builds with). Exposes `simd_indicators::calculate_standard_indicators` and the
// `cpu_engine` simulator to Python.
//
// `calculate_standard_indicators`/`sma`/`ema`/`rsi`/`atr`/`adx`/`bollinger_bands`
// are sourced from `simd_indicators` (an AVX2-oriented port, adapted to this
// crate's exact 23-indicator index table), not `fast_indicators` — switched
// deliberately, not a stale reference: comparing the two against an
// independent f64 reference found `fast_indicators::calculate_bollinger_
// bands_rust`'s `E[x²]-E[x]²` variance formula suffers real catastrophic
// cancellation on BTC-scale prices (up to ~370 units of absolute error on a
// 60k-bar series — verified via `examples/compare_indicators.rs`), which
// `simd_indicators::bollinger`'s two-pass `Σ(x-mean)²` formula does not. Its
// own SMA had a MATCHING bug the other direction (an f32 prefix-sum growing
// into the billions over a long series, then subtracted back down — fixed
// here to accumulate in f64, see `simd_indicators.rs`'s `sma_scalar`/
// `sma_avx2` doc comment). RSI/ATR (bounded Wilder recurrences) were already
// bit-identical between the two. This is a real behavior change, not a
// no-op: verified on `AllIndicatorsDemoStrategy` (the strategy that consumes
// all 23 indices), 834→857 trades, 429 of 714 matched trades changed outcome
// — expected, since its signals depend on the now-corrected Bollinger values.
//
// PERFORMANCE — this bridge is marshalling-bound, not compute-bound.
// Measured on a 20,000-bar pair before the current design:
//     calculate_standard_indicators   24.4 ms   (built 23 x 20k Python floats)
//     list -> numpy on the caller      4.6 ms
//     run_backtest (one direction)    18.2 ms   (12 arrays extracted per-element)
// The engine work inside those calls is a small fraction of it. So, per
// performance.txt's "borrow the signal arrays, never clone them":
//
//   * inputs are borrowed zero-copy as `PyReadonlyArray1` instead of being
//     extracted element-by-element into `Vec<f64>`;
//   * outputs are returned as numpy arrays instead of Python lists, which
//     removes ~460k PyFloat allocations per call AND the caller's np.array()
//     round-trip.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use numpy::{IntoPyArray, PyReadonlyArray1};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::simd_indicators::calculate_standard_indicators as calc_std;
use crate::simd_indicators::{
    calculate_adx_rust, calculate_atr_rust, calculate_bollinger_bands_rust, calculate_ema_rust,
    calculate_rsi_rust, calculate_sma_rust,
};
use crate::fast_indicators::{
    rolling_zscore as rolling_zscore_rust, significant_decimals as significant_decimals_rust,
};
use crate::multi_pair::{self, EngineConfig, PairInput};

/// Borrow a numpy f64 array and down-cast to the f32 the engine works in.
/// One tight pass over a contiguous slice, versus PyO3's per-element extraction.
fn f32_from(a: &PyReadonlyArray1<'_, f64>) -> Vec<f32> {
    match a.as_slice() {
        Ok(s) => s.iter().map(|&x| x as f32).collect(),
        // Non-contiguous input (rare): walk the strided view instead.
        Err(_) => a.as_array().iter().map(|&x| x as f32).collect(),
    }
}

fn u8_from(a: &PyReadonlyArray1<'_, u8>) -> Vec<u8> {
    match a.as_slice() {
        Ok(s) => s.to_vec(),
        Err(_) => a.as_array().iter().copied().collect(),
    }
}

fn f64_vec_from(a: &PyReadonlyArray1<'_, f64>) -> Vec<f64> {
    match a.as_slice() {
        Ok(s) => s.to_vec(),
        Err(_) => a.as_array().iter().copied().collect(),
    }
}

fn i64_from(a: &PyReadonlyArray1<'_, i64>) -> Vec<i64> {
    match a.as_slice() {
        Ok(s) => s.to_vec(),
        Err(_) => a.as_array().iter().copied().collect(),
    }
}

/// Compute the 23 standard indicators for the given OHLCV arrays.
///
/// Inputs are float64 numpy arrays (borrowed, not copied). Returns
/// `{index: numpy.ndarray(float32)}` matching `fast_indicators::
/// calculate_standard_indicators`' fixed-index map (see the table in
/// AllIndicatorsDemoStrategy). Warmup positions are NaN, exactly as the engine
/// produces them.
#[pyfunction]
fn calculate_standard_indicators<'py>(
    py: Python<'py>,
    close: PyReadonlyArray1<'py, f64>,
    high: PyReadonlyArray1<'py, f64>,
    low: PyReadonlyArray1<'py, f64>,
    volume: PyReadonlyArray1<'py, f64>,
) -> PyResult<Bound<'py, PyDict>> {
    let c = Arc::new(f32_from(&close));
    let h = Arc::new(f32_from(&high));
    let l = Arc::new(f32_from(&low));
    let v = Arc::new(f32_from(&volume));

    // Release the GIL for the (rayon-parallel) number crunching.
    let map = py.allow_threads(|| calc_std(&c, &h, &l, &v));

    let out = PyDict::new_bound(py);
    for (idx, vals) in map {
        out.set_item(idx, vals.into_pyarray_bound(py))?;
    }
    Ok(out)
}

/// Run the Rust backtest engine on precomputed signals for ONE direction.
///
/// Python computes indicators + entry/exit signals (its normal `populate_*`),
/// then hands the raw arrays here. `config_json` is a JSON object whose keys
/// match `UnifiedBacktestConfig`'s fields (missing keys use the Rust defaults).
/// `direction` is "long" or "short".
///
/// Limit orders: set `entry_order_type` / `exit_order_type` ("limit" or
/// "market"), `entry_limit_offset` / `exit_limit_offset` (fraction of price
/// the resting order sits on the favourable side of the bar open), and
/// `entry_fill_timeout_bars` / `exit_fill_timeout_bars` (bars an unfilled
/// order survives past its placement bar). A zero offset means every order is
/// immediately marketable, i.e. the historical instant-fill behaviour.
///
/// Returns a JSON string with the per-trade arrays the caller turns into trade
/// records: entry/exit bar indices, prices, profit ratio + $ pnl, durations
/// (bars), leverages, and exit-reason labels.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn run_backtest(
    py: Python<'_>,
    open: PyReadonlyArray1<'_, f64>,
    high: PyReadonlyArray1<'_, f64>,
    low: PyReadonlyArray1<'_, f64>,
    close: PyReadonlyArray1<'_, f64>,
    volume: PyReadonlyArray1<'_, f64>,
    rsi: PyReadonlyArray1<'_, f64>,
    macd_hist: PyReadonlyArray1<'_, f64>,
    bb_pos: PyReadonlyArray1<'_, f64>,
    atr: PyReadonlyArray1<'_, f64>,
    cci: PyReadonlyArray1<'_, f64>,
    entry_signals: PyReadonlyArray1<'_, u8>,
    exit_signals: PyReadonlyArray1<'_, u8>,
    direction: String,
    config_json: String,
) -> PyResult<String> {
    let o = f32_from(&open);
    let h = f32_from(&high);
    let l = f32_from(&low);
    let c = f32_from(&close);
    let vol = f32_from(&volume);
    let rsi = f32_from(&rsi);
    let mh = f32_from(&macd_hist);
    let bb = f32_from(&bb_pos);
    let atrv = f32_from(&atr);
    let cciv = f32_from(&cci);
    let ent = u8_from(&entry_signals);
    let ext = u8_from(&exit_signals);

    let mut cfg: crate::backtest::UnifiedBacktestConfig = serde_json::from_str(&config_json)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("bad config_json: {e}")))?;
    cfg.direction = direction;

    let res = py.allow_threads(|| {
        crate::cpu_engine::unified_backtest(
            &c, &o, &h, &l, &vol,
            &rsi, &mh, &bb, &atrv, &cciv,
            &ent, &ext,
            &cfg,
        )
    });

    let br = &res.backtest_result;
    let exit_reasons: Vec<String> = br.exit_reasons.iter().map(|r| format!("{r:?}")).collect();
    let out = serde_json::json!({
        "entry_indices": br.entry_indices,
        "exit_indices": br.exit_indices,
        "entry_prices": br.entry_prices,
        "exit_prices": br.exit_prices,
        "profits": br.profits,
        "pnl_amounts": br.pnl_amounts,
        "durations": br.durations,
        "leverages": br.leverages,
        "exit_reasons": exit_reasons,
        "final_equity": res.final_equity,
        "starting_balance": res.starting_balance,
        // Limit-order simulation counters — all zero unless the config asked
        // for resting orders (`entry_limit_offset` / `exit_limit_offset`).
        "entry_orders_placed": res.entry_orders_placed,
        "timedout_entry_orders": res.entry_orders_timedout,
        "exit_orders_placed": res.exit_orders_placed,
        "timedout_exit_orders": res.exit_orders_timedout,
    });
    Ok(out.to_string())
}

// =============================================================================
// Backtest-stats metrics (f64)
// =============================================================================
//
// `optimize_reports._generate_result_line` runs six per-group stats helpers
// (sharpe/sortino/calmar/sqn/expectancy/max_drawdown) once per pair AND once
// per exit-tag — 86 calls on a 45-pair run, measured at ~1.75s total.
//
// These are deliberately implemented in f64 rather than reusing
// `crate::metrics` (which is f32, sized for the engine's hot loop). Stats are
// user-facing numbers; matching pandas/numpy bit-for-bit matters more here than
// the few ns f32 would save. Formula notes, all mirroring VulcanTrader/data/metrics.py:
//   * sharpe/sortino use numpy's np.std -> POPULATION std (ddof=0)
//   * sqn uses pandas' .std()          -> SAMPLE std (ddof=1)
//   * a zero/NaN denominator yields -100.0 (the Python "deliberately bad score")
//   * max_drawdown prepends a zero row, so no-drawdown cases yield 0.0

fn mean_f64(xs: &[f64]) -> f64 {
    if xs.is_empty() { return f64::NAN; }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// numpy `np.std` (ddof=0). NaN on empty, matching numpy's behaviour.
fn pop_std(xs: &[f64]) -> f64 {
    if xs.is_empty() { return f64::NAN; }
    let m = mean_f64(xs);
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / xs.len() as f64).sqrt()
}

/// pandas `.std()` (ddof=1). NaN with fewer than 2 samples.
fn sample_std(xs: &[f64]) -> f64 {
    if xs.len() < 2 { return f64::NAN; }
    let m = mean_f64(xs);
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
}

/// Mirror of `_calculate_annualized_ratio`.
fn annualized(mean: f64, denom: f64, factor: f64) -> f64 {
    if denom != 0.0 && !denom.is_nan() { (mean / denom) * factor.sqrt() } else { -100.0 }
}

/// Compute every metric `_generate_result_line` needs, in one pass.
///
/// `profit_abs` must already be ordered by close_date (the order
/// `calculate_max_drawdown` sorts into). Returns a dict of scalars.
#[pyfunction]
fn compute_result_metrics<'py>(
    py: Python<'py>,
    profit_abs: PyReadonlyArray1<'py, f64>,
    profit_ratio: PyReadonlyArray1<'py, f64>,
    trade_duration: PyReadonlyArray1<'py, f64>,
    days_period: f64,
    starting_balance: f64,
) -> PyResult<Bound<'py, PyDict>> {
    let pa: Vec<f64> = match profit_abs.as_slice() {
        Ok(s) => s.to_vec(),
        Err(_) => profit_abs.as_array().iter().copied().collect(),
    };
    let pr: Vec<f64> = match profit_ratio.as_slice() {
        Ok(s) => s.to_vec(),
        Err(_) => profit_ratio.as_array().iter().copied().collect(),
    };
    let td: Vec<f64> = match trade_duration.as_slice() {
        Ok(s) => s.to_vec(),
        Err(_) => trade_duration.as_array().iter().copied().collect(),
    };

    let out = PyDict::new_bound(py);
    let n = pa.len();
    if n == 0 || starting_balance <= 0.0 {
        return Ok(out); // caller keeps its own empty-result handling
    }

    let (mut wins, mut draws, mut losses) = (0usize, 0usize, 0usize);
    let (mut win_sum, mut loss_sum) = (0.0f64, 0.0f64);
    let mut total_abs = 0.0f64;
    for &p in &pa {
        total_abs += p;
        if p > 0.0 { wins += 1; win_sum += p; }
        else if p < 0.0 { losses += 1; loss_sum += -p; }
        else { draws += 1; }
    }

    // ---- expectancy (mirrors calculate_expectancy) ----
    let avg_win = if wins > 0 { win_sum / wins as f64 } else { 0.0 };
    let avg_loss = if losses > 0 { loss_sum / losses as f64 } else { 0.0 };
    let winrate = wins as f64 / n as f64;
    let loserate = losses as f64 / n as f64;
    let expectancy = winrate * avg_win - loserate * avg_loss;
    let expectancy_ratio = if avg_loss > 0.0 {
        (1.0 + avg_win / avg_loss) * winrate - 1.0
    } else { 100.0 };

    // ---- ratios: all off profit_abs / starting_balance ----
    let ratios: Vec<f64> = pa.iter().map(|p| p / starting_balance).collect();
    let expected_returns_mean = ratios.iter().sum::<f64>() / days_period;
    let sharpe = annualized(expected_returns_mean, pop_std(&ratios), 365.0);
    let downside: Vec<f64> = pa.iter().filter(|&&p| p < 0.0)
        .map(|p| p / starting_balance).collect();
    let sortino = annualized(expected_returns_mean, pop_std(&downside), 365.0);

    // ---- sqn (pandas .std -> ddof=1), rounded to 4dp like Python ----
    let s_std = sample_std(&ratios);
    let sqn_raw = if s_std != 0.0 && !s_std.is_nan() {
        (n as f64).sqrt() * (mean_f64(&ratios) / s_std)
    } else { -100.0 };
    let sqn = (sqn_raw * 10_000.0).round() / 10_000.0;

    // ---- max drawdown (cumsum with a prepended zero row) ----
    let (mut cum, mut high, mut dd_abs, mut dd_rel) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut min_dd = 0.0f64; // the zero row's drawdown
    for &p in &pa {
        cum += p;
        if cum > high { high = cum; }
        let hv = high.max(0.0);
        let drawdown = cum - hv;
        if drawdown < min_dd {
            min_dd = drawdown;
            dd_abs = drawdown.abs();
            let max_balance = starting_balance + hv;
            dd_rel = if max_balance != 0.0 {
                (max_balance - (starting_balance + cum)) / max_balance
            } else { 0.0 };
        }
    }

    // ---- calmar (reuses the drawdown above, like the Python caller now does) ----
    let calmar = annualized((total_abs / starting_balance) / days_period * 100.0, dd_rel, 365.0);

    let profit_factor = if loss_sum != 0.0 { win_sum / loss_sum } else { 0.0 };

    out.set_item("profit_total_abs", total_abs)?;
    out.set_item("profit_mean", mean_f64(&pr))?;
    out.set_item("duration_avg_min", mean_f64(&td))?;
    out.set_item("wins", wins)?;
    out.set_item("draws", draws)?;
    out.set_item("losses", losses)?;
    out.set_item("winrate", winrate)?;
    out.set_item("expectancy", expectancy)?;
    out.set_item("expectancy_ratio", expectancy_ratio)?;
    out.set_item("sortino", sortino)?;
    out.set_item("sharpe", sharpe)?;
    out.set_item("calmar", calmar)?;
    out.set_item("sqn", sqn)?;
    out.set_item("profit_factor", profit_factor)?;
    out.set_item("max_drawdown_account", dd_rel)?;
    out.set_item("max_drawdown_abs", dd_abs)?;
    Ok(out)
}

/// Fill gaps in an OHLCV series onto a regular `step_ns` grid.
///
/// Port of `converter.ohlcv_fill_up_missing_data`, which does this with
/// `resample().agg()` + `ffill` + a `fillna(dict)` — measured at ~1.4s across
/// 45 pairs, the dominant cost of data loading. Here it is a single O(n) pass.
///
/// Semantics mirrored exactly:
///   * bins with rows aggregate open=first, high=max, low=min, close=last,
///     volume=sum (so duplicate timestamps inside one bin collapse the same way);
///   * empty bins take the previous close for open/high/low/close and volume 0;
///   * the grid spans first..=last input timestamp inclusive.
///
/// Returns (dates_ns, open, high, low, close, volume) as numpy arrays; the
/// caller reassembles the DataFrame.
#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn fill_missing_candles<'py>(
    py: Python<'py>,
    dates_ns: PyReadonlyArray1<'py, i64>,
    open: PyReadonlyArray1<'py, f64>,
    high: PyReadonlyArray1<'py, f64>,
    low: PyReadonlyArray1<'py, f64>,
    close: PyReadonlyArray1<'py, f64>,
    volume: PyReadonlyArray1<'py, f64>,
    step_ns: i64,
) -> PyResult<(
    Bound<'py, numpy::PyArray1<i64>>,
    Bound<'py, numpy::PyArray1<f64>>,
    Bound<'py, numpy::PyArray1<f64>>,
    Bound<'py, numpy::PyArray1<f64>>,
    Bound<'py, numpy::PyArray1<f64>>,
    Bound<'py, numpy::PyArray1<f64>>,
)> {
    let d = dates_ns.as_slice().map_err(|_| {
        pyo3::exceptions::PyValueError::new_err("dates_ns must be contiguous")
    })?;
    let o = open.as_slice().unwrap_or(&[]);
    let h = high.as_slice().unwrap_or(&[]);
    let l = low.as_slice().unwrap_or(&[]);
    let c = close.as_slice().unwrap_or(&[]);
    let v = volume.as_slice().unwrap_or(&[]);
    let n = d.len();

    if n == 0 || step_ns <= 0 {
        let e: Vec<f64> = Vec::new();
        return Ok((
            Vec::<i64>::new().into_pyarray_bound(py),
            e.clone().into_pyarray_bound(py), e.clone().into_pyarray_bound(py),
            e.clone().into_pyarray_bound(py), e.clone().into_pyarray_bound(py),
            e.into_pyarray_bound(py),
        ));
    }

    // Bin index of each input row, relative to the first timestamp.
    let start = d[0];
    let n_bins = ((d[n - 1] - start) / step_ns + 1).max(1) as usize;

    let mut out_d = Vec::with_capacity(n_bins);
    let mut out_o = Vec::with_capacity(n_bins);
    let mut out_h = Vec::with_capacity(n_bins);
    let mut out_l = Vec::with_capacity(n_bins);
    let mut out_c = Vec::with_capacity(n_bins);
    let mut out_v = Vec::with_capacity(n_bins);

    let mut i = 0usize;
    let mut prev_close = f64::NAN;
    for b in 0..n_bins {
        let bin_start = start + (b as i64) * step_ns;
        let bin_end = bin_start + step_ns;
        if i < n && d[i] < bin_end {
            // One or more rows land in this bin — aggregate them.
            let (mut hi, mut lo, mut vol) = (f64::NEG_INFINITY, f64::INFINITY, 0.0f64);
            let op = o[i];
            let mut cl = c[i];
            while i < n && d[i] < bin_end {
                if h[i] > hi { hi = h[i]; }
                if l[i] < lo { lo = l[i]; }
                vol += v[i];
                cl = c[i];
                i += 1;
            }
            out_o.push(op); out_h.push(hi); out_l.push(lo); out_c.push(cl); out_v.push(vol);
            prev_close = cl;
        } else {
            // Empty bin: carry the previous close, zero volume (matches the
            // Python ffill + fillna(close) + sum-of-nothing behaviour).
            out_o.push(prev_close); out_h.push(prev_close);
            out_l.push(prev_close); out_c.push(prev_close); out_v.push(0.0);
        }
        out_d.push(bin_start);
    }

    Ok((
        out_d.into_pyarray_bound(py),
        out_o.into_pyarray_bound(py),
        out_h.into_pyarray_bound(py),
        out_l.into_pyarray_bound(py),
        out_c.into_pyarray_bound(py),
        out_v.into_pyarray_bound(py),
    ))
}

/// SMA over an arbitrary/hyperopt-tunable period.
#[pyfunction]
fn sma<'py>(
    py: Python<'py>,
    close: PyReadonlyArray1<'py, f64>,
    period: usize,
) -> Bound<'py, numpy::PyArray1<f32>> {
    calculate_sma_rust(&f32_from(&close), period).into_pyarray_bound(py)
}

/// EMA over an arbitrary/hyperopt-tunable period.
#[pyfunction]
fn ema<'py>(
    py: Python<'py>,
    close: PyReadonlyArray1<'py, f64>,
    period: usize,
) -> Bound<'py, numpy::PyArray1<f32>> {
    calculate_ema_rust(&f32_from(&close), period).into_pyarray_bound(py)
}

/// RSI over an arbitrary/hyperopt-tunable period.
#[pyfunction]
fn rsi<'py>(
    py: Python<'py>,
    close: PyReadonlyArray1<'py, f64>,
    period: usize,
) -> Bound<'py, numpy::PyArray1<f32>> {
    calculate_rsi_rust(&f32_from(&close), period).into_pyarray_bound(py)
}

/// ATR (Wilder's smoothing) over an arbitrary/hyperopt-tunable period.
#[pyfunction]
fn atr<'py>(
    py: Python<'py>,
    high: PyReadonlyArray1<'py, f64>,
    low: PyReadonlyArray1<'py, f64>,
    close: PyReadonlyArray1<'py, f64>,
    period: usize,
) -> Bound<'py, numpy::PyArray1<f32>> {
    calculate_atr_rust(&f32_from(&high), &f32_from(&low), &f32_from(&close), period)
        .into_pyarray_bound(py)
}

/// ADX over an arbitrary/hyperopt-tunable period.
#[pyfunction]
fn adx<'py>(
    py: Python<'py>,
    high: PyReadonlyArray1<'py, f64>,
    low: PyReadonlyArray1<'py, f64>,
    close: PyReadonlyArray1<'py, f64>,
    period: usize,
) -> Bound<'py, numpy::PyArray1<f32>> {
    calculate_adx_rust(&f32_from(&high), &f32_from(&low), &f32_from(&close), period)
        .into_pyarray_bound(py)
}

/// Bollinger Bands over an arbitrary/hyperopt-tunable period + std multiplier.
/// Returns (upper, middle, lower) numpy arrays.
#[pyfunction]
fn bollinger_bands<'py>(
    py: Python<'py>,
    close: PyReadonlyArray1<'py, f64>,
    period: usize,
    std_mult: f32,
) -> (
    Bound<'py, numpy::PyArray1<f32>>,
    Bound<'py, numpy::PyArray1<f32>>,
    Bound<'py, numpy::PyArray1<f32>>,
) {
    let (upper, middle, lower) = calculate_bollinger_bands_rust(&f32_from(&close), period, std_mult);
    (
        upper.into_pyarray_bound(py),
        middle.into_pyarray_bound(py),
        lower.into_pyarray_bound(py),
    )
}

/// Rolling z-score over an arbitrary window: `(x[i] - window_mean) / window_std`
/// (population std, ddof=0 — matches numpy's `.std()` default). NaN-aware: a
/// window containing any NaN produces NaN, recovering once the NaN exits the
/// window (see `fast_indicators::rolling_zscore`'s doc comment) — safe for
/// `pct_change()`-based series, which always carry a leading NaN.
///
/// Replaces the pandas `series.rolling(window).apply(lambda w: ...)` pattern,
/// which calls a Python function once per bar — measured at 57% of a real
/// backtest's wall time for exactly this computation in
/// AllIndicatorsDemoStrategy.py, dwarfing the entire rest of the Rust engine.
#[pyfunction]
fn rolling_zscore<'py>(
    py: Python<'py>,
    values: PyReadonlyArray1<'py, f64>,
    window: usize,
) -> Bound<'py, numpy::PyArray1<f32>> {
    rolling_zscore_rust(&f32_from(&values), window).into_pyarray_bound(py)
}

/// Bridges `multi_pair::StrategyCallbacks` to real Python strategy calls.
///
/// Phase 2 (entry-time): `leverage`/`custom_stake_amount`/`confirm_trade_entry`/
/// `custom_entry_price`, each calling into Python only when `has_*` says the
/// strategy actually overrides it, otherwise returning the exact same value
/// `NativeDefaults` would. None of these take a live `trade` (confirmed from
/// `_resolve_stake_and_leverage`/`_confirm_entry`/`_resolve_entry_price`'s own
/// call sites in `rust_backtest.py` — all pass `trade=None` or omit it).
///
/// Phase 3 (per-bar, `needs_trade_bridge`): `custom_stoploss`/`custom_exit`
/// need a real `LocalTrade` (so `trade.max_rate`/`min_rate` reads inside a
/// strategy's own override work) plus a `DataProvider` cursor kept current via
/// `set_bar_cursor`, and a live `self.wallets` proxy so `self.wallets.
/// get_total_stake_amount()` (this codebase's ECS-style sizers read it to feed
/// their equity-curve engine's `.update(equity)`) sees this run's real,
/// changing balance rather than a frozen `dry_run_wallet` figure — installed
/// unconditionally (cheap: two attribute sets) so Phase-2-only strategies get
/// it too, closing the same gap for `leverage()`/`custom_stake_amount()`.
///
/// Every Python call is wrapped in exactly the same "exception -> fall back
/// to the default" semantics the original `_resolve_leverage`/
/// `_resolve_stake_and_leverage`/`_confirm_entry`/`_resolve_entry_price`/
/// `do_exit_check`'s custom_stoploss/custom_exit try/except blocks already
/// use — a strategy override that raises must degrade to the unoverridden
/// behavior, not abort the backtest.
struct PyStrategyCallbacks {
    strategy: Py<PyAny>,
    has_leverage: bool,
    has_custom_stake_amount: bool,
    has_confirm_trade_entry: bool,
    has_custom_entry_price: bool,
    has_custom_exit: bool,
    has_confirm_trade_exit: bool,
    use_custom_stoploss: bool,
    entry_order_type: String,
    entry_tif: String,
    exit_order_type: String,
    exit_tif: String,

    /// True iff this run needs the LocalTrade/DataProvider machinery at all
    /// (`use_custom_stoploss`, `custom_exit`, or `confirm_trade_exit`
    /// overridden) — false keeps
    /// `set_bar_cursor`/`on_trade_opened`/etc. as cheap no-ops, preserving
    /// Phase 2's fully-native per-bar loop for strategies that don't need this.
    needs_trade_bridge: bool,
    is_futures: bool,
    /// `eng["fee_taker"]` — `LocalTrade`'s `fee_close` kwarg (matches
    /// `fill_entry`'s own `fee_close=fee`; this engine's fee_maker/taker
    /// distinction only ever affects the ALREADY-realized `TradeOut.fee_close`
    /// this module computes separately, not the live `trade` object itself).
    fee_close_default: f64,
    dataprovider: Option<Py<PyAny>>,
    required_startup: i64,
    local_trade_cls: Option<Py<PyAny>>,
    trading_mode_futures: Option<Py<PyAny>>,
    trading_mode_spot: Option<Py<PyAny>>,
    /// Live `LocalTrade` objects for currently-open positions, keyed by the
    /// handle `on_trade_opened` returned. Handle `0` is reserved for "no live
    /// trade" (either `needs_trade_bridge` is false, or construction failed) —
    /// every other method treats a miss/zero handle as a safe no-op, mirroring
    /// the Python engine's own `except Exception: p.local_trade = None` +
    /// downstream `if p.local_trade is not None` guards.
    trades: HashMap<multi_pair::TradeHandle, Py<PyAny>>,
    next_handle: multi_pair::TradeHandle,

    running_balance_cell: Rc<Cell<f64>>,
    tied_up_capital_cell: Rc<Cell<f64>>,
}

/// `#[pyclass]` stand-in for the real `Wallets` object, installed onto
/// `strategy.wallets` for the duration of a native-engine run — mirrors
/// `_joint_multi_pair_backtest`'s own `_RustWalletsProxy` field-for-field
/// (`get_total_stake_amount`/`get_available_stake_amount`/
/// `get_starting_balance`), backed by `Rc<Cell<f64>>` handles the engine loop
/// updates directly (see `StrategyCallbacks::on_balance_changed`) rather than
/// a Python round-trip. `unsendable`: this whole engine runs single-threaded
/// under one held GIL, so `Rc`/`Cell` (not `Send`) are fine — never crosses a
/// thread boundary.
#[pyclass(unsendable)]
struct RustWalletsProxy {
    running_balance: Rc<Cell<f64>>,
    tied_up_capital: Rc<Cell<f64>>,
    tradable_ratio: f64,
    starting_balance: f64,
}

#[pymethods]
impl RustWalletsProxy {
    fn get_total_stake_amount(&self) -> f64 {
        self.running_balance.get() * self.tradable_ratio
    }

    fn get_available_stake_amount(&self) -> f64 {
        (self.running_balance.get() * self.tradable_ratio - self.tied_up_capital.get()).max(0.0)
    }

    fn get_starting_balance(&self) -> f64 {
        self.starting_balance * self.tradable_ratio
    }
}

impl PyStrategyCallbacks {
    /// `pd.Timestamp(bar_ns).tz_localize("UTC")` — `bar_ns` is nanoseconds
    /// since epoch, the same representation `datetime64[ns]` already uses
    /// internally, so this reconstructs exactly the same Timestamp the
    /// Python engine builds from its own `pd.Timestamp(p.dates[i])`.
    fn timestamp(py: Python<'_>, bar_ns: i64) -> PyResult<Py<PyAny>> {
        let pandas = py.import_bound("pandas")?;
        let ts = pandas.getattr("Timestamp")?.call1((bar_ns,))?;
        Ok(ts.call_method1("tz_localize", ("UTC",))?.into())
    }
}

impl multi_pair::StrategyCallbacks for PyStrategyCallbacks {
    fn leverage(&mut self, pair: &str, bar_ns: i64, rate: f64, max_leverage: f64, direction: &str) -> f64 {
        if !self.has_leverage { return 1.0; }
        let result = Python::with_gil(|py| -> PyResult<f64> {
            let current_time = Self::timestamp(py, bar_ns)?;
            let kwargs = PyDict::new_bound(py);
            kwargs.set_item("pair", pair)?;
            kwargs.set_item("current_time", current_time)?;
            kwargs.set_item("current_rate", rate)?;
            kwargs.set_item("proposed_leverage", 1.0f64)?;
            kwargs.set_item("max_leverage", max_leverage)?;
            kwargs.set_item("entry_tag", py.None())?;
            kwargs.set_item("side", direction)?;
            self.strategy.bind(py).call_method("leverage", (), Some(&kwargs))?.extract::<f64>()
        });
        // Mirrors `_resolve_leverage`: any failure, or a non-positive result,
        // falls back to 1.0 — the caller in multi_pair.rs still applies the
        // `min(max(lev, 1.0), max_leverage)` clamp on top of this.
        match result {
            Ok(v) if v > 0.0 => v,
            _ => 1.0,
        }
    }

    fn custom_stake_amount(&mut self, pair: &str, bar_ns: i64, rate: f64, proposed: f64,
        max_stake: f64, leverage: f64, direction: &str) -> f64 {
        if !self.has_custom_stake_amount { return proposed; }
        let result = Python::with_gil(|py| -> PyResult<f64> {
            let current_time = Self::timestamp(py, bar_ns)?;
            let kwargs = PyDict::new_bound(py);
            kwargs.set_item("pair", pair)?;
            kwargs.set_item("current_time", current_time)?;
            kwargs.set_item("current_rate", rate)?;
            kwargs.set_item("proposed_stake", proposed)?;
            kwargs.set_item("min_stake", 0.0f64)?;
            kwargs.set_item("max_stake", max_stake)?;
            kwargs.set_item("leverage", leverage)?;
            kwargs.set_item("entry_tag", py.None())?;
            kwargs.set_item("side", direction)?;
            let r = self.strategy.bind(py).call_method("custom_stake_amount", (), Some(&kwargs))?;
            if r.is_none() { return Ok(f64::NAN); }
            r.extract::<f64>()
        });
        match result {
            Ok(v) if v.is_finite() && v > 0.0 => v,
            _ => proposed,
        }
    }

    fn confirm_trade_entry(&mut self, pair: &str, bar_ns: i64, amount: f64, rate: f64, direction: &str) -> bool {
        if !self.has_confirm_trade_entry { return true; }
        let result = Python::with_gil(|py| -> PyResult<bool> {
            let current_time = Self::timestamp(py, bar_ns)?;
            let kwargs = PyDict::new_bound(py);
            kwargs.set_item("pair", pair)?;
            kwargs.set_item("order_type", &self.entry_order_type)?;
            kwargs.set_item("amount", amount)?;
            kwargs.set_item("rate", rate)?;
            kwargs.set_item("time_in_force", &self.entry_tif)?;
            kwargs.set_item("current_time", current_time)?;
            kwargs.set_item("entry_tag", py.None())?;
            kwargs.set_item("side", direction)?;
            self.strategy.bind(py).call_method("confirm_trade_entry", (), Some(&kwargs))?.extract::<bool>()
        });
        result.unwrap_or(true)
    }

    fn custom_entry_price(&mut self, pair: &str, bar_ns: i64, proposed_rate: f64, direction: &str) -> f64 {
        if !self.has_custom_entry_price { return proposed_rate; }
        let result = Python::with_gil(|py| -> PyResult<Option<f64>> {
            let current_time = Self::timestamp(py, bar_ns)?;
            let kwargs = PyDict::new_bound(py);
            kwargs.set_item("pair", pair)?;
            kwargs.set_item("trade", py.None())?;
            kwargs.set_item("current_time", current_time)?;
            kwargs.set_item("proposed_rate", proposed_rate)?;
            kwargs.set_item("entry_tag", py.None())?;
            kwargs.set_item("side", direction)?;
            let r = self.strategy.bind(py).call_method("custom_entry_price", (), Some(&kwargs))?;
            if r.is_none() { Ok(None) } else { Ok(Some(r.extract::<f64>()?)) }
        });
        match result {
            Ok(Some(v)) => v,
            _ => proposed_rate,
        }
    }

    fn on_trade_opened(&mut self, pair: &str, bar_ns: i64, is_short: bool, amount: f64,
        rate: f64, stake: f64, leverage: f64, fee_open: f64) -> multi_pair::TradeHandle {
        if !self.needs_trade_bridge { return 0; }
        let handle = self.next_handle;
        let result = Python::with_gil(|py| -> PyResult<Py<PyAny>> {
            let current_time = Self::timestamp(py, bar_ns)?;
            let trading_mode = if self.is_futures {
                self.trading_mode_futures.as_ref().unwrap()
            } else {
                self.trading_mode_spot.as_ref().unwrap()
            };
            let kwargs = PyDict::new_bound(py);
            kwargs.set_item("pair", pair)?;
            kwargs.set_item("is_short", is_short)?;
            kwargs.set_item("amount", amount)?;
            kwargs.set_item("amount_requested", amount)?;
            kwargs.set_item("open_rate", rate)?;
            kwargs.set_item("open_rate_requested", rate)?;
            kwargs.set_item("fee_open", fee_open)?;
            kwargs.set_item("fee_close", self.fee_close_default)?;
            kwargs.set_item("leverage", leverage)?;
            kwargs.set_item("trading_mode", trading_mode)?;
            kwargs.set_item("open_date", &current_time)?;
            kwargs.set_item("is_open", true)?;
            kwargs.set_item("stake_amount", stake)?;
            kwargs.set_item("max_stake_amount", stake)?;
            kwargs.set_item("orders", PyList::empty_bound(py))?;
            let cls = self.local_trade_cls.as_ref().unwrap().bind(py);
            let trade = cls.call((), Some(&kwargs))?;
            cls.call_method1("add_bt_trade", (&trade,))?;
            Ok(trade.into())
        });
        match result {
            Ok(trade) => {
                self.trades.insert(handle, trade);
                self.next_handle += 1;
                handle
            }
            // Mirrors `do_enter`'s own `except Exception: p.local_trade =
            // None` — degrade to "no live trade" rather than aborting; every
            // other method here already treats handle 0 / a missing entry as
            // a no-op.
            Err(_) => 0,
        }
    }

    fn adjust_min_max_rates(&mut self, handle: multi_pair::TradeHandle, high: f64, low: f64) {
        if handle == 0 { return; }
        let Some(trade) = self.trades.get(&handle) else { return; };
        let _ = Python::with_gil(|py| -> PyResult<()> {
            trade.bind(py).call_method1("adjust_min_max_rates", (high, low))?;
            Ok(())
        });
    }

    fn custom_stoploss(&mut self, handle: multi_pair::TradeHandle, pair: &str, bar_ns: i64,
        current_rate: f64, current_profit: f64) -> Option<f64> {
        if !self.use_custom_stoploss || handle == 0 { return None; }
        Python::with_gil(|py| -> Option<f64> {
            let trade = self.trades.get(&handle)?;
            let current_time = Self::timestamp(py, bar_ns).ok()?;
            let kwargs = PyDict::new_bound(py);
            kwargs.set_item("pair", pair).ok()?;
            kwargs.set_item("trade", trade).ok()?;
            kwargs.set_item("current_time", current_time).ok()?;
            kwargs.set_item("current_rate", current_rate).ok()?;
            kwargs.set_item("current_profit", current_profit).ok()?;
            kwargs.set_item("after_fill", false).ok()?;
            let r = self.strategy.bind(py).call_method("custom_stoploss", (), Some(&kwargs)).ok()?;
            if r.is_none() { return None; }
            match r.extract::<f64>() {
                Ok(v) if v.is_finite() => Some(v),
                _ => None,
            }
        })
    }

    fn custom_exit(&mut self, handle: multi_pair::TradeHandle, pair: &str, bar_ns: i64,
        current_rate: f64, current_profit: f64) -> Option<String> {
        if !self.has_custom_exit || handle == 0 { return None; }
        Python::with_gil(|py| -> Option<String> {
            let trade = self.trades.get(&handle)?;
            let current_time = Self::timestamp(py, bar_ns).ok()?;
            let kwargs = PyDict::new_bound(py);
            kwargs.set_item("pair", pair).ok()?;
            kwargs.set_item("trade", trade).ok()?;
            kwargs.set_item("current_time", current_time).ok()?;
            kwargs.set_item("current_rate", current_rate).ok()?;
            kwargs.set_item("current_profit", current_profit).ok()?;
            let r = self.strategy.bind(py).call_method("custom_exit", (), Some(&kwargs)).ok()?;
            // Mirrors `if reason_cust: ... tag = reason_cust if isinstance(
            // reason_cust, str) else None` exactly: a non-empty string IS the
            // tag; anything else Python-truthy triggers with no tag; anything
            // falsy (None/False/"") does not trigger. Checked in this order
            // (str before generic truthiness) so a non-empty string is never
            // misread through Python's `__bool__`/`__len__` protocol.
            if let Ok(s) = r.extract::<String>() {
                return if s.is_empty() { None } else { Some(s) };
            }
            match r.is_truthy() {
                Ok(true) => Some(String::new()),
                _ => None,
            }
        })
    }

    fn on_trade_closed(&mut self, handle: multi_pair::TradeHandle, close_bar_ns: i64, profit_ratio: f64, profit_abs: f64) {
        if handle == 0 { return; }
        let Some(trade) = self.trades.remove(&handle) else { return; };
        let _ = Python::with_gil(|py| -> PyResult<()> {
            let current_time = Self::timestamp(py, close_bar_ns)?;
            let trade_bound = trade.bind(py);
            trade_bound.setattr("close_date", current_time)?;
            trade_bound.setattr("close_profit", profit_ratio)?;
            trade_bound.setattr("close_profit_abs", profit_abs)?;
            trade_bound.setattr("is_open", false)?;
            self.local_trade_cls.as_ref().unwrap().bind(py).call_method1("close_bt_trade", (trade_bound,))?;
            Ok(())
        });
    }

    fn set_bar_cursor(&mut self, pair: &str, bar_idx: i64) {
        if !self.needs_trade_bridge { return; }
        let Some(dp) = &self.dataprovider else { return; };
        let _ = Python::with_gil(|py| -> PyResult<()> {
            dp.bind(py).call_method1("_set_dataframe_max_index", (pair, self.required_startup + bar_idx))?;
            Ok(())
        });
    }

    fn on_balance_changed(&mut self, running_balance: f64, tied_up_capital: f64) {
        self.running_balance_cell.set(running_balance);
        self.tied_up_capital_cell.set(tied_up_capital);
    }

    fn confirm_trade_exit(&mut self, handle: multi_pair::TradeHandle, pair: &str, bar_ns: i64,
        amount: f64, rate: f64, exit_reason: &str) -> bool {
        if !self.has_confirm_trade_exit || handle == 0 { return true; }
        let result = Python::with_gil(|py| -> Option<bool> {
            let trade = self.trades.get(&handle)?;
            let current_time = Self::timestamp(py, bar_ns).ok()?;
            let kwargs = PyDict::new_bound(py);
            kwargs.set_item("pair", pair).ok()?;
            kwargs.set_item("trade", trade).ok()?;
            kwargs.set_item("order_type", &self.exit_order_type).ok()?;
            kwargs.set_item("amount", amount).ok()?;
            kwargs.set_item("rate", rate).ok()?;
            kwargs.set_item("time_in_force", &self.exit_tif).ok()?;
            kwargs.set_item("sell_reason", exit_reason).ok()?;
            kwargs.set_item("exit_reason", exit_reason).ok()?;
            kwargs.set_item("current_time", current_time).ok()?;
            let r = self.strategy.bind(py).call_method("confirm_trade_exit", (), Some(&kwargs)).ok()?;
            r.extract::<bool>().ok()
        });
        // Mirrors `_confirm_exit`: any failure (exception, missing trade,
        // non-bool return) degrades to True — never let a broken override
        // silently trap a position open forever.
        result.unwrap_or(true)
    }
}

/// Native multi-pair backtest engine — see `crate::multi_pair` for the full
/// design rationale. Bridges `leverage`/`custom_stake_amount`/
/// `confirm_trade_entry`/`custom_entry_price`/`custom_stoploss`/`custom_exit`/
/// `confirm_trade_exit` to real Python calls for whichever of those a
/// strategy overrides, degrading to `NativeDefaults` for the rest;
/// `rust_backtest.py` is responsible for verifying that no OTHER hook
/// (`custom_exit_price`, `custom_sell`) is overridden, and that no
/// resting-order/market-expiry config applies, BEFORE calling this (falling
/// back to the original, untouched `_joint_multi_pair_backtest` otherwise) —
/// this function does not re-check any of that.
///
/// `pairs` is a list of dicts, one per pair, with keys: `pair` (str),
/// `dates_ns`/`funding_dates_ns` (int64 numpy arrays), `open`/`high`/`low`/
/// `precision`/`funding_cumsum` (float64 numpy arrays), `enter_long`/
/// `exit_long`/`enter_short`/`exit_short` (uint8 numpy arrays, already
/// `.shift(1)`-ed exactly like `_PairState.el/xl/es/xs`), and
/// `exchange_max_leverage`/`amount_precision`/`contract_size` (floats — the
/// latter two are the exchange's lot-size tick and contract size for this
/// pair, `NaN`/`0.0` amount_precision meaning "no exchange, don't round").
/// `funding_dates_ns`/`funding_cumsum` may be empty arrays (no funding data
/// for that pair).
///
/// `config_json` deserializes directly into `EngineConfig`.
///
/// Returns a JSON string `{"trades": [...], "rejected": N}` — each trade dict
/// matches `_joint_multi_pair_backtest`'s own output shape field-for-field, so
/// `run_rust_backtest`'s existing trade-formatting code consumes either
/// engine's output identically. The caller (`rust_backtest.py`) attaches
/// `_dates` per trade afterward from its own already-known per-pair dates
/// arrays, exactly as it does today for `_joint_multi_pair_backtest`'s output.
/// `strategy` is the live strategy instance; the `has_*` flags are computed
/// Python-side (per-hook `getattr(cls, name) is not base_fn` checks — see
/// `_strategy_overrides_any_hook` in `rust_backtest.py`) so a strategy
/// overriding only e.g. `leverage()` pays the Python-call cost only for that
/// one hook, not the others. `entry_order_type`/`entry_tif` are the run's
/// resolved (not necessarily default) `order_types`/`order_time_in_force`
/// values, forwarded to `confirm_trade_entry()` exactly as the Python engine
/// already does. `dataprovider`/`required_startup`/`fee_close_default` matter
/// only when `use_custom_stoploss` (via `config_json`) or `has_custom_exit` is
/// true — pass Python `None` for `dataprovider` and `0`/`0.0` for the other
/// two otherwise (`needs_trade_bridge` gates every use of them internally).
#[pyfunction]
#[allow(clippy::too_many_arguments)]
fn multi_pair_backtest<'py>(
    py: Python<'py>,
    pairs: Bound<'py, PyList>,
    config_json: String,
    strategy: Py<PyAny>,
    has_leverage: bool,
    has_custom_stake_amount: bool,
    has_confirm_trade_entry: bool,
    has_custom_entry_price: bool,
    has_custom_exit: bool,
    has_confirm_trade_exit: bool,
    entry_order_type: String,
    entry_tif: String,
    exit_order_type: String,
    exit_tif: String,
    dataprovider: Py<PyAny>,
    required_startup: i64,
    fee_close_default: f64,
) -> PyResult<String> {
    let cfg: EngineConfig = serde_json::from_str(&config_json)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("bad config_json: {e}")))?;

    let mut inputs: Vec<PairInput> = Vec::with_capacity(pairs.len());
    for item in pairs.iter() {
        let d = item.downcast::<PyDict>()
            .map_err(|_| pyo3::exceptions::PyTypeError::new_err("each pairs[] entry must be a dict"))?;
        let get = |key: &str| -> PyResult<Bound<'py, PyAny>> {
            d.get_item(key)?.ok_or_else(|| {
                pyo3::exceptions::PyKeyError::new_err(format!("pairs[] entry missing '{key}'"))
            })
        };
        let pair: String = get("pair")?.extract()?;
        let dates_ns = i64_from(&get("dates_ns")?.extract::<PyReadonlyArray1<'py, i64>>()?);
        let open = f64_vec_from(&get("open")?.extract::<PyReadonlyArray1<'py, f64>>()?);
        let high = f64_vec_from(&get("high")?.extract::<PyReadonlyArray1<'py, f64>>()?);
        let low = f64_vec_from(&get("low")?.extract::<PyReadonlyArray1<'py, f64>>()?);
        let enter_long = u8_from(&get("enter_long")?.extract::<PyReadonlyArray1<'py, u8>>()?);
        let exit_long = u8_from(&get("exit_long")?.extract::<PyReadonlyArray1<'py, u8>>()?);
        let enter_short = u8_from(&get("enter_short")?.extract::<PyReadonlyArray1<'py, u8>>()?);
        let exit_short = u8_from(&get("exit_short")?.extract::<PyReadonlyArray1<'py, u8>>()?);
        let precision = f64_vec_from(&get("precision")?.extract::<PyReadonlyArray1<'py, f64>>()?);
        let exchange_max_leverage: f64 = get("exchange_max_leverage")?.extract()?;
        let amount_precision: f64 = get("amount_precision")?.extract()?;
        let contract_size: f64 = get("contract_size")?.extract()?;
        // 0.0 ("no override") for every pair but Futuressim/Rithmic/Ninjatrader's -
        // see PairInput.fee_per_contract's doc comment.
        let fee_per_contract: f64 = get("fee_per_contract")?.extract()?;
        let funding_dates_ns = i64_from(&get("funding_dates_ns")?.extract::<PyReadonlyArray1<'py, i64>>()?);
        let funding_cumsum = f64_vec_from(&get("funding_cumsum")?.extract::<PyReadonlyArray1<'py, f64>>()?);

        inputs.push(PairInput {
            pair, dates_ns, open, high, low,
            enter_long, exit_long, enter_short, exit_short,
            precision, exchange_max_leverage, amount_precision, contract_size,
            fee_per_contract, funding_dates_ns, funding_cumsum,
        });
    }

    let needs_trade_bridge = cfg.use_custom_stoploss || has_custom_exit || has_confirm_trade_exit;

    // Reset the process-global bt_trades registry so a strategy reading
    // `Trade.get_trades_proxy()` (cross-trade portfolio-risk vetoes) never
    // sees trades left over from a previous run in this same process —
    // mirrors `_joint_multi_pair_backtest`'s own `LocalTrade.reset_trades()`.
    // Done unconditionally (cheap: resets four class-level lists/counters)
    // even for Phase-2-only runs, which never populate the registry anyway.
    let trade_model_mod = py.import_bound("VulcanTrader.persistence.trade_model")?;
    trade_model_mod.getattr("LocalTrade")?.call_method0("reset_trades")?;

    let (local_trade_cls, trading_mode_futures, trading_mode_spot, dp_opt) = if needs_trade_bridge {
        let local_trade_cls: Py<PyAny> = trade_model_mod.getattr("LocalTrade")?.into();
        let trading_mode_enum = py.import_bound("VulcanTrader.enums")?.getattr("TradingMode")?;
        let futures: Py<PyAny> = trading_mode_enum.getattr("FUTURES")?.into();
        let spot: Py<PyAny> = trading_mode_enum.getattr("SPOT")?.into();
        let dp = if dataprovider.is_none(py) { None } else { Some(dataprovider.clone_ref(py)) };
        (Some(local_trade_cls), Some(futures), Some(spot), dp)
    } else {
        (None, None, None, None)
    };

    // Install the wallets proxy unconditionally (see `PyStrategyCallbacks`'s
    // doc comment) — mirrors `_joint_multi_pair_backtest`'s own
    // `strategy.wallets = _RustWalletsProxy()` / final `strategy.wallets =
    // _orig_wallets` restore. No try/finally: a mid-run Python exception here
    // propagates out of this pyfunction, and `run_rust_backtest`'s own
    // try/except around the whole native-engine call already falls back to
    // the proven Python engine on ANY failure — same safety net the reference
    // engine relies on for its own (also finally-less) restore.
    let orig_wallets = strategy.bind(py).getattr("wallets").ok();
    let running_balance_cell = Rc::new(Cell::new(cfg.starting_balance));
    let tied_up_capital_cell = Rc::new(Cell::new(0.0f64));
    let proxy = Py::new(py, RustWalletsProxy {
        running_balance: running_balance_cell.clone(),
        tied_up_capital: tied_up_capital_cell.clone(),
        tradable_ratio: cfg.tradable_ratio,
        starting_balance: cfg.starting_balance,
    })?;
    strategy.bind(py).setattr("wallets", proxy)?;

    let mut callbacks = PyStrategyCallbacks {
        strategy: strategy.clone_ref(py),
        has_leverage,
        has_custom_stake_amount,
        has_confirm_trade_entry,
        has_custom_entry_price,
        has_custom_exit,
        has_confirm_trade_exit,
        use_custom_stoploss: cfg.use_custom_stoploss,
        entry_order_type,
        entry_tif,
        exit_order_type,
        exit_tif,
        needs_trade_bridge,
        is_futures: cfg.is_futures,
        fee_close_default,
        dataprovider: dp_opt,
        required_startup,
        local_trade_cls,
        trading_mode_futures,
        trading_mode_spot,
        trades: HashMap::new(),
        next_handle: 1,
        running_balance_cell,
        tied_up_capital_cell,
    };
    // NOT wrapped in `py.allow_threads`, unlike the other pyfunctions in this
    // file: the engine now calls back into Python mid-loop for strategies
    // that override any of the four entry-time hooks, which needs the GIL
    // available on this thread throughout, not released for the duration.
    let out = multi_pair::multi_pair_backtest(&inputs, &cfg, &mut callbacks);

    match orig_wallets {
        Some(w) => strategy.bind(py).setattr("wallets", w)?,
        None => strategy.bind(py).setattr("wallets", py.None())?,
    }

    let json = serde_json::json!({
        "trades": out.trades,
        "rejected": out.rejected,
    });
    Ok(json.to_string())
}

/// Number of significant decimal places per value — see
/// `fast_indicators::significant_decimals`'s doc comment for exactly which
/// Python function this replaces and why the "surprising" 15/13-digit
/// results on values like 100.123 are correct, not a bug.
#[pyfunction]
fn significant_decimals<'py>(
    py: Python<'py>,
    values: PyReadonlyArray1<'py, f64>,
) -> Bound<'py, numpy::PyArray1<i64>> {
    significant_decimals_rust(&f64_vec_from(&values)).into_pyarray_bound(py)
}

#[pymodule]
fn vulcan_rust_indicators(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(fill_missing_candles, m)?)?;
    m.add_function(wrap_pyfunction!(calculate_standard_indicators, m)?)?;
    m.add_function(wrap_pyfunction!(run_backtest, m)?)?;
    m.add_function(wrap_pyfunction!(compute_result_metrics, m)?)?;
    m.add_function(wrap_pyfunction!(sma, m)?)?;
    m.add_function(wrap_pyfunction!(ema, m)?)?;
    m.add_function(wrap_pyfunction!(rsi, m)?)?;
    m.add_function(wrap_pyfunction!(atr, m)?)?;
    m.add_function(wrap_pyfunction!(adx, m)?)?;
    m.add_function(wrap_pyfunction!(bollinger_bands, m)?)?;
    m.add_function(wrap_pyfunction!(rolling_zscore, m)?)?;
    m.add_function(wrap_pyfunction!(multi_pair_backtest, m)?)?;
    m.add_function(wrap_pyfunction!(significant_decimals, m)?)?;
    Ok(())
}
