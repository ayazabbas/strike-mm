# Strike MM Rewrite Plan — Dogfood strike-sdk

## Goal

Refactor strike-mm to use `strike-sdk` as a dependency, removing all duplicated chain interaction code. The MM should only contain trading strategy logic (pricing, risk, quoting decisions, event loop).

## Dependency

```toml
# strike-mm/Cargo.toml
[dependencies]
strike-sdk = { path = "../strike/sdk/rust" }
```

Remove: `alloy` as direct dependency (use re-exports from strike-sdk where needed). Keep `alloy` only if MM needs alloy types not re-exported by the SDK.

## What Gets Replaced

### 1. `contracts.rs` → DELETE
Currently defines `sol!` bindings for BatchAuction and MarketFactory.
**Replace with:** `strike_sdk::contracts::*` (all 8 contracts available)

### 2. `nonce_sender.rs` → DELETE
82-line shared nonce manager.
**Replace with:** `strike_sdk::nonce::NonceSender` (identical implementation, feature-gated)

### 3. `quoter.rs` → HEAVY REFACTOR (841 → ~300 lines)

**Delete these functions (now in SDK):**
- `recover_live_orders()` → `client.scan_orders(from_block, owner)`
- `approve_vault()` → `client.vault().approve_usdt()`
- `parse_placed_orders()` → handled internally by SDK's `orders().place()` / `orders().replace()`
- `cancel_tx()` helper → `client.orders().cancel()` / `cancel_one()`
- `cancel_via_indexer()` → not needed (on-chain recovery via SDK scan)
- `cancel_local_orders()` → `client.orders().cancel(&ids)`
- `cancel_local_orders_batch()` → `client.orders().cancel(&ids)`
- `startup_cancel_sweep()` → `client.orders().cancel(&all_ids)`

**Keep (strategy logic):**
- `MarketOrders` struct (tracks active order IDs + last fair tick + last quote time)
- `QuoteMode` enum (TwoSided, BidsOnly, AsksOnly)
- `build_order_params()` → builds `OrderParam` list based on risk/levels/mode
- `needs_requote()` → fair tick movement detection
- `place_quotes()` → SIMPLIFY: call `client.orders().place()`, store result
- `requote()` → SIMPLIFY: call `client.orders().replace()`, store result
- `cancel_everything()` → SIMPLIFY: collect all IDs, call `client.orders().cancel()`
- `restore_state()` → keep, but input comes from SDK scan result
- `is_quoting()` → keep

**New shape of Quoter:**
```rust
pub struct Quoter {
    client: StrikeClient,      // SDK client (replaces provider + nonce_sender + contract instances)
    config: QuotingConfig,
    active_orders: HashMap<u64, MarketOrders>,
    dry_run: bool,
}
```

No more `order_book: OrderBook::OrderBookInstance<P>`, no more `nonce_sender: Arc<Mutex<NonceSender>>`, no more generic `P: Provider`.

### 4. `redeemer.rs` → HEAVY REFACTOR (274 → ~80 lines)

**Delete:**
- `sol!` bindings for RedemptionContract, OutcomeToken, Multicall3
- `try_redeem_market()` — balance checks, multicall TX construction
- `Call3` / `MulticallResult` / `aggregate3` sol definitions

**Replace with:**
- `client.tokens().balance_of(owner, yes_token_id)` for balance checks
- `client.redeem().redeem(market_id, amount)` for redemption
- Still use Multicall3 for batching? SDK doesn't include multicall — keep it here or just call redeem twice (once for YES, once for NO). Two TXs is simpler and gas is cheap on BSC testnet.

**Keep:**
- `run_redeem_loop()` — the 10-min interval scheduling logic
- `fetch_resolved_markets()` — indexer call (use `client.indexer().get_markets()` + filter for resolved)
- `redeemed: HashSet<u64>` tracking

### 5. `market_manager.rs` → SIMPLIFY

**Delete:**
- `fetch_active_markets()` → `client.indexer().get_active_markets(feed_id)` 
- `MarketsResponse` struct → use `strike_sdk::indexer::types::Market`

**Keep:**
- `MarketManager` struct with reconcile logic (new/expired detection)
- Tests

### 6. `event_state.rs` → REFACTOR

**Keep** the struct but change `FillEvent` and `EventState` to use SDK event types where possible.

### 7. `main.rs` → REFACTOR (974 → ~600 lines)

**Delete:**
- `run_ws_subscriber()` / `try_subscribe_all()` — all WSS subscription boilerplate (filters, sub setup, stream handling)
- `fetch_positions()` — indexer positions fetch
- `IndexerOrder` / `PositionsResponse` structs
- Provider construction (signer, wallet, DynProvider) — SDK handles this
- USDT approval call — SDK handles this

**Replace with:**
- `StrikeClient::new(config).with_private_key(key).build()` — one line
- `client.events().subscribe()` → `EventStream` with auto-reconnect
- Match on `StrikeEvent::*` variants instead of raw log decoding

**Keep:**
- Main event loop (tick-based requoting, fill processing)
- Binance price feed integration
- Risk manager
- Pricing (Black-Scholes)
- CLI args (clap)
- Config loading
- Startup sequence (load markets → scan orders → cancel sweep → start quoting)

### 8. `binance.rs` → NO CHANGE (strategy, not infrastructure)
### 9. `pricing.rs` → NO CHANGE (strategy)
### 10. `risk.rs` → NO CHANGE (strategy)
### 11. `config.rs` → SIMPLIFY

Remove `ContractsConfig` (addresses come from SDK config). Keep:
- `RpcConfig` (rpc_url, wss_url — passed to SDK builder)
- `WalletConfig` (private_key_env)
- `IndexerConfig` (url — passed to SDK builder)
- `QuotingConfig` (spread, levels, requote params)
- `RiskConfig` (budget, skew, stale timeout)
- `VolatilityConfig` (method, fixed vol, window)

## Files After Rewrite

```
src/
  main.rs           # Event loop, startup, CLI (~600 lines, down from 974)
  config.rs         # MM-specific config only (~70 lines, down from 103)
  quoter.rs         # Quote placement strategy (~300 lines, down from 841)
  redeemer.rs       # Redeem scheduling (~80 lines, down from 274)
  market_manager.rs # Market reconciliation (~100 lines, down from 168)
  event_state.rs    # Shared event state (~40 lines, similar)
  binance.rs        # Price feed (unchanged, 114 lines)
  pricing.rs        # Black-Scholes (unchanged, 210 lines)
  risk.rs           # Position/risk tracking (unchanged, 416 lines)
```

**Deleted entirely:** `contracts.rs` (13 lines), `nonce_sender.rs` (82 lines)
**Net reduction:** ~3,232 → ~1,930 lines (~40% less code)

## ABI Files

Delete `strike-mm/abi/` directory entirely. SDK has its own ABIs.

## Cargo.toml Changes

```toml
[dependencies]
strike-sdk = { path = "../strike/sdk/rust" }
# Keep these (not in SDK):
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
eyre = "0.6"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
clap = { version = "4", features = ["derive"] }
toml = "0.8"
reqwest = { version = "0.12", features = ["json"] }
futures-util = "0.3"

# REMOVE these (now via SDK):
# alloy — use strike_sdk re-exports
```

Actually, keep `alloy` as a direct dep if the MM uses alloy types (U256, Address, etc.) extensively. The SDK re-exports some via `prelude`, but MM code references alloy types directly in many places. Simpler to keep it.

## Implementation Order

1. Add `strike-sdk` dep to Cargo.toml, verify it compiles
2. Delete `contracts.rs`, `nonce_sender.rs`, `abi/` — replace imports
3. Refactor `quoter.rs` — replace chain calls with SDK client
4. Refactor `redeemer.rs` — use SDK token/redeem clients
5. Refactor `market_manager.rs` — use SDK indexer types
6. Refactor `main.rs` — SDK client construction + EventStream
7. Simplify `config.rs` — remove contract addresses
8. `cargo build` + `cargo clippy` — verify clean
9. Test: restart strike-mm service, verify quoting works end-to-end

## Key Risk

The SDK's `orders().place()` / `orders().replace()` use `provider.send_transaction()` directly (no NonceSender). The MM currently relies on NonceSender for sequential nonce management. Two options:

**Option A:** SDK client uses NonceSender internally when the feature is enabled → requires SDK changes to wire NonceSender into OrdersClient.

**Option B:** MM keeps its own NonceSender and calls lower-level SDK contract bindings directly for TX construction, but uses NonceSender for sending. → Less clean but no SDK changes needed.

**Recommendation:** Option A — add `with_nonce_manager()` to StrikeClientBuilder. When enabled, all send_transaction calls go through NonceSender. This is the whole point of the SDK.
