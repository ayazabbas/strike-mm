# Strike Market Maker

A simple liquidity bot for Strike prediction markets on BSC testnet. Provides baseline liquidity using Black-Scholes pricing with a fat spread so early users have something to trade against.

## How It Works

1. Subscribes to Binance BTCUSDT WebSocket for real-time BTC price
2. Polls Strike indexer API for active BTC/USD markets
3. Computes fair value for each market using Black-Scholes: `P(YES) = Φ((ln(S/K) + σ²t/2) / (σ√t))`
4. Quotes bid/ask with a configurable spread around fair value
5. Requotes when BTC price moves enough to shift fair value by ≥ threshold ticks
6. Automatically discovers new markets and stops quoting expired ones

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

## Config Reference

All configuration is in TOML format. See `config/default.toml` for the full example.

### `[rpc]`
| Field | Type | Description |
|-------|------|-------------|
| `url` | string | BSC RPC endpoint |
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

### `[indexer]`
| Field | Type | Description |
|-------|------|-------------|
| `url` | string | Strike indexer API base URL |
| `poll_interval_secs` | u64 | How often to poll for new/expired markets |

### `[quoting]`
| Field | Type | Description |
|-------|------|-------------|
| `spread_ticks` | u64 | Total bid-ask spread in ticks (1 tick = 1 cent) |
| `lots_per_level` | u64 | Number of lots per price level ($1 per lot) |
| `num_levels` | u64 | Number of price levels per side |
| `requote_threshold_ticks` | u64 | Min tick movement before requoting |
| `requote_cooldown_secs` | u64 | Min seconds between requotes |
| `min_expiry_secs` | u64 | Stop quoting markets with less than this time remaining |

### `[risk]`
| Field | Type | Description |
|-------|------|-------------|
| `max_position_per_market` | i64 | Max lots per market (hard cap) |
| `max_total_exposure` | i64 | Max total lots across all markets |
| `stale_data_timeout_secs` | u64 | Cancel all orders if no Binance data for this long |

### `[volatility]`
| Field | Type | Description |
|-------|------|-------------|
| `method` | string | `"fixed"` or `"realized"` |
| `fixed_annual_vol` | f64 | Fixed annualized volatility (used when method = "fixed") |
| `realized_window_mins` | u64 | Window for rolling realized vol calculation |

## Dry Run vs Live Mode

- **Dry run** (`--dry-run`): Logs every order that would be placed/cancelled but does not submit transactions. Use this first to verify behavior.
- **Live mode** (default): Submits real transactions to BSC testnet. Approves Vault for USDT on startup.

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

Send `SIGTERM` or `SIGINT` (Ctrl+C) — the bot will cancel all outstanding orders before exiting.

## Risk Controls

| Control | Value | Notes |
|---------|-------|-------|
| Max position per market | 50 lots ($50) | Hard cap |
| Max total exposure | $200 across all markets | Pauses quoting if exceeded |
| Stale data guard | Cancel all if no Binance data for 10s | Don't quote blind |
| Inventory skew | Shift quotes 1 tick when position > 30 lots | Nudge to flatten |
| Min time to expiry | 30 seconds | Avoid adverse selection near expiry |
