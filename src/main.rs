mod binance;
mod config;
mod market_manager;
mod pricing;
mod quoter;
mod risk;

use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use clap::Parser;
use eyre::{Result, WrapErr};
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

    // Approve vault for USDT spending (idempotent)
    if !cli.dry_run {
        quoter::approve_vault(usdt_addr, vault_addr, provider.clone()).await?;
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

    // Initialize components
    let mut quoter = quoter::Quoter::new(
        order_book_addr,
        provider.clone(),
        cfg.quoting.clone(),
        cli.dry_run,
    );
    let mut market_mgr = market_manager::MarketManager::new();
    let mut risk_mgr = risk::RiskManager::new(
        cfg.risk.max_position_per_market,
        cfg.risk.max_total_exposure,
    );

    let http_client = reqwest::Client::new();

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

                let btc_price = price_data.unwrap().price;

                // Determine volatility
                let vol = match cfg.volatility.method.as_str() {
                    "realized" => {
                        let rets = returns.lock().await;
                        let v = pricing::realized_vol(&rets);
                        if v < 0.01 {
                            cfg.volatility.fixed_annual_vol // Fallback if not enough data
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
                    Ok(m) => m,
                    Err(e) => {
                        warn!(err = %e, "failed to fetch markets — skipping cycle");
                        continue;
                    }
                };

                let (new_markets, expired_markets) = market_mgr.reconcile(&markets);

                // Cancel orders on expired markets
                for market_id in &expired_markets {
                    quoter.cancel_all(*market_id).await?;
                    risk_mgr.remove_market(*market_id);
                }

                // Quote on all active markets
                let all_active: Vec<market_manager::Market> = markets;
                for market in &all_active {
                    let market_id = market.id as u64;
                    let strike = match market.strike_price {
                        Some(sp) => pricing::pyth_price_to_f64(sp),
                        None => {
                            warn!(market_id, "no strike price — skipping");
                            continue;
                        }
                    };

                    let tte = pricing::time_to_expiry_years(market.expiry_time);
                    if tte <= 0.0 {
                        continue;
                    }

                    let fair = pricing::fair_value(btc_price, strike, vol, tte);
                    let skew = risk_mgr.inventory_skew(market_id, 30);
                    let (bid_tick, ask_tick) =
                        pricing::compute_ticks(fair, cfg.quoting.spread_ticks, skew);

                    if quoter.needs_requote(market_id, bid_tick, ask_tick) {
                        info!(
                            market_id,
                            btc_price,
                            strike,
                            fair = format!("{fair:.4}"),
                            bid_tick,
                            ask_tick,
                            skew,
                            "requoting"
                        );
                        quoter
                            .requote(market_id, bid_tick, ask_tick, &mut risk_mgr)
                            .await?;
                    }
                }
            }
        }
    }
}
