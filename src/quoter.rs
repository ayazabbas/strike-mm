use alloy::primitives::U256;
use eyre::Result;
use std::collections::HashMap;
use std::time::Instant;
use tracing::{info, warn};

use strike_sdk::prelude::*;
use strike_sdk::types::{OrderParam, Side};

use crate::config::QuotingConfig;
use crate::risk::RiskManager;

/// Which sides to quote for a market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteMode {
    TwoSided,
    BidsOnly,
    AsksOnly,
}

/// Active orders we've placed for a market.
#[derive(Debug, Clone)]
pub struct MarketOrders {
    pub bid_order_ids: Vec<U256>,
    pub ask_order_ids: Vec<U256>,
    pub last_fair_tick: i64,
    pub last_quote_time: Instant,
}

/// How much to multiply the requote cooldown when no fills for a while.
const LOW_VOLUME_COOLDOWN_MULTIPLIER: u64 = 4;
/// Duration without fills before entering low-volume mode (5 minutes).
const LOW_VOLUME_THRESHOLD_SECS: u64 = 300;

/// The Quoter manages order placement and cancellation via the SDK client.
pub struct Quoter {
    client: StrikeClient,
    pub config: QuotingConfig,
    /// market_id → active orders
    pub active_orders: HashMap<u64, MarketOrders>,
    pub dry_run: bool,
    /// Last time any order was filled. Used to reduce quote frequency during low volume.
    last_fill_time: Instant,
}

impl Quoter {
    pub fn new(client: StrikeClient, config: QuotingConfig, dry_run: bool) -> Self {
        Self {
            client,
            config,
            active_orders: HashMap::new(),
            dry_run,
            last_fill_time: Instant::now(),
        }
    }

    /// Call when a fill is detected to reset the low-volume timer.
    pub fn record_fill(&mut self) {
        self.last_fill_time = Instant::now();
    }

    /// Whether we're in low-volume mode (no fills for 5+ minutes).
    pub fn is_low_volume(&self) -> bool {
        self.last_fill_time.elapsed().as_secs() >= LOW_VOLUME_THRESHOLD_SECS
    }

    /// Build OrderParam structs for bid and ask levels.
    /// Flattening orders (reducing existing position) are exempt from budget checks.
    /// New-risk orders are sized down to what's affordable instead of being skipped.
    #[allow(clippy::too_many_arguments)]
    /// Build OrderParam structs for bid and ask levels.
    /// Returns (all_params, bid_count) — bid_count indicates how many of the
    /// leading params are bids (rest are asks). This is used to correctly
    /// assign sides when the SDK can't determine side from resting events.
    fn build_order_params(
        &self,
        bid_tick: u64,
        ask_tick: u64,
        risk: &mut RiskManager,
        market_id: u64,
        mode: QuoteMode,
        bid_lots: u64,
        ask_lots: u64,
    ) -> (Vec<OrderParam>, Vec<OrderParam>) {
        let mut bid_params = Vec::new();
        let mut ask_params = Vec::new();
        let net_lots = risk.position(market_id);

        if mode != QuoteMode::AsksOnly && bid_lots > 0 {
            // A bid when net_lots < 0 is flattening (reducing short YES position)
            let is_flattening = net_lots < 0;
            for level in 0..self.config.num_levels {
                let tick = bid_tick.saturating_sub(level * 2);
                if tick < 1 {
                    continue;
                }
                let lots = if is_flattening {
                    // Flattening: allow up to abs(position) lots, no budget check
                    bid_lots.min(net_lots.unsigned_abs())
                } else {
                    // New risk: cap to affordable, minimum 1 lot to ensure order always places.
                    // Never skip — skipping causes orphaned orders when the quoter loses
                    // track of which orders are live on-chain.
                    risk.max_affordable_lots(market_id, tick, bid_lots, true).max(1)
                };
                bid_params.push(OrderParam::bid(tick as u8, lots));
            }
        }

        if mode != QuoteMode::BidsOnly && ask_lots > 0 {
            // An ask when net_lots > 0 is flattening (reducing long YES position)
            let is_flattening = net_lots > 0;
            for level in 0..self.config.num_levels {
                let tick = ask_tick.saturating_add(level * 2);
                if tick > 99 {
                    continue;
                }
                let lots = if is_flattening {
                    // Flattening: allow up to abs(position) lots, no budget check
                    ask_lots.min(net_lots.unsigned_abs())
                } else {
                    // New risk: cap to affordable, minimum 1 lot (same rationale as bids)
                    risk.max_affordable_lots(market_id, tick, ask_lots, false).max(1)
                };
                ask_params.push(OrderParam::ask(tick as u8, lots));
            }
        }

        (bid_params, ask_params)
    }

    // ── Restore recovered state ──────────────────────────────────────

    /// Restore active_orders from on-chain recovery results.
    pub fn restore_state(&mut self, live_orders: HashMap<u64, (Vec<U256>, Vec<U256>)>) {
        for (market_id, (bids, asks)) in live_orders {
            let count = bids.len() + asks.len();
            if count > 0 {
                info!(
                    market_id,
                    bids = bids.len(),
                    asks = asks.len(),
                    "restoring recovered orders"
                );
                self.active_orders.insert(
                    market_id,
                    MarketOrders {
                        bid_order_ids: bids,
                        ask_order_ids: asks,
                        last_fair_tick: 0,
                        last_quote_time: Instant::now(),
                    },
                );
            }
        }
    }

    // ── Startup Cancel Sweep ─────────────────────────────────────────

    /// Cancel ALL recovered live orders via batch cancelOrders.
    pub async fn startup_cancel_sweep(&mut self) -> Result<()> {
        let all_ids: Vec<U256> = self
            .active_orders
            .values()
            .flat_map(|m| m.bid_order_ids.iter().chain(m.ask_order_ids.iter()))
            .copied()
            .collect();

        if all_ids.is_empty() {
            info!("startup cancel sweep: no orders to cancel");
            return Ok(());
        }

        info!(
            count = all_ids.len(),
            "startup cancel sweep: batch cancelling recovered orders"
        );

        if self.dry_run {
            for order_id in &all_ids {
                info!(order_id = %order_id, "[DRY RUN] would cancel");
            }
            self.active_orders.clear();
            return Ok(());
        }

        match tokio::time::timeout(
            std::time::Duration::from_secs(60),
            self.client.orders().cancel(&all_ids),
        )
        .await
        {
            Ok(Ok(())) => {
                info!(count = all_ids.len(), "startup batch cancel confirmed");
            }
            Ok(Err(e)) => {
                warn!(err = %e, "startup batch cancel failed — orders may still be live");
            }
            Err(_) => {
                warn!("startup batch cancel timed out — orders may still be live");
            }
        }

        self.active_orders.clear();
        Ok(())
    }

    // ── Batch Order Placement ──────────────────────────────────────────

    /// Place initial quotes for a market using `placeOrders`.
    #[allow(clippy::too_many_arguments)]
    pub async fn place_quotes(
        &mut self,
        market_id: u64,
        bid_tick: u64,
        ask_tick: u64,
        fair_tick: i64,
        risk: &mut RiskManager,
        mode: QuoteMode,
        bid_lots: u64,
        ask_lots: u64,
    ) -> Result<()> {
        let (bid_params, ask_params) = self.build_order_params(
            bid_tick, ask_tick, risk, market_id, mode, bid_lots, ask_lots,
        );
        let bid_param_count = bid_params.len();
        let all_params: Vec<OrderParam> = bid_params.into_iter().chain(ask_params).collect();

        if all_params.is_empty() {
            info!(market_id, "no orders to place (all filtered by risk)");
            return Ok(());
        }

        if self.dry_run {
            for p in &all_params {
                let side = if p.side == Side::Bid { "bid" } else { "ask" };
                info!(
                    market_id,
                    side,
                    tick = p.tick,
                    lots = p.lots,
                    "[DRY RUN] would place order"
                );
            }
            self.active_orders.insert(
                market_id,
                MarketOrders {
                    bid_order_ids: Vec::new(),
                    ask_order_ids: Vec::new(),
                    last_fair_tick: fair_tick,
                    last_quote_time: Instant::now(),
                },
            );
            return Ok(());
        }

        let placed = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.client.orders().place(market_id, &all_params),
        )
        .await
        {
            Ok(Ok(placed)) => placed,
            Ok(Err(e)) => {
                warn!(market_id, err = %e, "placeOrders failed — will retry next cycle");
                self.active_orders.remove(&market_id);
                return Ok(());
            }
            Err(_) => {
                warn!(market_id, "placeOrders timed out — will retry next cycle");
                self.active_orders.remove(&market_id);
                return Ok(());
            }
        };

        // Assign sides from params (not from SDK response — resting orders
        // emit OrderResting without side info, so SDK defaults to Bid).
        // Params are always [bids..., asks...], and contract assigns IDs in order.
        let mut bid_ids = Vec::new();
        let mut ask_ids = Vec::new();
        for (i, p) in placed.iter().enumerate() {
            if i < bid_param_count {
                bid_ids.push(p.order_id);
            } else {
                ask_ids.push(p.order_id);
            }
        }

        if placed.len() != all_params.len() {
            warn!(
                market_id,
                expected = all_params.len(),
                actual = placed.len(),
                "placeOrders count mismatch — fewer placed than expected"
            );
        }

        info!(
            market_id,
            bids = bid_ids.len(),
            asks = ask_ids.len(),
            "placeOrders confirmed"
        );

        self.active_orders.insert(
            market_id,
            MarketOrders {
                bid_order_ids: bid_ids,
                ask_order_ids: ask_ids,
                last_fair_tick: fair_tick,
                last_quote_time: Instant::now(),
            },
        );
        Ok(())
    }

    // ── Cancellation ─────────────────────────────────────────────────

    /// Cancel all locally-tracked orders for a market via batch cancelOrders.
    /// Returns Ok(true) on success, Ok(false) if batch failed.
    pub async fn cancel_local_orders_batch(&mut self, market_id: u64) -> Result<bool> {
        let order_ids: Vec<U256> = match self.active_orders.get(&market_id) {
            Some(orders) => orders
                .bid_order_ids
                .iter()
                .chain(orders.ask_order_ids.iter())
                .copied()
                .collect(),
            None => return Ok(true),
        };

        if order_ids.is_empty() {
            self.active_orders.remove(&market_id);
            return Ok(true);
        }

        let count = order_ids.len();

        if self.dry_run {
            info!(market_id, count, "[DRY RUN] would batch cancel");
            self.active_orders.remove(&market_id);
            return Ok(true);
        }

        info!(market_id, count, "batch cancelling orders");

        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.client.orders().cancel(&order_ids),
        )
        .await
        {
            Ok(Ok(())) => {
                info!(market_id, count, "batch cancel confirmed");
            }
            Ok(Err(e)) => {
                warn!(market_id, err = %e, "batch cancel failed — falling back to sequential");
                return Ok(false);
            }
            Err(_) => {
                warn!(
                    market_id,
                    "batch cancel timed out — falling back to sequential"
                );
                return Ok(false);
            }
        }

        self.active_orders.remove(&market_id);
        Ok(true)
    }

    /// Cancel all locally-tracked orders for a market via sequential individual calls.
    pub async fn cancel_local_orders(&mut self, market_id: u64) -> Result<()> {
        let order_ids: Vec<U256> = match self.active_orders.get(&market_id) {
            Some(orders) => orders
                .bid_order_ids
                .iter()
                .chain(orders.ask_order_ids.iter())
                .copied()
                .collect(),
            None => return Ok(()),
        };

        if order_ids.is_empty() {
            self.active_orders.remove(&market_id);
            return Ok(());
        }

        info!(
            market_id,
            count = order_ids.len(),
            "cancelling locally-tracked orders sequentially"
        );

        if self.dry_run {
            for order_id in &order_ids {
                info!(market_id, order_id = %order_id, "[DRY RUN] would cancel");
            }
            self.active_orders.remove(&market_id);
            return Ok(());
        }

        for order_id in &order_ids {
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.client.orders().cancel_one(*order_id),
            )
            .await
            {
                Ok(Ok(())) => {
                    info!(market_id, order_id = %order_id, "cancel confirmed");
                }
                Ok(Err(e)) => {
                    warn!(market_id, order_id = %order_id, err = %e, "cancel failed — continuing");
                }
                Err(_) => {
                    warn!(market_id, order_id = %order_id, "cancel timed out — continuing");
                }
            }
        }

        self.active_orders.remove(&market_id);
        Ok(())
    }

    /// Cancel all orders across ALL markets (for shutdown / stale data guard).
    pub async fn cancel_everything(&mut self) -> Result<()> {
        let market_ids: Vec<u64> = self.active_orders.keys().copied().collect();
        for market_id in market_ids {
            if !self.cancel_local_orders_batch(market_id).await? {
                self.cancel_local_orders(market_id).await?;
            }
        }
        Ok(())
    }

    // ── Requoting ────────────────────────────────────────────────────

    /// Check if a market needs requoting based on fair tick movement.
    pub fn needs_requote(&self, market_id: u64, new_fair_tick: i64) -> bool {
        let orders = match self.active_orders.get(&market_id) {
            Some(o) => o,
            None => return true,
        };

        // In low-volume mode, multiply cooldown by 4x (75% fewer requotes)
        let cooldown = if self.is_low_volume() {
            self.config.requote_cooldown_secs * LOW_VOLUME_COOLDOWN_MULTIPLIER
        } else {
            self.config.requote_cooldown_secs
        };

        if orders.last_quote_time.elapsed().as_secs() < cooldown {
            return false;
        }

        let fair_diff = (new_fair_tick - orders.last_fair_tick).unsigned_abs();
        fair_diff >= self.config.requote_cents
    }

    /// Atomic requote: cancel existing orders and place new ones in a single TX
    /// via `replaceOrders`. Zero empty book time.
    #[allow(clippy::too_many_arguments)]
    pub async fn requote(
        &mut self,
        market_id: u64,
        bid_tick: u64,
        ask_tick: u64,
        fair_tick: i64,
        risk: &mut RiskManager,
        mode: QuoteMode,
        bid_lots: u64,
        ask_lots: u64,
    ) -> Result<()> {
        // If no existing orders, just place fresh
        let cancel_ids: Vec<U256> = match self.active_orders.get(&market_id) {
            Some(orders) => orders
                .bid_order_ids
                .iter()
                .chain(orders.ask_order_ids.iter())
                .copied()
                .collect(),
            None => {
                return self
                    .place_quotes(
                        market_id, bid_tick, ask_tick, fair_tick, risk, mode, bid_lots, ask_lots,
                    )
                    .await
            }
        };

        if cancel_ids.is_empty() {
            return self
                .place_quotes(
                    market_id, bid_tick, ask_tick, fair_tick, risk, mode, bid_lots, ask_lots,
                )
                .await;
        }

        let (bid_params, ask_params) = self.build_order_params(
            bid_tick, ask_tick, risk, market_id, mode, bid_lots, ask_lots,
        );
        let bid_param_count = bid_params.len();
        let all_params: Vec<OrderParam> = bid_params.into_iter().chain(ask_params).collect();

        if self.dry_run {
            info!(
                market_id,
                cancel_count = cancel_ids.len(),
                "[DRY RUN] would replaceOrders"
            );
            for p in &all_params {
                let side = if p.side == Side::Bid { "bid" } else { "ask" };
                info!(
                    market_id,
                    side,
                    tick = p.tick,
                    lots = p.lots,
                    "[DRY RUN] would place order"
                );
            }
            self.active_orders.insert(
                market_id,
                MarketOrders {
                    bid_order_ids: Vec::new(),
                    ask_order_ids: Vec::new(),
                    last_fair_tick: fair_tick,
                    last_quote_time: Instant::now(),
                },
            );
            return Ok(());
        }

        let cancel_count = cancel_ids.len();
        let place_count = all_params.len();

        let placed = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.client
                .orders()
                .replace(&cancel_ids, market_id, &all_params),
        )
        .await
        {
            Ok(Ok(placed)) => placed,
            Ok(Err(e)) => {
                warn!(market_id, err = %e, "replaceOrders failed — will retry next cycle");
                self.active_orders.remove(&market_id);
                return Ok(());
            }
            Err(_) => {
                warn!(market_id, "replaceOrders timed out — will retry next cycle");
                self.active_orders.remove(&market_id);
                return Ok(());
            }
        };

        // Assign sides from params (not SDK response — resting orders lack side info)
        let mut bid_ids = Vec::new();
        let mut ask_ids = Vec::new();
        for (i, p) in placed.iter().enumerate() {
            if i < bid_param_count {
                bid_ids.push(p.order_id);
            } else {
                ask_ids.push(p.order_id);
            }
        }

        if placed.len() != all_params.len() {
            warn!(
                market_id,
                expected = all_params.len(),
                actual = placed.len(),
                "replaceOrders count mismatch — fewer placed than expected"
            );
        }

        info!(
            market_id,
            cancelled = cancel_count,
            placed = placed.len(),
            bids = bid_ids.len(),
            asks = ask_ids.len(),
            "replaceOrders confirmed"
        );

        self.active_orders.insert(
            market_id,
            MarketOrders {
                bid_order_ids: bid_ids,
                ask_order_ids: ask_ids,
                last_fair_tick: fair_tick,
                last_quote_time: Instant::now(),
            },
        );
        Ok(())
    }

    /// Check if we're currently quoting a market.
    pub fn is_quoting(&self, market_id: u64) -> bool {
        self.active_orders.contains_key(&market_id)
    }
}
