use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol;
use alloy::sol_types::SolEvent;
use eyre::{Result, WrapErr};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tracing::{info, warn};

/// BSC testnet OrderBook deployment block — scan from here on startup
const DEPLOYMENT_BLOCK: u64 = 95285319;

/// Maximum block range per log query (BSC testnet RPCs reject large ranges)
const LOG_SCAN_CHUNK_SIZE: u64 = 50_000;

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

/// Extract ALL orderIds from OrderPlaced events in a transaction receipt.
fn parse_order_ids_from_receipt(receipt: &alloy::rpc::types::TransactionReceipt) -> Vec<U256> {
    let mut ids = Vec::new();
    for log in receipt.inner.logs() {
        if let Ok(event) = OrderBook::OrderPlaced::decode_log(&log.inner) {
            ids.push(event.orderId);
        }
    }
    ids
}

// ── Phase 1: On-Chain State Recovery ──────────────────────────────────

/// Recovered order info from an OrderPlaced event.
struct RecoveredOrder {
    order_id: U256,
    market_id: u64,
    side: u8, // 0 = bid, 1 = ask
}

/// Scan chain events to find orders placed by `owner` that haven't been cancelled.
/// Returns a map of market_id → (bid_order_ids, ask_order_ids).
/// Scans in chunks of 50,000 blocks to avoid RPC range limits.
pub async fn recover_live_orders<P: Provider + Clone>(
    provider: &P,
    order_book_addr: Address,
    owner: Address,
) -> Result<HashMap<u64, (Vec<U256>, Vec<U256>)>> {
    let latest_block = provider.get_block_number().await
        .wrap_err("failed to get latest block")?;

    info!(
        from_block = DEPLOYMENT_BLOCK,
        to_block = latest_block,
        owner = %owner,
        "scanning chain for live orders (chunked, chunk_size={})",
        LOG_SCAN_CHUNK_SIZE
    );

    let mut placed_orders: Vec<RecoveredOrder> = Vec::new();
    let mut cancelled_ids: HashSet<U256> = HashSet::new();

    // Scan in chunks
    let mut chunk_start = DEPLOYMENT_BLOCK;
    while chunk_start <= latest_block {
        let chunk_end = (chunk_start + LOG_SCAN_CHUNK_SIZE - 1).min(latest_block);

        // Query OrderPlaced events filtered by owner (topic3)
        let placed_filter = Filter::new()
            .address(order_book_addr)
            .event_signature(OrderBook::OrderPlaced::SIGNATURE_HASH)
            .topic3(owner)
            .from_block(chunk_start)
            .to_block(chunk_end);

        match provider.get_logs(&placed_filter).await {
            Ok(placed_logs) => {
                for log in &placed_logs {
                    if let Ok(event) = OrderBook::OrderPlaced::decode_log(&log.inner) {
                        placed_orders.push(RecoveredOrder {
                            order_id: event.orderId,
                            market_id: event.marketId.to::<u64>(),
                            side: event.side,
                        });
                    }
                }
            }
            Err(e) => {
                warn!(chunk_start, chunk_end, err = %e, "failed to fetch OrderPlaced logs for chunk — skipping");
            }
        }

        // Query OrderCancelled events filtered by owner (topic3)
        let cancelled_filter = Filter::new()
            .address(order_book_addr)
            .event_signature(OrderBook::OrderCancelled::SIGNATURE_HASH)
            .topic3(owner)
            .from_block(chunk_start)
            .to_block(chunk_end);

        match provider.get_logs(&cancelled_filter).await {
            Ok(cancelled_logs) => {
                for log in &cancelled_logs {
                    if let Ok(event) = OrderBook::OrderCancelled::decode_log(&log.inner) {
                        cancelled_ids.insert(event.orderId);
                    }
                }
            }
            Err(e) => {
                warn!(chunk_start, chunk_end, err = %e, "failed to fetch OrderCancelled logs for chunk — skipping");
            }
        }

        chunk_start = chunk_end + 1;
    }

    info!(placed_count = placed_orders.len(), cancelled_count = cancelled_ids.len(), "event scan complete");

    if placed_orders.is_empty() {
        return Ok(HashMap::new());
    }

    // Build live orders: placed but not cancelled
    let mut live: HashMap<u64, (Vec<U256>, Vec<U256>)> = HashMap::new();
    for order in placed_orders {
        if cancelled_ids.contains(&order.order_id) {
            continue;
        }
        let entry = live.entry(order.market_id).or_insert_with(|| (Vec::new(), Vec::new()));
        if order.side == 0 {
            entry.0.push(order.order_id);
        } else {
            entry.1.push(order.order_id);
        }
    }

    let total_live: usize = live.values().map(|(b, a)| b.len() + a.len()).sum();
    info!(
        live_orders = total_live,
        markets = live.len(),
        "on-chain state recovery complete"
    );

    Ok(live)
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
    /// Local nonce counter — incremented after each confirmed tx receipt
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

    /// Get current nonce without incrementing. Caller increments after confirmed receipt.
    fn current_nonce(&self) -> u64 {
        self.nonce.load(Ordering::SeqCst)
    }

    /// Increment nonce by 1 after a confirmed receipt.
    fn increment_nonce(&self) {
        self.nonce.fetch_add(1, Ordering::SeqCst);
    }

    // ── Phase 1: Restore recovered state ──────────────────────────────

    /// Restore active_orders from on-chain recovery results.
    pub fn restore_state(&mut self, live_orders: HashMap<u64, (Vec<U256>, Vec<U256>)>) {
        for (market_id, (bids, asks)) in live_orders {
            let count = bids.len() + asks.len();
            if count > 0 {
                info!(market_id, bids = bids.len(), asks = asks.len(), "restoring recovered orders");
                self.active_orders.insert(market_id, MarketOrders {
                    bid_order_ids: bids,
                    ask_order_ids: asks,
                    last_bid_tick: 0,
                    last_ask_tick: 0,
                    last_fair_tick: 0,
                    last_quote_time: Instant::now(),
                });
            }
        }
    }

    // ── Phase 2: Startup Cancel Sweep ─────────────────────────────────

    /// Cancel ALL recovered live orders via sequential individual cancelOrder calls.
    /// Catches errors per-order and continues (orders may already be expired/settled).
    pub async fn startup_cancel_sweep(&mut self) -> Result<()> {
        let all_ids: Vec<U256> = self.active_orders.values()
            .flat_map(|m| m.bid_order_ids.iter().chain(m.ask_order_ids.iter()))
            .copied()
            .collect();

        if all_ids.is_empty() {
            info!("startup cancel sweep: no orders to cancel");
            return Ok(());
        }

        info!(count = all_ids.len(), "startup cancel sweep: cancelling recovered orders sequentially");

        if self.dry_run {
            for order_id in &all_ids {
                info!(order_id = %order_id, "[DRY RUN] would cancel");
            }
            self.active_orders.clear();
            return Ok(());
        }

        for order_id in &all_ids {
            let nonce = self.current_nonce();
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.order_book.cancelOrder(*order_id).nonce(nonce).gas(200_000).send(),
            ).await {
                Ok(Ok(pending)) => {
                    let tx_hash = *pending.tx_hash();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        pending.get_receipt(),
                    ).await {
                        Ok(Ok(receipt)) => {
                            info!(order_id = %order_id, tx = %tx_hash, gas_used = receipt.gas_used, "startup cancel confirmed");
                            self.increment_nonce();
                        }
                        Ok(Err(e)) => {
                            warn!(order_id = %order_id, tx = %tx_hash, err = %e, "startup cancel receipt error — continuing");
                            self.increment_nonce(); // nonce was consumed
                        }
                        Err(_) => {
                            warn!(order_id = %order_id, tx = %tx_hash, "startup cancel receipt timed out — continuing");
                            self.increment_nonce();
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!(order_id = %order_id, err = %e, "startup cancel send failed — continuing");
                }
                Err(_) => {
                    warn!(order_id = %order_id, "startup cancel send timed out — continuing");
                }
            }
        }

        self.active_orders.clear();
        Ok(())
    }

    // ── Phase 3: Sequential Cancel + Quote ────────────────────────────

    /// Place initial quotes for a market via sequential individual placeOrder calls.
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

        let mut bid_order_ids: Vec<U256> = Vec::new();
        let mut ask_order_ids: Vec<U256> = Vec::new();

        // Place bid levels
        for level in 0..self.config.num_levels {
            let tick = bid_tick.saturating_sub(level * 2);
            if tick < 1 { continue; }
            if !risk.can_place(market_id, self.config.lots_per_level as i64) {
                warn!(market_id, tick, "risk limit — skipping bid");
                continue;
            }
            if self.dry_run {
                info!(market_id, side = "bid", tick, lots = self.config.lots_per_level, "[DRY RUN] would place order");
                continue;
            }

            let nonce = self.current_nonce();
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.order_book
                    .placeOrder(market_id_u256, 0, 1, U256::from(tick), lots)
                    .nonce(nonce)
                    .gas(500_000)
                    .send(),
            ).await {
                Ok(Ok(pending)) => {
                    let tx_hash = *pending.tx_hash();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        pending.get_receipt(),
                    ).await {
                        Ok(Ok(receipt)) => {
                            let ids = parse_order_ids_from_receipt(&receipt);
                            info!(
                                market_id, side = "bid", tick, nonce,
                                tx = %tx_hash, gas_used = receipt.gas_used,
                                order_ids = ?ids, "bid placed"
                            );
                            self.increment_nonce();
                            bid_order_ids.extend(ids);
                        }
                        Ok(Err(e)) => {
                            warn!(market_id, side = "bid", tick, tx = %tx_hash, err = %e, "place receipt error");
                            self.increment_nonce();
                        }
                        Err(_) => {
                            warn!(market_id, side = "bid", tick, tx = %tx_hash, "place receipt timed out");
                            self.increment_nonce();
                        }
                    }
                }
                Ok(Err(e)) => {
                    let err_str = e.to_string();
                    if err_str.contains("nonce") {
                        warn!(market_id, side = "bid", tick, err = %e, "nonce error placing bid — aborting remaining placements");
                        break;
                    }
                    warn!(market_id, side = "bid", tick, err = %e, "place send failed");
                }
                Err(_) => {
                    warn!(market_id, side = "bid", tick, "place send timed out");
                }
            }
        }

        // Place ask levels
        for level in 0..self.config.num_levels {
            let tick = ask_tick.saturating_add(level * 2);
            if tick > 99 { continue; }
            if !risk.can_place(market_id, -(self.config.lots_per_level as i64)) {
                warn!(market_id, tick, "risk limit — skipping ask");
                continue;
            }
            if self.dry_run {
                info!(market_id, side = "ask", tick, lots = self.config.lots_per_level, "[DRY RUN] would place order");
                continue;
            }

            let nonce = self.current_nonce();
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.order_book
                    .placeOrder(market_id_u256, 1, 1, U256::from(tick), lots)
                    .nonce(nonce)
                    .gas(500_000)
                    .send(),
            ).await {
                Ok(Ok(pending)) => {
                    let tx_hash = *pending.tx_hash();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        pending.get_receipt(),
                    ).await {
                        Ok(Ok(receipt)) => {
                            let ids = parse_order_ids_from_receipt(&receipt);
                            info!(
                                market_id, side = "ask", tick, nonce,
                                tx = %tx_hash, gas_used = receipt.gas_used,
                                order_ids = ?ids, "ask placed"
                            );
                            self.increment_nonce();
                            ask_order_ids.extend(ids);
                        }
                        Ok(Err(e)) => {
                            warn!(market_id, side = "ask", tick, tx = %tx_hash, err = %e, "place receipt error");
                            self.increment_nonce();
                        }
                        Err(_) => {
                            warn!(market_id, side = "ask", tick, tx = %tx_hash, "place receipt timed out");
                            self.increment_nonce();
                        }
                    }
                }
                Ok(Err(e)) => {
                    let err_str = e.to_string();
                    if err_str.contains("nonce") {
                        warn!(market_id, side = "ask", tick, err = %e, "nonce error placing ask — aborting remaining placements");
                        break;
                    }
                    warn!(market_id, side = "ask", tick, err = %e, "place send failed");
                }
                Err(_) => {
                    warn!(market_id, side = "ask", tick, "place send timed out");
                }
            }
        }

        self.active_orders.insert(market_id, MarketOrders {
            bid_order_ids,
            ask_order_ids,
            last_bid_tick: bid_tick,
            last_ask_tick: ask_tick,
            last_fair_tick: fair_tick,
            last_quote_time: Instant::now(),
        });
        Ok(())
    }

    /// Cancel all locally-tracked orders for a market via sequential individual calls.
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

        info!(market_id, count = order_ids.len(), "cancelling locally-tracked orders sequentially");

        if self.dry_run {
            for order_id in &order_ids {
                info!(market_id, order_id = %order_id, "[DRY RUN] would cancel");
            }
            self.active_orders.remove(&market_id);
            return Ok(());
        }

        for order_id in &order_ids {
            let nonce = self.current_nonce();
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.order_book.cancelOrder(*order_id).nonce(nonce).gas(200_000).send(),
            ).await {
                Ok(Ok(pending)) => {
                    let tx_hash = *pending.tx_hash();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        pending.get_receipt(),
                    ).await {
                        Ok(Ok(receipt)) => {
                            info!(market_id, order_id = %order_id, tx = %tx_hash, gas_used = receipt.gas_used, "cancel confirmed");
                            self.increment_nonce();
                        }
                        Ok(Err(e)) => {
                            warn!(market_id, order_id = %order_id, tx = %tx_hash, err = %e, "cancel receipt error");
                            self.increment_nonce();
                        }
                        Err(_) => {
                            warn!(market_id, order_id = %order_id, tx = %tx_hash, "cancel receipt timed out");
                            self.increment_nonce();
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!(market_id, order_id = %order_id, err = %e, "cancel send failed — continuing");
                }
                Err(_) => {
                    warn!(market_id, order_id = %order_id, "cancel send timed out — continuing");
                }
            }
        }

        self.active_orders.remove(&market_id);
        Ok(())
    }

    /// Cancel all open orders for this market by querying the indexer for order IDs.
    /// Cancels each order individually via sequential calls.
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

        info!(market_id, count = order_ids.len(), "cancelling indexer orders sequentially");

        if self.dry_run {
            for order_id in &order_ids {
                info!(market_id, order_id = %order_id, "[DRY RUN] would cancel");
            }
            return Ok(());
        }

        for order_id in &order_ids {
            let nonce = self.current_nonce();
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.order_book.cancelOrder(*order_id).nonce(nonce).gas(200_000).send(),
            ).await {
                Ok(Ok(pending)) => {
                    let tx_hash = *pending.tx_hash();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        pending.get_receipt(),
                    ).await {
                        Ok(Ok(receipt)) => {
                            info!(market_id, order_id = %order_id, tx = %tx_hash, gas_used = receipt.gas_used, "cancel confirmed");
                            self.increment_nonce();
                        }
                        Ok(Err(e)) => {
                            warn!(market_id, order_id = %order_id, tx = %tx_hash, err = %e, "cancel receipt error");
                            self.increment_nonce();
                        }
                        Err(_) => {
                            warn!(market_id, order_id = %order_id, tx = %tx_hash, "cancel receipt timed out");
                            self.increment_nonce();
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!(market_id, order_id = %order_id, err = %e, "cancel send failed — continuing");
                }
                Err(_) => {
                    warn!(market_id, order_id = %order_id, "cancel send timed out — continuing");
                }
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

    /// Requote: cancel existing orders, sync nonce, then place new ones.
    /// All calls are sequential individual transactions from the MM wallet.
    pub async fn requote(
        &mut self,
        market_id: u64,
        bid_tick: u64,
        ask_tick: u64,
        fair_tick: i64,
        risk: &mut RiskManager,
        mm_addr: Address,
    ) -> Result<()> {
        // Cancel existing orders
        self.cancel_local_orders(market_id).await?;

        // Re-sync nonce after cancels
        self.sync_nonce(mm_addr).await?;

        // Place new quotes
        self.place_quotes(market_id, bid_tick, ask_tick, fair_tick, risk).await
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
