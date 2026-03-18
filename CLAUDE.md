# Strike Market Maker v1 — CLAUDE.md

## What This Is

Market maker bot for Strike prediction markets on BNB Chain (BSC testnet). Quotes bid/ask on BTC/USD binary options using Black-Scholes pricing. Event-driven architecture with WebSocket subscriptions for both price data and on-chain events.

## Architecture

```
Binance WS → binance.rs → watch channel ──→ main.rs event loop
                                                    ↓
BSC WS → event_state.rs (fills, cancels) ──→ risk.rs → pricing.rs → quoter.rs → OrderBook
                                                    ↑
indexer API → market_manager.rs ────────────────────┘

redeemer.rs (background) ──→ Redemption contract
```

### Key Modules

- **`config.rs`** — TOML config parsing. All tunables live in `config/default.toml`.
- **`binance.rs`** — Binance BTCUSDT trade stream via WebSocket. Publishes latest price to a `tokio::sync::watch` channel. Collects 1-minute returns for realized vol.
- **`pricing.rs`** — Black-Scholes fair value + tick computation. `fair_value()` returns P(YES), `compute_ticks()` converts to bid/ask with spread and inventory skew. Supports one-sided quoting and time-decay spread near expiry.
- **`quoter.rs`** — Order placement, cancellation, requoting on the OrderBook contract via alloy. Tracks active orders per market. Uses `replaceOrders` for atomic cancel+place.
- **`market_manager.rs`** — Polls Strike indexer API for active BTC/USD markets. Discovers new markets and detects expired ones.
- **`risk.rs`** — Dollar-based risk model with `PositionState` tracking per market. Inventory skew calculation, directional quote sizing based on loss budget.
- **`event_state.rs`** — Processes on-chain events (OrderSettled, GtcAutoCancelled, BatchCleared) from BSC WebSocket subscription.
- **`nonce_sender.rs`** — Shared `NonceSender` (`Arc<Mutex>`) for serialized TX submission across components.
- **`redeemer.rs`** — Background task that auto-redeems positions in resolved markets via Redemption contract.
- **`contracts.rs`** — alloy `sol!` macro bindings for all contracts. ABIs in `abi/`.
- **`main.rs`** — CLI args, component wiring, event loop, graceful shutdown (SIGTERM → cancel all orders).

### Contract Interaction

Uses `alloy` with `sol!` macro for type-safe contract bindings from ABI JSON files in `abi/`.

**Batch operations (primary path):**
- **`placeOrders(marketId, OrderParam[] params)`** → `uint256[] orderIds` — places multiple orders in a single TX
- **`replaceOrders(uint256[] cancelIds, marketId, OrderParam[] params)`** → `uint256[] newOrderIds` — atomic cancel+place, zero empty book time
- **`OrderParam` struct**: `(Side side, OrderType orderType, uint8 tick, uint64 lots)`
- Side enum: 0=Bid, 1=Ask, 2=SellYes, 3=SellNo. OrderType: 0=GTC, 1=GTB

**Single-order operations (shutdown/cleanup only):**
- **`cancelOrder(orderId)`** / **`cancelOrders(uint256[] orderIds)`** — shutdown cancel sweep

**Architecture note:** Requotes use `replaceOrders` for atomic cancel+place (1 TX). Benefits from net settlement — when ticks barely change, minimal ERC20 transfers occur.

On startup, approves Vault for max USDT spend.

### On-Chain Events (BSC WebSocket)

- **MarketCreated** — from MarketFactory, triggers immediate quoting of new market
- **OrderSettled** — fill events, updates PositionState in risk.rs
- **GtcAutoCancelled** — order auto-cancelled by protocol, clears from quoter tracking
- **BatchCleared** — batch settlement complete

### Indexer API

- `GET /markets` → `{ "markets": [...] }` — each market has `id`, `expiry_time`, `status`, `pyth_feed_id`, `strike_price` (Pyth 8-decimal format), `batch_interval`
- `GET /positions?address=...` → open orders and filled positions for a wallet
- Bot filters for `status == "active"` and BTC/USD feed ID

## How to Build & Test

```bash
cargo build          # Build
cargo test           # Run unit tests
cargo run -- --dry-run  # Dry run (no txs)
```

## Important Details

- Strike prices from the indexer are in Pyth format (8 decimals) — `pricing::pyth_price_to_f64()` converts them.
- Markets are short-lived (5 minutes). The bot discovers new ones via MarketCreated events and indexer polling.
- The bot uses GTC (Good Til Cancel) orders so they persist across batch clearings.
- Collateral: Bid at tick T for L lots locks `L * T / 100` USDT. Ask locks `L * (100-T) / 100` USDT.
- Dollar-based risk: `max_loss_budget_usdt` caps total USDT at risk per market. Quote size is scaled based on remaining budget and directional exposure.
- All TX sends go through `NonceSender` to avoid nonce conflicts between quoter, redeemer, and shutdown logic.
