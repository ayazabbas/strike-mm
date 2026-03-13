mod binance;
mod config;
mod market_manager;
mod pricing;
mod quoter;
mod redeemer;
mod risk;

use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use clap::Parser;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{watch, Mutex};
use tracing::{error, info, warn};

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

    info!(address = %signer_addr, "wallet loaded");

    // Parse contract addresses
    let order_book_addr: Address = cfg.contracts.order_book.parse().wrap_err("bad order_book address")?;
    let vault_addr: Address = cfg.contracts.vault.parse().wrap_err("bad vault address")?;
    let usdt_addr: Address = cfg.contracts.usdt.parse().wrap_err("bad usdt address")?;
    let redemption_addr: Address = cfg.contracts.redemption.parse().wrap_err("bad redemption address")?;
    let outcome_token_addr: Address = cfg.contracts.outcome_token.parse().wrap_err("bad outcome_token address")?;

    // Approve vault for USDT spending (idempotent)
    if !cli.dry_run {
        quoter::approve_vault(usdt_addr, vault_addr, provider.clone()).await?;
    }

    // Wait for approval to mine, then sync nonce
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

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
        let redeem_provider = provider.clone();
        let redeem_indexer_url = cfg.indexer.url.clone();
        tokio::spawn(async move {
            redeemer::run_redeem_loop(
                redeem_provider,
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
        cfg.quoting.clone(),
        cli.dry_run,
    );
    quoter.sync_nonce(signer_addr).await?;
    let mut market_mgr = market_manager::MarketManager::new();
    let mut risk_mgr = risk::RiskManager::new(
        cfg.risk.max_position_per_market,
        cfg.risk.max_total_exposure,
    );

    let http_client = reqwest::Client::new();
    let mm_address = format!("{signer_addr:#x}");

    // Startup cleanup: cancel any orphaned orders from a previous run via indexer
    if !cli.dry_run {
        info!("startup: cleaning up orphaned orders via indexer");
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
            if !market_ids.is_empty() {
                // Resync nonce after startup cancels
                quoter.sync_nonce(signer_addr).await?;
            }
        }
    }

    // Track previous order states for fill detection
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
        tokio::select! {
            _ = &mut shutdown_rx => {
                info!("shutting down — cancelling all orders");
                quoter.cancel_everything().await?;
                info!("all orders cancelled, exiting");
                return Ok(());
            }
            _ = interval.tick() => {
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

                // Fetch active markets from indexer
                let markets = match market_manager::fetch_active_markets(
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

                // Cancel orders on expired markets
                for market_id in &expired_markets {
                    info!(market_id, "MARKET EXPIRED — cancelling all orders");
                    quoter.cancel_all_orders(*market_id, &http_client, &cfg.indexer.url, &mm_address).await?;
                    let final_pos = risk_mgr.position(*market_id);
                    info!(market_id, final_position = final_pos, "final position on expired market");
                    risk_mgr.remove_market(*market_id);
                }

                // Poll positions for fill tracking
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

                // Quote on all active markets
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

                    // Skip quoting at extremes — no edge, only risk
                    if fair_tick <= 2 || fair_tick >= 98 {
                        if quoter.is_quoting(market_id) {
                            info!(market_id, fair_tick, secs_left, "PULLING QUOTES — fair at extreme");
                            quoter.sync_nonce(signer_addr).await?;
                            quoter.cancel_all_orders(market_id, &http_client, &cfg.indexer.url, &mm_address).await?;
                        }
                        continue;
                    }

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
                            .requote(market_id, bid_tick, ask_tick, fair_tick, &mut risk_mgr, signer_addr, &http_client, &cfg.indexer.url, &mm_address)
                            .await?;
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
            }
        }
    }
}
