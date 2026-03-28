mod binance;
mod config;
mod event_state;
mod market_manager;
mod pricing;
mod quoter;
mod redeemer;
mod risk;

use alloy::primitives::U256;
use clap::Parser;
use eyre::{Result, WrapErr};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{watch, Mutex};
use tracing::{error, info, warn};

use strike_sdk::indexer::types::Market;
use strike_sdk::prelude::*;

use event_state::{EventState, FillEvent};

/// BTC/USD Pyth feed ID (mainnet) as bytes32.
const BTC_USD_PRICE_ID: &str = "0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";

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

    // Build StrikeClient from config
    let private_key = cfg.private_key()?;
    let sdk_config = cfg.strike_config()?;
    let mut client = StrikeClient::new(sdk_config)
        .with_private_key(&private_key)
        .build()
        .wrap_err("failed to build StrikeClient")?;

    let signer_addr = client
        .signer_address()
        .ok_or_else(|| eyre::eyre!("no signer address — private key not set"))?;

    info!(address = %signer_addr, "wallet loaded");

    // Initialize nonce sender for transaction sequencing
    if !cli.dry_run {
        client.init_nonce_sender().await?;
    }

    // Approve vault for USDT spending (idempotent)
    if !cli.dry_run {
        client
            .vault()
            .approve_usdt()
            .await
            .wrap_err("failed to approve vault for USDT")?;
    }

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

    // Start redeemer background task (every 10 min, reclaims USDT from resolved markets)
    if !cli.dry_run {
        let redeem_client = client.clone();
        tokio::spawn(async move {
            redeemer::run_redeem_loop(redeem_client).await;
        });
    }

    // Initialize components
    let mut quoter = quoter::Quoter::new(client.clone(), cfg.quoting.clone(), cli.dry_run);
    let mut market_mgr = market_manager::MarketManager::new();
    let mut risk_mgr =
        risk::RiskManager::new(cfg.risk.max_loss_budget_usdt, cfg.risk.max_skew_ticks);

    let mm_address = format!("{signer_addr:#x}");

    // Phase 1+2: On-chain state recovery and startup cancel sweep
    if !cli.dry_run {
        info!("startup: recovering live orders from chain events");
        let from_block = client.block_number().await?.saturating_sub(5000);
        match client.scan_orders(from_block, signer_addr).await {
            Ok(live_orders) => {
                quoter.restore_state(live_orders);
                quoter.startup_cancel_sweep().await?;
            }
            Err(e) => {
                warn!(err = %e, "startup: on-chain recovery failed — falling back to indexer cleanup");
                if let Ok(orders) = client.indexer().get_open_orders(&mm_address).await {
                    let order_ids: Vec<U256> = orders
                        .iter()
                        .filter(|o| o.status == "open")
                        .map(|o| U256::from(o.id as u64))
                        .collect();
                    if !order_ids.is_empty() {
                        if let Err(e) = client.orders().cancel(&order_ids).await {
                            warn!(err = %e, "startup: failed to cancel orphaned orders via indexer");
                        }
                    }
                }
            }
        }
    }

    // ── Event-Driven State ───────────────────────────────────────────
    let shared_state = Arc::new(Mutex::new(EventState::default()));

    // Initial market snapshot from indexer (one-time)
    let http_client = reqwest::Client::new();
    info!("loading initial market snapshot from indexer");
    match market_manager::fetch_active_markets(
        &http_client,
        &cfg.indexer.url,
        cfg.quoting.min_expiry_secs,
    )
    .await
    {
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

    // Start event stream (if WSS URL is configured)
    let mut event_stream = if cfg.rpc.wss_url.is_some() {
        match client.events().await {
            Ok(stream) => {
                info!("event stream connected");
                Some(stream)
            }
            Err(e) => {
                warn!(err = %e, "failed to connect event stream — will rely on polling");
                None
            }
        }
    } else {
        warn!("no WSS URL configured — event stream disabled");
        None
    };

    let ws_enabled = event_stream.is_some();

    // Track previous order states for fill detection (fallback when events not available)
    let mut prev_order_states: HashMap<i64, strike_sdk::indexer::types::IndexerOrder> =
        HashMap::new();

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
        // Wait for next wake: interval tick, on-chain event, or shutdown
        tokio::select! {
            _ = &mut shutdown_rx => {
                info!("shutting down — cancelling all orders");
                quoter.cancel_everything().await?;
                info!("all orders cancelled, exiting");
                return Ok(());
            }
            Some(event) = async {
                if let Some(ref mut s) = event_stream {
                    s.next().await
                } else {
                    std::future::pending().await
                }
            } => {
                match event {
                    StrikeEvent::MarketCreated { market_id, price_id, strike_price, expiry_time } => {
                        let price_id_hex = format!("0x{}", alloy::hex::encode(price_id));

                        if price_id_hex != BTC_USD_PRICE_ID {
                            tracing::debug!(
                                market_id,
                                price_id = %price_id_hex,
                                "ignoring non-BTC/USD MarketCreated"
                            );
                            continue;
                        }

                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs();

                        if expiry_time <= now + cfg.quoting.min_expiry_secs {
                            tracing::debug!(
                                market_id,
                                expiry_time,
                                "ignoring MarketCreated — too close to expiry"
                            );
                            continue;
                        }

                        let market = Market {
                            id: market_id as i64,
                            expiry_time: expiry_time as i64,
                            status: "active".to_string(),
                            pyth_feed_id: Some(price_id_hex),
                            strike_price: Some(strike_price),
                            batch_interval: 3,
                        };

                        info!(
                            market_id,
                            strike_price,
                            expiry_time,
                            "MarketCreated event — new BTC/USD market discovered"
                        );

                        shared_state.lock().await.active_markets.insert(market_id, market);
                    }
                    StrikeEvent::OrderSettled { order_id, filled_lots, .. } => {
                        if filled_lots == 0 {
                            tracing::debug!(order_id = %order_id, "OrderSettled with 0 lots — skipping");
                            continue;
                        }

                        let mut side = "unknown".to_string();
                        let mut market_id = 0u64;
                        for (&mid, mo) in quoter.active_orders.iter() {
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

                        let clearing_tick = shared_state.lock().await.clearing_ticks
                            .get(&market_id).copied().unwrap_or(50);
                        shared_state.lock().await.fills.push(FillEvent {
                            order_id: order_id.to::<u64>(),
                            market_id,
                            filled_lots,
                            side,
                            clearing_tick,
                        });
                    }
                    StrikeEvent::GtcAutoCancelled { order_id, .. } => {
                        info!(
                            order_id = %order_id,
                            "GtcAutoCancelled — order auto-cancelled by batch auction"
                        );
                    }
                    StrikeEvent::BatchCleared { market_id, batch_id, clearing_tick, matched_lots } => {
                        info!(
                            market_id,
                            batch_id,
                            clearing_tick,
                            matched_lots,
                            "BatchCleared"
                        );
                        shared_state.lock().await.clearing_ticks.insert(market_id, clearing_tick);
                        if matched_lots > 0 {
                            shared_state.lock().await.cleared_markets.insert(market_id);
                        }
                    }
                    _ => {}
                }
                // After event processing, fall through to quoting logic below
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
                // Read from shared state (populated by events + initial snapshot)
                let now_secs = (now_ms / 1000) as i64;
                let state = shared_state.lock().await;
                let active: Vec<Market> = state
                    .active_markets
                    .values()
                    .filter(|m| m.expiry_time > now_secs + cfg.quoting.min_expiry_secs as i64)
                    .cloned()
                    .collect();
                drop(state);

                if active.is_empty() {
                    info!(
                        "no active markets with >{} secs to expiry",
                        cfg.quoting.min_expiry_secs
                    );
                }
                active
            } else {
                // Fallback: poll indexer
                match market_manager::fetch_active_markets(
                    &http_client,
                    &cfg.indexer.url,
                    cfg.quoting.min_expiry_secs,
                )
                .await
                {
                    Ok(m) => {
                        if m.is_empty() {
                            info!(
                                "no active markets with >{} secs to expiry",
                                cfg.quoting.min_expiry_secs
                            );
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
                let strike_usd = m
                    .strike_price
                    .map(pricing::pyth_price_to_f64)
                    .unwrap_or(0.0);
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
                info!(
                    market_id,
                    final_position = final_pos,
                    "final position on expired market"
                );
                risk_mgr.remove_market(*market_id);

                // Remove from shared state
                if ws_enabled {
                    shared_state.lock().await.active_markets.remove(market_id);
                }
            }

            // ── Fill Detection ───────────────────────────────────
            if ws_enabled {
                // Drain fills from shared state (populated by events)
                let mut state = shared_state.lock().await;
                let pending_fills: Vec<FillEvent> = state.fills.drain(..).collect();
                drop(state);

                for fill in &pending_fills {
                    let is_bid = fill.side == "bid";
                    let cost = if is_bid {
                        risk::lots_to_usdt(fill.clearing_tick, fill.filled_lots)
                    } else {
                        (1.0 - fill.clearing_tick as f64 / 100.0) * fill.filled_lots as f64 * 0.01
                    };
                    info!(
                        order_id = fill.order_id,
                        market_id = fill.market_id,
                        side = %fill.side,
                        filled_lots = fill.filled_lots,
                        clearing_tick = fill.clearing_tick,
                        cost_usdt = format!("{cost:.2}"),
                        "FILL DETECTED (event) — position updated"
                    );
                    risk_mgr.record_fill(
                        fill.market_id,
                        fill.clearing_tick,
                        fill.filled_lots,
                        is_bid,
                    );
                    quoter.record_fill();
                }
            } else {
                // Fallback: poll indexer for fill tracking
                match client.indexer().get_open_orders(&mm_address).await {
                    Ok(orders) => {
                        let mut new_states: HashMap<i64, strike_sdk::indexer::types::IndexerOrder> =
                            HashMap::new();
                        for order in &orders {
                            // Detect transitions to filled
                            if order.status == "filled" {
                                if let Some(prev) = prev_order_states.get(&order.id) {
                                    if prev.status != "filled" {
                                        let is_bid = order.side == "bid";
                                        // Use order tick as clearing price approximation (indexer fallback)
                                        let clearing_tick = order.tick;
                                        info!(
                                            order_id = order.id,
                                            market_id = order.market_id,
                                            side = %order.side,
                                            tick = order.tick,
                                            lots = order.lots,
                                            "FILL DETECTED (indexer) — position updated"
                                        );
                                        risk_mgr.record_fill(
                                            order.market_id as u64,
                                            clearing_tick,
                                            order.lots,
                                            is_bid,
                                        );
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
            if ws_enabled {
                let cleared: Vec<u64> = {
                    let mut state = shared_state.lock().await;
                    state.cleared_markets.drain().collect()
                };
                for mid in cleared {
                    if let Some(orders) = quoter.active_orders.get_mut(&mid) {
                        info!(market_id = mid, "batch filled — forcing requote");
                        orders.last_fair_tick = -1;
                    }
                }
            }

            // ── Quoting ──────────────────────────────────────────
            let all_active: Vec<Market> = markets;
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

                let raw_fair = pricing::fair_value(btc_price, strike, vol, tte);
                let fair = pricing::exaggerate_fair(raw_fair, secs_left.max(0) as u64);
                let fair_tick = (fair * 100.0).round() as i64;

                // Clamp fair_tick to valid range (ticks 1-99)
                let fair_tick = fair_tick.clamp(1, 99);

                // Improvement #2: Time-decay spread widening
                let spread_multiplier = if secs_left <= 60 {
                    cfg.quoting.expiry_spread_multiplier_60s
                } else if secs_left <= 120 {
                    cfg.quoting.expiry_spread_multiplier_120s
                } else {
                    1.0
                };
                let effective_spread =
                    ((cfg.quoting.spread_ticks as f64 * spread_multiplier).round() as u64).max(2);

                // Size stays constant regardless of time-to-expiry
                let effective_lots = cfg.quoting.lots_per_level;

                // Always two-sided: extreme fair values clamp to tick 1 or 99
                // instead of not quoting one side entirely
                let quote_mode = quoter::QuoteMode::TwoSided;

                let position = risk_mgr.position(market_id);
                let pos_state = risk_mgr.position_state(market_id);
                let skew = risk_mgr.inventory_skew(market_id);
                let (bid_tick, ask_tick) = pricing::compute_ticks(fair, effective_spread, skew);

                // Directional quote sizing: reduce size on same side, full on opposite
                let max_loss_budget = risk_mgr.max_loss_budget();
                let (bid_lots, ask_lots) = {
                    let base = effective_lots;
                    let num_levels = cfg.quoting.num_levels;
                    if pos_state.net_lots > 0 {
                        // Long YES: reduce bids (same side), full asks (flattening)
                        let bl = pos_state.quote_lots_same_side(
                            bid_tick,
                            base,
                            num_levels,
                            max_loss_budget,
                        );
                        (bl, base)
                    } else if pos_state.net_lots < 0 {
                        // Short YES: full bids (flattening), reduce asks (same side)
                        let al = pos_state.quote_lots_same_side(
                            100 - ask_tick,
                            base,
                            num_levels,
                            max_loss_budget,
                        );
                        (base, al)
                    } else {
                        // Flat: both sides use budget-aware sizing
                        let bl = pos_state.quote_lots_same_side(
                            bid_tick,
                            base,
                            num_levels,
                            max_loss_budget,
                        );
                        let al = pos_state.quote_lots_same_side(
                            100 - ask_tick,
                            base,
                            num_levels,
                            max_loss_budget,
                        );
                        (bl, al)
                    }
                };

                // Force requote when quote mode changes
                let mode_changed = if let Some(orders) = quoter.active_orders.get(&market_id) {
                    let prev_had_bids = !orders.bid_order_ids.is_empty();
                    let prev_had_asks = !orders.ask_order_ids.is_empty();
                    match quote_mode {
                        quoter::QuoteMode::BidsOnly => prev_had_asks,
                        quoter::QuoteMode::AsksOnly => prev_had_bids,
                        quoter::QuoteMode::TwoSided => !prev_had_bids || !prev_had_asks,
                    }
                } else {
                    false
                };

                if quoter.needs_requote(market_id, fair_tick) || mode_changed {
                    let reason = if !quoter.is_quoting(market_id) {
                        "initial quote"
                    } else if mode_changed {
                        "quote mode changed"
                    } else {
                        "fair value moved >= requote threshold"
                    };
                    let exposure_ratio = pos_state.exposure_ratio(max_loss_budget);
                    let expected_pnl = pos_state.expected_pnl(fair);
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
                        effective_spread,
                        spread_multiplier = format!("{spread_multiplier:.1}"),
                        quote_mode = ?quote_mode,
                        bid_lots,
                        ask_lots,
                        exposure_ratio = format!("{exposure_ratio:.2}"),
                        expected_pnl = format!("{expected_pnl:.2}"),
                        max_loss = format!("{:.2}", pos_state.total_max_loss),
                        remaining_budget = format!("{:.2}", pos_state.remaining_budget(max_loss_budget)),
                        price_age_ms,
                        low_volume = quoter.is_low_volume(),
                        "REQUOTING"
                    );
                    quoter
                        .requote(
                            market_id,
                            bid_tick,
                            ask_tick,
                            fair_tick,
                            &mut risk_mgr,
                            quote_mode,
                            bid_lots,
                            ask_lots,
                        )
                        .await?;
                } else {
                    // Log why we didn't requote (at debug level to avoid spam)
                    if let Some(orders) = quoter.active_orders.get(&market_id) {
                        let fair_diff = (fair_tick - orders.last_fair_tick).unsigned_abs();
                        let cooldown_remaining = cfg
                            .quoting
                            .requote_cooldown_secs
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
        }
    }
}
