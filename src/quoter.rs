use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use eyre::{Result, WrapErr};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tracing::{info, warn};

/// Multicall3 at canonical address (deployed on all major chains)
const MULTICALL3: Address = Address::new([
    0xca, 0x11, 0xbd, 0xe0, 0x59, 0x77, 0xb3, 0x63, 0x11, 0x67,
    0x02, 0x88, 0x62, 0xbE, 0x2a, 0x17, 0x39, 0x76, 0xCA, 0x11,
]);

sol! {
    struct Call3 {
        address target;
        bool allowFailure;
        bytes callData;
    }

    struct MulticallResult {
        bool success;
        bytes returnData;
    }

    function aggregate3(Call3[] calldata calls) external payable returns (MulticallResult[] memory returnData);
}

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
    /// Local nonce counter — incremented after each successful send
    nonce: AtomicU64,
    nonce_initialized: bool,
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
            nonce: AtomicU64::new(0),
            nonce_initialized: false,
        }
    }

    /// Sync nonce from the chain (call once at startup or after errors).
    pub async fn sync_nonce(&mut self, address: Address) -> Result<()> {
        let n = self.order_book.provider().get_transaction_count(address).await
            .wrap_err("failed to get nonce")?;
        self.nonce.store(n, Ordering::SeqCst);
        self.nonce_initialized = true;
        info!(nonce = n, "nonce synced from chain");
        Ok(())
    }

    /// Get next nonce and increment.
    fn next_nonce(&self) -> u64 {
        self.nonce.fetch_add(1, Ordering::SeqCst)
    }

    /// Place bid and ask orders for a market at computed ticks.
    /// Waits for receipts and parses OrderPlaced events to track order IDs locally.
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

        let mut bid_order_ids = Vec::new();
        let mut ask_order_ids = Vec::new();

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
                info!(market_id, side = "bid", tick, lots = self.config.lots_per_level, "[DRY RUN] would place order");
            } else {
                let nonce = self.next_nonce();
                match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    async {
                        let pending = self.order_book.placeOrder(market_id_u256, 0, 1, U256::from(tick), lots)
                            .nonce(nonce).send().await?;
                        let receipt = pending.get_receipt().await?;
                        Ok::<_, alloy::contract::Error>(receipt)
                    },
                ).await {
                    Ok(Ok(receipt)) => {
                        if let Some(order_id) = parse_order_id_from_receipt(&receipt) {
                            info!(market_id, side = "bid", tick, lots = self.config.lots_per_level, nonce, order_id = %order_id, tx = %receipt.transaction_hash, "order placed");
                            bid_order_ids.push(order_id);
                        } else {
                            warn!(market_id, tick, nonce, tx = %receipt.transaction_hash, "bid order mined but no OrderPlaced event");
                        }
                    }
                    Ok(Err(e)) => {
                        warn!(market_id, tick, nonce, err = %e, "bid order failed");
                    }
                    Err(_) => {
                        warn!(market_id, tick, nonce, "bid order timed out after 15s");
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
                info!(market_id, side = "ask", tick, lots = self.config.lots_per_level, "[DRY RUN] would place order");
            } else {
                let nonce = self.next_nonce();
                match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    async {
                        let pending = self.order_book.placeOrder(market_id_u256, 1, 1, U256::from(tick), lots)
                            .nonce(nonce).send().await?;
                        let receipt = pending.get_receipt().await?;
                        Ok::<_, alloy::contract::Error>(receipt)
                    },
                ).await {
                    Ok(Ok(receipt)) => {
                        if let Some(order_id) = parse_order_id_from_receipt(&receipt) {
                            info!(market_id, side = "ask", tick, lots = self.config.lots_per_level, nonce, order_id = %order_id, tx = %receipt.transaction_hash, "order placed");
                            ask_order_ids.push(order_id);
                        } else {
                            warn!(market_id, tick, nonce, tx = %receipt.transaction_hash, "ask order mined but no OrderPlaced event");
                        }
                    }
                    Ok(Err(e)) => {
                        warn!(market_id, tick, nonce, err = %e, "ask order failed");
                    }
                    Err(_) => {
                        warn!(market_id, tick, nonce, "ask order timed out after 15s");
                    }
                }
            }
        }

        let placed_count = bid_order_ids.len() + ask_order_ids.len();
        info!(market_id, placed_count, bid_tick, ask_tick, "quotes placed");

        self.active_orders.insert(
            market_id,
            MarketOrders {
                bid_order_ids,
                ask_order_ids,
                last_bid_tick: bid_tick,
                last_ask_tick: ask_tick,
                last_fair_tick: fair_tick,
                last_quote_time: Instant::now(),
            },
        );

        Ok(())
    }

    /// Cancel all locally-tracked orders for a market via multicall.
    pub async fn cancel_local_orders(&mut self, market_id: u64) -> Result<()> {
        let order_ids: Vec<U256> = match self.active_orders.get(&market_id) {
            Some(orders) => orders.bid_order_ids.iter()
                .chain(orders.ask_order_ids.iter())
                .copied()
                .collect(),
            None => return Ok(()),
        };

        if order_ids.is_empty() {
            self.active_orders.remove(&market_id);
            return Ok(());
        }

        info!(market_id, count = order_ids.len(), "cancelling locally-tracked orders via multicall");

        if self.dry_run {
            for order_id in &order_ids {
                info!(market_id, order_id = %order_id, "[DRY RUN] would cancel");
            }
            self.active_orders.remove(&market_id);
            return Ok(());
        }

        let ob_addr = *self.order_book.address();
        let calls: Vec<Call3> = order_ids
            .iter()
            .map(|order_id| {
                let calldata = OrderBook::cancelOrderCall { orderId: *order_id }.abi_encode();
                Call3 {
                    target: ob_addr,
                    allowFailure: true,
                    callData: Bytes::from(calldata),
                }
            })
            .collect();

        let multicall_data = aggregate3Call { calls }.abi_encode();

        let nonce = self.next_nonce();
        let mut tx = alloy::rpc::types::TransactionRequest::default()
            .to(MULTICALL3)
            .input(Bytes::from(multicall_data).into())
            .nonce(nonce);
        tx.gas = Some(1_000_000);

        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.order_book.provider().send_transaction(tx),
        ).await {
            Ok(Ok(pending)) => {
                info!(
                    market_id,
                    count = order_ids.len(),
                    nonce,
                    tx = %pending.tx_hash(),
                    "multicall cancel sent"
                );
            }
            Ok(Err(e)) => {
                warn!(market_id, err = %e, "multicall cancel failed");
            }
            Err(_) => {
                warn!(market_id, "multicall cancel timed out after 15s");
            }
        }

        self.active_orders.remove(&market_id);
        Ok(())
    }

    /// Cancel all open orders for this market by querying the indexer for order IDs.
    pub async fn cancel_via_indexer(
        &mut self,
        market_id: u64,
        http_client: &reqwest::Client,
        indexer_url: &str,
        mm_address: &str,
    ) -> Result<()> {
        // Fetch our open orders from the indexer
        let url = format!("{indexer_url}/positions/{mm_address}");
        let resp = match http_client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(market_id, err = %e, "cancel: failed to fetch positions");
                return Ok(());
            }
        };

        let data: serde_json::Value = match resp.json().await {
            Ok(d) => d,
            Err(e) => {
                warn!(market_id, err = %e, "cancel: failed to parse positions");
                return Ok(());
            }
        };

        let open_orders = data["open_orders"].as_array();
        let order_ids: Vec<U256> = open_orders
            .map(|orders| {
                orders
                    .iter()
                    .filter(|o| {
                        o["market_id"].as_i64() == Some(market_id as i64)
                            && o["status"].as_str() == Some("open")
                    })
                    .filter_map(|o| o["id"].as_i64().map(|id| U256::from(id as u64)))
                    .collect()
            })
            .unwrap_or_default();

        if order_ids.is_empty() {
            return Ok(());
        }

        info!(market_id, count = order_ids.len(), "cancelling orders via multicall");

        if self.dry_run {
            for order_id in &order_ids {
                info!(market_id, order_id = %order_id, "[DRY RUN] would cancel");
            }
            return Ok(());
        }

        // Build Multicall3 aggregate3 call — batch all cancels into 1 tx
        let ob_addr = *self.order_book.address();
        let calls: Vec<Call3> = order_ids
            .iter()
            .map(|order_id| {
                let calldata = OrderBook::cancelOrderCall { orderId: *order_id }.abi_encode();
                Call3 {
                    target: ob_addr,
                    allowFailure: true, // allow individual cancels to fail (already cancelled)
                    callData: Bytes::from(calldata),
                }
            })
            .collect();

        let multicall_data = aggregate3Call { calls }.abi_encode();

        let nonce = self.next_nonce();
        let mut tx = alloy::rpc::types::TransactionRequest::default()
            .to(MULTICALL3)
            .input(Bytes::from(multicall_data).into())
            .nonce(nonce);
        tx.gas = Some(1_000_000);

        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.order_book.provider().send_transaction(tx),
        ).await {
            Ok(Ok(pending)) => {
                info!(
                    market_id,
                    count = order_ids.len(),
                    nonce,
                    tx = %pending.tx_hash(),
                    "multicall cancel sent"
                );
            }
            Ok(Err(e)) => {
                warn!(market_id, err = %e, "multicall cancel failed");
            }
            Err(_) => {
                warn!(market_id, "multicall cancel timed out after 15s");
            }
        }

        Ok(())
    }

    /// Cancel all orders across ALL markets (for shutdown / stale data guard).
    pub async fn cancel_everything(&mut self) -> Result<()> {
        let market_ids: Vec<u64> = self.active_orders.keys().copied().collect();
        for market_id in market_ids {
            self.cancel_local_orders(market_id).await?;
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

    /// Requote: cancel locally-tracked orders and place new ones.
    pub async fn requote(
        &mut self,
        market_id: u64,
        bid_tick: u64,
        ask_tick: u64,
        fair_tick: i64,
        risk: &mut RiskManager,
        mm_addr: Address,
    ) -> Result<()> {
        // Resync nonce from chain before starting the cancel+place cycle
        self.sync_nonce(mm_addr).await?;
        self.cancel_local_orders(market_id).await?;
        self.place_quotes(market_id, bid_tick, ask_tick, fair_tick, risk).await?;
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
