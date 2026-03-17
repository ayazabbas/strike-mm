use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, TransactionRequest};
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use eyre::{Result, WrapErr};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::QuotingConfig;
use crate::nonce_sender::{NonceSender, PendingTx};
use crate::risk::RiskManager;

/// Which sides to quote for a market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteMode {
    TwoSided,
    BidsOnly,
    AsksOnly,
}

/// How many blocks back from head to scan on startup.
/// For 5-min markets with sub-1s blocks, 3000 blocks (~50 min) covers any live order.
const STARTUP_SCAN_LOOKBACK: u64 = 3_000;

/// Maximum block range per log query (BSC testnet RPCs reject large ranges)
const LOG_SCAN_CHUNK_SIZE: u64 = 3_000;

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

    let scan_from = latest_block.saturating_sub(STARTUP_SCAN_LOOKBACK);

    info!(
        from_block = scan_from,
        to_block = latest_block,
        owner = %owner,
        "scanning chain for live orders (chunked, chunk_size={})",
        LOG_SCAN_CHUNK_SIZE
    );

    let mut placed_orders: Vec<RecoveredOrder> = Vec::new();
    let mut cancelled_ids: HashSet<U256> = HashSet::new();

    // Scan in chunks
    let mut chunk_start = scan_from;
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
    nonce_sender: Arc<Mutex<NonceSender>>,
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
        nonce_sender: Arc<Mutex<NonceSender>>,
        config: QuotingConfig,
        dry_run: bool,
    ) -> Self {
        Self {
            order_book: OrderBook::new(order_book_addr, provider),
            nonce_sender,
            config,
            active_orders: HashMap::new(),
            dry_run,
        }
    }

    /// Helper: build a cancel TransactionRequest for a given order ID.
    fn cancel_tx(&self, order_id: U256) -> TransactionRequest {
        let calldata = OrderBook::cancelOrderCall { orderId: order_id }.abi_encode();
        let mut tx = TransactionRequest::default()
            .to(*self.order_book.address())
            .input(Bytes::from(calldata).into());
        tx.gas = Some(200_000);
        tx
    }

    /// Build OrderParam structs for bid and ask levels.
    fn build_order_params(
        &self,
        bid_tick: u64,
        ask_tick: u64,
        risk: &mut RiskManager,
        market_id: u64,
        mode: QuoteMode,
        lots_override: u64,
    ) -> (Vec<OrderBook::OrderParam>, Vec<OrderBook::OrderParam>) {
        let lots = lots_override;
        let mut bid_params = Vec::new();
        let mut ask_params = Vec::new();

        if mode != QuoteMode::AsksOnly {
            for level in 0..self.config.num_levels {
                let tick = bid_tick.saturating_sub(level * 2);
                if tick < 1 { continue; }
                if !risk.can_place(market_id, lots as i64) {
                    warn!(market_id, tick, "risk limit — skipping bid");
                    continue;
                }
                bid_params.push(OrderBook::OrderParam {
                    side: 0, // Bid
                    orderType: 1, // GTC (enum: 0=GTB, 1=GTC)
                    tick: tick as u8,
                    lots,
                });
            }
        }

        if mode != QuoteMode::BidsOnly {
            for level in 0..self.config.num_levels {
                let tick = ask_tick.saturating_add(level * 2);
                if tick > 99 { continue; }
                if !risk.can_place(market_id, -(lots as i64)) {
                    warn!(market_id, tick, "risk limit — skipping ask");
                    continue;
                }
                ask_params.push(OrderBook::OrderParam {
                    side: 1, // Ask
                    orderType: 1, // GTC (enum: 0=GTB, 1=GTC)
                    tick: tick as u8,
                    lots,
                });
            }
        }

        (bid_params, ask_params)
    }

    /// Parse OrderPlaced events from a receipt, splitting into bid/ask order IDs.
    fn parse_placed_orders(receipt: &alloy::rpc::types::TransactionReceipt) -> (Vec<U256>, Vec<U256>) {
        let mut bid_ids = Vec::new();
        let mut ask_ids = Vec::new();
        for log in receipt.inner.logs() {
            if let Ok(event) = OrderBook::OrderPlaced::decode_log(&log.inner) {
                if event.side == 0 {
                    bid_ids.push(event.orderId);
                } else {
                    ask_ids.push(event.orderId);
                }
            }
        }
        (bid_ids, ask_ids)
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
    pub async fn startup_cancel_sweep(&mut self) -> Result<()> {
        let all_ids: Vec<U256> = self.active_orders.values()
            .flat_map(|m| m.bid_order_ids.iter().chain(m.ask_order_ids.iter()))
            .copied()
            .collect();

        if all_ids.is_empty() {
            info!("startup cancel sweep: no orders to cancel");
            return Ok(());
        }

        info!(count = all_ids.len(), "startup cancel sweep: batch cancelling recovered orders");

        if self.dry_run {
            for order_id in &all_ids {
                info!(order_id = %order_id, "[DRY RUN] would cancel");
            }
            self.active_orders.clear();
            return Ok(());
        }

        // Batch cancel via cancelOrders — one TX for all orders
        let calldata = OrderBook::cancelOrdersCall { orderIds: all_ids.clone() }.abi_encode();
        let tx = alloy::rpc::types::TransactionRequest::default()
            .to(*self.order_book.address())
            .input(calldata.into())
            ;
        let mut tx = tx;
        tx.gas = Some(100_000 * all_ids.len() as u64);

        let ns = self.nonce_sender.clone();
        match tokio::time::timeout(
            std::time::Duration::from_secs(60),
            async {
                let pending = ns.lock().await.send(tx).await?;
                let tx_hash = *pending.tx_hash();
                info!(tx = %tx_hash, count = all_ids.len(), "startup batch cancel tx sent");
                let receipt = pending.get_receipt().await?;
                Ok::<_, eyre::Report>((tx_hash, receipt))
            },
        ).await {
            Ok(Ok((tx_hash, receipt))) => {
                info!(tx = %tx_hash, gas_used = receipt.gas_used, count = all_ids.len(), "startup batch cancel confirmed");
            }
            Ok(Err(e)) => {
                warn!(err = %e, "startup batch cancel failed — orders may still be live (keeper will prune)");
            }
            Err(_) => {
                warn!("startup batch cancel timed out — orders may still be live (keeper will prune)");
            }
        }

        self.active_orders.clear();
        Ok(())
    }

    // ── Batch Order Placement ──────────────────────────────────────────

    /// Place initial quotes for a market using `placeOrders` — single TX for all levels.
    pub async fn place_quotes(
        &mut self,
        market_id: u64,
        bid_tick: u64,
        ask_tick: u64,
        fair_tick: i64,
        risk: &mut RiskManager,
        mode: QuoteMode,
        lots_override: u64,
    ) -> Result<()> {
        let (bid_params, ask_params) = self.build_order_params(bid_tick, ask_tick, risk, market_id, mode, lots_override);
        let all_params: Vec<OrderBook::OrderParam> = bid_params.into_iter().chain(ask_params).collect();

        if all_params.is_empty() {
            info!(market_id, "no orders to place (all filtered by risk)");
            return Ok(());
        }

        if self.dry_run {
            for p in &all_params {
                let side = if p.side == 0 { "bid" } else { "ask" };
                info!(market_id, side, tick = p.tick, lots = p.lots, "[DRY RUN] would place order");
            }
            self.active_orders.insert(market_id, MarketOrders {
                bid_order_ids: Vec::new(),
                ask_order_ids: Vec::new(),
                last_bid_tick: bid_tick,
                last_ask_tick: ask_tick,
                last_fair_tick: fair_tick,
                last_quote_time: Instant::now(),
            });
            return Ok(());
        }

        let order_count = all_params.len();
        let market_id_u256 = U256::from(market_id);

        // Build placeOrders call
        let calldata = OrderBook::placeOrdersCall {
            marketId: market_id_u256,
            params: all_params,
        }.abi_encode();
        let mut tx = TransactionRequest::default()
            .to(*self.order_book.address())
            .input(Bytes::from(calldata).into());
        tx.gas = Some(350_000 * order_count as u64);

        let ns = self.nonce_sender.clone();
        let pending = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ns.lock().await.send(tx),
        ).await
            .map_err(|_| eyre::eyre!("placeOrders send timed out"))?
            .wrap_err("placeOrders send failed")?;

        let tx_hash = *pending.tx_hash();
        info!(market_id, order_count, tx = %tx_hash, "placeOrders tx sent");

        let receipt = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            pending.get_receipt(),
        ).await
            .map_err(|_| eyre::eyre!("placeOrders receipt timed out"))?
            .wrap_err("placeOrders receipt failed")?;

        let (bid_ids, ask_ids) = Self::parse_placed_orders(&receipt);
        info!(
            market_id,
            tx = %tx_hash,
            gas_used = receipt.gas_used,
            bids = bid_ids.len(),
            asks = ask_ids.len(),
            "placeOrders confirmed"
        );

        self.active_orders.insert(market_id, MarketOrders {
            bid_order_ids: bid_ids,
            ask_order_ids: ask_ids,
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

        let ns = self.nonce_sender.clone();
        for order_id in &order_ids {
            let tx = self.cancel_tx(*order_id);
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                ns.lock().await.send(tx),
            ).await {
                Ok(Ok(pending)) => {
                    let pending: PendingTx = pending;
                    let tx_hash = *pending.tx_hash();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        pending.get_receipt(),
                    ).await {
                        Ok(Ok(receipt)) => {
                            info!(market_id, order_id = %order_id, tx = %tx_hash, gas_used = receipt.gas_used, "cancel confirmed");
                        }
                        Ok(Err(e)) => {
                            warn!(market_id, order_id = %order_id, tx = %tx_hash, err = %e, "cancel receipt error");
                        }
                        Err(_) => {
                            warn!(market_id, order_id = %order_id, tx = %tx_hash, "cancel receipt timed out");
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

    /// Cancel all locally-tracked orders for a market in a single tx via cancelOrders().
    /// Returns Ok(true) on success, Ok(false) if batch failed (caller should fall back).
    pub async fn cancel_local_orders_batch(&mut self, market_id: u64) -> Result<bool> {
        let order_ids: Vec<U256> = match self.active_orders.get(&market_id) {
            Some(orders) => orders.bid_order_ids.iter()
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

        // Build cancelOrders call
        let calldata = OrderBook::cancelOrdersCall { orderIds: order_ids }.abi_encode();
        let mut tx = TransactionRequest::default()
            .to(*self.order_book.address())
            .input(Bytes::from(calldata).into());
        tx.gas = Some(500_000); // batch needs more gas than single cancel

        let ns = self.nonce_sender.clone();
        let pending = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ns.lock().await.send(tx),
        ).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                warn!(market_id, err = %e, "batch cancel failed — falling back to sequential");
                return Ok(false);
            }
            Err(_) => {
                warn!(market_id, "batch cancel send timed out — falling back to sequential");
                return Ok(false);
            }
        };

        let pending: PendingTx = pending;
        let tx_hash = *pending.tx_hash();
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            pending.get_receipt(),
        ).await {
            Ok(Ok(receipt)) => {
                info!(
                    market_id,
                    count,
                    tx = %receipt.transaction_hash,
                    gas_used = receipt.gas_used,
                    "batch cancel confirmed"
                );
            }
            Ok(Err(e)) => {
                warn!(market_id, tx = %tx_hash, err = %e, "batch cancel receipt error — falling back");
                return Ok(false);
            }
            Err(_) => {
                warn!(market_id, tx = %tx_hash, "batch cancel receipt timed out — falling back");
                return Ok(false);
            }
        }

        self.active_orders.remove(&market_id);
        Ok(true)
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

        info!(market_id, count = order_ids.len(), "cancelling indexer orders sequentially");

        if self.dry_run {
            for order_id in &order_ids {
                info!(market_id, order_id = %order_id, "[DRY RUN] would cancel");
            }
            return Ok(());
        }

        let ns = self.nonce_sender.clone();
        for order_id in &order_ids {
            let tx = self.cancel_tx(*order_id);
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                ns.lock().await.send(tx),
            ).await {
                Ok(Ok(pending)) => {
                    let pending: PendingTx = pending;
                    let tx_hash = *pending.tx_hash();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        pending.get_receipt(),
                    ).await {
                        Ok(Ok(receipt)) => {
                            info!(market_id, order_id = %order_id, tx = %tx_hash, gas_used = receipt.gas_used, "cancel confirmed");
                        }
                        Ok(Err(e)) => {
                            warn!(market_id, order_id = %order_id, tx = %tx_hash, err = %e, "cancel receipt error");
                        }
                        Err(_) => {
                            warn!(market_id, order_id = %order_id, tx = %tx_hash, "cancel receipt timed out");
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
            if !self.cancel_local_orders_batch(market_id).await? {
                self.cancel_local_orders(market_id).await?;
            }
        }
        Ok(())
    }

    /// Check if a market needs requoting based on fair tick movement.
    pub fn needs_requote(
        &self,
        market_id: u64,
        new_fair_tick: i64,
    ) -> bool {
        let orders = match self.active_orders.get(&market_id) {
            Some(o) => o,
            None => return true,
        };

        if orders.last_quote_time.elapsed().as_secs() < self.config.requote_cooldown_secs {
            return false;
        }

        let fair_diff = (new_fair_tick - orders.last_fair_tick).unsigned_abs();
        fair_diff >= self.config.requote_cents
    }

    /// Atomic requote: cancel existing orders and place new ones in a single TX
    /// via `replaceOrders`. Zero empty book time.
    pub async fn requote(
        &mut self,
        market_id: u64,
        bid_tick: u64,
        ask_tick: u64,
        fair_tick: i64,
        risk: &mut RiskManager,
        mode: QuoteMode,
        lots_override: u64,
    ) -> Result<()> {
        // If no existing orders, just place fresh
        let cancel_ids: Vec<U256> = match self.active_orders.get(&market_id) {
            Some(orders) => orders.bid_order_ids.iter()
                .chain(orders.ask_order_ids.iter())
                .copied()
                .collect(),
            None => return self.place_quotes(market_id, bid_tick, ask_tick, fair_tick, risk, mode, lots_override).await,
        };

        if cancel_ids.is_empty() {
            return self.place_quotes(market_id, bid_tick, ask_tick, fair_tick, risk, mode, lots_override).await;
        }

        let (bid_params, ask_params) = self.build_order_params(bid_tick, ask_tick, risk, market_id, mode, lots_override);
        let all_params: Vec<OrderBook::OrderParam> = bid_params.into_iter().chain(ask_params).collect();

        if self.dry_run {
            info!(market_id, cancel_count = cancel_ids.len(), "[DRY RUN] would replaceOrders");
            for p in &all_params {
                let side = if p.side == 0 { "bid" } else { "ask" };
                info!(market_id, side, tick = p.tick, lots = p.lots, "[DRY RUN] would place order");
            }
            self.active_orders.insert(market_id, MarketOrders {
                bid_order_ids: Vec::new(),
                ask_order_ids: Vec::new(),
                last_bid_tick: bid_tick,
                last_ask_tick: ask_tick,
                last_fair_tick: fair_tick,
                last_quote_time: Instant::now(),
            });
            return Ok(());
        }

        let cancel_count = cancel_ids.len();
        let place_count = all_params.len();
        let market_id_u256 = U256::from(market_id);

        // Build replaceOrders call — atomic cancel + place
        let calldata = OrderBook::replaceOrdersCall {
            cancelIds: cancel_ids,
            marketId: market_id_u256,
            params: all_params,
        }.abi_encode();
        let mut tx = TransactionRequest::default()
            .to(*self.order_book.address())
            .input(Bytes::from(calldata).into());
        tx.gas = Some(350_000 * (cancel_count + place_count) as u64);

        let ns = self.nonce_sender.clone();
        let pending = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ns.lock().await.send(tx),
        ).await
            .map_err(|_| eyre::eyre!("replaceOrders send timed out"))?
            .wrap_err("replaceOrders send failed")?;

        let tx_hash = *pending.tx_hash();
        info!(market_id, cancel_count, place_count, tx = %tx_hash, "replaceOrders tx sent");

        let receipt = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            pending.get_receipt(),
        ).await
            .map_err(|_| eyre::eyre!("replaceOrders receipt timed out"))?
            .wrap_err("replaceOrders receipt failed")?;

        let (bid_ids, ask_ids) = Self::parse_placed_orders(&receipt);
        info!(
            market_id,
            tx = %tx_hash,
            gas_used = receipt.gas_used,
            cancelled = cancel_count,
            bids = bid_ids.len(),
            asks = ask_ids.len(),
            "replaceOrders confirmed"
        );

        self.active_orders.insert(market_id, MarketOrders {
            bid_order_ids: bid_ids,
            ask_order_ids: ask_ids,
            last_bid_tick: bid_tick,
            last_ask_tick: ask_tick,
            last_fair_tick: fair_tick,
            last_quote_time: Instant::now(),
        });
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

    /// Look up which side an order was placed on (from active_orders).
    pub fn order_side(&self, order_id: U256) -> Option<&'static str> {
        for orders in self.active_orders.values() {
            if orders.bid_order_ids.contains(&order_id) {
                return Some("bid");
            }
            if orders.ask_order_ids.contains(&order_id) {
                return Some("ask");
            }
        }
        None
    }

    /// Look up which market an order belongs to (from active_orders).
    pub fn order_market(&self, order_id: U256) -> Option<u64> {
        for (&market_id, orders) in &self.active_orders {
            if orders.bid_order_ids.contains(&order_id)
                || orders.ask_order_ids.contains(&order_id)
            {
                return Some(market_id);
            }
        }
        None
    }
}

/// Approve the Vault contract to spend USDT on behalf of the signer.
pub async fn approve_vault<P>(
    usdt_addr: Address,
    vault_addr: Address,
    signer_addr: Address,
    provider: P,
) -> Result<()>
where
    P: Provider + Clone,
{
    let usdt = MockUSDT::new(usdt_addr, provider);
    let max_approval = U256::MAX;

    // Check current allowance — skip if already max-approved (idempotent)
    if let Ok(current) = usdt.allowance(signer_addr, vault_addr).call().await {
        if current >= (U256::MAX >> 1) {
            info!("vault already approved for USDT — skipping");
            return Ok(());
        }
    }

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
