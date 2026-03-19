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
}
