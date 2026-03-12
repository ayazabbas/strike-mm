# Strike Market Maker — CLAUDE.md

## What This Is

A simple liquidity bot for Strike prediction markets. Quotes bid/ask on BTC/USD binary options using Black-Scholes pricing with a wide spread. Runs on BSC testnet.

## Architecture

```
Binance WS → binance.rs → watch channel → main.rs loop
                                              ↓
indexer API → market_manager.rs ──────→ pricing.rs → quoter.rs → OrderBook contract
                                              ↑
                                          risk.rs
```

### Key Modules

- **`config.rs`** — TOML config parsing. All tunables live in `config/default.toml`.
- **`binance.rs`** — Connects to Binance BTCUSDT trade stream via WebSocket. Publishes latest price to a `tokio::sync::watch` channel. Also collects 1-minute returns for realized vol.
- **`pricing.rs`** — Black-Scholes fair value calculation + tick computation. `fair_value()` returns P(YES), `compute_ticks()` converts to bid/ask with spread and inventory skew.
- **`quoter.rs`** — Manages order placement and cancellation on the OrderBook contract via alloy. Tracks active orders per market. Handles requoting logic (cancel old, place new).
- **`market_manager.rs`** — Polls the Strike indexer API for active BTC/USD markets. Reconciles with current state to discover new markets and detect expired ones.
- **`risk.rs`** — Position tracking per market, per-market and total exposure limits, inventory skew calculation.
- **`main.rs`** — CLI args, component wiring, main loop (poll markets → compute fair values → requote). Handles graceful shutdown (SIGTERM → cancel all orders).

### Contract Interaction

Uses `alloy` v1 with `sol!` macro to generate type-safe contract bindings from ABI JSON files in `abi/`.

- **placeOrder(marketId, side, orderType, tick, lots)** — side: 0=Bid, 1=Ask; orderType: 1=GTC; tick: 1-99; lots: count
- **cancelOrder(orderId)** — cancels a specific order
- On startup, approves Vault for max USDT spend

### Indexer API

- `GET /markets` → `{ "markets": [...] }` — each market has `id`, `expiry_time`, `status`, `pyth_feed_id`, `strike_price` (Pyth 8-decimal format), `batch_interval`
- Bot filters for `status == "active"` and BTC/USD feed ID

## How to Build & Test

```bash
cargo build          # Build
cargo test           # Run unit tests
cargo run -- --dry-run  # Dry run (no txs)
```

## Adding Features

- **New asset (e.g., ETH/USD):** Add feed ID to `market_manager.rs`, the bot already handles multiple concurrent markets.
- **Realized vol:** Set `volatility.method = "realized"` in config. The Binance WS already collects 1-minute returns.
- **More price levels:** Increase `quoting.num_levels` in config.
- **Better inventory management:** Modify `risk.rs::inventory_skew()` for asymmetric spread adjustment.

## Important Details

- Strike prices from the indexer are in Pyth format (8 decimals) — `pricing::pyth_price_to_f64()` converts them.
- Markets are short-lived (5 minutes). The bot polls every 5 seconds to discover new ones.
- The bot uses GTC (Good Til Cancel) orders so they persist across batch clearings.
- Collateral: Bid at tick T for L lots locks `L * T / 100` USDT. Ask locks `L * (100-T) / 100` USDT.
