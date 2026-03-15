# Strike MM Rework Plan
<!-- Written: 2026-03-15 -->

## Problem Summary

The MM bot (`strike-mm`) is unintentionally self-trading, accounting for ~99% of all testnet volume.
Root causes (in order of severity):

1. **25 systemd restarts in 2 days** — each restart wipes in-memory order state. Orphaned orders accumulate with no way to cancel them.
2. **Broken nonce management** — when a tx times out, the local nonce counter advances but the tx is still pending in the mempool. Next tx at the same nonce gets rejected with `"could not replace existing tx"`. Cancels silently fail, new orders are placed on top of stale ones.
3. **Indexer dependency for cancel logic** — cancel_via_indexer has ~5s lag, can return stale data, and fails silently if the indexer is unreachable.
4. **Non-atomic cancel + requote** — cancel and new quote are separate txs. Window between them allows the stale order to participate in a batch clear.

## Design Goals

- MM never has uncancelled stale orders when placing new ones
- MM can recover full order state from chain on restart (no disk state needed)
- Cancel + new quotes happen atomically in one tx, one nonce
- No dependency on the indexer for order management
- Nonce never goes out of sync

---

## Plan

### Phase 1 — On-Chain State Recovery on Startup

Replace indexer-based order tracking with direct chain event scanning on boot.

**How:**
- On startup, scan `OrderPlaced` events from the MM wallet (filtered by `owner`) for all active (non-expired) markets
- For each placed order, check if it was settled or cancelled by scanning `OrderSettled` / `GtcAutoCancelled` events
- Anything placed but not settled/cancelled = still live on-chain → add to local state
- Then issue a startup cancel sweep (see Phase 2) before quoting begins

**Why not disk persistence:**
The chain IS the ground truth. Reconstructing from events is always correct, even after crashes, redeploys, or manual interventions. Disk state can go stale; chain state can't.

**Implementation:**
```rust
// On startup, after wallet init:
let live_orders = recover_live_orders(&provider, mm_address, from_block).await?;
quoter.restore_state(live_orders);
quoter.startup_cancel_sweep().await?; // cancel everything before quoting
```

Scan from the block of the current contract deployment (hardcoded constant) to avoid scanning genesis.

---

### Phase 2 — Startup Cancel Sweep

Before placing any order, cancel ALL live orders recovered in Phase 1.

Use Multicall3 with `allowFailure=true` (orders may already be cancelled on-chain if the market expired).
Wait for confirmation before entering the main loop.

```rust
async fn startup_cancel_sweep(&mut self) -> Result<()> {
    let all_ids: Vec<U256> = self.active_orders.values()
        .flat_map(|m| m.bid_order_ids.iter().chain(m.ask_order_ids.iter()))
        .copied()
        .collect();
    if all_ids.is_empty() { return Ok(()); }
    // multicall cancel, allowFailure=true, await receipt
    self.multicall_cancel(all_ids, true).await
}
```

---

### Phase 3 — Atomic Cancel + Quote via Multicall3

Replace the current two-step (cancel tx → place tx) with a single Multicall3 tx:

```
multicall([
  cancelOrder(id1),   // allowFailure=false — must succeed
  cancelOrder(id2),   // allowFailure=false — must succeed
  placeOrder(bid1),
  placeOrder(bid2),
  placeOrder(ask1),
  placeOrder(ask2),
])
```

`allowFailure=false` on cancels means the entire tx reverts if any cancel fails.
New orders **cannot land on-chain** unless all cancels confirmed.
Single nonce consumed → no nonce desync possible.

**Edge case:** first quote for a market (no orders to cancel) — just bundle the places.

**Implementation change in `requote()`:**
```rust
// Instead of:
self.cancel_all_orders(...).await?;
self.sync_nonce(mm_addr).await?;
self.place_quotes(...).await?;

// New:
self.atomic_cancel_and_quote(cancel_ids, new_orders).await?;
// single multicall tx, single nonce, single receipt wait
```

---

### Phase 4 — Block Subscription for Book State

Replace the indexer polling loop with direct BSC log subscription.

**Subscribe to these events from the MM's own address:**
- `OrderPlaced(orderId, owner, ...)` — track new order IDs
- `OrderSettled(orderId, owner, filledLots, collateralReleased)` — remove settled orders
- `GtcAutoCancelled(orderId, owner)` — remove cancelled orders

**Subscribe to these for batch timing:**
- `BatchCleared(marketId, batchId, clearingTick, matchedLots)` — know when a batch closes, triggering position reconciliation
- `MarketCreated` / `MarketStateChanged` — discover new markets without polling the indexer

**Why this matters:**
- Removes all indexer dependency from the hot path
- Latency drops from ~5s (indexer poll) to ~1 block (~3s)
- Position state is always current — no stale cancel lists

**Implementation:**
```rust
// Replace market_manager's indexer polling with:
let filter = Filter::new()
    .address(order_book_addr)
    .events([OrderPlaced::SIGNATURE, OrderSettled::SIGNATURE, ...]);
let mut stream = provider.subscribe_logs(&filter).await?;
while let Some(log) = stream.next().await {
    handle_log(log, &mut state);
}
```

---

### Phase 5 — Nonce Management Simplification

The current approach (local AtomicU64, sync on error) gets confused by pending mempool txs.

**New approach:** Since Phase 3 means we send one multicall per requote cycle (not 4-8 individual txs), nonce management becomes trivial:
- Sync nonce once at startup
- Increment by 1 after each confirmed multicall receipt
- On any nonce error, sync from chain and retry once

With one tx per cycle instead of 4-8, nonce conflicts become nearly impossible.

---

### Phase 6 — Systemd Stability

Fix the restart storm (25 restarts in 2 days).

- Add `RestartSec=10` to the systemd unit (currently likely 0 or 1s — rapid restart loop)
- Add `StartLimitIntervalSec=300` and `StartLimitBurst=5` to cap restart rate
- Ensure the MM exits cleanly (not with a panic) on recoverable errors so systemd doesn't restart unnecessarily
- Log the restart reason clearly

---

## Implementation Order

| Phase | What | Why first |
|-------|------|-----------|
| 1+2 | Startup recovery + sweep | Stops orphan accumulation immediately |
| 3 | Atomic cancel+quote multicall | Eliminates self-trading at source |
| 5 | Nonce simplification | Falls out of Phase 3 naturally |
| 6 | Systemd stability | Reduces restart frequency |
| 4 | Block subscription | Latency + removes indexer dep (bigger refactor, do last) |

Phases 1-3+5+6 can be done together as a focused fix. Phase 4 is a larger refactor to do afterwards once the core correctness is fixed.

---

## What We Are NOT Changing

- Black-Scholes pricing model — works correctly
- Spread/level config — 6 ticks spread with 2 levels is fine
- Risk manager — correct
- Redeemer — correct
- Contract — no changes needed (Multicall3 gives us atomicity without a new contract function)
