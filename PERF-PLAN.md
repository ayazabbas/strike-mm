# Strike MM Performance Improvement Plan
<!-- Created: 2026-03-16 | Updated: 2026-03-16 -->

## Goal
Minimize time where the orderbook has no liquidity. Keep cancel-before-place order.

> **Note:** Items 1-6 are independent bot-side improvements. Item 7 depends on
> contract changes (`replaceOrders` / `placeOrders`) tracked in
> `~/dev/strike/PERF-PLAN.md` — that is the single biggest win (5 TXs → 1 TX,
> zero empty book). These bot-side items are still valuable alongside it.

---

## 1. Config Tuning
**Impact:** Immediate improvement, zero code changes  
**Effort:** Trivial  

| Setting | Current | New | Rationale |
|---------|---------|-----|-----------|
| `poll_interval_secs` | 5 | 1 | 5-min markets need faster reaction; sub-1s blocks make this cheap |
| `requote_cooldown_secs` | 15 | 5 | Each requote is fast; 15s cooldown is too conservative |
| `requote_cents` | 20 | 10 | 20 cents is too wide for 5-min markets; react to smaller moves |

**Files:** `config/default.toml`

---

## 2. Event-Driven Requote Wake
**Impact:** Eliminates 0-5s reaction delay after fills  
**Effort:** Medium  

Currently: fills arrive via WS → logged → main loop only checks on next poll tick (up to 5s later).

**Change:** Add a `tokio::sync::Notify` that the event subscriber signals on any `OrderSettled` (with filled_lots > 0) or `BatchCleared` (with matched_lots > 0). Main loop `tokio::select!` adds a branch for this notify alongside the interval tick.

```rust
// In main loop select!
_ = fill_notify.notified() => {
    // Skip straight to quoting logic — a fill just happened
}
_ = interval.tick() => {
    // Normal poll cycle
}
```

**Files:** `main.rs` (add `fill_notify` Arc, pass to subscriber, add select branch)

---

## 3. Fix Startup Log Scan
**Impact:** Clean startup, no error spam  
**Effort:** Small  

`DEPLOYMENT_BLOCK` is 170k+ blocks behind head. Chainstack rejects 50k-block range queries. Every restart produces 8 warnings and recovers zero orders.

**Change:** Scan from `latest - 3000` instead of `DEPLOYMENT_BLOCK`. For 5-min markets with sub-1s blocks, 3000 blocks (~50 min) covers any conceivable live order. Also reduce `LOG_SCAN_CHUNK_SIZE` to 3000 (well within Chainstack limits).

```rust
let scan_from = latest_block.saturating_sub(3000);
const LOG_SCAN_CHUNK_SIZE: u64 = 3_000;
```

**Files:** `quoter.rs` (`recover_live_orders`, constants)

---

## 4. Update `quoter_orders` Immediately After Placement
**Impact:** Fixes "unknown order" log spam for freshly-placed orders  
**Effort:** Trivial  

Currently `quoter_orders` (shared with event subscriber) is only synced at end of main loop. Fills on orders placed this cycle are invisible to the subscriber.

**Change:** Sync `quoter_orders` immediately after each `place_quotes` or `requote` call, before continuing to the next market.

```rust
quoter.requote(market_id, bid_tick, ask_tick, fair_tick, &mut risk_mgr).await?;
// Sync immediately so event subscriber can look up new order IDs
*quoter_orders.lock().await = quoter.active_orders.clone();
```

**Files:** `main.rs` (quoting section)

---

## 5. Combine WS Subscriptions Into One Connection
**Impact:** Faster startup (one WS handshake instead of two), fewer resources  
**Effort:** Medium  

`try_subscribe_market_created` and `try_subscribe_order_events` each open a separate WS connection. Merging them into one `connect_ws` call with all 4 subscriptions on the same provider would:
- Cut WS setup time in half (~3.5s saved)
- Reduce `sub_ready` gate time
- Simplify reconnection logic

**Files:** `main.rs` (merge both subscriber functions)

---

## 6. Remove 3s Post-Approval Sleep
**Impact:** 3s faster startup  
**Effort:** Trivial  

```rust
// Current — always sleeps even when approval was skipped
tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
```

Move inside the branch where approval actually happens, or remove entirely (NonceSender already fetches nonce from chain after any pending TX confirms).

**Files:** `main.rs`

---

## 7. Integrate `replaceOrders` / `placeOrders` Contract Functions
**Impact:** 5 TXs → 1 TX per requote, zero empty book  
**Effort:** Medium  
**Status:** Contracts deployed (V8, block 96078687). OrderBook at `0x311Fb3059BCD31076e5215674D22f1c7c8b8110A`.

### Contract API (actual deployed signatures)

```solidity
struct OrderParam {
    Side side;        // 0=Bid, 1=Ask, 2=SellYes, 3=SellNo
    OrderType orderType; // 0=GTC, 1=GTB
    uint8 tick;       // 1-99
    uint64 lots;
}

function placeOrders(uint256 marketId, OrderParam[] calldata params) 
    external returns (uint256[] memory orderIds);

function replaceOrders(uint256[] calldata cancelIds, uint256 marketId, OrderParam[] calldata params) 
    external returns (uint256[] memory newOrderIds);
```

### Bot Changes

1. **Add sol! bindings** for `placeOrders` and `replaceOrders` with `OrderParam` tuple struct in `contracts.rs`
2. **Rewrite `place_quotes()`** → use `placeOrders(marketId, params)` — 1 TX instead of 4
3. **Rewrite `requote()`** → use `replaceOrders(cancelIds, marketId, params)` — atomic cancel+place, zero empty book
4. **Remove `cancel_local_orders` / `cancel_local_orders_batch`** — no longer needed for requoting (keep `cancel_everything` for shutdown/graceful exit)
5. **Parse `OrderPlaced` events from receipt logs** to extract new order IDs and update local tracking

```rust
pub async fn requote(&mut self, market_id: u64, bid_tick: u64, ask_tick: u64, ...) -> Result<()> {
    let cancel_ids: Vec<U256> = self.get_active_order_ids(market_id);
    let params = self.build_order_params(bid_tick, ask_tick); // Vec<OrderParam>
    
    let tx = self.order_book.replaceOrders(cancel_ids, U256::from(market_id), params);
    let receipt = ns.lock().await.send(tx).await?.get_receipt().await?;
    
    let new_ids = receipt.inner.logs().iter()
        .filter_map(|log| OrderBook::OrderPlaced::decode_log(log, true).ok())
        .map(|e| e.orderId)
        .collect::<Vec<_>>();
    
    self.update_active_orders(market_id, new_ids, bid_tick, ask_tick);
    Ok(())
}

pub async fn place_quotes(&mut self, market_id: u64, bid_tick: u64, ask_tick: u64, ...) -> Result<()> {
    let params = self.build_order_params(bid_tick, ask_tick);
    let tx = self.order_book.placeOrders(U256::from(market_id), params);
    let receipt = ns.lock().await.send(tx).await?.get_receipt().await?;
    // ... parse OrderPlaced events, update tracking
}
```

**Files:** `contracts.rs` (new sol! bindings), `quoter.rs` (rewrite requote + place_quotes)

---

## 8. Update Documentation
**Impact:** Keeps docs accurate for future development  
**Effort:** Small  

### CLAUDE.md Updates
- Update "Contract Interaction" section: document `placeOrders` and `replaceOrders` with `OrderParam` struct
- Update architecture note: requote is now 1 TX (atomic cancel+place) not 5 TXs
- Note `replaceOrders` net settlement behaviour (minimal ERC20 transfers when ticks barely change)
- Update contract addresses to V8

### README.md Updates
- Update "How It Works" step 5: "Requotes atomically via `replaceOrders` (cancel old + place new in single TX)"
- Add performance note: "Zero empty book time during requotes"
- Update any referenced contract addresses

---

## ~~Parallel Nonce Pre-allocation~~ (DROPPED)

~~Pre-allocate N nonces and fire TXs concurrently.~~

**Dropped:** Once `replaceOrders` ships, each requote is 1 TX — no parallelization needed. Not worth implementing for the interim period.

---

## Implementation Order

| Priority | Item | Depends On |
|----------|------|------------|
| **Now** | #1 Config tuning | Nothing |
| **Now** | #3 Fix startup log scan | Nothing |
| **Now** | #4 Sync quoter_orders after placement | Nothing |
| **Now** | #6 Remove 3s sleep | Nothing |
| **Next** | #2 Event-driven requote wake | Nothing |
| **Next** | #5 Combine WS connections | Nothing |
| **Now** | #7 Integrate replaceOrders/placeOrders | Contracts deployed (V8) ✅ |
| **Now** | #8 Update docs (CLAUDE.md, README.md) | After all code changes |

All items can now be done in one session. #7 is the biggest win.
Items 1, 3, 4, 6 are trivial. Item 2 is the main bot-side latency win.
Item 7 eliminates empty book entirely. Item 8 keeps docs accurate.
