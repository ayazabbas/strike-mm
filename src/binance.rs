use eyre::{Result, WrapErr};
use futures_util::StreamExt;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_tungstenite::connect_async;
use tracing::{error, info, warn};

const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@trade";

#[derive(Debug, Deserialize)]
struct BinanceTrade {
    /// Trade price
    p: String,
    /// Trade time (ms)
    #[serde(rename = "T")]
    trade_time: u64,
}

/// Shared price state from the Binance feed.
#[derive(Debug, Clone)]
pub struct PriceData {
    pub price: f64,
    pub timestamp_ms: u64,
}

/// Runs the Binance WebSocket client, publishing price updates to the watch channel.
/// Also collects 1-minute returns for realized vol calculation.
pub async fn run_binance_ws(
    price_tx: watch::Sender<Option<PriceData>>,
    returns_tx: Arc<tokio::sync::Mutex<Vec<f64>>>,
    realized_window_mins: u64,
) -> Result<()> {
    loop {
        match connect_and_stream(&price_tx, &returns_tx, realized_window_mins).await {
            Ok(()) => {
                warn!("binance ws closed cleanly, reconnecting...");
            }
            Err(e) => {
                error!(err = %e, "binance ws error, reconnecting in 2s...");
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
        }
    }
}

async fn connect_and_stream(
    price_tx: &watch::Sender<Option<PriceData>>,
    returns_tx: &Arc<tokio::sync::Mutex<Vec<f64>>>,
    realized_window_mins: u64,
) -> Result<()> {
    let (ws_stream, _) = connect_async(BINANCE_WS_URL)
        .await
        .wrap_err("connecting to Binance WS")?;

    info!("connected to Binance BTCUSDT stream");

    let (_, mut read) = ws_stream.split();

    // Track minute-level prices for vol calculation
    let mut last_minute_price: Option<f64> = None;
    let mut last_minute_ts: u64 = 0;

    while let Some(msg) = read.next().await {
        let msg = msg.wrap_err("reading ws message")?;
        let text = match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            tokio_tungstenite::tungstenite::Message::Ping(_) => continue,
            tokio_tungstenite::tungstenite::Message::Close(_) => return Ok(()),
            _ => continue,
        };

        let trade: BinanceTrade = match serde_json::from_str(&text) {
            Ok(t) => t,
            Err(e) => {
                warn!(err = %e, "failed to parse binance trade");
                continue;
            }
        };

        let price: f64 = match trade.p.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Publish latest price
        let _ = price_tx.send(Some(PriceData {
            price,
            timestamp_ms: trade.trade_time,
        }));

        // Collect 1-minute returns for realized vol
        let current_minute = trade.trade_time / 60_000;
        if current_minute > last_minute_ts {
            if let Some(prev_price) = last_minute_price {
                if prev_price > 0.0 {
                    let log_return = (price / prev_price).ln();
                    let mut returns = returns_tx.lock().await;
                    returns.push(log_return);
                    // Keep only the window we need
                    let max_returns = realized_window_mins as usize;
                    if returns.len() > max_returns {
                        let drain_count = returns.len() - max_returns;
                        returns.drain(..drain_count);
                    }
                }
            }
            last_minute_price = Some(price);
            last_minute_ts = current_minute;
        }
    }

    Ok(())
}
