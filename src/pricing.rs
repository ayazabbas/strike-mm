use statrs::distribution::{ContinuousCDF, Normal};

/// Black-Scholes probability that BTC will be above strike at expiry.
/// Returns P(YES) = Φ((ln(S/K) + σ²t/2) / (σ√t))
///
/// - `spot`: current BTC price
/// - `strike`: strike price in USD
/// - `vol`: annualized volatility (e.g. 0.50 for 50%)
/// - `time_to_expiry`: time to expiry in years
pub fn fair_value(spot: f64, strike: f64, vol: f64, time_to_expiry: f64) -> f64 {
    // Edge cases
    if time_to_expiry <= 0.0 {
        return if spot >= strike { 1.0 } else { 0.0 };
    }
    if vol <= 0.0 {
        return if spot >= strike { 1.0 } else { 0.0 };
    }
    if strike <= 0.0 || spot <= 0.0 {
        return 0.0;
    }

    let d =
        ((spot / strike).ln() + vol * vol * time_to_expiry / 2.0) / (vol * time_to_expiry.sqrt());

    let normal = Normal::new(0.0, 1.0).unwrap();
    let p = normal.cdf(d);

    // Clamp to [0.001, 0.999] — allow extreme probabilities near expiry
    p.clamp(0.001, 0.999)
}

/// Compute bid and ask ticks from fair value probability.
///
/// - `fair_prob`: probability in [0, 1]
/// - `spread_ticks`: total spread in ticks (e.g. 6)
/// - `inventory_skew`: shift in ticks due to inventory (positive = shift down)
///
/// Returns (bid_tick, ask_tick) clamped to [1, 99].
pub fn compute_ticks(fair_prob: f64, spread_ticks: u64, inventory_skew: i64) -> (u64, u64) {
    let fair_tick = (fair_prob * 100.0).round() as i64;
    let half_spread = spread_ticks as i64 / 2;

    let bid = fair_tick - half_spread - inventory_skew;
    let ask = fair_tick + half_spread - inventory_skew;

    let bid = bid.clamp(1, 99) as u64;
    let ask = ask.clamp(1, 99) as u64;

    // Ensure bid < ask
    if bid >= ask {
        // If they overlap, center around fair tick
        let center = fair_tick.clamp(2, 98) as u64;
        return (center - 1, center + 1);
    }

    (bid, ask)
}

/// Compute realized volatility from 1-minute log returns.
/// Annualizes using sqrt(525600) (minutes in a year).
pub fn realized_vol(returns: &[f64]) -> f64 {
    if returns.len() < 2 {
        return 0.0;
    }

    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let std_dev = variance.sqrt();

    // Annualize from 1-minute frequency
    std_dev * (525_600.0_f64).sqrt()
}

/// Convert Pyth-format strike price (8 decimals) to USD.
pub fn pyth_price_to_f64(pyth_price: i64) -> f64 {
    pyth_price as f64 / 1e8
}

/// Convert unix expiry timestamp to time-to-expiry in years.
pub fn time_to_expiry_years(expiry_unix: i64) -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let remaining_secs = expiry_unix - now;
    if remaining_secs <= 0 {
        return 0.0;
    }
    remaining_secs as f64 / (365.25 * 24.0 * 3600.0)
}

/// Exaggerate fair value to benefit the MM at extremes and near expiry.
///
/// A) Directional exaggeration: when fair is far from 0.5, push it further.
/// B) Time decay exaggeration: near expiry (<45s), push harder.
pub fn exaggerate_fair(fair: f64, secs_left: u64) -> f64 {
    let mut adjusted = fair;

    // A) Directional exaggeration — push extremes further
    let dist = (fair - 0.5).abs();
    if dist > 0.15 {
        // Scale: at dist=0.15 → 0% extra, at dist=0.45 → 30% extra push (capped at 15 cents)
        let extra = ((dist - 0.15) / 0.30).min(1.0) * 0.15;
        if fair > 0.5 {
            adjusted += extra;
        } else {
            adjusted -= extra;
        }
    }

    // B) Time decay exaggeration — near expiry, push harder
    if secs_left < 45 {
        // Scale: at 45s → 0% extra, at 0s → 15% extra push
        let time_factor = 1.0 - (secs_left as f64 / 45.0);
        let time_extra = time_factor * 0.15;
        let current_dist = (adjusted - 0.5).abs();
        if current_dist > 0.05 {
            if adjusted > 0.5 {
                adjusted += time_extra;
            } else {
                adjusted -= time_extra;
            }
        }
    }

    adjusted.clamp(0.01, 0.99)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deep_itm() {
        // BTC at 100k, strike at 50k — should be ~1.0
        let p = fair_value(100_000.0, 50_000.0, 0.50, 5.0 / 525_600.0);
        assert!(p > 0.95, "deep ITM should be ~1.0, got {p}");
    }

    #[test]
    fn test_deep_otm() {
        // BTC at 50k, strike at 100k — should be ~0.0
        let p = fair_value(50_000.0, 100_000.0, 0.50, 5.0 / 525_600.0);
        assert!(p < 0.05, "deep OTM should be ~0.0, got {p}");
    }

    #[test]
    fn test_atm() {
        // BTC at 80k, strike at 80k — should be ~0.5
        let p = fair_value(80_000.0, 80_000.0, 0.50, 5.0 / 525_600.0);
        assert!((0.45..=0.55).contains(&p), "ATM should be ~0.5, got {p}");
    }

    #[test]
    fn test_zero_time() {
        assert_eq!(fair_value(100.0, 90.0, 0.5, 0.0), 1.0);
        assert_eq!(fair_value(90.0, 100.0, 0.5, 0.0), 0.0);
    }

    #[test]
    fn test_extreme_vol() {
        // Very high vol should push towards 0.5
        let p = fair_value(80_000.0, 80_000.0, 5.0, 5.0 / 525_600.0);
        assert!(
            (0.40..=0.60).contains(&p),
            "extreme vol ATM should still be ~0.5, got {p}"
        );
    }

    #[test]
    fn test_price_equals_strike() {
        let p = fair_value(80_000.0, 80_000.0, 0.50, 1.0 / 525_600.0);
        assert!(
            (0.45..=0.55).contains(&p),
            "price == strike should be ~0.5, got {p}"
        );
    }

    #[test]
    fn test_compute_ticks_basic() {
        let (bid, ask) = compute_ticks(0.50, 6, 0);
        assert_eq!(bid, 47);
        assert_eq!(ask, 53);
    }

    #[test]
    fn test_compute_ticks_with_skew() {
        let (bid, ask) = compute_ticks(0.50, 6, 1);
        // Skew shifts both down by 1
        assert_eq!(bid, 46);
        assert_eq!(ask, 52);
    }

    #[test]
    fn test_compute_ticks_clamping() {
        // Very high probability — ask should clamp to 99
        let (bid, ask) = compute_ticks(0.99, 6, 0);
        assert!(bid >= 1);
        assert!(ask <= 99);
        assert!(bid < ask);
    }

    #[test]
    fn test_compute_ticks_low_prob() {
        // Very low probability — bid should clamp to 1
        let (bid, ask) = compute_ticks(0.01, 6, 0);
        assert!(bid >= 1);
        assert!(ask <= 99);
        assert!(bid < ask);
    }

    #[test]
    fn test_realized_vol() {
        // Known returns
        let returns = vec![0.001, -0.002, 0.0015, -0.001, 0.0005];
        let vol = realized_vol(&returns);
        assert!(vol > 0.0, "vol should be positive");
        // Roughly: std_dev ~ 0.0013, annualized ~ 0.0013 * 725 ≈ 0.94
        assert!(vol < 5.0, "vol should be reasonable, got {vol}");
    }

    #[test]
    fn test_realized_vol_empty() {
        assert_eq!(realized_vol(&[]), 0.0);
        assert_eq!(realized_vol(&[0.001]), 0.0);
    }

    #[test]
    fn test_inventory_skew_shifts_quotes() {
        // Position > threshold: skew = 1 tick shift
        let (bid_no_skew, ask_no_skew) = compute_ticks(0.50, 6, 0);
        let (bid_skew, ask_skew) = compute_ticks(0.50, 6, 1);
        assert_eq!(bid_skew, bid_no_skew - 1);
        assert_eq!(ask_skew, ask_no_skew - 1);
    }

    #[test]
    fn test_pyth_price_conversion() {
        // 80000.00000000 in Pyth format = 8_000_000_000_000
        let usd = pyth_price_to_f64(8_000_000_000_000);
        assert!((usd - 80_000.0).abs() < 0.01);
    }

    // ── exaggerate_fair tests ─────────────────────────────────────

    #[test]
    fn test_exaggerate_fair_no_change_near_center() {
        // Fair near 0.5 with plenty of time — no exaggeration
        let result = exaggerate_fair(0.50, 300);
        assert!((result - 0.50).abs() < 0.001);

        // Within the 0.15 dead zone
        let result = exaggerate_fair(0.60, 300);
        assert!((result - 0.60).abs() < 0.001);
    }

    #[test]
    fn test_exaggerate_fair_directional_high() {
        // Fair = 0.85 (dist = 0.35), extra = ((0.35-0.15)/0.30).min(1.0) * 0.15 = 0.1
        let result = exaggerate_fair(0.85, 300);
        assert!(result > 0.85, "should push higher, got {result}");
        assert!((result - 0.95).abs() < 0.01, "expected ~0.95, got {result}");
    }

    #[test]
    fn test_exaggerate_fair_directional_low() {
        // Fair = 0.15 (dist = 0.35), same magnitude push downward
        let result = exaggerate_fair(0.15, 300);
        assert!(result < 0.15, "should push lower, got {result}");
        assert!((result - 0.05).abs() < 0.01, "expected ~0.05, got {result}");
    }

    #[test]
    fn test_exaggerate_fair_time_decay() {
        // Fair = 0.60 with 10s left — time factor = (1 - 10/45) ≈ 0.778, extra ≈ 0.117
        // dist = 0.10 < 0.15, so no directional push; after no directional, adjusted=0.60
        // current_dist = 0.10 > 0.05, so time push applies
        let result = exaggerate_fair(0.60, 10);
        assert!(result > 0.60, "should push higher near expiry, got {result}");

        // No time push with plenty of time
        let result_far = exaggerate_fair(0.60, 300);
        assert!((result_far - 0.60).abs() < 0.001);
    }

    #[test]
    fn test_exaggerate_fair_combined() {
        // Fair = 0.85, 0 secs left — both directional and time push
        let result = exaggerate_fair(0.85, 0);
        assert!(result > 0.95, "should push very high with both effects, got {result}");
        assert!(result <= 0.99, "should clamp to 0.99");
    }

    #[test]
    fn test_exaggerate_fair_clamp() {
        // Extreme fair value clamped
        let result = exaggerate_fair(0.99, 0);
        assert!(result <= 0.99);
        assert!(result >= 0.01);

        let result = exaggerate_fair(0.01, 0);
        assert!(result >= 0.01);
    }

    #[test]
    fn test_exaggerate_fair_no_time_push_near_center() {
        // Fair = 0.52, 5s left — current_dist = 0.02 < 0.05, no time push
        let result = exaggerate_fair(0.52, 5);
        assert!((result - 0.52).abs() < 0.001, "no push expected, got {result}");
    }
}
