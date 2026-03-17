mod binance;
mod config;
mod contracts;
mod event_state;
mod market_manager;
mod nonce_sender;
mod pricing;
mod quoter;
mod redeemer;
mod risk;

use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::Filter;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolEvent;
use clap::Parser;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{watch, Mutex};
use tracing::{error, info, warn};

use contracts::{BatchAuction, MarketFactory};
use event_state::{EventState, FillEvent};

/// BTC/USD Pyth feed ID (mainnet) as bytes32.
const BTC_USD_PRICE_ID: &str =
    "0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";

#[derive(Parser)]
#[command(name = "strike-mm", about = "Strike Market Maker Bot")]
struct Cli {
    /// Path to config TOML file
    #[arg(short, long, default_value = "config/default.toml")]
    config: PathBuf,

    /// Dry run mode — log orders without submitting transactions
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct IndexerOrder {
    id: i64,
    market_id: i64,
    side: String,
    tick: u64,
    lots: u64,
    filled_lots: u64,
    status: String,
}

#[derive(Debug, Deserialize)]
struct PositionsResponse {
    open_orders: Vec<IndexerOrder>,
    #[allow(dead_code)]
    filled_positions: Vec<serde_json::Value>,
}

/// Fetch positions for the MM wallet from the indexer.
async fn fetch_positions(
    client: &reqwest::Client,
    indexer_url: &str,
    mm_address: &str,
) -> Result<Vec<IndexerOrder>> {
    let url = format!("{indexer_url}/positions/{mm_address}");
    let resp: PositionsResponse = client
        .get(&url)
        .send()
        .await
        .wrap_err("fetching positions")?
        .json()
        .await
        .wrap_err("parsing positions response")?;
    Ok(resp.open_orders)
}

// ── WS Event Subscription Task (single connection) ──────────────────

/// All WS subscriptions on a single connection: MarketCreated, OrderSettled,
/// GtcAutoCancelled, and BatchCleared.
async fn run_ws_subscriber(
    wss_url: String,
    market_factory_addr: Address,
    batch_auction_addr: Address,
    mm_address: Address,
    shared_state: Arc<Mutex<EventState>>,
    quoter_orders: Arc<Mutex<HashMap<u64, quoter::MarketOrders>>>,
    sub_ready: Arc<tokio::sync::Notify>,
    fill_notify: Arc<tokio::sync::Notify>,
    min_expiry_secs: u64,
) {
    let mut first_connect = true;
    loop {
        let ready_signal = if first_connect { Some(sub_ready.clone()) } else { None };
        first_connect = false;
        match try_subscribe_all(
            &wss_url,
            market_factory_addr,
            batch_auction_addr,
            mm_address,
            &shared_state,
            &quoter_orders,
            ready_signal,
            &fill_notify,
            min_expiry_secs,
        )
        .await
        {
            Ok(()) => {
                info!("WS subscriber exited cleanly");
                break;
            }
            Err(e) => {
                warn!(err = %e, "WS subscription dropped — reconnecting in 5s");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    }
}

async fn try_subscribe_all(
    wss_url: &str,
    market_factory_addr: Address,
    batch_auction_addr: Address,
    mm_address: Address,
    shared_state: &Arc<Mutex<EventState>>,
    quoter_orders: &Arc<Mutex<HashMap<u64, quoter::MarketOrders>>>,
    sub_ready: Option<Arc<tokio::sync::Notify>>,
    fill_notify: &Arc<tokio::sync::Notify>,
    min_expiry_secs: u64,
) -> Result<()> {
    let ws = WsConnect::new(wss_url);
    let provider = ProviderBuilder::new().connect_ws(ws).await
        .wrap_err("failed to connect WS")?;

    // MarketCreated from MarketFactory
    let mc_filter = Filter::new()
        .address(market_factory_addr)
        .event_signature(MarketFactory::MarketCreated::SIGNATURE_HASH);
    let mc_sub = provider
        .subscribe_logs(&mc_filter)
        .await
        .wrap_err("failed to subscribe to MarketCreated")?;
    info!("subscribed to MarketCreated events");

    // OrderSettled filtered by owner (topic2)
    let settled_filter = Filter::new()
        .address(batch_auction_addr)
        .event_signature(BatchAuction::OrderSettled::SIGNATURE_HASH)
        .topic2(mm_address);
    let settled_sub = provider
        .subscribe_logs(&settled_filter)
        .await
        .wrap_err("failed to subscribe to OrderSettled")?;
    info!("subscribed to OrderSettled events");

    // GtcAutoCancelled filtered by owner (topic2)
    let gtc_filter = Filter::new()
        .address(batch_auction_addr)
        .event_signature(BatchAuction::GtcAutoCancelled::SIGNATURE_HASH)
        .topic2(mm_address);
    let gtc_sub = provider
        .subscribe_logs(&gtc_filter)
        .await
        .wrap_err("failed to subscribe to GtcAutoCancelled")?;
    info!("subscribed to GtcAutoCancelled events");

    // BatchCleared (no owner filter — all markets)
    let batch_filter = Filter::new()
        .address(batch_auction_addr)
        .event_signature(BatchAuction::BatchCleared::SIGNATURE_HASH);
    let batch_sub = provider
        .subscribe_logs(&batch_filter)
        .await
        .wrap_err("failed to subscribe to BatchCleared")?;
    info!("subscribed to BatchCleared events");

    // Signal that all subscriptions are ready — main loop can start quoting
    if let Some(ready) = sub_ready {
        ready.notify_one();
        info!("signalled sub_ready — main loop unblocked");
    }

    let mut mc_stream = mc_sub.into_stream();
    let mut settled_stream = settled_sub.into_stream();
    let mut gtc_stream = gtc_sub.into_stream();
    let mut batch_stream = batch_sub.into_stream();

    use futures_util::StreamExt;

    loop {
        tokio::select! {
            Some(log) = mc_stream.next() => {
                match MarketFactory::MarketCreated::decode_log(&log.inner) {
                    Ok(event) => {
                        let price_id = format!("0x{}", alloy::hex::encode(event.priceId));
                        let order_book_market_id = event.orderBookMarketId.to::<u64>();
                        let strike_price = event.strikePrice;
                        let expiry_time = event.expiryTime.to::<i64>();

                        if price_id != BTC_USD_PRICE_ID {
                            tracing::debug!(
                                order_book_market_id,
                                price_id,
                                "ignoring non-BTC/USD MarketCreated"
                            );
                            continue;
                        }

                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs() as i64;

                        if expiry_time <= now + min_expiry_secs as i64 {
                            tracing::debug!(
                                order_book_market_id,
                                expiry_time,
                                "ignoring MarketCreated — too close to expiry"
                            );
                            continue;
                        }

                        let market = market_manager::Market {
                            id: order_book_market_id as i64,
                            expiry_time,
                            status: "active".to_string(),
                            pyth_feed_id: Some(price_id),
                            strike_price: Some(strike_price),
                            batch_interval: 3,
                        };

                        info!(
                            order_book_market_id,
                            strike_price,
                            expiry_time,
                            "MarketCreated event — new BTC/USD market discovered"
                        );

                        shared_state.lock().await.active_markets.insert(
                            order_book_market_id,
                            market,
                        );
                    }
                    Err(e) => {
                        warn!(err = %e, "failed to decode MarketCreated event");
                    }
                }
            }
            Some(log) = settled_stream.next() => {
                match BatchAuction::OrderSettled::decode_log(&log.inner) {
                    Ok(event) => {
                        let order_id = event.orderId;
                        let filled_lots = event.filledLots.to::<u64>();

                        if filled_lots == 0 {
                            tracing::debug!(order_id = %order_id, "OrderSettled with 0 lots — skipping");
                            continue;
                        }

                        let orders = quoter_orders.lock().await;
                        let mut side = "unknown".to_string();
                        let mut market_id = 0u64;
                        for (&mid, mo) in orders.iter() {
                            if mo.bid_order_ids.contains(&order_id) {
                                side = "bid".to_string();
                                market_id = mid;
                                break;
                            }
                            if mo.ask_order_ids.contains(&order_id) {
                                side = "ask".to_string();
                                market_id = mid;
                                break;
                            }
                        }
                        drop(orders);

                        if side == "unknown" {
                            tracing::debug!(
                                order_id = %order_id,
                                filled_lots,
                                "OrderSettled for unknown order — ignoring (likely from previous session)"
                            );
                            continue;
                        }

                        info!(
                            order_id = %order_id,
                            market_id,
                            side,
                            filled_lots,
                            "FILL EVENT — OrderSettled"
                        );

                        shared_state.lock().await.fills.push(FillEvent {
                            order_id: order_id.to::<u64>(),
                            market_id,
                            filled_lots,
                            side,
                        });
                        fill_notify.notify_one();
                    }
                    Err(e) => {
                        warn!(err = %e, "failed to decode OrderSettled event");
                    }
                }
            }
            Some(log) = gtc_stream.next() => {
                match BatchAuction::GtcAutoCancelled::decode_log(&log.inner) {
                    Ok(event) => {
                        info!(
                            order_id = %event.orderId,
                            "GtcAutoCancelled — order auto-cancelled by batch auction"
                        );
                    }
                    Err(e) => {
                        warn!(err = %e, "failed to decode GtcAutoCancelled event");
                    }
                }
            }
            Some(log) = batch_stream.next() => {
                match BatchAuction::BatchCleared::decode_log(&log.inner) {
                    Ok(event) => {
                        let market_id = event.marketId.to::<u64>();
                        let matched = event.matchedLots.to::<u64>();
                        info!(
                            market_id = %event.marketId,
                            batch_id = %event.batchId,
                            clearing_tick = %event.clearingTick,
                            matched_lots = matched,
                            "BatchCleared"
                        );
                        if matched > 0 {
                            // Only invalidate on actual fills — GTC zero-fill orders roll automatically
                            shared_state.lock().await.cleared_markets.insert(market_id);
                            fill_notify.notify_one();
                        }
                    }
                    Err(e) => {
                        warn!(err = %e, "failed to decode BatchCleared event");
                    }
                }
            }
            else => {
                eyre::bail!("all event streams ended");
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = config::Config::load(&cli.config)?;

    info!(
        dry_run = cli.dry_run,
        rpc = %cfg.rpc.url,
        wss = cfg.rpc.wss_url.as_deref().unwrap_or("none"),
        "starting strike-mm"
    );

    // Build provider with signer
    let private_key = cfg.private_key()?;
    let signer: PrivateKeySigner = private_key
        .parse()
        .wrap_err("failed to parse private key")?;
    let signer_addr = signer.address();
    let wallet = alloy::network::EthereumWallet::from(signer);

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(cfg.rpc.url.parse().wrap_err("failed to parse RPC URL")?);

    // BSC testnet has sub-1s blocks; alloy defaults to 7s polling which makes
    // every get_receipt() call needlessly slow.  500ms polls ~2x per block.
    provider.client().set_poll_interval(std::time::Duration::from_millis(500));

    info!(address = %signer_addr, "wallet loaded");

    // Parse contract addresses
    let order_book_addr: Address = cfg.contracts.order_book.parse().wrap_err("bad order_book address")?;
    let vault_addr: Address = cfg.contracts.vault.parse().wrap_err("bad vault address")?;
    let usdt_addr: Address = cfg.contracts.usdt.parse().wrap_err("bad usdt address")?;
    let redemption_addr: Address = cfg.contracts.redemption.parse().wrap_err("bad redemption address")?;
    let outcome_token_addr: Address = cfg.contracts.outcome_token.parse().wrap_err("bad outcome_token address")?;

    let batch_auction_addr: Option<Address> = cfg.contracts.batch_auction.as_ref()
        .map(|s| s.parse().wrap_err("bad batch_auction address"))
        .transpose()?;
    let market_factory_addr: Option<Address> = cfg.contracts.market_factory.as_ref()
        .map(|s| s.parse().wrap_err("bad market_factory address"))
        .transpose()?;

    // Approve vault for USDT spending (idempotent)
    if !cli.dry_run {
        quoter::approve_vault(usdt_addr, vault_addr, signer_addr, provider.clone()).await?;
    }

    // No sleep needed — NonceSender fetches nonce from chain after any pending TX confirms

    // Shared state
    let (price_tx, price_rx) = watch::channel(None);
    let returns: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));

    // Start Binance WebSocket in background
    let returns_clone = returns.clone();
    let realized_window = cfg.volatility.realized_window_mins;
    tokio::spawn(async move {
        if let Err(e) = binance::run_binance_ws(price_tx, returns_clone, realized_window).await {
            error!(err = %e, "binance ws fatal error");
        }
    });

    // Create shared NonceSender — all tx sends go through this
    let nonce_sender = Arc::new(Mutex::new(
        nonce_sender::NonceSender::new(
            DynProvider::new(provider.clone()),
            signer_addr,
        ).await?,
    ));

    // Start redeemer background task (every 10 min, reclaims USDT from resolved markets)
    if !cli.dry_run {
        let redeem_provider = provider.clone();
        let redeem_nonce_sender = Arc::clone(&nonce_sender);
        let redeem_indexer_url = cfg.indexer.url.clone();
        tokio::spawn(async move {
            redeemer::run_redeem_loop(
                redeem_provider,
                redeem_nonce_sender,
                redemption_addr,
                outcome_token_addr,
                signer_addr,
                redeem_indexer_url,
            )
            .await;
        });
    }

    // Initialize components
    let mut quoter = quoter::Quoter::new(
        order_book_addr,
        provider.clone(),
        Arc::clone(&nonce_sender),
        cfg.quoting.clone(),
        cli.dry_run,
    );
    let mut market_mgr = market_manager::MarketManager::new();
    let mut risk_mgr = risk::RiskManager::new(
        cfg.risk.max_position_per_market,
        cfg.risk.max_total_exposure,
    );

    let http_client = reqwest::Client::new();
    let mm_address = format!("{signer_addr:#x}");

    // Phase 1+2: On-chain state recovery and startup cancel sweep
    if !cli.dry_run {
        info!("startup: recovering live orders from chain events");
        match quoter::recover_live_orders(&provider, order_book_addr, signer_addr).await {
            Ok(live_orders) => {
                quoter.restore_state(live_orders);
                quoter.startup_cancel_sweep().await?;
            }
            Err(e) => {
                warn!(err = %e, "startup: on-chain recovery failed — falling back to indexer cleanup");
                if let Ok(orders) = fetch_positions(&http_client, &cfg.indexer.url, &mm_address).await {
                    let market_ids: Vec<u64> = orders.iter()
                        .filter(|o| o.status == "open")
                        .map(|o| o.market_id as u64)
                        .collect::<std::collections::HashSet<_>>()
                        .into_iter()
                        .collect();
                    for &market_id in &market_ids {
                        if let Err(e) = quoter.cancel_via_indexer(market_id, &http_client, &cfg.indexer.url, &mm_address).await {
                            warn!(market_id, err = %e, "startup: failed to cancel orphaned orders");
                        }
                    }
                    // NonceSender handles nonce sync internally
                }
            }
        }
    }

    // ── Event-Driven State ───────────────────────────────────────────
    let shared_state = Arc::new(Mutex::new(EventState::default()));

    // Initial market snapshot from indexer (one-time)
    info!("loading initial market snapshot from indexer");
    match market_manager::fetch_active_markets(&http_client, &cfg.indexer.url, cfg.quoting.min_expiry_secs).await {
        Ok(initial_markets) => {
            let count = initial_markets.len();
            let mut state = shared_state.lock().await;
            for m in initial_markets {
                state.active_markets.insert(m.id as u64, m);
            }
            state.initialized = true;
            info!(count, "initial market snapshot loaded");
        }
        Err(e) => {
            warn!(err = %e, "failed to load initial market snapshot — will rely on events");
            shared_state.lock().await.initialized = true;
        }
    }

    // Shared reference to quoter's active_orders for event subscribers
    // We share a snapshot that gets updated periodically from the main loop
    let quoter_orders: Arc<Mutex<HashMap<u64, quoter::MarketOrders>>> =
        Arc::new(Mutex::new(quoter.active_orders.clone()));

    // Start WS event subscriptions (if wss_url and contract addresses are configured)
    let ws_enabled = cfg.rpc.wss_url.is_some()
        && batch_auction_addr.is_some()
        && market_factory_addr.is_some();

    // fill_notify: event subscriber signals this on fills/matches so main loop wakes immediately
    let fill_notify: Option<Arc<tokio::sync::Notify>> = if ws_enabled { Some(Arc::new(tokio::sync::Notify::new())) } else { None };

    if ws_enabled {
        let wss_url = cfg.rpc.wss_url.clone().unwrap();

        // Single WS connection for all subscriptions
        let sub_ready = Arc::new(tokio::sync::Notify::new());
        let ws_wss = wss_url.clone();
        let ws_factory = market_factory_addr.unwrap();
        let ws_batch = batch_auction_addr.unwrap();
        let ws_state = shared_state.clone();
        let ws_orders = quoter_orders.clone();
        let ws_ready = sub_ready.clone();
        let ws_fill = fill_notify.clone().unwrap();
        let ws_min_expiry = cfg.quoting.min_expiry_secs;
        tokio::spawn(async move {
            run_ws_subscriber(
                ws_wss, ws_factory, ws_batch, signer_addr,
                ws_state, ws_orders, ws_ready, ws_fill, ws_min_expiry,
            ).await;
        });

        info!("WS event subscriptions started — waiting for subs to be ready");

        // Gate: don't enter main loop until BatchCleared subscription is confirmed
        tokio::select! {
            _ = sub_ready.notified() => {
                info!("BatchCleared subscription ready — proceeding to main loop");
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {
                warn!("BatchCleared subscription not ready after 60s — proceeding anyway");
            }
        }
    } else {
        warn!("WS subscriptions disabled — missing wss_url, batch_auction, or market_factory config");
    }

    // Track previous order states for fill detection (fallback when WS not available)
    let mut prev_order_states: HashMap<i64, IndexerOrder> = HashMap::new();

    // Graceful shutdown handler
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("SIGTERM/SIGINT received — shutting down");
        let _ = shutdown_tx.send(());
    });

    info!("entering main loop");

    let poll_interval = tokio::time::Duration::from_secs(cfg.indexer.poll_interval_secs);
    let stale_timeout = tokio::time::Duration::from_secs(cfg.risk.stale_data_timeout_secs);
    let mut interval = tokio::time::interval(poll_interval);

    loop {
        // Wait for next wake: interval tick, fill event, or shutdown
        tokio::select! {
            _ = &mut shutdown_rx => {
                info!("shutting down — cancelling all orders");
                quoter.cancel_everything().await?;
                info!("all orders cancelled, exiting");
                return Ok(());
            }
            _ = async {
                match &fill_notify {
                    Some(n) => n.notified().await,
                    None => std::future::pending().await,
                }
            } => {
                // A fill or matched batch just happened — fall through to quoting
            }
            _ = interval.tick() => {
                // Normal poll cycle — fall through to quoting
            }
        };

        {
                // Check for stale Binance data
                let price_data = price_rx.borrow().clone();
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;

                match &price_data {
                    None => {
                        warn!("no binance data yet — waiting");
                        continue;
                    }
                    Some(pd) if now_ms - pd.timestamp_ms > stale_timeout.as_millis() as u64 => {
                        warn!(
                            last_update_ms = pd.timestamp_ms,
                            "stale binance data — cancelling all orders"
                        );
                        quoter.cancel_everything().await?;
                        continue;
                    }
                    _ => {}
                }

                let pd = price_data.unwrap();
                let btc_price = pd.price;
                let price_age_ms = now_ms - pd.timestamp_ms;

                // Determine volatility
                let vol = match cfg.volatility.method.as_str() {
                    "realized" => {
                        let rets = returns.lock().await;
                        let v = pricing::realized_vol(&rets);
                        if v < 0.01 {
                            info!(
                                realized_vol = format!("{v:.6}"),
                                fallback_vol = cfg.volatility.fixed_annual_vol,
                                samples = rets.len(),
                                "realized vol too low, using fixed"
                            );
                            cfg.volatility.fixed_annual_vol
                        } else {
                            v
                        }
                    }
                    _ => cfg.volatility.fixed_annual_vol,
                };

                // ── Market Discovery ─────────────────────────────────
                let markets = if ws_enabled {
                    // Read from shared state (populated by WS events + initial snapshot)
                    let now_secs = (now_ms / 1000) as i64;
                    let state = shared_state.lock().await;
                    let active: Vec<market_manager::Market> = state
                        .active_markets
                        .values()
                        .filter(|m| {
                            m.expiry_time > now_secs + cfg.quoting.min_expiry_secs as i64
                        })
                        .cloned()
                        .collect();
                    drop(state);

                    if active.is_empty() {
                        info!("no active markets with >{} secs to expiry", cfg.quoting.min_expiry_secs);
                    }
                    active
                } else {
                    // Fallback: poll indexer
                    match market_manager::fetch_active_markets(
                        &http_client,
                        &cfg.indexer.url,
                        cfg.quoting.min_expiry_secs,
                    ).await {
                        Ok(m) => {
                            if m.is_empty() {
                                info!("no active markets with >{} secs to expiry", cfg.quoting.min_expiry_secs);
                            }
                            m
                        }
                        Err(e) => {
                            warn!(err = %e, "failed to fetch markets — skipping cycle");
                            continue;
                        }
                    }
                };

                let (new_markets, expired_markets) = market_mgr.reconcile(&markets);

                for m in &new_markets {
                    let strike_usd = m.strike_price.map(pricing::pyth_price_to_f64).unwrap_or(0.0);
                    let secs_left = m.expiry_time - (now_ms / 1000) as i64;
                    info!(
                        market_id = m.id,
                        strike = format!("{strike_usd:.2}"),
                        secs_to_expiry = secs_left,
                        batch_interval = m.batch_interval,
                        "NEW MARKET — starting to quote"
                    );
                }

                // Cancel orders on expired markets and clean up shared state
                for market_id in &expired_markets {
                    info!(market_id, "MARKET EXPIRED — cancelling all orders");
                    if !quoter.cancel_local_orders_batch(*market_id).await? {
                        quoter.cancel_local_orders(*market_id).await?;
                    }
                    let final_pos = risk_mgr.position(*market_id);
                    info!(market_id, final_position = final_pos, "final position on expired market");
                    risk_mgr.remove_market(*market_id);

                    // Remove from shared state
                    if ws_enabled {
                        shared_state.lock().await.active_markets.remove(market_id);
                    }
                }

                // ── Fill Detection ───────────────────────────────────
                if ws_enabled {
                    // Drain fills from shared state (populated by WS events)
                    let mut state = shared_state.lock().await;
                    let pending_fills: Vec<FillEvent> = state.fills.drain(..).collect();
                    drop(state);

                    for fill in &pending_fills {
                        let sign: i64 = if fill.side == "bid" { 1 } else { -1 };
                        let lots = fill.filled_lots as i64 * sign;
                        info!(
                            order_id = fill.order_id,
                            market_id = fill.market_id,
                            side = %fill.side,
                            filled_lots = fill.filled_lots,
                            signed_lots = lots,
                            "FILL DETECTED (event) — position updated"
                        );
                        risk_mgr.record_fill(fill.market_id, lots);
                    }
                } else {
                    // Fallback: poll indexer for fill tracking
                    match fetch_positions(&http_client, &cfg.indexer.url, &mm_address).await {
                        Ok(orders) => {
                            let mut new_states: HashMap<i64, IndexerOrder> = HashMap::new();
                            for order in &orders {
                                // Detect transitions to filled
                                if order.status == "filled" {
                                    if let Some(prev) = prev_order_states.get(&order.id) {
                                        if prev.status != "filled" {
                                            let sign: i64 = if order.side == "bid" { 1 } else { -1 };
                                            let lots = order.lots as i64 * sign;
                                            info!(
                                                order_id = order.id,
                                                market_id = order.market_id,
                                                side = %order.side,
                                                tick = order.tick,
                                                lots = order.lots,
                                                signed_lots = lots,
                                                "FILL DETECTED — position updated"
                                            );
                                            risk_mgr.record_fill(order.market_id as u64, lots);
                                        }
                                    }
                                }
                                new_states.insert(order.id, order.clone());
                            }
                            prev_order_states = new_states;
                        }
                        Err(e) => {
                            warn!(err = %e, "failed to fetch positions — skipping fill check");
                        }
                    }
                }

                // ── Batch-filled: force requote ──────────────────────
                // When a batch has fills, our orders' lots changed on-chain.
                // Force a requote (replaceOrders) to cancel stale orders and
                // place fresh ones. Do NOT remove active_orders — we need the
                // IDs so replaceOrders can cancel them properly. Otherwise
                // placeOrders leaves old GTC orders alive and they can self-cross.
                if ws_enabled {
                    let cleared: Vec<u64> = {
                        let mut state = shared_state.lock().await;
                        state.cleared_markets.drain().collect()
                    };
                    for mid in cleared {
                        if let Some(orders) = quoter.active_orders.get_mut(&mid) {
                            info!(market_id = mid, "batch filled — forcing requote");
                            // Reset last_fair_tick to force needs_requote() = true
                            orders.last_fair_tick = -1;
                        }
                    }
                }

                // ── Quoting ──────────────────────────────────────────
                let all_active: Vec<market_manager::Market> = markets;
                for market in &all_active {
                    let market_id = market.id as u64;
                    let strike = match market.strike_price {
                        Some(sp) => pricing::pyth_price_to_f64(sp),
                        None => {
                            warn!(market_id, "no strike price — skipping market");
                            continue;
                        }
                    };

                    let tte = pricing::time_to_expiry_years(market.expiry_time);
                    let secs_left = (tte * 365.25 * 24.0 * 3600.0) as i64;
                    if tte <= 0.0 {
                        info!(market_id, "market expired (tte<=0) — skipping quote");
                        continue;
                    }

                    let fair = pricing::fair_value(btc_price, strike, vol, tte);
                    let fair_tick = (fair * 100.0).round() as i64;

                    // Clamp fair_tick to valid range (ticks 1-99)
                    let fair_tick = fair_tick.clamp(1, 99);

                    let position = risk_mgr.position(market_id);
                    let skew = risk_mgr.inventory_skew(market_id, cfg.quoting.spread_ticks as i64);
                    let (bid_tick, ask_tick) =
                        pricing::compute_ticks(fair, cfg.quoting.spread_ticks, skew);

                    if quoter.needs_requote(market_id, fair_tick) {
                        let reason = if !quoter.is_quoting(market_id) {
                            "initial quote"
                        } else {
                            "fair value moved >= requote threshold"
                        };
                        info!(
                            market_id,
                            reason,
                            btc_price = format!("{btc_price:.2}"),
                            strike = format!("{strike:.2}"),
                            price_diff = format!("{:.2}", btc_price - strike),
                            vol = format!("{vol:.4}"),
                            secs_left,
                            fair = format!("{fair:.4}"),
                            fair_tick,
                            position,
                            skew,
                            bid_tick,
                            ask_tick,
                            spread = ask_tick as i64 - bid_tick as i64,
                            price_age_ms,
                            "REQUOTING"
                        );
                        quoter
                            .requote(market_id, bid_tick, ask_tick, fair_tick, &mut risk_mgr)
                            .await?;
                        // Sync immediately so event subscriber can look up new order IDs
                        if ws_enabled {
                            *quoter_orders.lock().await = quoter.active_orders.clone();
                        }
                    } else {
                        // Log why we didn't requote (at debug level to avoid spam)
                        if let Some(orders) = quoter.active_orders.get(&market_id) {
                            let fair_diff = (fair_tick - orders.last_fair_tick).unsigned_abs();
                            let cooldown_remaining = cfg.quoting.requote_cooldown_secs
                                .saturating_sub(orders.last_quote_time.elapsed().as_secs());
                            tracing::debug!(
                                market_id,
                                fair_tick,
                                last_fair_tick = orders.last_fair_tick,
                                fair_diff,
                                requote_threshold = cfg.quoting.requote_cents,
                                cooldown_remaining_secs = cooldown_remaining,
                                "no requote needed"
                            );
                        }
                    }
                }

                // Sync quoter active_orders to shared reference for event subscribers
                if ws_enabled {
                    *quoter_orders.lock().await = quoter.active_orders.clone();
                }
        }
    }
}
