use std::collections::HashMap;
use tracing::{info, warn};

/// Tracks positions and enforces risk limits.
pub struct RiskManager {
    /// Net position per market (positive = long YES, negative = short YES)
    positions: HashMap<u64, i64>,
    /// Max lots per market
    max_position_per_market: i64,
    /// Max total absolute exposure across all markets
    max_total_exposure: i64,
}

impl RiskManager {
    pub fn new(max_position_per_market: i64, max_total_exposure: i64) -> Self {
        Self {
            positions: HashMap::new(),
            max_position_per_market,
            max_total_exposure,
        }
    }

    /// Record a fill. `lots` is signed: positive for bid fill (bought YES), negative for ask fill.
    pub fn record_fill(&mut self, market_id: u64, lots: i64) {
        let pos = self.positions.entry(market_id).or_insert(0);
        *pos += lots;
        info!(market_id, position = *pos, lots, "position updated");
    }

    /// Get current position for a market.
    pub fn position(&self, market_id: u64) -> i64 {
        self.positions.get(&market_id).copied().unwrap_or(0)
    }

    /// Total absolute exposure across all markets.
    pub fn total_exposure(&self) -> i64 {
        self.positions.values().map(|p| p.abs()).sum()
    }

    /// Check if placing `lots` on `market_id` would breach limits.
    /// `lots` is signed (positive = buying YES, negative = selling YES).
    pub fn can_place(&self, market_id: u64, lots: i64) -> bool {
        let current = self.position(market_id);
        let new_pos = current + lots;

        if new_pos.abs() > self.max_position_per_market {
            warn!(
                market_id,
                current,
                lots,
                max = self.max_position_per_market,
                "would breach per-market limit"
            );
            return false;
        }

        let new_total = self.total_exposure() - current.abs() + new_pos.abs();
        if new_total > self.max_total_exposure {
            warn!(
                market_id,
                new_total,
                max = self.max_total_exposure,
                "would breach total exposure limit"
            );
            return false;
        }

        true
    }

    /// Remove market from tracking (e.g., after expiry).
    pub fn remove_market(&mut self, market_id: u64) {
        if let Some(pos) = self.positions.remove(&market_id) {
            info!(market_id, final_position = pos, "market removed from risk tracking");
        }
    }

    /// Compute proportional inventory skew in ticks for a market.
    /// Returns positive value to shift quotes down (encourage selling when long).
    /// At max position, shifts by max_skew_ticks; scales linearly.
    pub fn inventory_skew(&self, market_id: u64, max_skew_ticks: i64) -> i64 {
        let pos = self.position(market_id);
        if self.max_position_per_market == 0 {
            return 0;
        }
        let ratio = pos as f64 / self.max_position_per_market as f64;
        (ratio * max_skew_ticks as f64).round() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_tracking() {
        let mut rm = RiskManager::new(50, 200);
        rm.record_fill(1, 5);
        assert_eq!(rm.position(1), 5);
        rm.record_fill(1, -3);
        assert_eq!(rm.position(1), 2);
    }

    #[test]
    fn test_per_market_limit() {
        let mut rm = RiskManager::new(50, 200);
        rm.record_fill(1, 45);
        assert!(rm.can_place(1, 5)); // 45 + 5 = 50, exactly at limit
        assert!(!rm.can_place(1, 6)); // 45 + 6 = 51, over limit
    }

    #[test]
    fn test_total_exposure_limit() {
        let mut rm = RiskManager::new(50, 200);
        rm.record_fill(1, 50);
        rm.record_fill(2, 50);
        rm.record_fill(3, 50);
        rm.record_fill(4, 50); // total = 200
        assert!(!rm.can_place(5, 1)); // would be 201
    }

    #[test]
    fn test_remove_market() {
        let mut rm = RiskManager::new(50, 200);
        rm.record_fill(1, 30);
        assert_eq!(rm.total_exposure(), 30);
        rm.remove_market(1);
        assert_eq!(rm.total_exposure(), 0);
        assert_eq!(rm.position(1), 0);
    }

    #[test]
    fn test_inventory_skew() {
        let mut rm = RiskManager::new(50, 200);

        // At 50% of max position (25/50), skew = round(0.5 * 6) = 3
        rm.record_fill(1, 25);
        assert_eq!(rm.inventory_skew(1, 6), 3);

        // At 100% of max position (50/50), skew = round(1.0 * 6) = 6
        rm.record_fill(1, 25); // now at 50
        assert_eq!(rm.inventory_skew(1, 6), 6);

        // Negative position: at -50% (-25/50), skew = round(-0.5 * 6) = -3
        rm.record_fill(2, -25);
        assert_eq!(rm.inventory_skew(2, 6), -3);

        // Zero position → no skew
        assert_eq!(rm.inventory_skew(99, 6), 0);
    }

    #[test]
    fn test_stale_data_guard_cancel_condition() {
        // This tests the logic that would trigger a cancel — just the risk check
        let rm = RiskManager::new(50, 200);
        // No position = can place
        assert!(rm.can_place(1, 5));
    }
}
