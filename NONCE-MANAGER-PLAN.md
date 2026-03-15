# Shared Nonce Manager — Implementation Plan

## Problem

The quoter and redeemer both use the same wallet and independently send transactions.
Each manages its own nonce state. When both are active simultaneously, they assign the
same nonce to different txs — causing "nonce too low" / "could not replace existing tx" errors.

Current flow (broken under concurrency):
```
Quoter:   sync_nonce() → nonce=10693 → place ask at 10693 ✅
Redeemer: (fires simultaneously) → also uses 10693 → COLLISION ❌
Quoter:   place next ask → nonce=10694 (stale) → "nonce too low" ❌
```

## Solution: `NonceSender`

A single shared struct wrapping the provider. All tx sends go through it.
It holds the canonical next nonce and increments atomically.

```
Arc<Mutex<NonceSender>>
    ├── quoter.requote()     → NonceSender::send(tx)
    └── redeemer loop       → NonceSender::send(tx)
```

## New File: `src/nonce_sender.rs`

```rust
pub struct NonceSender<P> {
    provider: P,
    signer_addr: Address,
    nonce: u64,
}

impl<P: Provider + Clone> NonceSender<P> {
    /// Initialize by fetching nonce from chain.
    pub async fn new(provider: P, signer_addr: Address) -> Result<Self>

    /// Re-fetch nonce from chain (use after errors or startup).
    pub async fn sync(&mut self) -> Result<()>

    /// Send a transaction request, stamping it with the next nonce.
    /// Increments nonce optimistically on success.
    /// On "nonce too low" error: syncs from chain and retries once.
    pub async fn send(&mut self, tx: TransactionRequest) -> Result<PendingTransactionBuilder>
}
```

### Error handling in `send()`

```
1. Stamp tx with self.nonce
2. send().await
3a. Success → self.nonce += 1 → return receipt
3b. "nonce too low" or "replacement" error:
    → self.sync().await   (re-fetch from chain)
    → retry once with new nonce
    → if still fails → return Err
```

## Changes Required

### `src/quoter.rs`
- Remove `nonce: u64` field from `Quoter`
- Remove `sync_nonce()` method (replaced by `NonceSender::sync()`)
- Remove `startup_cancel_sweep` nonce bookkeeping
- Change all `provider.send_transaction(tx)` calls to `nonce_sender.lock().await.send(tx)`
- Accept `Arc<Mutex<NonceSender<P>>>` in constructor

### `src/redeemer.rs`
- Change signature of `run_redeem_loop()` to accept `Arc<Mutex<NonceSender<P>>>`
- Replace all tx sends with `nonce_sender.lock().await.send(tx)`
- Remove internal nonce tracking

### `src/main.rs`
- Create `NonceSender` once after wallet init
- Wrap in `Arc<Mutex<NonceSender>>`
- Pass the same `Arc` to both `quoter` and `run_redeem_loop`
- Remove the separate `quoter.sync_nonce()` call at startup and after sweep
  (NonceSender initializes from chain in its constructor)

### `src/approve_vault()` (in quoter.rs)
- This runs before the main loop, no concurrency risk — keep sending directly
  OR just pass the `NonceSender` and use it there too for consistency

## Type Approach: Non-Generic via `Arc<dyn Provider>`

`NonceSender` uses a type-erased provider internally — no generic parameter:

```rust
pub struct NonceSender {
    provider: Arc<dyn Provider>,   // type-erased — no <P>
    signer_addr: Address,
    nonce: u64,
}
```

`Arc<Mutex<NonceSender>>` is a plain concrete type. No bounds to thread through the call stack.

`Quoter<P>` keeps its `P` for other provider calls (eth_call, get_balance, etc.) but holds
`NonceSender` without any extra generic:

```rust
pub struct Quoter<P> {
    provider: P,                            // unchanged — used for non-send calls
    nonce_sender: Arc<Mutex<NonceSender>>,  // no <P> here
    ...
}
```

Why it works: `NonceSender` only calls `get_transaction_count()` (for sync) and
`send_transaction()` — both are on the base `Provider` trait. The wallet signing happens
upstream in the `WalletFiller` layer already wrapping the provider, so the signed tx
arrives at `send_transaction()` already prepared. No wallet-specific methods needed inside
`NonceSender`.

Construction in `main.rs`:
```rust
let http_provider = ProviderBuilder::new().wallet(wallet).connect_http(...);
// Erase the type — Arc<dyn Provider> satisfies NonceSender
let nonce_sender = Arc::new(Mutex::new(
    NonceSender::new(Arc::new(http_provider.clone()) as Arc<dyn Provider>, signer_addr).await?
));
// Pass the same Arc to both
let quoter = Quoter::new(..., Arc::clone(&nonce_sender));
tokio::spawn(redeemer::run_redeem_loop(..., Arc::clone(&nonce_sender)));
```

## Locking Strategy

The `Mutex` lock is held only for the duration of the `send()` call.
This means txs are serialized — quoter and redeemer cannot send simultaneously.
That's intentional: serialized sends guarantee no nonce collisions.

Since tx confirmation is awaited outside the lock (via the returned `PendingTransactionBuilder`),
throughput isn't meaningfully impacted — the lock is only held during the RPC send call (~50ms),
not the full confirmation wait (~3s BSC block time).

```rust
// Usage pattern
let pending = {
    let mut ns = nonce_sender.lock().await;
    ns.send(tx).await?
};
// Lock released here — other senders can proceed
let receipt = pending.get_receipt().await?;  // Wait for confirmation outside lock
```

## Sequence After Fix

```
t=0:  Quoter locks NonceSender, sends cancel at nonce=10693, releases lock
t=1:  Redeemer locks NonceSender, sends redeem at nonce=10694, releases lock
t=2:  Quoter locks NonceSender, sends place bid at nonce=10695, releases lock
t=3:  Quoter locks NonceSender, sends place ask at nonce=10696, releases lock
```

No collisions. No "nonce too low". No aborted order sequences.

## Files to Create/Modify

| File | Change |
|------|--------|
| `src/nonce_sender.rs` | **New** — NonceSender struct |
| `src/main.rs` | Wire up Arc<Mutex<NonceSender>>, pass to quoter + redeemer |
| `src/quoter.rs` | Replace nonce field + sync_nonce with NonceSender |
| `src/redeemer.rs` | Replace direct provider sends with NonceSender |
| `src/lib.rs` | Add `mod nonce_sender` |

## Estimated Complexity

Medium. The logic change is simple; the tricky part is threading the
`Arc<Mutex<NonceSender<P>>>` through the type system, especially the
generic `P: Provider` bound that quoter already carries.

One option to simplify: make `NonceSender` non-generic by boxing the provider
with `Arc<dyn Provider>` internally — avoids generic propagation.
