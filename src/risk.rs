use std::collections::HashMap;
use tracing::{info, warn};

/// Convert lots at a given tick to USDT value.
/// On-chain LOT_SIZE = 1e16, so lots * LOT_SIZE / 1e18 = lots * 0.01.
/// cost_usdt = tick / 100.0 * lots * 0.01
pub fn lots_to_usdt(tick: u64, lots: u64) -> f64 {
    tick as f64 / 100.0 * lots as f64 * 0.01
}

/// Dollar-based position tracking for a single market.
/// Tracks cost, max loss, max gain, and net lots from fills.
#[derive(Debug, Clone, Default)]
pub struct PositionState {
    /// USDT spent (sum of all fills' cost)
    pub total_cost: f64,
    /// Worst-case loss (sum of all fills' max_loss)
    pub total_max_loss: f64,
    /// Best-case gain (sum of all fills' max_gain)
    pub total_max_gain: f64,
    /// Signed lot count (positive = long YES, negative = short YES / long NO)
    pub net_lots: i64,
}

impl PositionState {
    /// Record a fill using clearing tick from BatchCleared event.
    /// `is_bid` = true means buying YES, false means selling YES (buying NO).
    pub fn record_fill(&mut self, clearing_tick: u64, lots: u64, is_bid: bool) {
        let cost = lots_to_usdt(clearing_tick, lots);
        let gain = (1.0 - clearing_tick as f64 / 100.0) * lots as f64 * 0.01;

        if is_bid {
            // Buying YES: cost is what we pay, gain is what we win if YES resolves
            self.total_cost += cost;
            self.total_max_loss += cost;
            self.total_max_gain += gain;
            self.net_lots += lots as i64;
        } else {
            // Selling YES (buying NO): cost is (100-tick)/100 * lots * 0.01
            let no_cost = gain; // same formula: (1 - tick/100) * lots * 0.01
            let no_gain = cost; // if NO resolves, we win the tick side
            self.total_cost += no_cost;
            self.total_max_loss += no_cost;
            self.total_max_gain += no_gain;
            self.net_lots -= lots as i64;
        }
    }

    /// Remaining USDT budget before hitting max_loss_budget.
    pub fn remaining_budget(&self, max_loss_budget: f64) -> f64 {
        (max_loss_budget - self.total_max_loss).max(0.0)
    }

    /// Expected P&L given current fair probability.
    pub fn expected_pnl(&self, fair_prob: f64) -> f64 {
        if self.net_lots > 0 {
            // Net long YES
            self.total_max_gain * fair_prob - self.total_max_loss * (1.0 - fair_prob)
        } else if self.net_lots < 0 {
            // Net long NO (short YES)
            self.total_max_gain * (1.0 - fair_prob) - self.total_max_loss * fair_prob
        } else {
            0.0
        }
    }

    /// Exposure ratio (0.0 to 1.0) for skew calculation.
    pub fn exposure_ratio(&self, max_loss_budget: f64) -> f64 {
        if max_loss_budget <= 0.0 {
            return 0.0;
        }
        (self.total_max_loss / max_loss_budget).clamp(0.0, 1.0)
    }

    /// Compute lots to quote on the same side as current position (reduced by budget).
    /// For opposite side, caller should use full base_lots.
    pub fn quote_lots_same_side(
        &self,
        tick: u64,
        base_lots: u64,
        num_levels: u64,
        max_loss_budget: f64,
    ) -> u64 {
        let remaining = self.remaining_budget(max_loss_budget);
        if remaining <= 0.0 || tick == 0 || num_levels == 0 {
            return 0;
        }
        let cost_per_lot = tick as f64 / 100.0 * 0.01;
        if cost_per_lot <= 0.0 {
            return 0;
        }
        let max_lots = (remaining / cost_per_lot / num_levels as f64) as u64;
        max_lots.min(base_lots)
    }
}

/// Tracks positions and enforces risk limits using dollar-based budgets.
pub struct RiskManager {
    /// Per-market position state (dollar-based tracking)
    positions: HashMap<u64, PositionState>,
    /// Max USDT at risk per market
    max_loss_budget: f64,
    /// Max skew ticks for inventory skew calculation
    max_skew_ticks: i64,
}

impl RiskManager {
    pub fn new(max_loss_budget: f64, max_skew_ticks: i64) -> Self {
        Self {
            positions: HashMap::new(),
            max_loss_budget,
            max_skew_ticks,
        }
    }

    /// Record a fill with clearing tick and side.
    pub fn record_fill(&mut self, market_id: u64, clearing_tick: u64, lots: u64, is_bid: bool) {
        let pos = self.positions.entry(market_id).or_default();
        pos.record_fill(clearing_tick, lots, is_bid);
        info!(
            market_id,
            net_lots = pos.net_lots,
            total_max_loss = format!("{:.2}", pos.total_max_loss),
            total_max_gain = format!("{:.2}", pos.total_max_gain),
            remaining_budget = format!("{:.2}", pos.remaining_budget(self.max_loss_budget)),
            "position updated (dollar-based)"
        );
    }

    /// Get current net lots for a market (positive = long YES).
    pub fn position(&self, market_id: u64) -> i64 {
        self.positions
            .get(&market_id)
            .map(|p| p.net_lots)
            .unwrap_or(0)
    }

    /// Get position state for a market.
    pub fn position_state(&self, market_id: u64) -> PositionState {
        self.positions.get(&market_id).cloned().unwrap_or_default()
    }

    /// Check if placing on this market would breach the dollar budget.
    /// `is_bid` = true for buying YES, false for selling YES.
    /// `tick` is the tick at which the order would be placed.
    /// `lots` is the unsigned lot count.
    pub fn can_place(&self, market_id: u64, tick: u64, lots: u64, is_bid: bool) -> bool {
        let pos = self.positions.get(&market_id).cloned().unwrap_or_default();
        let additional_cost = if is_bid {
            lots_to_usdt(tick, lots)
        } else {
            (1.0 - tick as f64 / 100.0) * lots as f64 * 0.01
        };
        let new_max_loss = pos.total_max_loss + additional_cost;
        if new_max_loss > self.max_loss_budget {
            warn!(
                market_id,
                new_max_loss = format!("{:.2}", new_max_loss),
                budget = format!("{:.2}", self.max_loss_budget),
                "would breach dollar loss budget"
            );
            return false;
        }
        true
    }

    /// Remove market from tracking (e.g., after expiry).
    pub fn remove_market(&mut self, market_id: u64) {
        if let Some(pos) = self.positions.remove(&market_id) {
            info!(
                market_id,
                final_net_lots = pos.net_lots,
                total_cost = format!("{:.2}", pos.total_cost),
                total_max_loss = format!("{:.2}", pos.total_max_loss),
                "market removed from risk tracking"
            );
        }
    }

    /// Compute dollar-exposure-based inventory skew in ticks for a market.
    /// Returns positive value to shift quotes down (encourage selling when long YES).
    pub fn inventory_skew(&self, market_id: u64) -> i64 {
        let pos = match self.positions.get(&market_id) {
            Some(p) => p,
            None => return 0,
        };
        if pos.net_lots == 0 {
            return 0;
        }
        let ratio = pos.exposure_ratio(self.max_loss_budget);
        let skew = (ratio * self.max_skew_ticks as f64).round() as i64;
        // Direction: positive when long YES, negative when short YES
        if pos.net_lots > 0 {
            skew
        } else {
            -skew
        }
    }

    /// Get max_loss_budget for external use (quote sizing).
    pub fn max_loss_budget(&self) -> f64 {
        self.max_loss_budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lots_to_usdt() {
        // 25000 lots at tick 50 → 50/100 * 25000 * 0.01 = $125
        assert!((lots_to_usdt(50, 25000) - 125.0).abs() < 0.001);

        // 25000 lots at tick 5 → 5/100 * 25000 * 0.01 = $12.50
        assert!((lots_to_usdt(5, 25000) - 12.5).abs() < 0.001);

        // 25000 lots at tick 85 → 85/100 * 25000 * 0.01 = $212.50
        assert!((lots_to_usdt(85, 25000) - 212.5).abs() < 0.001);

        // Edge: tick 0
        assert_eq!(lots_to_usdt(0, 1000), 0.0);

        // Edge: 0 lots
        assert_eq!(lots_to_usdt(50, 0), 0.0);

        // Tick 1 (near-free YES)
        assert!((lots_to_usdt(1, 100000) - 10.0).abs() < 0.001);

        // Tick 99 (near-certain YES)
        assert!((lots_to_usdt(99, 100000) - 990.0).abs() < 0.001);
    }

    #[test]
    fn test_position_state_record_bid_fill() {
        let mut ps = PositionState::default();
        // Buy 25000 lots YES at clearing tick 50
        ps.record_fill(50, 25000, true);
        assert_eq!(ps.net_lots, 25000);
        assert!((ps.total_cost - 125.0).abs() < 0.001);
        assert!((ps.total_max_loss - 125.0).abs() < 0.001);
        assert!((ps.total_max_gain - 125.0).abs() < 0.001);
    }

    #[test]
    fn test_position_state_record_ask_fill() {
        let mut ps = PositionState::default();
        // Sell YES (buy NO) 25000 lots at clearing tick 50
        ps.record_fill(50, 25000, false);
        assert_eq!(ps.net_lots, -25000);
        assert!((ps.total_cost - 125.0).abs() < 0.001);
        assert!((ps.total_max_loss - 125.0).abs() < 0.001);
        assert!((ps.total_max_gain - 125.0).abs() < 0.001);
    }

    #[test]
    fn test_position_state_asymmetric_ticks() {
        let mut ps = PositionState::default();
        // Buy YES at tick 10: cheap, high upside
        ps.record_fill(10, 25000, true);
        assert!((ps.total_max_loss - 25.0).abs() < 0.001); // 10/100 * 25000 * 0.01
        assert!((ps.total_max_gain - 225.0).abs() < 0.001); // 90/100 * 25000 * 0.01

        // Buy YES at tick 90: expensive, low upside
        let mut ps2 = PositionState::default();
        ps2.record_fill(90, 25000, true);
        assert!((ps2.total_max_loss - 225.0).abs() < 0.001);
        assert!((ps2.total_max_gain - 25.0).abs() < 0.001);
    }

    #[test]
    fn test_remaining_budget() {
        let mut ps = PositionState::default();
        ps.record_fill(50, 25000, true); // cost $125
        assert!((ps.remaining_budget(500.0) - 375.0).abs() < 0.001);

        // Exhaust budget
        ps.record_fill(50, 75000, true); // +$375 → total $500
        assert!(ps.remaining_budget(500.0).abs() < 0.001);

        // Over budget → floor at 0
        ps.record_fill(50, 1000, true);
        assert_eq!(ps.remaining_budget(500.0), 0.0);
    }

    #[test]
    fn test_expected_pnl() {
        let mut ps = PositionState::default();
        // Buy YES at tick 50
        ps.record_fill(50, 10000, true);
        // max_loss = $50, max_gain = $50

        // Fair prob 0.5 → break-even
        assert!((ps.expected_pnl(0.5)).abs() < 0.001);

        // Fair prob 0.8 → winning
        let pnl = ps.expected_pnl(0.8);
        // 50 * 0.8 - 50 * 0.2 = 40 - 10 = 30
        assert!((pnl - 30.0).abs() < 0.001);

        // Fair prob 0.2 → losing
        let pnl = ps.expected_pnl(0.2);
        // 50 * 0.2 - 50 * 0.8 = 10 - 40 = -30
        assert!((pnl - (-30.0)).abs() < 0.001);
    }

    #[test]
    fn test_expected_pnl_short() {
        let mut ps = PositionState::default();
        // Sell YES at tick 50 (buy NO)
        ps.record_fill(50, 10000, false);
        // net_lots = -10000

        // Fair prob 0.2 → NO likely → winning
        let pnl = ps.expected_pnl(0.2);
        // max_gain * (1 - 0.2) - max_loss * 0.2 = 50*0.8 - 50*0.2 = 40-10 = 30
        assert!((pnl - 30.0).abs() < 0.001);
    }

    #[test]
    fn test_exposure_ratio() {
        let mut ps = PositionState::default();
        assert_eq!(ps.exposure_ratio(500.0), 0.0);

        ps.record_fill(50, 25000, true); // $125 max_loss
        assert!((ps.exposure_ratio(500.0) - 0.25).abs() < 0.001);

        ps.record_fill(50, 75000, true); // +$375 → $500 total
        assert!((ps.exposure_ratio(500.0) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_quote_lots_same_side() {
        let mut ps = PositionState::default();
        // No fills yet → full budget
        // $500 budget, tick 50, base 25000, 2 levels
        // remaining = 500, cost_per_lot = 0.005, per_level = 500/0.005/2 = 50000
        // capped at 25000
        assert_eq!(ps.quote_lots_same_side(50, 25000, 2, 500.0), 25000);

        // Fill $400 worth
        ps.record_fill(50, 80000, true); // 80000 * 0.005 = $400
                                         // remaining = 100, max_lots = 100/0.005/2 = 10000
        assert_eq!(ps.quote_lots_same_side(50, 25000, 2, 500.0), 10000);

        // Budget exhausted
        ps.record_fill(50, 20000, true); // +$100 → $500 total
        assert_eq!(ps.quote_lots_same_side(50, 25000, 2, 500.0), 0);
    }

    #[test]
    fn test_quote_lots_cheap_tick() {
        let ps = PositionState::default();
        // At tick 10: cost_per_lot = 0.001, budget 500, 2 levels
        // max = 500/0.001/2 = 250000, capped at 25000
        assert_eq!(ps.quote_lots_same_side(10, 25000, 2, 500.0), 25000);
    }

    #[test]
    fn test_risk_manager_dollar_based() {
        let mut rm = RiskManager::new(500.0, 6);
        rm.record_fill(1, 50, 25000, true);
        assert_eq!(rm.position(1), 25000);

        let ps = rm.position_state(1);
        assert!((ps.total_max_loss - 125.0).abs() < 0.001);
    }

    #[test]
    fn test_risk_manager_can_place() {
        let mut rm = RiskManager::new(500.0, 6);
        rm.record_fill(1, 50, 80000, true); // $400 max_loss

        // $100 remaining — 25000 lots at tick 50 = $125 → over budget
        assert!(!rm.can_place(1, 50, 25000, true));

        // 10000 lots at tick 50 = $50 → ok
        assert!(rm.can_place(1, 50, 10000, true));
    }

    #[test]
    fn test_inventory_skew_dollar_based() {
        let mut rm = RiskManager::new(500.0, 6);

        // Fill 50% of budget long YES
        rm.record_fill(1, 50, 50000, true); // $250 max_loss = 50% budget
        let skew = rm.inventory_skew(1);
        assert_eq!(skew, 3); // round(0.5 * 6) = 3, positive because long YES

        // Short YES position
        let mut rm2 = RiskManager::new(500.0, 6);
        rm2.record_fill(1, 50, 50000, false); // $250 max_loss short
        let skew = rm2.inventory_skew(1);
        assert_eq!(skew, -3); // negative because short YES

        // No position
        assert_eq!(rm.inventory_skew(99), 0);
    }

    #[test]
    fn test_remove_market() {
        let mut rm = RiskManager::new(500.0, 6);
        rm.record_fill(1, 50, 25000, true);
        rm.remove_market(1);
        assert_eq!(rm.position(1), 0);
    }

    #[test]
    fn test_edge_tick_1() {
        let mut ps = PositionState::default();
        ps.record_fill(1, 100000, true);
        assert!((ps.total_max_loss - 10.0).abs() < 0.001); // 1/100 * 100000 * 0.01
        assert!((ps.total_max_gain - 990.0).abs() < 0.001); // 99/100 * 100000 * 0.01
    }

    #[test]
    fn test_edge_tick_99() {
        let mut ps = PositionState::default();
        ps.record_fill(99, 100000, true);
        assert!((ps.total_max_loss - 990.0).abs() < 0.001);
        assert!((ps.total_max_gain - 10.0).abs() < 0.001);
    }
}
