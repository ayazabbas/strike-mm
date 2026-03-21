# Strike Market Maker v1

Market maker bot for [Strike](https://strike.pm) prediction markets on BNB Chain (BSC testnet). Provides liquidity on BTC/USD binary options using Black-Scholes pricing.

## How It Works

1. Subscribes to Binance BTCUSDT WebSocket for real-time BTC price
2. Subscribes to BSC WebSocket for on-chain events (MarketCreated, OrderSettled, BatchCleared, GtcAutoCancelled)
3. Computes fair value using Black-Scholes: `P(YES) = Φ((ln(S/K) + σ²t/2) / (σ√t))`
4. Quotes bid/ask with configurable spread, one-sided quoting near extremes, time-decay spread near expiry
5. Requotes atomically via `replaceOrders` (cancel + place in single TX)
6. Dollar-based risk model with position tracking and inventory skew
7. Auto-redeems resolved market positions in background

## Quick Start

```bash
# Set wallet private key (funded with testnet USDT)
export MM_PRIVATE_KEY="0x..."

# Dry run (logs orders without submitting transactions)
cargo run -- --dry-run

# Live on BSC testnet
cargo run

# Custom config
cargo run -- --config path/to/config.toml
```

## BSC Testnet Addresses (v1)

| Contract | Address |
|----------|---------|
| OrderBook | `0x9675bab261a6f168dd76fedb6d8706021e338c16` |
| MarketFactory | `0xf3ad14f117348de4886c29764fdcaf9c62794535` |
| BatchAuction | `0x62224a55d05175eaeb22fc6263355c820c77e849` |
| Vault | `0x04606a6f4909d0e9d9d763083d7649a2229eb679` |
| Redemption | `0xd181cc898bbbf4d2ddaebf6f245f043dd8f93704` |
| MockUSDT | `0xb242dc031998b06772C63596Bfce091c80D4c3fA` |

## Config Reference

All configuration is in TOML format. See `config/default.toml` for the full example.

### `[rpc]`
| Field | Type | Description |
|-------|------|-------------|
| `url` | string | BSC RPC endpoint (HTTPS) |
| `wss_url` | string | BSC RPC endpoint (WebSocket, for event subscriptions) |
| `chain_id` | u64 | Chain ID (97 for BSC testnet) |

### `[wallet]`
| Field | Type | Description |
|-------|------|-------------|
| `private_key_env` | string | Environment variable name containing the wallet private key |

### `[contracts]`
| Field | Type | Description |
|-------|------|-------------|
| `order_book` | string | OrderBook contract address |
| `vault` | string | Vault contract address |
| `usdt` | string | MockUSDT contract address |
| `redemption` | string | Redemption contract address |
| `batch_auction` | string | BatchAuction contract address |
| `market_factory` | string | MarketFactory contract address |

### `[indexer]`
| Field | Type | Description |
|-------|------|-------------|
| `url` | string | Strike indexer API base URL |
| `poll_interval_secs` | u64 | How often to poll for new/expired markets |

### `[quoting]`
| Field | Type | Description |
|-------|------|-------------|
| `spread_ticks` | u64 | Total bid-ask spread in ticks (1 tick = 1 cent) |
| `lots_per_level` | u64 | Number of lots per price level |
| `num_levels` | u64 | Number of price levels per side |
| `requote_cents` | u64 | Min cent movement before requoting |
| `requote_cooldown_secs` | u64 | Min seconds between requotes |
| `min_expiry_secs` | u64 | Stop quoting markets with less than this time remaining |
| `one_sided_threshold` | f64 | Go one-sided when fair prob exceeds this (default 0.90) |
| `expiry_spread_multiplier_120s` | f64 | Spread multiplier when <120s to expiry (default 1.5) |
| `expiry_spread_multiplier_60s` | f64 | Spread multiplier when <60s to expiry (default 2.0) |
| `min_quote_secs` | u64 | Stop quoting entirely below this many seconds (default 30) |

### `[risk]`
| Field | Type | Description |
|-------|------|-------------|
| `max_loss_budget_usdt` | f64 | Max USDT at risk per market (dollar-based budget) |
| `max_skew_ticks` | i64 | Max tick shift from inventory skew (default 6) |
| `stale_data_timeout_secs` | u64 | Cancel all orders if no Binance data for this long |

### `[volatility]`
| Field | Type | Description |
|-------|------|-------------|
| `method` | string | `"fixed"` or `"realized"` |
| `fixed_annual_vol` | f64 | Fixed annualized volatility (used when method = "fixed") |
| `realized_window_mins` | u64 | Window for rolling realized vol calculation |

## Deployment

### systemd Service

```ini
[Unit]
Description=Strike Market Maker
After=network.target

[Service]
Type=simple
User=strike
Environment=MM_PRIVATE_KEY=0x...
Environment=RUST_LOG=info
WorkingDirectory=/opt/strike-mm
ExecStart=/opt/strike-mm/strike-mm --config /opt/strike-mm/config/default.toml
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
# Install
cargo build --release
sudo cp target/release/strike-mm /opt/strike-mm/

# Enable and start
sudo systemctl enable strike-mm
sudo systemctl start strike-mm

# View logs
journalctl -u strike-mm -f
```

### Graceful Shutdown

Send `SIGTERM` or `SIGINT` (Ctrl+C) — the bot cancels all outstanding orders before exiting.
