//! Fast financial indicators — canonical implementations for `simd_indicators2`.
//!
//! Two APIs are provided for every indicator:
//!
//! **Batch functions** — accept full price slices, return allocated `Vec`s.
//! Use these when you need all historical values at once (backtesting, charts).
//!
//! | Function              | Notes                                         |
//! |-----------------------|-----------------------------------------------|
//! | `true_range`          | Scalar (auto-vectorised by LLVM)              |
//! | `true_range_asm`      | Hand-written AVX2 inline asm                  |
//! | `sma`                 | Prefix-sum sliding window                     |
//! | `ema`                 | Wilder-seeded EMA (serial recurrence)         |
//! | `rsi`                 | Wilder RSI via `gains_losses`                 |
//! | `gains_losses`        | AVX2 vmaxps split                             |
//! | `atr`                 | Wilder ATR (serial recurrence)                |
//! | `bollinger` / `bollinger_on` | Scalar Bollinger variants               |
//! | `macd`                | EMA fast/slow/signal                          |
//! | `choppiness_scalar`   | Rolling HH/LL + ln                            |
//!
//! **Per-tick state structs** — maintain O(1) ring-buffer state; call `.update()`
//! once per bar.  No heap allocation in the hot path.  Use these in live trading
//! or any streaming context where you process bars one at a time.
//!
//! | Struct            | Output              | Complexity   |
//! |-------------------|---------------------|--------------|
//! | `SmaState`        | SMA                 | O(1)         |
//! | `EmaState`        | EMA                 | O(1)         |
//! | `RsiState`        | RSI                 | O(1) Wilder  |
//! | `AtrState`        | ATR                 | O(1) Wilder  |
//! | `BollingerState`  | (lower, mid, upper) | O(1) E[x²]   |
//! | `AdxState`        | ADX                 | O(1) Wilder  |
//! | `ChopState`       | Choppiness          | O(period) HH/LL |
//! | `FvgState`        | Fair Value Gap      | O(1)         |
//! | `VwapState`       | VWAP                | O(1) cumul.  |
//! | `RocState`        | ROC                 | O(1) ring    |
//! | `CciState`        | CCI                 | O(period) MAD |
//! | `MfiState`        | MFI                 | O(1) sliding |
//! | `EffRatioState`   | Kaufman ER          | O(1)         |
//! | `CtiState`        | Chop+ER+Z-scores×4  | O(1)         |
//! | `StochKState`     | Stochastic %K       | O(period)    |
//! | `ObvState`        | OBV                 | O(1)         |
//! | `CmfState`        | CMF                 | O(1) sliding |
//! | `FibState`        | Fib retracement×2   | O(period)    |
//! | `WilliamsRState`  | Williams %R         | O(period)    |
//! | `ParabolicSarState` | PSAR              | O(1)         |
//! | `BosState`        | Break of Structure  | O(period)    |
//! | `ChochState`      | Change of Character | O(period)    |
//! | `StochRsiState`   | (K, D)              | O(1)         |
//!
//! ## Performance (505 808 bars, AVX2+FMA, release opt-level=3)
//! The per-tick structs run 1–3 ns/bar for scalar-recurrence indicators
//! (SMA 0.8, EMA 2.2, RSI 2.9, ATR 2.7, Bollinger 1.7) and 5–16 ns/bar
//! for indicators that require an O(period) scan per tick (Choppiness, CCI,
//! StochK, Fibonacci).  Running all 23 structs fused in one loop costs
//! ~137 ns/tick vs ~153 ns/tick sequentially (~10% cache-synergy savings).
//!
//! ## Why per-tick beats batch for Bollinger / SMA / EMA
//! The batch functions allocate a `Vec` on every call and write the full
//! output array, thrashing L1/L2.  The state structs keep Σx and Σx² as
//! plain `f32` scalars in registers; a ring-buffer evicts the oldest sample
//! in one store, so each tick is ~3 arithmetic ops + 1 sqrt — no allocation.


#![allow(dead_code)]

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;


// =============================================================================
// CPU feature detection / reporting
// =============================================================================
pub fn cpu_info() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        let avx2   = is_x86_feature_detected!("avx2");
        let fma    = is_x86_feature_detected!("fma");
        let avx512 = is_x86_feature_detected!("avx512f");
        let sse4   = is_x86_feature_detected!("sse4.1");
        format!("SSE4.1={sse4}  AVX2={avx2}  FMA={fma}  AVX-512F={avx512}")
    }
    #[cfg(not(target_arch = "x86_64"))]
    "non-x86 (scalar only)".to_string()
}

// =============================================================================
// Helpers
// =============================================================================

/// Horizontal sum of all 8 f32 lanes in a __m256.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn hsum_avx2(v: __m256) -> f32 {
    let hi  = _mm256_extractf128_ps(v, 1);
    let lo  = _mm256_castps256_ps128(v);
    let s4  = _mm_add_ps(hi, lo);
    let shuf = _mm_movehdup_ps(s4);
    let s2  = _mm_add_ps(s4, shuf);
    let shuf2 = _mm_movehl_ps(shuf, s2);
    _mm_cvtss_f32(_mm_add_ss(s2, shuf2))
}

/// |x| via sign-bit clear: x & ~(-0.0)
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn abs_ps(v: __m256) -> __m256 {
    _mm256_andnot_ps(_mm256_set1_ps(-0.0f32), v)
}



// =============================================================================
// FAST SCALAR LN — bit-manipulation range reduction + 4-term atanh series
// =============================================================================
// ln(x) = e·ln2 + 2·atanh(s)   where s = (m−1)/(m+1), m ∈ [1,2).
// s ∈ [0, 1/3] for m ∈ [1,2).  Four-term 2·atanh series gives max absolute
// error < 2e-5 when the division is exact.  With rcpss (∼1.5e-4 relative error)
// max CHOP error < 0.005 units — well within the 0.05-unit cross-test tolerance.
// rcpss replaces divss (~13 cy latency) with a 4-cycle reciprocal estimate.
// =============================================================================
#[inline(always)]
fn fast_ln_f32(x: f32) -> f32 {
    const LN2: f32 = core::f32::consts::LN_2;
    let bits = x.to_bits();
    let e = (bits >> 23) as i32 - 127;
    let m = f32::from_bits((bits & 0x007F_FFFF) | 0x3F80_0000); // m ∈ [1, 2)
    // rcpss ≈ 1/(m+1), error ∼ 1.5×10⁻⁴ relative → CHOP error < 0.005 units
    #[cfg(target_arch = "x86_64")]
    let s = unsafe { (m - 1.0) * _mm_cvtss_f32(_mm_rcp_ss(_mm_set_ss(m + 1.0))) };
    #[cfg(not(target_arch = "x86_64"))]
    let s = (m - 1.0) / (m + 1.0);
    let s2   = s * s;
    let ln_m = s * (2.0 + s2 * (0.666_666_7 + s2 * (0.4 + s2 * 0.285_714_3)));
    ln_m + e as f32 * LN2
}

// =============================================================================
// TRUE RANGE  —  AVX2 kernel + scalar fallback
// =============================================================================
// TR[0] = H[0] – L[0]
// TR[i] = max( H[i]–L[i], |H[i]–C[i-1]|, |L[i]–C[i-1]| )
//
// Assembly equivalent (one 8-wide AVX2 iteration):
//   vmovups   ymm_h,  [high+i*4]
//   vmovups   ymm_l,  [low+i*4]
//   vmovups   ymm_cp, [close+(i-1)*4]
//   vsubps    ymm_hl,  ymm_h, ymm_l
//   vsubps    ymm_hc,  ymm_h, ymm_cp
//   vsubps    ymm_lc,  ymm_l, ymm_cp
//   vandnps   ymm_hc,  ymm_sign_mask, ymm_hc   ; abs
//   vandnps   ymm_lc,  ymm_sign_mask, ymm_lc   ; abs
//   vmaxps    ymm_tr,  ymm_hl, ymm_hc
//   vmaxps    ymm_tr,  ymm_tr, ymm_lc
//   vmovups   [output+i*4], ymm_tr
pub fn true_range(high: &[f32], low: &[f32], close: &[f32]) -> Vec<f32> {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") {
        return unsafe { true_range_avx2(high, low, close) };
    }
    true_range_scalar(high, low, close)
}

fn true_range_scalar(high: &[f32], low: &[f32], close: &[f32]) -> Vec<f32> {
    let n = high.len();
    let mut tr = vec![0.0f32; n];
    if n == 0 { return tr; }
    tr[0] = high[0] - low[0];
    for i in 1..n {
        let hl = high[i] - low[i];
        let hc = (high[i] - close[i - 1]).abs();
        let lc = (low[i]  - close[i - 1]).abs();
        tr[i] = hl.max(hc).max(lc);
    }
    tr
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn true_range_avx2(high: &[f32], low: &[f32], close: &[f32]) -> Vec<f32> {
    let n = high.len();
    let mut tr = vec![0.0f32; n];
    if n == 0 { return tr; }
    tr[0] = high[0] - low[0];
    if n < 2 { return tr; }

    let hp  = high.as_ptr();
    let lp  = low.as_ptr();
    let cp  = close.as_ptr();
    let out = tr.as_mut_ptr();

    let mut i = 1usize;
    while i + 8 <= n {
        let h      = _mm256_loadu_ps(hp.add(i));
        let l      = _mm256_loadu_ps(lp.add(i));
        let c_prev = _mm256_loadu_ps(cp.add(i - 1));   // previous close

        let hl = _mm256_sub_ps(h, l);
        let hc = abs_ps(_mm256_sub_ps(h, c_prev));
        let lc = abs_ps(_mm256_sub_ps(l, c_prev));

        let result = _mm256_max_ps(_mm256_max_ps(hl, hc), lc);
        _mm256_storeu_ps(out.add(i), result);
        i += 8;
    }
    // Scalar tail
    while i < n {
        let hl = high[i] - low[i];
        let hc = (high[i] - close[i - 1]).abs();
        let lc = (low[i]  - close[i - 1]).abs();
        tr[i] = hl.max(hc).max(lc);
        i += 1;
    }
    tr
}

// =============================================================================
// TRUE RANGE — inline-assembly version (demonstrating raw AVX2 asm)
// =============================================================================
// Functionally identical to true_range_avx2 above.
// Uses Rust's `asm!` macro (analogous to GCC's __asm__ volatile or a
// handwritten *.asm NASM kernel) to drive the AVX2 instructions directly.
// This is the same technique used in FFmpeg's libavcodec/x86/*.asm files.
pub fn true_range_asm(high: &[f32], low: &[f32], close: &[f32]) -> Vec<f32> {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") {
        return unsafe { true_range_raw_asm(high, low, close) };
    }
    true_range_scalar(high, low, close)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn true_range_raw_asm(high: &[f32], low: &[f32], close: &[f32]) -> Vec<f32> {
    let n = high.len();
    let mut tr = vec![0.0f32; n];
    if n == 0 { return tr; }
    tr[0] = high[0] - low[0];
    if n < 2 { return tr; }

    // sign-bit mask for abs() via VANDNPS
    let sign_mask = 0x80000000u32;
    let sign_f: f32 = f32::from_bits(sign_mask);
    let sign_arr = [sign_f; 8];

    let hp  = high.as_ptr();
    let lp  = low.as_ptr();
    let cp  = close.as_ptr();
    let out = tr.as_mut_ptr();
    let sm  = sign_arr.as_ptr();

    let mut i = 1usize;
    while i + 8 <= n {
        // Raw AVX2 assembly: each `vmovups/vsubps/vandnps/vmaxps` maps
        // one-to-one to the same mnemonic in NASM/MASM syntax.
        std::arch::asm!(
            "vmovups     {h},  [{hp} + {off}]",       // load 8 highs
            "vmovups     {l},  [{lp} + {off}]",       // load 8 lows
            "vmovups     {cp_v}, [{cp} + {off} - 4]", // load 8 prev-closes
            "vmovups     {sm_v}, [{sm}]",              // load sign mask
            "vsubps      {hl},  {h},    {l}",          // H - L
            "vsubps      {hc},  {h},    {cp_v}",       // H - C_prev
            "vsubps      {lc},  {l},    {cp_v}",       // L - C_prev
            "vandnps     {hc},  {sm_v}, {hc}",         // |H - C_prev|
            "vandnps     {lc},  {sm_v}, {lc}",         // |L - C_prev|
            "vmaxps      {tr},  {hl},   {hc}",         // max(HL, HC)
            "vmaxps      {tr},  {tr},   {lc}",         // max(max, LC)
            "vmovups     [{out} + {off}], {tr}",        // store result
            hp    = in(reg) hp,
            lp    = in(reg) lp,
            cp    = in(reg) cp,
            out   = in(reg) out,
            sm    = in(reg) sm,
            off   = in(reg) i * 4,
            h     = out(ymm_reg) _,
            l     = out(ymm_reg) _,
            cp_v  = out(ymm_reg) _,
            sm_v  = out(ymm_reg) _,
            hl    = out(ymm_reg) _,
            hc    = out(ymm_reg) _,
            lc    = out(ymm_reg) _,
            tr    = out(ymm_reg) _,
            options(nostack),
        );
        i += 8;
    }
    // Scalar tail
    while i < n {
        let hl = high[i] - low[i];
        let hc = (high[i] - close[i - 1]).abs();
        let lc = (low[i]  - close[i - 1]).abs();
        tr[i] = hl.max(hc).max(lc);
        i += 1;
    }
    tr
}


// =============================================================================
// SMA  —  prefix-sum approach
// =============================================================================
// prefix[i+1] = Σ prices[0..=i]     (serial, ~memory-bandwidth bound)
// SMA[i] = (prefix[i+1] - prefix[i+1-period]) / period
//         ↑ this subtraction + multiply is vectorised with AVX2
pub fn sma(prices: &[f32], period: usize) -> Vec<f32> {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") {
        return unsafe { sma_avx2(prices, period) };
    }
    sma_scalar(prices, period)
}

// The prefix sum MUST accumulate in f64, not f32. `prefix[i]` grows without
// bound over the series (Σ of every price seen so far) — for ~60k bars of
// BTC-magnitude (~1e5) prices that reaches ~5.7e9, and f32 only carries ~7
// significant decimal digits at that magnitude (ULP ≈ 512). Subtracting two
// such prefix values to recover a ~20-bar window sum (~2e6) is textbook
// catastrophic cancellation: measured up to ~28 units of absolute error on a
// 20-period SMA late in a 60k-bar series (vs <0.5 for a plain sliding-window
// running sum, which never grows past the window's own scale). f64's ~15-16
// digits keep the ULP at that same magnitude around 1e-6 — the cancellation
// error becomes negligible. Only the FINAL per-bar result is cast to f32.
fn sma_scalar(prices: &[f32], period: usize) -> Vec<f32> {
    let n = prices.len();
    let mut result = vec![f32::NAN; n];
    if n < period || period == 0 { return result; }
    let mut prefix = vec![0.0f64; n + 1];
    for i in 0..n { prefix[i + 1] = prefix[i] + prices[i] as f64; }
    let inv = 1.0 / period as f64;
    for i in (period - 1)..n {
        result[i] = ((prefix[i + 1] - prefix[i + 1 - period]) * inv) as f32;
    }
    result
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn sma_avx2(prices: &[f32], period: usize) -> Vec<f32> {
    let n = prices.len();
    let mut result = vec![f32::NAN; n];
    if n < period || period == 0 { return result; }

    // Build prefix sum in f64 (serial) — see `sma_scalar`'s doc comment for
    // why this can't be f32. AVX2 f64 lanes are 4-wide (not 8, like f32), so
    // this vectorized path halves the previous per-iteration width — moot in
    // practice, since every AVX2 kernel in this module was already measured
    // (VulcanTrader session notes) to give no gain over LLVM auto-vectorizing
    // the equivalent scalar loop.
    let mut prefix = vec![0.0f64; n + 1];
    for i in 0..n { prefix[i + 1] = prefix[i] + prices[i] as f64; }

    let inv_v  = _mm256_set1_pd(1.0f64 / period as f64);
    let pfx    = prefix.as_ptr();
    let start  = period - 1;
    let mut i  = start;

    // Vectorised: 4 SMA values per iteration (f64 lanes), cast down to f32 on store.
    while i + 4 <= n {
        let end_v   = _mm256_loadu_pd(pfx.add(i + 1));
        let start_v = _mm256_loadu_pd(pfx.add(i + 1 - period));
        let sma_v   = _mm256_mul_pd(_mm256_sub_pd(end_v, start_v), inv_v);
        let mut lanes = [0.0f64; 4];
        _mm256_storeu_pd(lanes.as_mut_ptr(), sma_v);
        for (k, &v) in lanes.iter().enumerate() { result[i + k] = v as f32; }
        i += 4;
    }
    while i < n {
        result[i] = ((prefix[i + 1] - prefix[i + 1 - period]) / period as f64) as f32;
        i += 1;
    }
    result
}

// =============================================================================
// EMA  —  scalar only (serial recurrence, no meaningful SIMD speedup)
// =============================================================================
// y[i] = α·x[i] + (1-α)·y[i-1]
//
// This is a first-order IIR filter. The data dependency y[i] ← y[i-1]
// prevents SIMD parallelism along the time axis. Block-carry techniques
// exist but their overhead exceeds the gain for typical market data sizes.
// We include EMA here so benchmark [3] computes the same set as [2].
pub fn ema(prices: &[f32], period: usize) -> Vec<f32> {
    let n = prices.len();
    let mut result = vec![f32::NAN; n];
    if n < period || period == 0 { return result; }
    let alpha = 2.0f32 / (period as f32 + 1.0);
    let beta  = 1.0f32 - alpha;
    // Seed with SMA of first `period` bars (standard convention)
    let mut val: f32 = prices[..period].iter().sum::<f32>() / period as f32;
    result[period - 1] = val;
    for i in period..n {
        val = alpha.mul_add(prices[i], beta * val);
        result[i] = val;
    }
    result
}

// =============================================================================
// RSI (Wilder)  —  SIMD gains/losses + scalar smoothing
// =============================================================================
pub fn rsi(close: &[f32], period: usize) -> Vec<f32> {
    let n = close.len();
    let mut result = vec![f32::NAN; n];
    if n <= period || period == 0 { return result; }

    // Hot path: SIMD-accelerated gains/losses
    let (gains, losses) = gains_losses(close);

    // Wilder smoothing (serial)
    let p = period as f32;
    let mut ag: f32 = gains[1..=period].iter().sum::<f32>() / p;
    let mut al: f32 = losses[1..=period].iter().sum::<f32>() / p;

    let rsi_of = |ag: f32, al: f32| -> f32 {
        if al < 1e-10 { 100.0 } else { 100.0 - 100.0 / (1.0 + ag / al) }
    };
    result[period] = rsi_of(ag, al);
    for i in (period + 1)..n {
        ag = (ag * (p - 1.0) + gains[i])  / p;
        al = (al * (p - 1.0) + losses[i]) / p;
        result[i] = rsi_of(ag, al);
    }
    result
}

pub fn gains_losses(close: &[f32]) -> (Vec<f32>, Vec<f32>) {
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") {
        return unsafe { gains_losses_avx2(close) };
    }
    gains_losses_scalar(close)
}

fn gains_losses_scalar(close: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let n = close.len();
    let mut gains  = vec![0.0f32; n];
    let mut losses = vec![0.0f32; n];
    for i in 1..n {
        let d = close[i] - close[i - 1];
        if d > 0.0 { gains[i] = d; } else { losses[i] = -d; }
    }
    (gains, losses)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn gains_losses_avx2(close: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let n = close.len();
    let mut gains  = vec![0.0f32; n];
    let mut losses = vec![0.0f32; n];

    let cp   = close.as_ptr();
    let gp   = gains.as_mut_ptr();
    let lp   = losses.as_mut_ptr();
    let zero = _mm256_setzero_ps();

    let mut i = 1usize;
    while i + 8 <= n {
        let c      = _mm256_loadu_ps(cp.add(i));
        let c_prev = _mm256_loadu_ps(cp.add(i - 1));
        let delta  = _mm256_sub_ps(c, c_prev);

        // gain  = max(delta,  0)   — branchless
        // loss  = max(-delta, 0)   — branchless
        let gain = _mm256_max_ps(delta, zero);
        let loss = _mm256_max_ps(_mm256_sub_ps(zero, delta), zero);

        _mm256_storeu_ps(gp.add(i), gain);
        _mm256_storeu_ps(lp.add(i), loss);
        i += 8;
    }
    while i < n {
        let d = close[i] - close[i - 1];
        if d > 0.0 { gains[i] = d; } else { losses[i] = -d; }
        i += 1;
    }
    (gains, losses)
}

// =============================================================================
// ATR (Wilder)  —  SIMD True Range + scalar smoothing
// =============================================================================
pub fn atr(high: &[f32], low: &[f32], close: &[f32], period: usize) -> Vec<f32> {
    let n = high.len();
    let mut result = vec![f32::NAN; n];
    if n <= period || period == 0 { return result; }

    // Hot path: SIMD-accelerated True Range
    let tr = true_range(high, low, close);

    // Wilder smoothing (serial)
    let p = period as f32;
    let mut val: f32 = tr[..period].iter().sum::<f32>() / p;
    result[period - 1] = val;
    for i in period..n {
        val = (val * (p - 1.0) + tr[i]) / p;
        result[i] = val;
    }
    result
}

// =============================================================================
// BOLLINGER BANDS  —  SIMD σ² + scalar SMA/sqrt
// =============================================================================
// σ²[i] = mean_i( (x[j] - SMA[i])² )
//
// Inner squared-deviation loop vectorised with AVX2 FMA-like mul+add:
//   vsubps   ymm_dev, ymm_x, ymm_mean
//   vmulps   ymm_sq,  ymm_dev, ymm_dev
//   vaddps   ymm_acc, ymm_acc, ymm_sq
pub fn bollinger(prices: &[f32], period: usize, mult: f32)
    -> (Vec<f32>, Vec<f32>, Vec<f32>)
{
    let middle = sma(prices, period);
    let n = prices.len();
    let mut upper = vec![f32::NAN; n];
    let mut lower = vec![f32::NAN; n];

    #[cfg(target_arch = "x86_64")]
    let use_simd = is_x86_feature_detected!("avx2");
    #[cfg(not(target_arch = "x86_64"))]
    let use_simd = false;

    for i in (period - 1)..n {
        let mean = middle[i];
        if mean.is_nan() { continue; }
        let slice = &prices[(i + 1 - period)..=i];
        let var = if use_simd {
            #[cfg(target_arch = "x86_64")]
            { (unsafe { sum_sq_dev_avx2(slice, mean) }) / period as f32 }
            #[cfg(not(target_arch = "x86_64"))]
            { sum_sq_dev_scalar(slice, mean) / period as f32 }
        } else {
            sum_sq_dev_scalar(slice, mean) / period as f32
        };
        let std = var.sqrt();
        upper[i] = mean + mult * std;
        lower[i] = mean - mult * std;
    }
    (upper, middle, lower)
}

fn sum_sq_dev_scalar(slice: &[f32], mean: f32) -> f32 {
    slice.iter().map(|&x| { let d = x - mean; d * d }).sum()
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn sum_sq_dev_avx2(slice: &[f32], mean: f32) -> f32 {
    let mean_v = _mm256_set1_ps(mean);
    let mut acc = _mm256_setzero_ps();
    let n = slice.len();
    let p = slice.as_ptr();
    let mut i = 0;

    while i + 8 <= n {
        let x   = _mm256_loadu_ps(p.add(i));
        let dev = _mm256_sub_ps(x, mean_v);
        acc = _mm256_add_ps(acc, _mm256_mul_ps(dev, dev));
        i += 8;
    }
    let mut total = hsum_avx2(acc);
    while i < n {
        let d = slice[i] - mean;
        total += d * d;
        i += 1;
    }
    total
}

// =============================================================================
// MACD  —  3 serial EMAs (no SIMD speedup; shown for completeness)
// =============================================================================
pub fn macd(prices: &[f32], fast: usize, slow: usize, signal_p: usize)
    -> (Vec<f32>, Vec<f32>, Vec<f32>)
{
    let ema_fast = ema(prices, fast);
    let ema_slow = ema(prices, slow);
    let n = prices.len();
    let mut macd_line = vec![f32::NAN; n];
    for i in (slow - 1)..n {
        macd_line[i] = ema_fast[i] - ema_slow[i];
    }
    // Signal EMA over the valid portion
    let sig_start = slow - 1;
    let valid: Vec<f32> = macd_line[sig_start..].to_vec();
    let sig_ema = ema(&valid, signal_p);
    let mut signal_line = vec![f32::NAN; n];
    for (j, &v) in sig_ema.iter().enumerate() {
        signal_line[sig_start + j] = v;
    }
    let mut histogram = vec![f32::NAN; n];
    for i in 0..n {
        if !macd_line[i].is_nan() && !signal_line[i].is_nan() {
            histogram[i] = macd_line[i] - signal_line[i];
        }
    }
    (macd_line, signal_line, histogram)
}


pub struct BatchResult {
    pub rsi14:       Vec<f32>,
    pub sma20:       Vec<f32>,   // = Bollinger middle band (SMA 20)
    pub sma50:       Vec<f32>,
    pub ema9:        Vec<f32>,
    pub ema21:       Vec<f32>,
    pub ema55:       Vec<f32>,
    pub macd:        Vec<f32>,   // MACD line           (index 7)
    pub macd_signal: Vec<f32>,   // signal EMA(9)       (index 8)
    pub macd_h:      Vec<f32>,   // histogram           (index 9)
    pub bb_pos:      Vec<f32>,   // Bollinger %B        (index 10)
    pub bb_upper:    Vec<f32>,   //                     (index 11)
    pub bb_lower:    Vec<f32>,   //                     (index 13)
    pub atr14:       Vec<f32>,
}

/// O(n) Bollinger Bands via running E[x²]−E[x]²: one pass, no inner loop.
pub fn bollinger_on(prices: &[f32], period: usize, mult: f32)
    -> (Vec<f32>, Vec<f32>, Vec<f32>)
{
    let n = prices.len();
    let mut upper  = vec![f32::NAN; n];
    let mut middle = vec![f32::NAN; n];
    let mut lower  = vec![f32::NAN; n];
    if n < period { return (upper, middle, lower); }

    let inv_p = 1.0_f32 / period as f32;
    let mut sum_x : f32 = prices[..period].iter().sum();
    let mut sum_x2: f32 = prices[..period].iter().map(|&x| x * x).sum();

    for i in (period - 1)..n {
        let mean = sum_x * inv_p;
        let var  = (sum_x2 * inv_p) - (mean * mean);
        let std  = var.max(0.0).sqrt(); // clamp away fp rounding negatives
        middle[i] = mean;
        upper[i]  = mean + mult * std;
        lower[i]  = mean - mult * std;
        if i + 1 < n {
            let old = prices[i + 1 - period];
            let new = prices[i + 1];
            sum_x  += new - old;
            sum_x2 += new * new - old * old;
        }
    }
    (upper, middle, lower)
}



// =============================================================================
// Choppiness Index — scalar (winner) vs AVX2-log batch
// CHOP(period) = 100 · ln(Σ ATR₁ / (HH − LL)) / ln(period)
//   ATR₁ = true range (single-bar), summed over `period` bars
//   HH   = highest high over `period` bars
//   LL   = lowest  low  over `period` bars
//
// The O(period) rolling-high/low scan dominates for both scalar and AVX2.
// The `ln()` call is the target of vectorisation: AVX2 processes 8 bars/cycle
// versus the platform libm scalar (~15–20 cycles per call).
// =============================================================================
pub fn choppiness_scalar(
    high:   &[f32],
    low:    &[f32],
    close:  &[f32],
    period: usize,
) -> Vec<f32> {
    let n = high.len();
    let inv_ln_p = 1.0f32 / (period as f32).ln();
    let mut result = vec![50.0f32; n];

    // True ranges
    let mut tr = vec![0.0f32; n];
    tr[0] = high[0] - low[0];
    for i in 1..n {
        tr[i] = (high[i] - low[i])
            .max((high[i] - close[i - 1]).abs())
            .max((low[i]  - close[i - 1]).abs());
    }

    // Sliding ATR sum via ring buffer; O(period) scan per bar for HH/LL
    let mut tr_buf = vec![0.0f32; period];
    let mut sum_tr = tr[..period].iter().sum::<f32>();
    for (k, v) in tr[..period].iter().enumerate() { tr_buf[k] = *v; }

    for i in (period - 1)..n {
        if i >= period {
            let slot = i % period;
            sum_tr = sum_tr - tr_buf[slot] + tr[i];
            tr_buf[slot] = tr[i];
        }
        let base = i + 1 - period;
        let mut hh = f32::NEG_INFINITY;
        let mut ll = f32::INFINITY;
        for j in base..=i {
            if high[j] > hh { hh = high[j]; }
            if low[j]  < ll { ll = low[j]; }
        }
        let range = hh - ll;
        if range > 1e-10 {
            result[i] = (100.0 * (sum_tr / range).ln() * inv_ln_p).clamp(0.0, 100.0);
        }
    }
    result
}

// ─────────────────────────────────────────────────────────────────────────────
// SmaState — O(1) per-tick Simple Moving Average (ring-buffer sum)
// ─────────────────────────────────────────────────────────────────────────────
/// Returns the SMA.  During warmup returns the running mean so far.
#[derive(Clone)]
pub struct SmaState {
    buf:    Vec<f32>,
    sx:     f32,
    idx:    usize,
    count:  usize,
    period: usize,
    inv_p:  f32,
}

impl SmaState {
    pub fn new(period: usize) -> Self {
        let p = period.max(1);
        Self { buf: vec![0.0; p], sx: 0.0, idx: 0, count: 0, period: p, inv_p: 1.0 / p as f32 }
    }

    #[inline]
    pub fn update(&mut self, close: f32) -> f32 {
        let old = self.buf[self.idx];
        self.sx += close - old;
        self.buf[self.idx] = close;
        self.idx = if self.idx + 1 == self.period { 0 } else { self.idx + 1 };
        if self.count < self.period { self.count += 1; }
        self.sx * if self.count < self.period { 1.0 / self.count as f32 } else { self.inv_p }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// EmaState — O(1) per-tick Exponential Moving Average
// ─────────────────────────────────────────────────────────────────────────────
/// Seeds with running mean for the first `period` bars, then applies k = 2/(p+1).
/// Returns the current EMA every tick.
#[derive(Clone)]
pub struct EmaState {
    ema:    f32,
    k:      f32,
    count:  usize,
    period: usize,
}

impl EmaState {
    pub fn new(period: usize) -> Self {
        let p = period.max(1);
        Self { ema: 0.0, k: 2.0 / (p as f32 + 1.0), count: 0, period: p }
    }

    #[inline]
    pub fn update(&mut self, close: f32) -> f32 {
        if self.count < self.period {
            self.count += 1;
            self.ema += (close - self.ema) / self.count as f32; // Welford running mean (SMA seed)
        } else {
            self.ema += self.k * (close - self.ema);
        }
        self.ema
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// RsiState — O(1) per-tick RSI (Wilder smoothing)
// ─────────────────────────────────────────────────────────────────────────────
/// Wilder-smoothed RSI.  α = 1/period.  Warmup: SMA of first `period` gains/losses.
/// Returns RSI in [0, 100].  Returns 50.0 during warmup.
#[derive(Clone)]
pub struct RsiState {
    avg_gain:   f32,
    avg_loss:   f32,
    prev_close: f32,
    count:      usize,
    period:     usize,
    inv_p:      f32,
}

impl RsiState {
    pub fn new(period: usize) -> Self {
        let p = period.max(1);
        Self { avg_gain: 0.0, avg_loss: 0.0, prev_close: 0.0, count: 0, period: p, inv_p: 1.0 / p as f32 }
    }

    #[inline]
    pub fn update(&mut self, close: f32) -> f32 {
        let diff  = close - self.prev_close;
        let gain  = diff.max(0.0);
        let loss  = (-diff).max(0.0);
        self.prev_close = close;
        self.count += 1;
        if self.count <= self.period {
            self.avg_gain += gain;
            self.avg_loss += loss;
            if self.count < self.period { return 50.0; }
            self.avg_gain *= self.inv_p;
            self.avg_loss *= self.inv_p;
        } else {
            let w = (self.period - 1) as f32;
            self.avg_gain = (self.avg_gain * w + gain) * self.inv_p;
            self.avg_loss = (self.avg_loss * w + loss) * self.inv_p;
            // Flush to zero before subnormal range to avoid FP-assist penalty
            // (~100 cycles/op on Intel when values fall below f32::MIN_POSITIVE).
            if self.avg_gain < 1e-30 { self.avg_gain = 0.0; }
            if self.avg_loss < 1e-30 { self.avg_loss = 0.0; }
        }
        if self.avg_loss < 1e-10 { return 100.0; }
        100.0 - 100.0 / (1.0 + self.avg_gain / self.avg_loss)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AtrState — O(1) per-tick ATR (Wilder smoothing)
// ─────────────────────────────────────────────────────────────────────────────
/// Wilder-smoothed ATR.  α = 1/period.  Warmup: SMA of first `period` true ranges.
/// Returns ATR.  Returns the raw TR during warmup.
#[derive(Clone)]
pub struct AtrState {
    atr:        f32,
    prev_close: f32,
    count:      usize,
    period:     usize,
    inv_p:      f32,
}

impl AtrState {
    pub fn new(period: usize) -> Self {
        let p = period.max(1);
        Self { atr: 0.0, prev_close: 0.0, count: 0, period: p, inv_p: 1.0 / p as f32 }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32) -> f32 {
        let tr = if self.count == 0 {
            high - low
        } else {
            (high - low)
                .max((high - self.prev_close).abs())
                .max((low  - self.prev_close).abs())
        };
        self.prev_close = close;
        self.count += 1;
        if self.count <= self.period {
            self.atr += tr;
            if self.count < self.period { return tr; }
            self.atr *= self.inv_p;
        } else {
            let w = (self.period - 1) as f32;
            self.atr = (self.atr * w + tr) * self.inv_p;
        }
        self.atr
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BollingerState — O(1) per-tick Bollinger Bands
// ─────────────────────────────────────────────────────────────────────────────
// Uses running E[x] and E[x²] ring-buffer (same technique as T9 in main.rs):
//   evict oldest sample, insert new sample, update sliding sums O(1),
//   then std = sqrt(E[x²] − E[x]²) — one sqrt per tick, no inner loop.
// ~2× faster than the batch bollinger_on because no Vec is allocated
// and the state stays in registers / L1 cache across all bars.
/// Returns (lower, middle, upper).  During warmup (count < period) returns (close, close, close).
#[derive(Clone)]
pub struct BollingerState {
    buf:    Vec<f32>,
    sx:     f32,   // Σ x
    sx2:    f32,   // Σ x²
    idx:    usize,
    count:  usize,
    period: usize,
    inv_p:  f32,   // 1/period, hoisted out of hot path
    mult:   f32,
}

impl BollingerState {
    pub fn new(period: usize, mult: f32) -> Self {
        let p = period.max(1);
        Self {
            buf: vec![0.0; p],
            sx: 0.0, sx2: 0.0, idx: 0, count: 0,
            period: p, inv_p: 1.0 / p as f32, mult,
        }
    }

    #[inline]
    pub fn update(&mut self, close: f32) -> (f32, f32, f32) {
        let old = self.buf[self.idx];
        self.sx  += close - old;
        self.sx2 += close * close - old * old;
        self.buf[self.idx] = close;
        self.idx = if self.idx + 1 == self.period { 0 } else { self.idx + 1 };
        if self.count < self.period { self.count += 1; }
        if self.count < self.period { return (close, close, close); }
        let mean = self.sx * self.inv_p;
        let std  = ((self.sx2 * self.inv_p) - mean * mean).max(0.0).sqrt();
        (mean - self.mult * std, mean, mean + self.mult * std)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AdxState — single Wilder ADX for one period
// ─────────────────────────────────────────────────────────────────────────────
/// Scalar Wilder ADX for a single period.  α = 1/period (correct Wilder smoothing).
#[derive(Clone)]
pub struct AdxState {
    smooth_tr:  f32,
    smooth_pdm: f32,
    smooth_mdm: f32,
    smooth_dx:  f32,
    alpha:      f32,
    prev_high:  f32,
    prev_low:   f32,
    prev_close: f32,
    seeded:     bool,
}

impl AdxState {
    pub fn new(period: usize) -> Self {
        Self {
            smooth_tr: 0.0, smooth_pdm: 0.0, smooth_mdm: 0.0, smooth_dx: 0.0,
            alpha: 1.0 / period as f32,
            prev_high: 0.0, prev_low: 0.0, prev_close: 0.0, seeded: false,
        }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32) -> f32 {
        if !self.seeded {
            self.prev_high = high; self.prev_low = low; self.prev_close = close;
            self.seeded = true;
            return 0.0;
        }
        let tr  = (high - low)
            .max((high - self.prev_close).abs())
            .max((low  - self.prev_close).abs());
        let up  = high - self.prev_high;
        let dn  = self.prev_low - low;
        let pdm = if up > dn && up > 0.0 { up } else { 0.0 };
        let mdm = if dn > up && dn > 0.0 { dn } else { 0.0 };
        self.smooth_tr  += self.alpha * (tr  - self.smooth_tr);
        self.smooth_pdm += self.alpha * (pdm - self.smooth_pdm);
        self.smooth_mdm += self.alpha * (mdm - self.smooth_mdm);
        let pdi = if self.smooth_tr > 0.0 { 100.0 * self.smooth_pdm / self.smooth_tr } else { 0.0 };
        let mdi = if self.smooth_tr > 0.0 { 100.0 * self.smooth_mdm / self.smooth_tr } else { 0.0 };
        let di_sum = pdi + mdi;
        let dx = if di_sum > 0.0 { 100.0 * (pdi - mdi).abs() / di_sum } else { 0.0 };
        self.smooth_dx += self.alpha * (dx - self.smooth_dx);
        self.prev_high = high; self.prev_low = low; self.prev_close = close;
        self.smooth_dx
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ChopState — per-tick Choppiness Index  (O(1) amortised per tick)
// ─────────────────────────────────────────────────────────────────────────────
// Three O(period) scans (ΣTR, max HH, min LL) are eliminated:
//   • ΣTR       → O(1) sliding sum: subtract evicted TR, add new TR
//   • max HH, min LL → O(period) scan over the ring buffer
//     For period ≤ ~64 this is faster than a monotone deque: LLVM auto-vectorises
//     the contiguous scan into vmaxps/vminps, beating ~8-12 deque ops at 4-5 cy each.
/// CHOP = 100 × ln(ΣTR / (HH − LL)) / ln(period), clamped [0,100].
/// High values (~62+) = choppy/ranging; low values (~38−) = trending.
#[derive(Clone)]
pub struct ChopState {
    tr_buf:    Vec<f32>,
    hi_buf:    Vec<f32>,
    lo_buf:    Vec<f32>,
    sum_tr:    f32,   // O(1) sliding TR sum
    bar:       usize,
    period:    usize,
    ln_period: f32,
    prev_close: f32,
    seeded:    bool,
}

impl ChopState {
    pub fn new(period: usize) -> Self {
        Self {
            tr_buf:    vec![0.0; period],
            hi_buf:    vec![0.0; period],
            lo_buf:    vec![0.0; period],
            sum_tr:    0.0,
            bar:       0,
            period,
            ln_period: (period as f32).ln(),
            prev_close: 0.0,
            seeded:    false,
        }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32) -> f32 {
        let tr = if self.seeded {
            (high - low)
                .max((high - self.prev_close).abs())
                .max((low  - self.prev_close).abs())
        } else { high - low };
        self.seeded = true;
        self.prev_close = close;

        let slot = self.bar % self.period;
        // Slide TR sum (tr_buf[slot] holds the value being evicted)
        self.sum_tr += tr - self.tr_buf[slot];

        // Write new bar
        self.tr_buf[slot] = tr;
        self.hi_buf[slot] = high;
        self.lo_buf[slot] = low;
        self.bar += 1;

        if self.bar < self.period { return 50.0; }

        // Single-pass dual-scan — LLVM vectorises with vmaxps+vminps on AVX2
        let (mut hh, mut ll) = (f32::NEG_INFINITY, f32::INFINITY);
        for (&h, &l) in self.hi_buf.iter().zip(self.lo_buf.iter()) {
            if h > hh { hh = h; }
            if l < ll { ll = l; }
        }
        let range = hh - ll;
        if range < 1e-10 || self.sum_tr < 1e-10 { return 50.0; }
        // rcpss replaces divss (~13 cy) with 4-cy reciprocal; error < 0.005 CHOP units
        #[cfg(target_arch = "x86_64")]
        let ratio = unsafe { self.sum_tr * _mm_cvtss_f32(_mm_rcp_ss(_mm_set_ss(range))) };
        #[cfg(not(target_arch = "x86_64"))]
        let ratio = self.sum_tr / range;
        (100.0 * fast_ln_f32(ratio) / self.ln_period).clamp(0.0, 100.0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// FvgState — Fair Value Gap (3-candle pattern)
// ─────────────────────────────────────────────────────────────────────────────
/// Returns 1.0 bullish FVG, -1.0 bearish FVG, 0.0 none.
/// Bullish: current_low > high_2_bars_ago.  Bearish: current_high < low_2_bars_ago.
#[derive(Clone)]
pub struct FvgState {
    hi:    [f32; 3],
    lo:    [f32; 3],
    pos:   usize,
    count: usize,
}

impl FvgState {
    pub fn new() -> Self { Self { hi: [0.0; 3], lo: [0.0; 3], pos: 0, count: 0 } }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32) -> f32 {
        self.hi[self.pos] = high;
        self.lo[self.pos] = low;
        // (pos+1)%3 is the oldest slot = 2 bars ago once we have 3 bars
        let old = (self.pos + 1) % 3;
        let result = if self.count >= 2 {
            let h_old = self.hi[old];
            let l_old = self.lo[old];
            if  low > h_old { 1.0 } else if high < l_old { -1.0 } else { 0.0 }
        } else { 0.0 };
        self.pos = (self.pos + 1) % 3;
        if self.count < 2 { self.count += 1; }
        result
    }
}

impl Default for FvgState { fn default() -> Self { Self::new() } }

// ─────────────────────────────────────────────────────────────────────────────
// VwapState — cumulative VWAP
// ─────────────────────────────────────────────────────────────────────────────
/// Cumulative VWAP = Σ(TP × Vol) / Σ(Vol).  f64 accumulators for precision.
#[derive(Clone)]
pub struct VwapState {
    cum_pv:  f64,
    cum_vol: f64,
}

impl VwapState {
    pub fn new() -> Self { Self { cum_pv: 0.0, cum_vol: 0.0 } }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32, volume: f32) -> f32 {
        let tp = (high as f64 + low as f64 + close as f64) / 3.0;
        self.cum_vol += volume as f64;
        self.cum_pv  += tp * volume as f64;
        if self.cum_vol > 0.0 { (self.cum_pv / self.cum_vol) as f32 } else { close }
    }

    pub fn reset(&mut self) { self.cum_pv = 0.0; self.cum_vol = 0.0; }
}

impl Default for VwapState { fn default() -> Self { Self::new() } }

// ─────────────────────────────────────────────────────────────────────────────
// RocState — Rate of Change
// ─────────────────────────────────────────────────────────────────────────────
/// ROC(period) = (close − close[period_bars_ago]) / close[period_bars_ago] × 100.
#[derive(Clone)]
pub struct RocState {
    buf:    Vec<f32>,
    idx:    usize,
    count:  usize,
    period: usize,
}

impl RocState {
    pub fn new(period: usize) -> Self {
        Self { buf: vec![0.0; period + 1], idx: 0, count: 0, period }
    }

    #[inline]
    pub fn update(&mut self, close: f32) -> f32 {
        self.buf[self.idx] = close;
        let roc = if self.count >= self.period {
            let old = self.buf[(self.idx + 1) % self.buf.len()];
            if old > 0.0 { (close - old) / old * 100.0 } else { 0.0 }
        } else { 0.0 };
        self.idx = (self.idx + 1) % self.buf.len();
        if self.count < self.buf.len() { self.count += 1; }
        roc
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CciState — Commodity Channel Index
// ─────────────────────────────────────────────────────────────────────────────
/// CCI(period) = (TP − SMA(TP)) / (0.015 × MAD).
/// O(1) SMA via running sum; O(period) MAD scan per tick.
#[derive(Clone)]
pub struct CciState {
    buf:    Vec<f32>,
    idx:    usize,
    count:  usize,
    period: usize,
    sum_tp: f32,
}

impl CciState {
    pub fn new(period: usize) -> Self {
        Self { buf: vec![0.0; period], idx: 0, count: 0, period, sum_tp: 0.0 }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32) -> f32 {
        let tp = (high + low + close) / 3.0;
        self.sum_tp += tp - self.buf[self.idx];
        self.buf[self.idx] = tp;
        self.idx = (self.idx + 1) % self.period;
        if self.count < self.period { self.count += 1; }
        if self.count < self.period { return 0.0; }
        let sma = self.sum_tp / self.period as f32;
        let mad: f32 = self.buf.iter().map(|&x| (x - sma).abs()).sum::<f32>()
                       / self.period as f32;
        if mad < 1e-10 { return 0.0; }
        (tp - sma) / (0.015 * mad)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MfiState — Money Flow Index  (O(1) sliding sums)
// ─────────────────────────────────────────────────────────────────────────────
/// MFI(period).  O(1): slides pos/neg money-flow sums rather than rescanning.
#[derive(Clone)]
pub struct MfiState {
    pos_buf: Vec<f32>,
    neg_buf: Vec<f32>,
    sum_pos: f32,
    sum_neg: f32,
    idx:     usize,
    count:   usize,
    period:  usize,
    prev_tp: f32,
    seeded:  bool,
}

impl MfiState {
    pub fn new(period: usize) -> Self {
        Self {
            pos_buf: vec![0.0; period], neg_buf: vec![0.0; period],
            sum_pos: 0.0, sum_neg: 0.0, idx: 0, count: 0, period,
            prev_tp: 0.0, seeded: false,
        }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32, volume: f32) -> f32 {
        let tp  = (high + low + close) / 3.0;
        let rmf = tp * volume;
        let (pos, neg) = if !self.seeded {
            (0.0, 0.0)
        } else if tp > self.prev_tp {
            (rmf, 0.0)
        } else if tp < self.prev_tp {
            (0.0, rmf)
        } else {
            (0.0, 0.0)
        };
        self.prev_tp = tp;
        self.seeded = true;
        self.sum_pos += pos - self.pos_buf[self.idx];
        self.sum_neg += neg - self.neg_buf[self.idx];
        self.pos_buf[self.idx] = pos;
        self.neg_buf[self.idx] = neg;
        self.idx = (self.idx + 1) % self.period;
        if self.count < self.period { self.count += 1; }
        if self.count < self.period { return 50.0; }
        if self.sum_neg < 1e-10 { return 100.0; }
        100.0 - 100.0 / (1.0 + self.sum_pos / self.sum_neg)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// EffRatioState — Kaufman Efficiency Ratio (TREND_EFF)  (O(1) per tick)
// ─────────────────────────────────────────────────────────────────────────────
// The O(period) path-sum loop is replaced by O(1) sliding maintenance:
//   • add  new edge : |close      − prev_close|
//   • drop old edge : |new_oldest − old_oldest|   (the pair being evicted)
/// ER = |close − close[period_bars_ago]| / Σ|close[i] − close[i−1]|.
/// 1.0 = perfectly trending, 0.0 = totally choppy/random.
#[derive(Clone)]
pub struct EffRatioState {
    buf:        Vec<f32>,   // ring buffer length period+1
    sum_path:   f32,        // O(1) sliding Σ|Δclose|
    idx:        usize,
    count:      usize,
    period:     usize,
    prev_close: f32,
}

impl EffRatioState {
    pub fn new(period: usize) -> Self {
        Self { buf: vec![0.0; period + 1], sum_path: 0.0, idx: 0, count: 0, period, prev_close: 0.0 }
    }

    #[inline]
    pub fn update(&mut self, close: f32) -> f32 {
        let len = self.buf.len();
        let next_idx = if self.idx + 1 == len { 0 } else { self.idx + 1 };
        // Capture values about to be evicted (read BEFORE write)
        let old_oldest = self.buf[self.idx];
        let new_oldest = self.buf[next_idx];

        // Add new edge; remove oldest edge once the buffer is full
        if self.count > 0 {
            self.sum_path += (close - self.prev_close).abs();
        }
        if self.count >= len {
            let drop = (new_oldest - old_oldest).abs();
            self.sum_path -= drop;
            if self.sum_path < 0.0 { self.sum_path = 0.0; } // guard FP drift
        }

        self.buf[self.idx] = close;
        self.idx = next_idx;
        if self.count < len { self.count += 1; }
        self.prev_close = close;

        if self.count < len { return 0.0; }

        let oldest = self.buf[self.idx]; // idx now points at oldest slot
        let net    = (close - oldest).abs();
        if self.sum_path < 1e-10 { return 0.0; }
        // rcpss replaces divss (~13 cy); error ~1.5e-4 << 1e-3 cross-test tolerance
        #[cfg(target_arch = "x86_64")]
        return unsafe {
            (net * _mm_cvtss_f32(_mm_rcp_ss(_mm_set_ss(self.sum_path)))).clamp(0.0, 1.0)
        };
        #[cfg(not(target_arch = "x86_64"))]
        (net / self.sum_path).clamp(0.0, 1.0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CtiState — Choppiness + Trend Efficiency + Z-scores
// ─────────────────────────────────────────────────────────────────────────────
/// CTI: combines Choppiness Index and Kaufman Efficiency Ratio, both
/// normalised to Z-scores over a rolling window.
/// Returns (chop, trend_eff, chop_z, trend_z).  Negative chop_z = trending.
#[derive(Clone)]
pub struct CtiState {
    chop:         ChopState,
    eff:          EffRatioState,
    chop_buf:     Vec<f32>,
    eff_buf:      Vec<f32>,
    z_idx:        usize,
    z_count:      usize,
    z_window:     usize,
    inv_z_window: f32,   // precomputed 1/z_window — avoids 4 divisions per tick
    chop_sum:     f32,
    chop_sum2:    f32,
    eff_sum:      f32,
    eff_sum2:     f32,
}

impl CtiState {
    pub fn new(period: usize, z_window: usize) -> Self {
        Self {
            chop: ChopState::new(period),
            eff:  EffRatioState::new(period),
            chop_buf: vec![0.0; z_window],
            eff_buf:  vec![0.0; z_window],
            z_idx: 0, z_count: 0, z_window,
            inv_z_window: 1.0 / z_window as f32,
            chop_sum: 0.0, chop_sum2: 0.0,
            eff_sum:  0.0, eff_sum2:  0.0,
        }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32) -> (f32, f32, f32, f32) {
        let chop = self.chop.update(high, low, close);
        let eff  = self.eff.update(close);
        let old_chop = self.chop_buf[self.z_idx];
        let old_eff  = self.eff_buf[self.z_idx];
        self.chop_sum  += chop - old_chop;
        self.chop_sum2 += chop * chop - old_chop * old_chop;
        self.eff_sum   += eff  - old_eff;
        self.eff_sum2  += eff  * eff  - old_eff  * old_eff;
        self.chop_buf[self.z_idx] = chop;
        self.eff_buf[self.z_idx]  = eff;
        self.z_idx = if self.z_idx + 1 == self.z_window { 0 } else { self.z_idx + 1 };
        if self.z_count < self.z_window { self.z_count += 1; }
        if self.z_count < self.z_window { return (chop, eff, 0.0, 0.0); }
        let inv_n = self.inv_z_window;
        let cm = self.chop_sum * inv_n;
        let cv = (self.chop_sum2 * inv_n - cm * cm).max(0.0);
        let cz = if cv > 1e-10 {
            // rsqrt replaces sqrt+divss (~25 cy) with 4-cy reciprocal sqrt
            #[cfg(target_arch = "x86_64")]
            { unsafe { (chop - cm) * _mm_cvtss_f32(_mm_rsqrt_ss(_mm_set_ss(cv))) } }
            #[cfg(not(target_arch = "x86_64"))]
            { (chop - cm) / cv.sqrt() }
        } else { 0.0 };
        let em = self.eff_sum  * inv_n;
        let ev = (self.eff_sum2 * inv_n - em * em).max(0.0);
        let ez = if ev > 1e-10 {
            #[cfg(target_arch = "x86_64")]
            { unsafe { (eff - em) * _mm_cvtss_f32(_mm_rsqrt_ss(_mm_set_ss(ev))) } }
            #[cfg(not(target_arch = "x86_64"))]
            { (eff - em) / ev.sqrt() }
        } else { 0.0 };
        (chop, eff, cz, ez)
    }
}

impl Default for CtiState { fn default() -> Self { Self::new(14, 50) } }

// ─────────────────────────────────────────────────────────────────────────────
// StochKState — Stochastic Fast %K
// ─────────────────────────────────────────────────────────────────────────────
/// Stochastic Fast %K: (close - LL) / (HH - LL) × 100 over a rolling window.
/// Returns 50.0 during warmup (bar < period - 1).
/// tick[i] ≡ stoch_k[i] for i >= period - 1.
#[derive(Clone)]
pub struct StochKState {
    hi_buf: Vec<f32>,
    lo_buf: Vec<f32>,
    idx:    usize,
    count:  usize,
    period: usize,
}

impl StochKState {
    pub fn new(period: usize) -> Self {
        let p = period.max(1);
        Self {
            hi_buf: vec![f32::NEG_INFINITY; p],
            lo_buf: vec![f32::INFINITY; p],
            idx:    0,
            count:  0,
            period: p,
        }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32) -> f32 {
        self.hi_buf[self.idx] = high;
        self.lo_buf[self.idx] = low;
        self.idx = if self.idx + 1 == self.period { 0 } else { self.idx + 1 };
        self.count += 1;
        if self.count < self.period { return 50.0; }
        // O(period) dual-scan — single pass; LLVM emits vmaxps+vminps with AVX2
        let (mut hh, mut ll) = (f32::NEG_INFINITY, f32::INFINITY);
        for (&h, &l) in self.hi_buf.iter().zip(self.lo_buf.iter()) {
            if h > hh { hh = h; }
            if l < ll { ll = l; }
        }
        let range = hh - ll;
        if range == 0.0 { 50.0 } else { (close - ll) / range * 100.0 }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ObvState — On-Balance Volume
// ─────────────────────────────────────────────────────────────────────────────
/// Cumulative OBV. First call returns 0.0.  tick[i] ≡ obv[i] for all i.
#[derive(Clone)]
pub struct ObvState {
    obv:        f32,
    prev_close: f32,
    seeded:     bool,
}

impl ObvState {
    pub fn new() -> Self { Self { obv: 0.0, prev_close: 0.0, seeded: false } }

    #[inline]
    pub fn update(&mut self, close: f32, volume: f32) -> f32 {
        if !self.seeded {
            self.prev_close = close;
            self.seeded = true;
            return 0.0;
        }
        if      close > self.prev_close { self.obv += volume; }
        else if close < self.prev_close { self.obv -= volume; }
        self.prev_close = close;
        self.obv
    }
}

impl Default for ObvState { fn default() -> Self { Self::new() } }

// ─────────────────────────────────────────────────────────────────────────────
// CmfState — Chaikin Money Flow
// ─────────────────────────────────────────────────────────────────────────────
/// Sliding CMF = Σ(MFV) / Σ(volume) over a rolling window.
/// Returns 0.0 during warmup (bar < period - 1).
/// tick[i] ≡ cmf[i] for i >= period - 1.
#[derive(Clone)]
pub struct CmfState {
    mfv_buf: Vec<f32>,
    vol_buf: Vec<f32>,
    sum_mfv: f32,
    sum_vol: f32,
    idx:     usize,
    count:   usize,
    period:  usize,
}

impl CmfState {
    pub fn new(period: usize) -> Self {
        let p = period.max(1);
        Self {
            mfv_buf: vec![0.0; p],
            vol_buf: vec![0.0; p],
            sum_mfv: 0.0,
            sum_vol: 0.0,
            idx:     0,
            count:   0,
            period:  p,
        }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32, volume: f32) -> f32 {
        let hl  = high - low;
        let clv = if hl == 0.0 { 0.0 } else { ((close - low) - (high - close)) / hl };
        let mfv = clv * volume;

        self.sum_mfv += mfv      - self.mfv_buf[self.idx];
        self.sum_vol += volume   - self.vol_buf[self.idx];
        self.mfv_buf[self.idx] = mfv;
        self.vol_buf[self.idx] = volume;
        self.idx = (self.idx + 1) % self.period;
        self.count += 1;

        if self.count < self.period { return 0.0; }
        if self.sum_vol == 0.0 { 0.0 } else { self.sum_mfv / self.sum_vol }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// FibState — Fibonacci retracement levels from rolling HH/LL
// ─────────────────────────────────────────────────────────────────────────────
/// Rolling Fibonacci 38.2 % and 61.8 % retracement levels.
/// Returns (NAN, NAN) during warmup (bar < lookback - 1).
/// tick[i] ≡ fibonacci_levels[i] for i >= lookback - 1.
#[derive(Clone)]
pub struct FibState {
    hi_buf:   Vec<f32>,
    lo_buf:   Vec<f32>,
    idx:      usize,
    count:    usize,
    lookback: usize,
}

impl FibState {
    pub fn new(lookback: usize) -> Self {
        let lb = lookback.max(1);
        Self {
            hi_buf:   vec![f32::NEG_INFINITY; lb],
            lo_buf:   vec![f32::INFINITY; lb],
            idx:      0,
            count:    0,
            lookback: lb,
        }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32) -> (f32, f32) {
        self.hi_buf[self.idx] = high;
        self.lo_buf[self.idx] = low;
        self.idx = if self.idx + 1 == self.lookback { 0 } else { self.idx + 1 };
        self.count += 1;
        if self.count < self.lookback { return (f32::NAN, f32::NAN); }
        // O(lookback) dual-scan — single pass; LLVM emits vmaxps+vminps with AVX2
        let (mut hh, mut ll) = (f32::NEG_INFINITY, f32::INFINITY);
        for (&h, &l) in self.hi_buf.iter().zip(self.lo_buf.iter()) {
            if h > hh { hh = h; }
            if l < ll { ll = l; }
        }
        let range = hh - ll;
        (ll + range * 0.382, ll + range * 0.618)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// WilliamsRState — Williams %R
// ─────────────────────────────────────────────────────────────────────────────
/// Williams %R = (HH - close) / (HH - LL) × -100.  Range [-100, 0].
/// Returns -50.0 during warmup (bar < period - 1).
/// tick[i] ≡ williams_r[i] for i >= period - 1.
/// Internally wraps StochKState: WilliamsR = StochK - 100.
#[derive(Clone)]
pub struct WilliamsRState(StochKState);

impl WilliamsRState {
    pub fn new(period: usize) -> Self { Self(StochKState::new(period)) }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32) -> f32 {
        self.0.update(high, low, close) - 100.0
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ParabolicSarState — Parabolic SAR
// ─────────────────────────────────────────────────────────────────────────────
/// Trend-following stop-and-reverse indicator.
/// Initialises as uptrend.  tick[i] ≡ parabolic_sar[i] for all i.
#[derive(Clone)]
pub struct ParabolicSarState {
    sar:    f32,
    ep:     f32,
    af:     f32,
    step:   f32,
    max_af: f32,
    trend:  i8,          // 1 = uptrend, -1 = downtrend
    hi_prev: [f32; 2],   // [most recent, one before]
    lo_prev: [f32; 2],
    seeded: bool,
}

impl ParabolicSarState {
    pub fn new(step: f32, max_af: f32) -> Self {
        Self {
            sar: 0.0, ep: 0.0, af: step, step, max_af,
            trend: 1,
            hi_prev: [0.0; 2], lo_prev: [0.0; 2],
            seeded: false,
        }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32) -> f32 {
        if !self.seeded {
            self.sar    = low;
            self.ep     = high;
            self.af     = self.step;
            self.hi_prev = [high; 2];
            self.lo_prev = [low; 2];
            self.seeded = true;
            return low;
        }

        // Candidate SAR using the two bars before the current one
        let s = self.sar + self.af * (self.ep - self.sar);
        let new_sar = if self.trend == 1 {
            s.min(self.lo_prev[0]).min(self.lo_prev[1])
        } else {
            s.max(self.hi_prev[0]).max(self.hi_prev[1])
        };

        if self.trend == 1 && low <= new_sar {
            self.trend = -1; self.sar = self.ep; self.ep = low;  self.af = self.step;
        } else if self.trend == -1 && high >= new_sar {
            self.trend =  1; self.sar = self.ep; self.ep = high; self.af = self.step;
        } else {
            self.sar = new_sar;
            if self.trend == 1 && high > self.ep {
                self.ep = high; self.af = (self.af + self.step).min(self.max_af);
            } else if self.trend == -1 && low < self.ep {
                self.ep = low;  self.af = (self.af + self.step).min(self.max_af);
            }
        }

        // Shift prev buffers so next call sees current bar as [0]
        self.hi_prev[1] = self.hi_prev[0]; self.hi_prev[0] = high;
        self.lo_prev[1] = self.lo_prev[0]; self.lo_prev[0] = low;

        self.sar
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BosState — Break of Structure
// ─────────────────────────────────────────────────────────────────────────────
/// Fires 1.0 on a bullish BOS (close breaks above last confirmed swing high),
/// -1.0 on bearish BOS, 0.0 otherwise.
/// Each level only fires once; reset when a new swing at that side is confirmed.
/// Returns 0.0 during warmup (bar < left + right + 1).
/// tick[i] ≡ bos[i] for i >= left + right.
#[derive(Clone)]
pub struct BosState {
    hi_buf:    Vec<f32>,
    lo_buf:    Vec<f32>,
    bar:       usize,
    slot:      usize,   // = bar % size, tracked branchlessly
    left:      usize,
    right:     usize,
    size:      usize,    // left + right + 1
    last_sh:   f32,
    last_sl:   f32,
    sh_broken: bool,
    sl_broken: bool,
}

impl BosState {
    pub fn new(left: usize, right: usize) -> Self {
        let size = left + right + 1;
        Self {
            hi_buf: vec![0.0; size], lo_buf: vec![0.0; size],
            bar: 0, slot: 0, left, right, size,
            last_sh: f32::NEG_INFINITY, last_sl: f32::INFINITY,
            sh_broken: false, sl_broken: false,
        }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32) -> f32 {
        let slot = self.slot;
        self.hi_buf[slot] = high;
        self.lo_buf[slot] = low;
        self.slot = if slot + 1 == self.size { 0 } else { slot + 1 };
        self.bar += 1;

        if self.bar < self.size { return 0.0; }

        // Pivot candidate: bar `right` steps behind current slot — branchless, no % non-pow2.
        let pivot_slot = if slot >= self.right { slot - self.right } else { slot + self.left + 1 };
        let ph = self.hi_buf[pivot_slot];
        let pl = self.lo_buf[pivot_slot];

        // is_ph ↔ ph is the maximum of ALL hi_buf values (all left+right neighbours ≤ ph).
        // is_pl ↔ pl is the minimum of ALL lo_buf values (all neighbours ≥ pl).
        // Single-pass scan: LLVM emits vmaxps+vminps with AVX2 for the size=11 window.
        let (mut hh, mut ll) = (f32::NEG_INFINITY, f32::INFINITY);
        for (&h, &l) in self.hi_buf.iter().zip(self.lo_buf.iter()) {
            if h > hh { hh = h; }
            if l < ll { ll = l; }
        }
        let is_ph = ph >= hh;   // true iff ph == hh (pivot is the peak)
        let is_pl = pl <= ll;   // true iff pl == ll (pivot is the trough)

        if is_ph { self.last_sh = ph; self.sh_broken = false; }
        if is_pl { self.last_sl = pl; self.sl_broken = false; }

        if close > self.last_sh && !self.sh_broken {
            self.sh_broken = true;  return  1.0;
        } else if close < self.last_sl && !self.sl_broken {
            self.sl_broken = true;  return -1.0;
        }
        0.0
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ChochState — Change of Character
// ─────────────────────────────────────────────────────────────────────────────
/// CHoCH = first BOS that reverses the prevailing market bias.
/// Returns 1.0 bullish CHoCH, -1.0 bearish CHoCH, 0.0 otherwise.
/// tick[i] ≡ choch[i] for i >= left + right.
#[derive(Clone)]
pub struct ChochState {
    bos:  BosState,
    bias: i8,   // -1 bearish, 0 none, 1 bullish
}

impl ChochState {
    pub fn new(left: usize, right: usize) -> Self {
        Self { bos: BosState::new(left, right), bias: 0 }
    }

    #[inline]
    pub fn update(&mut self, high: f32, low: f32, close: f32) -> f32 {
        let sig = self.bos.update(high, low, close);
        if sig == 1.0 {
            let choch = if self.bias == -1 { 1.0 } else { 0.0 };
            self.bias = 1;
            choch
        } else if sig == -1.0 {
            let choch = if self.bias == 1 { -1.0 } else { 0.0 };
            self.bias = -1;
            choch
        } else { 0.0 }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// StochRsiState — Stochastic RSI
// ─────────────────────────────────────────────────────────────────────────────
/// Embeds a Wilder RSI state, then applies StochK logic to the RSI ring buffer.
/// Returns (%K, %D) where %D = 3-period SMA of %K, both in [0, 100].
/// (NAN, NAN) during warmup.
/// tick[i] matches stoch_rsi batch[i] within float tolerance.
#[derive(Clone)]
pub struct StochRsiState {
    // ── RSI (Wilder) ────────────────────────────────────────────────────────
    avg_gain:   f32,
    avg_loss:   f32,
    prev_close: f32,
    rsi_alpha:  f32,       // = 1 / rsi_period
    rsi_period: usize,
    seed_gain:  f32,       // accumulated during seeding phase
    seed_loss:  f32,
    rsi_bar:    usize,     // call counter (1-indexed after first call)
    // ── StochRSI ────────────────────────────────────────────────────────────
    rsi_buf:      Vec<f32>,    // ring buffer of RSI values
    rsi_idx:      usize,       // next write slot
    rsi_count:    usize,       // RSI values fed so far
    stoch_period: usize,
    // ── %D: 3-period SMA of %K ───────────────────────────────────────────
    k_buf:  [f32; 3],
    k_idx:  usize,
    k_count: usize,
}

impl StochRsiState {
    pub fn new(rsi_period: usize, stoch_period: usize) -> Self {
        let sp = stoch_period.max(1);
        Self {
            avg_gain: 0.0, avg_loss: 0.0, prev_close: 0.0,
            rsi_alpha: 1.0 / rsi_period as f32,
            rsi_period,
            seed_gain: 0.0, seed_loss: 0.0,
            rsi_bar: 0,
            rsi_buf:      vec![0.0; sp],
            rsi_idx:      0,
            rsi_count:    0,
            stoch_period: sp,
            k_buf: [0.0; 3], k_idx: 0, k_count: 0,
        }
    }

    #[inline]
    pub fn update(&mut self, close: f32) -> (f32, f32) {
        self.rsi_bar += 1;

        // First call: seed prev_close, no output
        if self.rsi_bar == 1 {
            self.prev_close = close;
            return (f32::NAN, f32::NAN);
        }

        let d = close - self.prev_close;
        self.prev_close = close;
        let g = if d > 0.0 { d } else { 0.0 };
        let l = if d < 0.0 { -d } else { 0.0 };

        let rsi_val: f32;
        if self.rsi_bar <= self.rsi_period + 1 {
            // Seeding: accumulate bars 2..=rsi_period+1 (= differences 1..=rsi_period)
            self.seed_gain += g;
            self.seed_loss += l;
            if self.rsi_bar < self.rsi_period + 1 { return (f32::NAN, f32::NAN); }
            // rsi_bar == rsi_period + 1: finalise seeding → matches batch rsi[rsi_period]
            self.avg_gain = self.seed_gain * self.rsi_alpha;
            self.avg_loss = self.seed_loss * self.rsi_alpha;
            rsi_val = if self.avg_loss == 0.0 { 100.0 }
                      else { 100.0 - 100.0 / (1.0 + self.avg_gain / self.avg_loss) };
        } else {
            let a = self.rsi_alpha;
            let b = 1.0 - a;
            self.avg_gain = b * self.avg_gain + a * g;
            self.avg_loss = b * self.avg_loss + a * l;
            rsi_val = if self.avg_loss == 0.0 { 100.0 }
                      else { 100.0 - 100.0 / (1.0 + self.avg_gain / self.avg_loss) };
        }

        // O(stoch_period) scan — write RSI to ring buffer then find HH/LL
        self.rsi_buf[self.rsi_idx] = rsi_val;
        self.rsi_idx = if self.rsi_idx + 1 == self.stoch_period { 0 } else { self.rsi_idx + 1 };
        self.rsi_count += 1;

        if self.rsi_count < self.stoch_period { return (f32::NAN, f32::NAN); }

        let (mut hh, mut ll) = (f32::NEG_INFINITY, f32::INFINITY);
        for &v in &self.rsi_buf {
            if v > hh { hh = v; }
            if v < ll { ll = v; }
        }
        let range = hh - ll;
        let k = if range < 1e-10 { 50.0 } else { (rsi_val - ll) / range * 100.0 };

        // %D: 3-period SMA of %K
        self.k_buf[self.k_idx] = k;
        self.k_idx = (self.k_idx + 1) % 3;
        self.k_count += 1;
        let d_line = if self.k_count >= 3 {
            (self.k_buf[0] + self.k_buf[1] + self.k_buf[2]) / 3.0
        } else { f32::NAN };

        (k, d_line)
    }
}


// =============================================================================
// LIVE TRADING INFRASTRUCTURE
// =============================================================================

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::collections::HashMap;
use std::sync::Arc as StdArc;

/// Maximum bars to keep in ring buffer (powers of 2 for fast modulo)
pub const RING_BUFFER_SIZE: usize = 1024;
pub const RING_BUFFER_MASK: usize = RING_BUFFER_SIZE - 1;

// ─────────────────────────────────────────────────────────────────────────────
// OhlcvRingBuffer — lock-free OHLCV storage
// ─────────────────────────────────────────────────────────────────────────────
#[repr(align(64))]
pub struct OhlcvRingBuffer {
    sequence:  AtomicU64,
    write_pos: AtomicUsize,
    bar_count: AtomicUsize,
    pub timestamps: Box<[i64; RING_BUFFER_SIZE]>,
    pub open:       Box<[f32; RING_BUFFER_SIZE]>,
    pub high:       Box<[f32; RING_BUFFER_SIZE]>,
    pub low:        Box<[f32; RING_BUFFER_SIZE]>,
    pub close:      Box<[f32; RING_BUFFER_SIZE]>,
    pub volume:     Box<[f32; RING_BUFFER_SIZE]>,
}

impl OhlcvRingBuffer {
    pub fn new() -> Self {
        Self {
            sequence:   AtomicU64::new(0),
            write_pos:  AtomicUsize::new(0),
            bar_count:  AtomicUsize::new(0),
            timestamps: Box::new([0i64; RING_BUFFER_SIZE]),
            open:       Box::new([0.0f32; RING_BUFFER_SIZE]),
            high:       Box::new([0.0f32; RING_BUFFER_SIZE]),
            low:        Box::new([0.0f32; RING_BUFFER_SIZE]),
            close:      Box::new([0.0f32; RING_BUFFER_SIZE]),
            volume:     Box::new([0.0f32; RING_BUFFER_SIZE]),
        }
    }

    #[inline(always)]
    pub fn sequence(&self) -> u64 { self.sequence.load(Ordering::Acquire) }

    #[inline(always)]
    pub fn len(&self) -> usize { self.bar_count.load(Ordering::Acquire).min(RING_BUFFER_SIZE) }

    #[inline(always)]
    pub fn is_empty(&self) -> bool { self.len() == 0 }

    #[inline]
    pub fn push(&mut self, ts: i64, o: f32, h: f32, l: f32, c: f32, v: f32) {
        self.sequence.fetch_add(1, Ordering::Release);
        let pos = self.write_pos.load(Ordering::Relaxed);
        let idx = pos & RING_BUFFER_MASK;
        self.timestamps[idx] = ts;
        self.open[idx] = o;
        self.high[idx] = h;
        self.low[idx] = l;
        self.close[idx] = c;
        self.volume[idx] = v;
        self.write_pos.store(pos + 1, Ordering::Release);
        let old_count = self.bar_count.fetch_add(1, Ordering::Release);
        if old_count >= RING_BUFFER_SIZE {
            self.bar_count.store(RING_BUFFER_SIZE, Ordering::Release);
        }
        self.sequence.fetch_add(1, Ordering::Release);
    }

    #[inline]
    pub fn update_last(&mut self, h: f32, l: f32, c: f32, v: f32) {
        if self.bar_count.load(Ordering::Relaxed) == 0 { return; }
        self.sequence.fetch_add(1, Ordering::Release);
        let pos = self.write_pos.load(Ordering::Relaxed);
        let idx = (pos.saturating_sub(1)) & RING_BUFFER_MASK;
        if h > self.high[idx] { self.high[idx] = h; }
        if l < self.low[idx]  { self.low[idx]  = l; }
        self.close[idx]  = c;
        self.volume[idx] = v;
        self.sequence.fetch_add(1, Ordering::Release);
    }

    #[inline(always)]
    pub fn get(&self, logical_idx: usize) -> Option<(i64, f32, f32, f32, f32, f32)> {
        let len = self.len();
        if logical_idx >= len { return None; }
        let write_pos = self.write_pos.load(Ordering::Acquire);
        let start = if write_pos >= len { write_pos - len } else { 0 };
        let phys = (start + logical_idx) & RING_BUFFER_MASK;
        Some((self.timestamps[phys], self.open[phys], self.high[phys],
              self.low[phys], self.close[phys], self.volume[phys]))
    }

    #[inline(always)]
    pub fn close_at(&self, logical_idx: usize) -> f32 {
        let len = self.len();
        if logical_idx >= len { return 0.0; }
        let write_pos = self.write_pos.load(Ordering::Acquire);
        let start = if write_pos >= len { write_pos - len } else { 0 };
        self.close[(start + logical_idx) & RING_BUFFER_MASK]
    }

    #[inline]
    pub fn last_closes(&self, n: usize) -> Vec<f32> {
        let len = self.len();
        let actual_n = n.min(len);
        let mut result = Vec::with_capacity(actual_n);
        let write_pos = self.write_pos.load(Ordering::Acquire);
        let start = if write_pos >= len { write_pos - len } else { 0 };
        for i in (len - actual_n)..len {
            result.push(self.close[(start + i) & RING_BUFFER_MASK]);
        }
        result
    }

    pub fn load_from_ohlcv(&mut self, ohlcv: &[Vec<f64>]) {
        self.write_pos.store(0, Ordering::Release);
        self.bar_count.store(0, Ordering::Release);
        self.sequence.fetch_add(1, Ordering::Release);
        for bar in ohlcv.iter().take(RING_BUFFER_SIZE) {
            if bar.len() >= 6 {
                let pos = self.write_pos.load(Ordering::Relaxed);
                let idx = pos & RING_BUFFER_MASK;
                self.timestamps[idx] = bar[0] as i64;
                self.open[idx]   = bar[1] as f32;
                self.high[idx]   = bar[2] as f32;
                self.low[idx]    = bar[3] as f32;
                self.close[idx]  = bar[4] as f32;
                self.volume[idx] = bar[5] as f32;
                self.write_pos.store(pos + 1, Ordering::Release);
                self.bar_count.fetch_add(1, Ordering::AcqRel);
            }
        }
        self.sequence.fetch_add(1, Ordering::Release);
    }
}

impl Default for OhlcvRingBuffer { fn default() -> Self { Self::new() } }

// ─────────────────────────────────────────────────────────────────────────────
// MacdState — three EmaState instances
// ─────────────────────────────────────────────────────────────────────────────
#[derive(Clone)]
pub struct MacdState {
    pub fast_ema:   EmaState,
    pub slow_ema:   EmaState,
    pub signal_ema: EmaState,
}

impl MacdState {
    pub fn new(fast: usize, slow: usize, signal: usize) -> Self {
        Self {
            fast_ema:   EmaState::new(fast),
            slow_ema:   EmaState::new(slow),
            signal_ema: EmaState::new(signal),
        }
    }

    #[inline]
    pub fn update(&mut self, price: f32) -> (f32, f32, f32) {
        let fast_v    = self.fast_ema.update(price);
        let slow_v    = self.slow_ema.update(price);
        let macd_line = fast_v - slow_v;
        let signal    = self.signal_ema.update(macd_line);
        (macd_line, signal, macd_line - signal)
    }
}

impl Default for MacdState { fn default() -> Self { Self::new(12, 26, 9) } }

// ─────────────────────────────────────────────────────────────────────────────
// FastIndicatorCalculator — all-in-one per-tick calculator
// ─────────────────────────────────────────────────────────────────────────────
pub struct FastIndicatorCalculator {
    pub rsi_14:    RsiState,
    pub sma_10:    SmaState,
    pub sma_20:    SmaState,
    pub sma_50:    SmaState,
    pub ema_9:     EmaState,
    pub ema_21:    EmaState,
    pub ema_55:    EmaState,
    pub macd:      MacdState,
    pub bollinger: BollingerState,
    pub atr_14:    AtrState,
    pub roc_10:    RocState,
    pub mfi_14:    MfiState,
    pub cci_20:    CciState,
    pub adx_14:    AdxState,
    pub fvg:       FvgState,
    pub vwap:      VwapState,
    pub cti:       CtiState,
    last_values: [f32; 25],
    tick_count:  u64,
}

impl FastIndicatorCalculator {
    pub fn new() -> Self {
        Self {
            rsi_14:    RsiState::new(14),
            sma_10:    SmaState::new(10),
            sma_20:    SmaState::new(20),
            sma_50:    SmaState::new(50),
            ema_9:     EmaState::new(9),
            ema_21:    EmaState::new(21),
            ema_55:    EmaState::new(55),
            macd:      MacdState::new(12, 26, 9),
            bollinger: BollingerState::new(20, 2.0),
            atr_14:    AtrState::new(14),
            roc_10:    RocState::new(10),
            mfi_14:    MfiState::new(14),
            cci_20:    CciState::new(20),
            adx_14:    AdxState::new(14),
            fvg:       FvgState::new(),
            vwap:      VwapState::new(),
            cti:       CtiState::new(14, 50),
            last_values: [0.0; 25],
            tick_count:  0,
        }
    }

    #[inline]
    pub fn update(&mut self, _open: f32, high: f32, low: f32, close: f32, volume: f32) -> &[f32; 25] {
        self.last_values[0]  = self.rsi_14.update(close);
        self.last_values[1]  = self.sma_10.update(close);
        self.last_values[2]  = self.sma_20.update(close);
        self.last_values[3]  = self.sma_50.update(close);
        self.last_values[4]  = self.ema_9.update(close);
        self.last_values[5]  = self.ema_21.update(close);
        self.last_values[6]  = self.ema_55.update(close);
        let (macd_line, signal, histogram) = self.macd.update(close);
        self.last_values[7]  = macd_line;
        self.last_values[8]  = signal;
        self.last_values[9]  = histogram;
        // BollingerState returns (lower, middle, upper)
        let (bb_lower, bb_middle, bb_upper) = self.bollinger.update(close);
        let bb_pos = if (bb_upper - bb_lower).abs() > 1e-10 {
            (close - bb_lower) / (bb_upper - bb_lower)
        } else { 0.5 };
        self.last_values[10] = bb_pos;
        self.last_values[11] = bb_upper;
        self.last_values[12] = bb_middle;
        self.last_values[13] = bb_lower;
        self.last_values[14] = self.atr_14.update(high, low, close);
        self.last_values[15] = self.roc_10.update(close);
        self.last_values[16] = self.mfi_14.update(high, low, close, volume);
        self.last_values[17] = self.cci_20.update(high, low, close);
        self.last_values[18] = self.adx_14.update(high, low, close);
        self.last_values[19] = self.fvg.update(high, low);
        self.last_values[20] = self.vwap.update(high, low, close, volume);
        // CtiState.update takes (high, low, close) — no prev_close arg
        let (chop, trend, chop_z, trend_z) = self.cti.update(high, low, close);
        self.last_values[21] = chop;
        self.last_values[22] = trend;
        self.last_values[23] = chop_z;
        self.last_values[24] = trend_z;
        self.tick_count += 1;
        &self.last_values
    }

    #[inline]
    pub fn update_selective(&mut self, _open: f32, high: f32, low: f32, close: f32, volume: f32, mask: u32) -> &[f32; 25] {
        let all = mask == 0;
        if all || (mask & (1 << 0))  != 0 { self.last_values[0]  = self.rsi_14.update(close); }
        if all || (mask & (1 << 1))  != 0 { self.last_values[1]  = self.sma_10.update(close); }
        if all || (mask & (1 << 2))  != 0 { self.last_values[2]  = self.sma_20.update(close); }
        if all || (mask & (1 << 3))  != 0 { self.last_values[3]  = self.sma_50.update(close); }
        if all || (mask & (1 << 4))  != 0 { self.last_values[4]  = self.ema_9.update(close); }
        if all || (mask & (1 << 5))  != 0 { self.last_values[5]  = self.ema_21.update(close); }
        if all || (mask & (1 << 6))  != 0 { self.last_values[6]  = self.ema_55.update(close); }
        if all || (mask & 0x380)     != 0 {
            let (m, s, h) = self.macd.update(close);
            self.last_values[7] = m; self.last_values[8] = s; self.last_values[9] = h;
        }
        if all || (mask & 0x3C00)    != 0 {
            let (bb_lower, bb_middle, bb_upper) = self.bollinger.update(close);
            let bb_pos = if (bb_upper - bb_lower).abs() > 1e-10 {
                (close - bb_lower) / (bb_upper - bb_lower)
            } else { 0.5 };
            self.last_values[10] = bb_pos;
            self.last_values[11] = bb_upper;
            self.last_values[12] = bb_middle;
            self.last_values[13] = bb_lower;
        }
        if all || (mask & (1 << 14)) != 0 { self.last_values[14] = self.atr_14.update(high, low, close); }
        if all || (mask & (1 << 15)) != 0 { self.last_values[15] = self.roc_10.update(close); }
        if all || (mask & (1 << 16)) != 0 { self.last_values[16] = self.mfi_14.update(high, low, close, volume); }
        if all || (mask & (1 << 17)) != 0 { self.last_values[17] = self.cci_20.update(high, low, close); }
        if all || (mask & (1 << 18)) != 0 { self.last_values[18] = self.adx_14.update(high, low, close); }
        if all || (mask & (1 << 19)) != 0 { self.last_values[19] = self.fvg.update(high, low); }
        if all || (mask & (1 << 20)) != 0 { self.last_values[20] = self.vwap.update(high, low, close, volume); }
        if all || (mask & 0x1E00000) != 0 {
            let (chop, trend, chop_z, trend_z) = self.cti.update(high, low, close);
            self.last_values[21] = chop; self.last_values[22] = trend;
            self.last_values[23] = chop_z; self.last_values[24] = trend_z;
        }
        self.tick_count += 1;
        &self.last_values
    }

    #[inline(always)]
    pub fn get(&self, idx: usize) -> f32 { if idx < 25 { self.last_values[idx] } else { 0.0 } }

    pub fn to_hashmap(&self) -> HashMap<usize, Vec<f32>> {
        let mut map = HashMap::new();
        for i in 0..25 { map.insert(i, vec![self.last_values[i]]); }
        map
    }

    pub fn initialize_from_history(&mut self, ohlcv: &[(f32, f32, f32, f32, f32)]) {
        for &(o, h, l, c, v) in ohlcv { self.update(o, h, l, c, v); }
    }

    pub fn tick_count(&self) -> u64 { self.tick_count }
}

impl Default for FastIndicatorCalculator { fn default() -> Self { Self::new() } }

// ─────────────────────────────────────────────────────────────────────────────
// SharedIndicatorCache + IndicatorHistory
// ─────────────────────────────────────────────────────────────────────────────
pub struct SharedIndicatorCache {
    pub history: Arc<parking_lot::RwLock<IndicatorHistory>>,
    pub latest:  Arc<[AtomicU64; 25]>,
}

pub struct IndicatorHistory {
    pub values:      [Vec<f32>; 25],
    pub max_history: usize,
}

impl IndicatorHistory {
    pub fn new(max_history: usize) -> Self {
        Self { values: Default::default(), max_history }
    }

    pub fn push(&mut self, values: &[f32; 25]) {
        for (i, v) in values.iter().enumerate() {
            self.values[i].push(*v);
            if self.values[i].len() > self.max_history { self.values[i].remove(0); }
        }
    }

    pub fn get(&self, idx: usize) -> &[f32] { if idx < 25 { &self.values[idx] } else { &[] } }
    pub fn len(&self) -> usize { self.values[0].len() }
    pub fn is_empty(&self) -> bool { self.values[0].is_empty() }
}

impl Default for IndicatorHistory { fn default() -> Self { Self::new(1000) } }

impl SharedIndicatorCache {
    pub fn new(max_history: usize) -> Self {
        Self {
            history: Arc::new(parking_lot::RwLock::new(IndicatorHistory::new(max_history))),
            latest:  Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }

    pub fn update(&self, values: &[f32; 25]) {
        for (i, &v) in values.iter().enumerate() {
            self.latest[i].store(v.to_bits() as u64, Ordering::Release);
        }
        self.history.write().push(values);
    }

    #[inline(always)]
    pub fn get_latest(&self, idx: usize) -> f32 {
        if idx < 25 {
            f32::from_bits(self.latest[idx].load(Ordering::Acquire) as u32)
        } else { 0.0 }
    }

    pub fn get_history(&self, idx: usize) -> Vec<f32> { self.history.read().get(idx).to_vec() }

    pub fn to_signal_context_map(&self, mask: u32) -> Arc<HashMap<usize, Vec<f32>>> {
        let history = self.history.read();
        if mask == 0 {
            let mut map = HashMap::with_capacity(25);
            for i in 0..25 { map.insert(i, history.get(i).to_vec()); }
            Arc::new(map)
        } else {
            let n = mask.count_ones() as usize;
            let mut map = HashMap::with_capacity(n);
            for i in 0..25 {
                if mask & (1u32 << i) != 0 { map.insert(i, history.get(i).to_vec()); }
            }
            Arc::new(map)
        }
    }
}

impl Default for SharedIndicatorCache { fn default() -> Self { Self::new(1000) } }

// =============================================================================
// BATCH COMPUTATION WRAPPERS
// =============================================================================

#[inline]
pub fn calculate_sma_rust(prices: &[f32], period: usize) -> Vec<f32> { sma(prices, period) }

#[inline]
pub fn calculate_sma_arc(prices: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(sma(prices, period))
}

#[inline]
pub fn calculate_ema_rust(prices: &[f32], period: usize) -> Vec<f32> { ema(prices, period) }

#[inline]
pub fn calculate_ema_arc(prices: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(ema(prices, period))
}

#[inline]
pub fn calculate_rsi_rust(prices: &[f32], period: usize) -> Vec<f32> { rsi(prices, period) }

#[inline]
pub fn calculate_rsi_arc(prices: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(rsi(prices, period))
}

/// Returns (macd_line, signal_line, histogram)
#[inline]
pub fn calculate_macd_rust(prices: &[f32], fast: usize, slow: usize, signal: usize) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    macd(prices, fast, slow, signal)
}

#[inline]
pub fn calculate_macd_arc(prices: &StdArc<Vec<f32>>, fast: usize, slow: usize, signal: usize)
    -> (StdArc<Vec<f32>>, StdArc<Vec<f32>>, StdArc<Vec<f32>>)
{
    let (m, s, h) = macd(prices, fast, slow, signal);
    (StdArc::new(m), StdArc::new(s), StdArc::new(h))
}

/// Returns (upper, middle, lower)
#[inline]
pub fn calculate_bollinger_bands_rust(prices: &[f32], period: usize, std_mult: f32) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    bollinger(prices, period, std_mult)
}

#[inline]
pub fn calculate_bollinger_bands_arc(prices: &StdArc<Vec<f32>>, period: usize, std_mult: f32)
    -> (StdArc<Vec<f32>>, StdArc<Vec<f32>>, StdArc<Vec<f32>>)
{
    let (u, m, l) = bollinger(prices, period, std_mult);
    (StdArc::new(u), StdArc::new(m), StdArc::new(l))
}

#[inline]
pub fn calculate_atr_rust(high: &[f32], low: &[f32], close: &[f32], period: usize) -> Vec<f32> {
    atr(high, low, close, period)
}

#[inline]
pub fn calculate_atr_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>, close: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(atr(high, low, close, period))
}

// =============================================================================
// HELPER FUNCTIONS
// =============================================================================

/// Bollinger Band position (0.0 = at lower, 1.0 = at upper)
#[inline]
pub fn bb_position(close: &[f32], upper: &[f32], lower: &[f32]) -> Vec<f32> {
    let n = close.len();
    let mut result = vec![f32::NAN; n];
    for i in 0..n {
        if !upper[i].is_nan() && !lower[i].is_nan() {
            let range = upper[i] - lower[i];
            result[i] = if range == 0.0 { 0.5 } else { (close[i] - lower[i]) / range };
        }
    }
    result
}

#[inline]
pub fn bb_position_arc(close: &StdArc<Vec<f32>>, upper: &StdArc<Vec<f32>>, lower: &StdArc<Vec<f32>>) -> StdArc<Vec<f32>> {
    StdArc::new(bb_position(close, upper, lower))
}

/// Stochastic Fast %K
#[inline]
pub fn stoch_k(high: &[f32], low: &[f32], close: &[f32], period: usize) -> Vec<f32> {
    let n = high.len();
    let mut result = vec![f32::NAN; n];
    if n < period || period == 0 { return result; }
    for i in (period - 1)..n {
        let hh = high[(i + 1 - period)..=i].iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let ll = low[(i + 1 - period)..=i].iter().copied().fold(f32::INFINITY, f32::min);
        let range = hh - ll;
        result[i] = if range == 0.0 { 50.0 } else { (close[i] - ll) / range * 100.0 };
    }
    result
}

#[inline]
pub fn stoch_k_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>, close: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(stoch_k(high, low, close, period))
}

/// On-Balance Volume
#[inline]
pub fn obv(close: &[f32], volume: &[f32]) -> Vec<f32> {
    let n = close.len();
    let mut result = vec![0.0f32; n];
    for i in 1..n {
        if      close[i] > close[i - 1] { result[i] = result[i - 1] + volume[i]; }
        else if close[i] < close[i - 1] { result[i] = result[i - 1] - volume[i]; }
        else                             { result[i] = result[i - 1]; }
    }
    result
}

#[inline]
pub fn obv_arc(close: &StdArc<Vec<f32>>, volume: &StdArc<Vec<f32>>) -> StdArc<Vec<f32>> {
    StdArc::new(obv(close, volume))
}

/// Chaikin Money Flow
#[inline]
pub fn cmf(high: &[f32], low: &[f32], close: &[f32], volume: &[f32], period: usize) -> Vec<f32> {
    let n = high.len();
    let mut result = vec![f32::NAN; n];
    if n < period || period == 0 { return result; }
    let mut mfv = vec![0.0f32; n];
    for i in 0..n {
        let hl = high[i] - low[i];
        let clv = if hl == 0.0 { 0.0 } else { ((close[i] - low[i]) - (high[i] - close[i])) / hl };
        mfv[i] = clv * volume[i];
    }
    for i in (period - 1)..n {
        let sum_mfv: f32 = mfv[(i + 1 - period)..=i].iter().sum();
        let sum_vol: f32 = volume[(i + 1 - period)..=i].iter().sum();
        result[i] = if sum_vol == 0.0 { 0.0 } else { sum_mfv / sum_vol };
    }
    result
}

#[inline]
pub fn cmf_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>, close: &StdArc<Vec<f32>>, volume: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(cmf(high, low, close, volume, period))
}

/// Rolling standard deviation
#[inline]
pub fn rolling_std(prices: &[f32], period: usize) -> Vec<f32> {
    let n = prices.len();
    let mut result = vec![f32::NAN; n];
    if n < period || period == 0 { return result; }
    let mean = sma(prices, period);
    for i in (period - 1)..n {
        if mean[i].is_nan() { continue; }
        let slice = &prices[(i + 1 - period)..=i];
        let var: f32 = slice.iter().map(|x| (x - mean[i]).powi(2)).sum::<f32>() / period as f32;
        result[i] = var.sqrt();
    }
    result
}

#[inline]
pub fn rolling_std_arc(prices: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(rolling_std(prices, period))
}

/// Rolling max over a window
#[inline]
pub fn rolling_max(prices: &[f32], period: usize) -> Vec<f32> {
    let n = prices.len();
    let mut result = vec![f32::NAN; n];
    if n < period || period == 0 { return result; }
    for i in (period - 1)..n {
        result[i] = prices[(i + 1 - period)..=i].iter().copied().fold(f32::NEG_INFINITY, f32::max);
    }
    result
}

#[inline]
pub fn rolling_max_arc(prices: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(rolling_max(prices, period))
}

/// Rolling min over a window
#[inline]
pub fn rolling_min(prices: &[f32], period: usize) -> Vec<f32> {
    let n = prices.len();
    let mut result = vec![f32::NAN; n];
    if n < period || period == 0 { return result; }
    for i in (period - 1)..n {
        result[i] = prices[(i + 1 - period)..=i].iter().copied().fold(f32::INFINITY, f32::min);
    }
    result
}

#[inline]
pub fn rolling_min_arc(prices: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(rolling_min(prices, period))
}

/// Typical price
#[inline]
pub fn typical_price(high: &[f32], low: &[f32], close: &[f32]) -> Vec<f32> {
    high.iter().zip(low).zip(close).map(|((h, l), c)| (h + l + c) / 3.0).collect()
}

#[inline]
pub fn typical_price_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>, close: &StdArc<Vec<f32>>) -> StdArc<Vec<f32>> {
    StdArc::new(typical_price(high, low, close))
}

/// Keltner Channel — returns (middle, upper, lower)
#[inline]
pub fn keltner_channel(
    close: &[f32], high: &[f32], low: &[f32],
    ema_period: usize, atr_period: usize, mult: f32,
) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let n = close.len();
    let mid = ema(close, ema_period);
    let a   = atr(high, low, close, atr_period);
    let mut upper = vec![f32::NAN; n];
    let mut lower = vec![f32::NAN; n];
    for i in 0..n {
        if !mid[i].is_nan() && !a[i].is_nan() {
            upper[i] = mid[i] + a[i] * mult;
            lower[i] = mid[i] - a[i] * mult;
        }
    }
    (mid, upper, lower)
}

#[inline]
pub fn keltner_channel_arc(
    close: &StdArc<Vec<f32>>, high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>,
    ema_period: usize, atr_period: usize, mult: f32,
) -> (StdArc<Vec<f32>>, StdArc<Vec<f32>>, StdArc<Vec<f32>>) {
    let (m, u, l) = keltner_channel(close, high, low, ema_period, atr_period, mult);
    (StdArc::new(m), StdArc::new(u), StdArc::new(l))
}

/// Fibonacci retracement levels from rolling high/low
#[inline]
pub fn fibonacci_levels(high: &[f32], low: &[f32], lookback: usize) -> (Vec<f32>, Vec<f32>) {
    let n = high.len();
    let mut fib_382 = vec![f32::NAN; n];
    let mut fib_618 = vec![f32::NAN; n];
    if n < lookback || lookback == 0 { return (fib_382, fib_618); }
    for i in (lookback - 1)..n {
        let hh = high[(i + 1 - lookback)..=i].iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let ll = low[(i + 1 - lookback)..=i].iter().copied().fold(f32::INFINITY, f32::min);
        let range = hh - ll;
        fib_382[i] = ll + range * 0.382;
        fib_618[i] = ll + range * 0.618;
    }
    (fib_382, fib_618)
}

#[inline]
pub fn fibonacci_levels_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>, lookback: usize) -> (StdArc<Vec<f32>>, StdArc<Vec<f32>>) {
    let (f382, f618) = fibonacci_levels(high, low, lookback);
    (StdArc::new(f382), StdArc::new(f618))
}

// =============================================================================
// ADDITIONAL INDICATORS
// =============================================================================

/// Rate of Change
#[inline]
pub fn calculate_roc_rust(prices: &[f32], period: usize) -> Vec<f32> {
    let n = prices.len();
    let mut roc = vec![f32::NAN; n];
    if n <= period || period == 0 { return roc; }
    for i in period..n {
        roc[i] = if prices[i - period] != 0.0 {
            ((prices[i] - prices[i - period]) / prices[i - period]) * 100.0
        } else { 0.0 };
    }
    roc
}

#[inline]
pub fn calculate_roc_arc(prices: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(calculate_roc_rust(prices, period))
}

/// Money Flow Index
#[inline]
pub fn calculate_mfi_rust(high: &[f32], low: &[f32], close: &[f32], volume: &[f32], period: usize) -> Vec<f32> {
    let n = high.len();
    let mut mfi = vec![f32::NAN; n];
    if n <= period || period == 0 { return mfi; }
    let mut tp  = vec![0.0f32; n];
    let mut rmf = vec![0.0f32; n];
    for i in 0..n {
        tp[i]  = (high[i] + low[i] + close[i]) / 3.0;
        rmf[i] = tp[i] * volume[i];
    }
    let mut pos = 0.0f32;
    let mut neg = 0.0f32;
    for i in 1..=period {
        if      tp[i] > tp[i - 1] { pos += rmf[i]; }
        else if tp[i] < tp[i - 1] { neg += rmf[i]; }
    }
    mfi[period] = if neg == 0.0 { 100.0 } else { 100.0 - 100.0 / (1.0 + pos / neg) };
    for i in period + 1..n {
        if      tp[i - period] > tp[i - period - 1] { pos -= rmf[i - period]; }
        else if tp[i - period] < tp[i - period - 1] { neg -= rmf[i - period]; }
        if      tp[i] > tp[i - 1] { pos += rmf[i]; }
        else if tp[i] < tp[i - 1] { neg += rmf[i]; }
        pos = pos.max(0.0);
        neg = neg.max(0.0);
        mfi[i] = if neg == 0.0 { 100.0 } else { 100.0 - 100.0 / (1.0 + pos / neg) };
    }
    mfi
}

#[inline]
pub fn calculate_mfi_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>, close: &StdArc<Vec<f32>>, volume: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(calculate_mfi_rust(high, low, close, volume, period))
}

/// Commodity Channel Index
#[inline]
pub fn calculate_cci_rust(high: &[f32], low: &[f32], close: &[f32], period: usize) -> Vec<f32> {
    let n = high.len();
    let mut cci = vec![f32::NAN; n];
    if n < period || period == 0 { return cci; }
    let mut tp = vec![0.0f32; n];
    for i in 0..n { tp[i] = (high[i] + low[i] + close[i]) / 3.0; }
    let mut sma_tp = vec![f32::NAN; n];
    let mut sum: f32 = tp[0..period].iter().sum();
    sma_tp[period - 1] = sum / period as f32;
    for i in period..n {
        sum = sum - tp[i - period] + tp[i];
        sma_tp[i] = sum / period as f32;
    }
    for i in period - 1..n {
        let ma = sma_tp[i];
        let mut mad = 0.0f32;
        for j in (i + 1 - period)..=i { mad += (tp[j] - ma).abs(); }
        mad /= period as f32;
        cci[i] = if mad != 0.0 { (tp[i] - ma) / (0.015 * mad) } else { 0.0 };
    }
    cci
}

#[inline]
pub fn calculate_cci_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>, close: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(calculate_cci_rust(high, low, close, period))
}

/// ADX using Wilder's smoothing
#[inline]
pub fn calculate_adx_rust(high: &[f32], low: &[f32], close: &[f32], period: usize) -> Vec<f32> {
    let n = high.len();
    let mut adx_out = vec![f32::NAN; n];
    if n <= period * 2 || period == 0 { return adx_out; }
    let mut tr = vec![0.0f32; n];
    let mut dmp = vec![0.0f32; n];
    let mut dmm = vec![0.0f32; n];
    tr[0] = high[0] - low[0];
    for i in 1..n {
        tr[i]  = (high[i] - low[i]).max((high[i] - close[i - 1]).abs()).max((low[i] - close[i - 1]).abs());
        let up   = high[i] - high[i - 1];
        let dn   = low[i - 1] - low[i];
        if up > dn && up > 0.0 { dmp[i] = up; }
        if dn > up && dn > 0.0 { dmm[i] = dn; }
    }
    let mut tr_s  = vec![0.0f32; n];
    let mut dmp_s = vec![0.0f32; n];
    let mut dmm_s = vec![0.0f32; n];
    tr_s[period]  = tr[1..=period].iter().sum::<f32>();
    dmp_s[period] = dmp[1..=period].iter().sum();
    dmm_s[period] = dmm[1..=period].iter().sum();
    let mut dx = vec![f32::NAN; n];
    {
        let i = period;
        let dip = if tr_s[i] != 0.0 { 100.0 * dmp_s[i] / tr_s[i] } else { 0.0 };
        let dim = if tr_s[i] != 0.0 { 100.0 * dmm_s[i] / tr_s[i] } else { 0.0 };
        let sd  = dip + dim;
        dx[i] = if sd != 0.0 { 100.0 * (dip - dim).abs() / sd } else { 0.0 };
    }
    for i in period + 1..n {
        tr_s[i]  = tr_s[i - 1]  - tr_s[i - 1]  / period as f32 + tr[i];
        dmp_s[i] = dmp_s[i - 1] - dmp_s[i - 1] / period as f32 + dmp[i];
        dmm_s[i] = dmm_s[i - 1] - dmm_s[i - 1] / period as f32 + dmm[i];
        let dip = if tr_s[i] != 0.0 { 100.0 * dmp_s[i] / tr_s[i] } else { 0.0 };
        let dim = if tr_s[i] != 0.0 { 100.0 * dmm_s[i] / tr_s[i] } else { 0.0 };
        let sd  = dip + dim;
        dx[i] = if sd != 0.0 { 100.0 * (dip - dim).abs() / sd } else { 0.0 };
    }
    let adx_start = period * 2 - 1;
    if n > adx_start {
        let mut sum_dx = 0.0f32;
        for i in period..=adx_start { sum_dx += dx[i]; }
        adx_out[adx_start] = sum_dx / period as f32;
        for i in adx_start + 1..n {
            adx_out[i] = (adx_out[i - 1] * (period as f32 - 1.0) + dx[i]) / period as f32;
        }
    }
    adx_out
}

#[inline]
pub fn calculate_adx_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>, close: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(calculate_adx_rust(high, low, close, period))
}

/// FVG as ratio
#[inline]
pub fn calculate_fvg_rust(high: &[f32], low: &[f32], close: &[f32], open: &[f32]) -> Vec<f32> {
    let n = high.len();
    let mut fvg_out = vec![0.0f32; n];
    if n < 3 { return fvg_out; }
    for i in 2..n {
        let up_gap   = low[i] - high[i - 2];
        let down_gap = low[i - 2] - high[i];
        let body     = (close[i - 1] - open[i - 1]).abs();
        let cp       = close[i];
        if      up_gap   > body * 0.5 && cp != 0.0 { fvg_out[i] =  up_gap   / cp; }
        else if down_gap > body * 0.5 && cp != 0.0 { fvg_out[i] = -down_gap / cp; }
    }
    fvg_out
}

#[inline]
pub fn calculate_fvg_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>, close: &StdArc<Vec<f32>>, open: &StdArc<Vec<f32>>) -> StdArc<Vec<f32>> {
    StdArc::new(calculate_fvg_rust(high, low, close, open))
}

/// VWAP
#[inline]
pub fn calculate_vwap_rust(high: &[f32], low: &[f32], close: &[f32], volume: &[f32]) -> Vec<f32> {
    let n = high.len();
    let mut vwap_out = vec![f32::NAN; n];
    if n == 0 { return vwap_out; }
    let mut cum_vol = 0.0f32;
    let mut cum_pv  = 0.0f32;
    for i in 0..n {
        let tp = (high[i] + low[i] + close[i]) / 3.0;
        cum_vol += volume[i];
        cum_pv  += tp * volume[i];
        if cum_vol != 0.0 { vwap_out[i] = cum_pv / cum_vol; }
    }
    vwap_out
}

#[inline]
pub fn calculate_vwap_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>, close: &StdArc<Vec<f32>>, volume: &StdArc<Vec<f32>>) -> StdArc<Vec<f32>> {
    StdArc::new(calculate_vwap_rust(high, low, close, volume))
}

/// FVG simplified — 1.0 bullish, -1.0 bearish, 0.0 none
#[inline]
pub fn fvg_simple(high: &[f32], low: &[f32]) -> Vec<f32> {
    let n = high.len();
    let mut result = vec![0.0f32; n];
    if n < 3 { return result; }
    for i in 2..n {
        if      low[i]  > high[i - 2] { result[i] =  1.0; }
        else if high[i] < low[i - 2]  { result[i] = -1.0; }
    }
    result
}

#[inline]
pub fn fvg_simple_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>) -> StdArc<Vec<f32>> {
    StdArc::new(fvg_simple(high, low))
}

/// Choppiness Index
#[inline]
pub fn choppiness_index(high: &[f32], low: &[f32], close: &[f32], period: usize) -> Vec<f32> {
    let n = high.len();
    let mut result = vec![f32::NAN; n];
    if n <= period || period == 0 { return result; }
    let a = atr(high, low, close, 1);
    for i in period..n {
        let hh = high[(i + 1 - period)..=i].iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let ll = low[(i + 1 - period)..=i].iter().copied().fold(f32::INFINITY, f32::min);
        let range = hh - ll;
        if range > 0.0 {
            let sum_atr: f32 = a[(i + 1 - period)..=i].iter().filter(|x| !x.is_nan()).sum();
            let ratio = sum_atr / range;
            if ratio > 0.0 { result[i] = 100.0 * ratio.ln() / (period as f32).ln(); }
        }
    }
    result
}

#[inline]
pub fn choppiness_index_arc(high: &StdArc<Vec<f32>>, low: &StdArc<Vec<f32>>, close: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(choppiness_index(high, low, close, period))
}

/// Trend efficiency ratio (Kaufman)
#[inline]
pub fn trend_efficiency(close: &[f32], period: usize) -> Vec<f32> {
    let n = close.len();
    let mut result = vec![f32::NAN; n];
    if n <= period || period == 0 { return result; }
    for i in period..n {
        let direction  = (close[i] - close[i - period]).abs();
        let volatility: f32 = (1..=period)
            .map(|j| (close[i - period + j] - close[i - period + j - 1]).abs())
            .sum();
        result[i] = if volatility == 0.0 { 0.0 } else { direction / volatility };
    }
    result
}

#[inline]
pub fn trend_efficiency_arc(close: &StdArc<Vec<f32>>, period: usize) -> StdArc<Vec<f32>> {
    StdArc::new(trend_efficiency(close, period))
}

/// CTI batch — returns (chop, trend, chop_z, trend_z)
#[inline]
pub fn calculate_cti_rust(
    high: &[f32], low: &[f32], close: &[f32],
    n: usize, z_window: usize,
) -> (Vec<f32>, Vec<f32>, Vec<f32>, Vec<f32>) {
    let len = close.len();
    let mut chop   = vec![f32::NAN; len];
    let mut trend  = vec![f32::NAN; len];
    let mut chop_z = vec![f32::NAN; len];
    let mut trend_z = vec![f32::NAN; len];
    if len < n + 1 || n == 0 { return (chop, trend, chop_z, trend_z); }
    let mut tr = vec![0.0f32; len];
    for i in 1..len {
        tr[i] = (high[i] - low[i])
            .max((high[i] - close[i - 1]).abs())
            .max((low[i]  - close[i - 1]).abs());
    }
    let mut abs_diff = vec![0.0f32; len];
    for i in 1..len { abs_diff[i] = (close[i] - close[i - 1]).abs(); }
    let log10_n = (n as f32).log10();
    for i in n..len {
        let atr_sum: f32 = tr[i - n + 1..=i].iter().sum();
        let (mut hh, mut ll) = (f32::MIN, f32::MAX);
        for j in (i - n + 1)..=i {
            if high[j] > hh { hh = high[j]; }
            if low[j]  < ll { ll = low[j]; }
        }
        let price_range = hh - ll;
        chop[i] = if price_range > 1e-10 && atr_sum > 1e-10 {
            (100.0 * (atr_sum / price_range).log10() / log10_n).max(0.0).min(100.0)
        } else { 50.0 };
        let net_move: f32  = (close[i] - close[i - n]).abs();
        let gross_move: f32 = abs_diff[i - n + 1..=i].iter().sum();
        trend[i] = if gross_move > 1e-10 { (net_move / gross_move).max(0.0).min(1.0) } else { 0.0 };
    }
    let min_samples = n.max(20);
    for i in (n + min_samples)..len {
        let ws = if i >= z_window { i - z_window } else { n };
        let wl = i - ws;
        if wl >= min_samples {
            let cs: Vec<f32> = (ws..i).filter_map(|j| if chop[j].is_nan() { None } else { Some(chop[j]) }).collect();
            if cs.len() >= min_samples {
                let cm: f32 = cs.iter().sum::<f32>() / cs.len() as f32;
                let cv: f32 = cs.iter().map(|x| (x - cm).powi(2)).sum::<f32>() / cs.len() as f32;
                let cstd = cv.sqrt();
                if cstd > 1e-10 && !chop[i].is_nan() { chop_z[i] = (chop[i] - cm) / cstd; }
            }
            let ts: Vec<f32> = (ws..i).filter_map(|j| if trend[j].is_nan() { None } else { Some(trend[j]) }).collect();
            if ts.len() >= min_samples {
                let tm: f32 = ts.iter().sum::<f32>() / ts.len() as f32;
                let tv: f32 = ts.iter().map(|x| (x - tm).powi(2)).sum::<f32>() / ts.len() as f32;
                let tstd = tv.sqrt();
                if tstd > 1e-10 && !trend[i].is_nan() { trend_z[i] = (trend[i] - tm) / tstd; }
            }
        }
    }
    (chop, trend, chop_z, trend_z)
}

// =============================================================================
// ORDER FLOW INDICATORS
// =============================================================================

#[inline]
pub fn calculate_delta_ema_rust(delta: &[f32], period: usize) -> Vec<f32> {
    let n = delta.len();
    let mut out = vec![0.0f32; n];
    if n == 0 || period == 0 { return out; }
    let alpha = 2.0 / (period + 1) as f32;
    out[0] = delta[0];
    for i in 1..n { out[i] = alpha * delta[i] + (1.0 - alpha) * out[i - 1]; }
    out
}

#[inline]
pub fn calculate_cvd_rust(delta: &[f32]) -> Vec<f32> {
    let n = delta.len();
    let mut cvd = vec![0.0f32; n];
    if n == 0 { return cvd; }
    cvd[0] = delta[0];
    for i in 1..n { cvd[i] = cvd[i - 1] + delta[i]; }
    cvd
}

#[inline]
pub fn calculate_cvd_divergence_rust(cvd: &[f32], close: &[f32], period: usize) -> Vec<f32> {
    let n = cvd.len();
    let mut div = vec![0.0f32; n];
    if n <= period || period == 0 { return div; }
    for i in period..n {
        let cc = if cvd[i - period].abs() > 1e-10 { (cvd[i] - cvd[i - period]) / cvd[i - period].abs() } else { 0.0 };
        let pc = if close[i - period] > 1e-10 { (close[i] - close[i - period]) / close[i - period] } else { 0.0 };
        div[i] = cc - pc;
    }
    div
}

#[inline]
pub fn calculate_imbalance_intensity_rust(imbalance: &[f32], volume: &[f32], period: usize) -> Vec<f32> {
    let n = imbalance.len();
    let mut intensity = vec![0.0f32; n];
    if n < period || period == 0 { return intensity; }
    let mut vol_ma = vec![0.0f32; n];
    for i in period - 1..n {
        vol_ma[i] = volume[i - period + 1..=i].iter().sum::<f32>() / period as f32;
    }
    for i in period - 1..n {
        let ci = (imbalance[i] - 0.5) * 2.0;
        let vw = if vol_ma[i] > 1e-10 { (volume[i] / vol_ma[i]).min(3.0) } else { 1.0 };
        intensity[i] = ci * vw;
    }
    intensity
}

#[inline]
pub fn calculate_absorption_rust(delta: &[f32], close: &[f32], period: usize) -> Vec<f32> {
    let n = delta.len();
    let mut absorption = vec![0.0f32; n];
    if n <= period || period == 0 { return absorption; }
    let delta_std = delta.iter().map(|x| x.abs()).sum::<f32>() / n as f32;
    let mut pc = vec![0.0f32; n];
    for i in 1..n {
        if close[i - 1] > 1e-10 { pc[i] = (close[i] - close[i - 1]) / close[i - 1]; }
    }
    let price_std = pc.iter().map(|x| x.abs()).sum::<f32>() / n as f32;
    for i in period..n {
        let dw = delta[i - period..i].iter().map(|x| x.abs()).sum::<f32>();
        let pw = pc[i - period..i].iter().map(|x| x.abs()).sum::<f32>();
        let dn = dw / (delta_std + 1e-10);
        let pn = pw / (price_std + 1e-10);
        if dn > 0.5 { absorption[i] = (dn / (pn + 1e-10)).min(10.0); }
    }
    absorption
}

#[inline]
pub fn calculate_order_imbalance_rust(bid_ct: &[f32], ask_ct: &[f32]) -> Vec<f32> {
    let n = bid_ct.len();
    let mut out = vec![0.5f32; n];
    for i in 0..n {
        let total = bid_ct[i] + ask_ct[i];
        if total > 0.0 { out[i] = bid_ct[i] / total; }
    }
    out
}

#[inline]
pub fn calculate_size_imbalance_rust(bid_sz: &[f32], ask_sz: &[f32]) -> Vec<f32> {
    let n = bid_sz.len();
    let mut out = vec![0.5f32; n];
    for i in 0..n {
        let total = bid_sz[i] + ask_sz[i];
        if total > 0.0 { out[i] = bid_sz[i] / total; }
    }
    out
}

#[inline]
pub fn calculate_spread_pct_rust(bid_px: &[f32], ask_px: &[f32]) -> Vec<f32> {
    let n = bid_px.len();
    let mut out = vec![0.0f32; n];
    for i in 0..n {
        let mid = (bid_px[i] + ask_px[i]) / 2.0;
        if mid > 1e-10 { out[i] = (ask_px[i] - bid_px[i]) / mid; }
    }
    out
}

// =============================================================================
// COMPOSITE
// =============================================================================

/// 0:RSI_14  1:SMA_10  2:SMA_20  3:SMA_50  4:EMA_9  5:EMA_21  6:EMA_55
/// 7:MACD  8:MACD_SIG  9:MACD_HIST  10:BB_POS  11:BB_UPPER  12:BB_MID
/// 13:BB_LOWER  14:ATR_14  15:ROC_10  16:MFI_14  17:CCI_20  18:ADX_14
/// 19:FVG  20:VWAP  21:CHOP  22:TREND_EFF
pub fn calculate_standard_indicators(
    close: &StdArc<Vec<f32>>, high: &StdArc<Vec<f32>>,
    low: &StdArc<Vec<f32>>, volume: &StdArc<Vec<f32>>,
) -> HashMap<usize, Vec<f32>> {
    let mut m = HashMap::new();
    m.insert(0, rsi(close, 14));
    m.insert(1, sma(close, 10));
    m.insert(2, sma(close, 20));
    m.insert(3, sma(close, 50));
    m.insert(4, ema(close, 9));
    m.insert(5, ema(close, 21));
    m.insert(6, ema(close, 55));
    let (ml, sl, hl) = macd(close, 12, 26, 9);
    m.insert(7, ml); m.insert(8, sl); m.insert(9, hl);
    let (bb_u, bb_m, bb_l) = bollinger(close, 20, 2.0);
    m.insert(10, bb_position(close, &bb_u, &bb_l));
    m.insert(11, bb_u); m.insert(12, bb_m); m.insert(13, bb_l);
    m.insert(14, atr(high, low, close, 14));
    m.insert(15, calculate_roc_rust(close, 10));
    m.insert(16, calculate_mfi_rust(high, low, close, volume, 14));
    m.insert(17, calculate_cci_rust(high, low, close, 20));
    m.insert(18, calculate_adx_rust(high, low, close, 14));
    m.insert(19, fvg_simple(high, low));
    m.insert(20, calculate_vwap_rust(high, low, close, volume));
    m.insert(21, choppiness_index(high, low, close, 14));
    m.insert(22, trend_efficiency(close, 10));
    m
}

/// Legacy string-keyed composite
pub fn calculate_all(
    close: &[f32], high: &[f32], low: &[f32], _volume: &[f32],
) -> HashMap<String, Vec<f32>> {
    let mut result = HashMap::new();
    result.insert("rsi_14".into(),   rsi(close, 14));
    result.insert("ema_9".into(),    ema(close, 9));
    result.insert("ema_21".into(),   ema(close, 21));
    result.insert("sma_50".into(),   sma(close, 50));
    result.insert("sma_200".into(),  sma(close, 200));
    result.insert("atr_14".into(),   atr(high, low, close, 14));
    let (macd_line, macd_sig, macd_hist) = macd(close, 12, 26, 9);
    result.insert("macd".into(),        macd_line);
    result.insert("macd_signal".into(), macd_sig);
    result.insert("macd_hist".into(),   macd_hist);
    let (bb_upper, bb_middle, bb_lower) = bollinger(close, 20, 2.0);
    result.insert("bb_upper".into(),  bb_upper);
    result.insert("bb_middle".into(), bb_middle);
    result.insert("bb_lower".into(),  bb_lower);
    result
}

#[cfg(test)]
mod precision_tests {
    use super::*;

    /// Deterministic pseudo-random walk around `base` — mimics real BTC-scale
    /// price magnitude (large mean, small window-to-window variance), which is
    /// exactly the regime where an unbounded-growing accumulator (a global
    /// prefix sum, or `E[x²]-E[x]²`) loses precision to catastrophic
    /// cancellation while a bounded sliding-window/two-pass computation does
    /// not. `n=60_000` matches roughly 1.5 years of 15m bars — a realistic
    /// single-pair backtest length.
    fn synth_prices(n: usize, base: f32) -> Vec<f32> {
        let mut seed: u64 = 88172645463325252;
        let mut next = || {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
            (seed as f64 / u64::MAX as f64) as f32
        };
        let mut v = Vec::with_capacity(n);
        let mut p = base;
        for _ in 0..n {
            let r = next() - 0.5;
            p *= 1.0 + r * 0.01;
            v.push(p);
        }
        v
    }

    /// Regression test for a real bug found and fixed 2026-09-27: the
    /// original AsgardEngine port's `sma()` built ONE global f32 prefix sum
    /// over the whole series and subtracted two entries to get each window's
    /// sum. Over a long series at BTC-scale prices the prefix reaches into
    /// the billions, where f32's ~7 significant digits make that subtraction
    /// lose ~25-28 units of precision on a 20-period window (verified against
    /// an independent f64 reference sum) — silently corrupting every
    /// downstream consumer, especially Bollinger Bands (which uses this SMA
    /// as its middle band, and was off by ~370 units absolute on the same
    /// data before the fix). The fix (accumulate the prefix sum in f64,
    /// f32 only at the final per-bar result) must not regress.
    #[test]
    fn sma_matches_f64_reference_at_scale() {
        let n = 60_000;
        let close = synth_prices(n, 95_000.0);
        let period = 20;
        let result = sma(&close, period);

        for &idx in &[100usize, 30_000, n - 1] {
            let window_ref: f64 = close[(idx + 1 - period)..=idx]
                .iter().map(|&x| x as f64).sum::<f64>() / period as f64;
            let diff = (result[idx] as f64 - window_ref).abs();
            assert!(
                diff < 1.0,
                "sma[{idx}] = {} but f64 reference = {window_ref} (diff {diff} >= 1.0 tolerance — \
                 the f32 prefix-sum cancellation bug may have regressed)",
                result[idx]
            );
        }
    }

    /// Companion regression test: Bollinger Bands (upper/lower) must track an
    /// independent two-pass f64 reference at BTC-scale prices — guards
    /// against a future edit reintroducing a growing-accumulator variance
    /// formula (e.g. reverting to `E[x²]-E[x]²`) in `bollinger()`.
    #[test]
    fn bollinger_matches_f64_reference_at_scale() {
        let n = 60_000;
        let close = synth_prices(n, 95_000.0);
        let period = 20;
        let mult = 2.0;
        let (upper, _middle, lower) = bollinger(&close, period, mult);

        for &idx in &[100usize, 30_000, n - 1] {
            let window: Vec<f64> = close[(idx + 1 - period)..=idx].iter().map(|&x| x as f64).collect();
            let mean: f64 = window.iter().sum::<f64>() / period as f64;
            let var: f64 = window.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / period as f64;
            let std = var.sqrt();
            let ref_upper = mean + mult as f64 * std;
            let ref_lower = mean - mult as f64 * std;
            assert!(
                (upper[idx] as f64 - ref_upper).abs() < 1.0,
                "bollinger upper[{idx}] = {} but f64 reference = {ref_upper}", upper[idx]
            );
            assert!(
                (lower[idx] as f64 - ref_lower).abs() < 1.0,
                "bollinger lower[{idx}] = {} but f64 reference = {ref_lower}", lower[idx]
            );
        }
    }
}