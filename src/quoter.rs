use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolEvent;
use eyre::{Result, WrapErr};
use std::collections::HashMap;
use std::time::Instant;
use tracing::{info, warn};

use crate::config::QuotingConfig;
use crate::risk::RiskManager;

sol!(
    #[sol(rpc)]
    OrderBook,
    "abi/OrderBook.json"
);

sol!(
    #[sol(rpc)]
    MockUSDT,
    "abi/MockUSDT.json"
);

/// Extract the orderId from an OrderPlaced event in a transaction receipt.
fn parse_order_id_from_receipt(receipt: &alloy::rpc::types::TransactionReceipt) -> Option<U256> {
    for log in receipt.inner.logs() {
        if let Ok(event) = OrderBook::OrderPlaced::decode_log(&log.inner) {
            return Some(event.orderId);
        }
    }
    None
}

/// Active orders we've placed for a market.
#[derive(Debug, Clone)]
pub struct MarketOrders {
    pub bid_order_ids: Vec<U256>,
    pub ask_order_ids: Vec<U256>,
    pub last_bid_tick: u64,
    pub last_ask_tick: u64,
    pub last_fair_tick: i64,
    pub last_quote_time: Instant,
}

/// The Quoter manages order placement and cancellation on the OrderBook contract.
pub struct Quoter<P> {
    order_book: OrderBook::OrderBookInstance<P>,
    pub config: QuotingConfig,
    /// market_id → active orders
    pub active_orders: HashMap<u64, MarketOrders>,
    pub dry_run: bool,
}

impl<P> Quoter<P>
where
    P: Provider + Clone,
{
    pub fn new(
        order_book_addr: Address,
        provider: P,
        config: QuotingConfig,
        dry_run: bool,
    ) -> Self {
        Self {
            order_book: OrderBook::new(order_book_addr, provider),
            config,
            active_orders: HashMap::new(),
            dry_run,
        }
    }

    /// Place bid and ask orders for a market at computed ticks.
    pub async fn place_quotes(
        &mut self,
        market_id: u64,
        bid_tick: u64,
        ask_tick: u64,
        fair_tick: i64,
        risk: &mut RiskManager,
    ) -> Result<()> {
        let market_id_u256 = U256::from(market_id);
        let lots = U256::from(self.config.lots_per_level);

        let mut bid_ids = Vec::new();
        let mut ask_ids = Vec::new();

        // Place bid levels
        for level in 0..self.config.num_levels {
            let tick = bid_tick.saturating_sub(level * 2);
            if tick < 1 {
                continue;
            }

            let lots_i64 = self.config.lots_per_level as i64;
            if !risk.can_place(market_id, lots_i64) {
                warn!(market_id, tick, "risk limit — skipping bid");
                continue;
            }

            if self.dry_run {
                info!(
                    market_id,
                    side = "bid",
                    tick,
                    lots = self.config.lots_per_level,
                    "[DRY RUN] would place order"
                );
                bid_ids.push(U256::ZERO);
            } else {
                match self
                    .order_book
                    .placeOrder(
                        market_id_u256,
                        0, // Bid
                        1, // GTC
                        U256::from(tick),
                        lots,
                    )
                    .send()
                    .await
                {
                    Ok(pending) => match pending.get_receipt().await {
                        Ok(receipt) => {
                            let order_id = parse_order_id_from_receipt(&receipt)
                                .unwrap_or(U256::ZERO);
                            info!(
                                market_id,
                                side = "bid",
                                tick,
                                lots = self.config.lots_per_level,
                                tx = %receipt.transaction_hash,
                                order_id = %order_id,
                                "order placed"
                            );
                            bid_ids.push(order_id);
                        }
                        Err(e) => {
                            warn!(market_id, tick, err = %e, "bid order receipt failed");
                        }
                    },
                    Err(e) => {
                        warn!(market_id, tick, err = %e, "bid order send failed");
                    }
                }
            }
        }

        // Place ask levels
        for level in 0..self.config.num_levels {
            let tick = ask_tick.saturating_add(level * 2);
            if tick > 99 {
                continue;
            }

            let lots_i64 = -(self.config.lots_per_level as i64);
            if !risk.can_place(market_id, lots_i64) {
                warn!(market_id, tick, "risk limit — skipping ask");
                continue;
            }

            if self.dry_run {
                info!(
                    market_id,
                    side = "ask",
                    tick,
                    lots = self.config.lots_per_level,
                    "[DRY RUN] would place order"
                );
                ask_ids.push(U256::ZERO);
            } else {
                match self
                    .order_book
                    .placeOrder(
                        market_id_u256,
                        1, // Ask
                        1, // GTC
                        U256::from(tick),
                        lots,
                    )
                    .send()
                    .await
                {
                    Ok(pending) => match pending.get_receipt().await {
                        Ok(receipt) => {
                            let order_id = parse_order_id_from_receipt(&receipt)
                                .unwrap_or(U256::ZERO);
                            info!(
                                market_id,
                                side = "ask",
                                tick,
                                lots = self.config.lots_per_level,
                                tx = %receipt.transaction_hash,
                                order_id = %order_id,
                                "order placed"
                            );
                            ask_ids.push(order_id);
                        }
                        Err(e) => {
                            warn!(market_id, tick, err = %e, "ask order receipt failed");
                        }
                    },
                    Err(e) => {
                        warn!(market_id, tick, err = %e, "ask order send failed");
                    }
                }
            }
        }

        self.active_orders.insert(
            market_id,
            MarketOrders {
                bid_order_ids: bid_ids,
                ask_order_ids: ask_ids,
                last_bid_tick: bid_tick,
                last_ask_tick: ask_tick,
                last_fair_tick: fair_tick,
                last_quote_time: Instant::now(),
            },
        );

        Ok(())
    }

    /// Cancel all active orders for a market.
    pub async fn cancel_all(&mut self, market_id: u64) -> Result<()> {
        let orders = match self.active_orders.remove(&market_id) {
            Some(o) => o,
            None => return Ok(()),
        };

        let all_ids: Vec<U256> = orders
            .bid_order_ids
            .iter()
            .chain(orders.ask_order_ids.iter())
            .copied()
            .filter(|id| *id != U256::ZERO)
            .collect();

        for order_id in &all_ids {
            if self.dry_run {
                info!(market_id, order_id = %order_id, "[DRY RUN] would cancel order");
            } else {
                match self.order_book.cancelOrder(*order_id).send().await {
                    Ok(pending) => match pending.get_receipt().await {
                        Ok(receipt) => {
                            info!(
                                market_id,
                                order_id = %order_id,
                                tx = %receipt.transaction_hash,
                                "order cancelled"
                            );
                        }
                        Err(e) => {
                            warn!(market_id, order_id = %order_id, err = %e, "cancel receipt failed");
                        }
                    },
                    Err(e) => {
                        warn!(market_id, order_id = %order_id, err = %e, "cancel send failed");
                    }
                }
            }
        }

        info!(market_id, count = all_ids.len(), "cancelled all orders");
        Ok(())
    }

    /// Cancel all orders across ALL markets (for shutdown / stale data guard).
    pub async fn cancel_everything(&mut self) -> Result<()> {
        let market_ids: Vec<u64> = self.active_orders.keys().copied().collect();
        for market_id in market_ids {
            self.cancel_all(market_id).await?;
        }
        Ok(())
    }

    /// Check if a market needs requoting based on fair tick movement.
    /// Requotes when |new_fair_tick - last_fair_tick| >= requote_cents.
    pub fn needs_requote(
        &self,
        market_id: u64,
        new_fair_tick: i64,
    ) -> bool {
        let orders = match self.active_orders.get(&market_id) {
            Some(o) => o,
            None => return true, // No existing orders → need to quote
        };

        // Check cooldown
        if orders.last_quote_time.elapsed().as_secs() < self.config.requote_cooldown_secs {
            return false;
        }

        // Check if fair tick has moved enough
        let fair_diff = (new_fair_tick - orders.last_fair_tick).unsigned_abs();
        fair_diff >= self.config.requote_cents
    }

    /// Requote: cancel existing orders and place new ones.
    pub async fn requote(
        &mut self,
        market_id: u64,
        bid_tick: u64,
        ask_tick: u64,
        fair_tick: i64,
        risk: &mut RiskManager,
    ) -> Result<()> {
        self.cancel_all(market_id).await?;
        self.place_quotes(market_id, bid_tick, ask_tick, fair_tick, risk).await?;
        info!(market_id, bid_tick, ask_tick, "requoted");
        Ok(())
    }

    /// Check if we're currently quoting a market.
    pub fn is_quoting(&self, market_id: u64) -> bool {
        self.active_orders.contains_key(&market_id)
    }

    /// Get list of markets we're quoting.
    pub fn quoting_markets(&self) -> Vec<u64> {
        self.active_orders.keys().copied().collect()
    }
}

/// Approve the Vault contract to spend USDT on behalf of the signer.
pub async fn approve_vault<P>(
    usdt_addr: Address,
    vault_addr: Address,
    provider: P,
) -> Result<()>
where
    P: Provider + Clone,
{
    let usdt = MockUSDT::new(usdt_addr, provider);
    let max_approval = U256::MAX;

    info!("approving vault for max USDT spend...");
    let pending = usdt
        .approve(vault_addr, max_approval)
        .send()
        .await
        .wrap_err("approve send failed")?;
    let receipt = pending
        .get_receipt()
        .await
        .wrap_err("approve receipt failed")?;
    info!(tx = %receipt.transaction_hash, "vault approved for USDT");

    Ok(())
}
