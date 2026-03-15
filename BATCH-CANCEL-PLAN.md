# batchCancel — Implementation Plan

## Problem

`cancelOrder` is single-order only. Cancelling 4 orders before a requote takes ~28s
(4 sequential txs × ~7s each). This delays every requote cycle and means the MM is
effectively off the market for 28s while repositioning.

## Solution

Add `cancelOrders(uint256[] calldata orderIds)` to `OrderBook.sol` — the owner-checked
equivalent of the existing `cancelExpiredOrders` (which anyone can call, but only on
expired markets). This lets the MM cancel all 4 orders in a single tx.

---

## Contract Changes (`contracts/src/OrderBook.sol`)

Add one new function, directly modelled on `cancelExpiredOrders`:

```solidity
/// @notice Batch cancel multiple orders. Caller must be the owner of every order.
/// @dev Skips orders that are already cancelled/filled (lots == 0) rather than
///      reverting, so partial cancels don't fail the whole batch.
function cancelOrders(uint256[] calldata orderIds) external nonReentrant {
    for (uint256 i = 0; i < orderIds.length; i++) {
        Order storage o = orders[orderIds[i]];

        // Skip already-cancelled/filled orders silently
        if (o.lots == 0) continue;

        // Owner check — revert the whole batch if any order isn't ours.
        // This is intentional: the MM should never pass an order it doesn't own.
        require(o.owner == msg.sender, "OrderBook: not owner");

        uint256 lots = o.lots;
        uint256 tick = o.tick;
        uint256 marketId = o.marketId;
        Side side = o.side;

        uint256 collateral;
        if (side == Side.Bid) {
            collateral = (lots * LOT_SIZE * tick) / 100;
        } else {
            collateral = (lots * LOT_SIZE * (100 - tick)) / 100;
        }

        uint256 fee = feeModel.calculateFee(collateral);
        uint256 totalReturn = collateral + fee;

        o.lots = 0;

        if (side == Side.Bid) {
            bidTrees[marketId].update(tick, -int256(lots));
        } else {
            askTrees[marketId].update(tick, -int256(lots));
        }

        vault.unlock(msg.sender, totalReturn);
        vault.withdrawTo(msg.sender, totalReturn);

        emit OrderCancelled(orderIds[i], marketId, msg.sender);
    }
}
```

### Design decisions

**Skip vs revert on already-cancelled orders:**
`if (o.lots == 0) continue` — skip silently. Orders can get settled/cancelled by the
keeper between when the MM decides to cancel and when the tx lands. Reverting on stale
IDs would cause the entire cancel batch to fail, which is worse than just skipping them.

**Revert on wrong owner:**
`require(o.owner == msg.sender)` — hard revert. The MM should never try to cancel
someone else's order. If this fires, something is wrong with the MM logic; fail loudly.

**No max batch size:**
The MM will pass at most `num_levels * 2` orders (currently 4). No need to cap at
contract level. Gas is linear and bounded by the MM's own config.

---

## Tests (`contracts/test/OrderBook.t.sol`)

Add a new test section:

```solidity
// ── cancelOrders (batch) ──────────────────────────────────────────────────

function test_CancelOrders_BatchCancelsAll() public {
    // Place 4 orders (2 bids, 2 asks), cancel all in one call
    // Assert: all 4 emitted OrderCancelled, lots==0, USDT returned
}

function test_CancelOrders_SkipsAlreadyCancelled() public {
    // Place 2 orders, cancel order[0] individually, then call cancelOrders([0,1])
    // Assert: doesn't revert, order[1] is cancelled, order[0] skipped silently
}

function test_CancelOrders_RevertsIfNotOwner() public {
    // Place order as alice, try to cancelOrders([orderId]) as bob
    // Assert: reverts "OrderBook: not owner"
}

function test_CancelOrders_MixedSides() public {
    // Place 2 bids + 2 asks at different ticks, cancelOrders all 4
    // Assert: segment trees updated correctly for each side/tick
    // Assert: correct collateral+fee returned for each
}
```

---

## MM Changes (`strike-mm/src/quoter.rs`)

### New method: `cancel_local_orders_batch()`

Replace the existing `cancel_local_orders()` sequential loop with a single batch call:

```rust
pub async fn cancel_local_orders_batch(
    &mut self,
    market_id: u64,
    nonce_sender: &Arc<Mutex<NonceSender>>,
) -> Result<()> {
    let order_ids: Vec<U256> = self.active_orders
        .get(&market_id)
        .map(|s| s.order_ids.iter().copied().collect())
        .unwrap_or_default();

    if order_ids.is_empty() {
        return Ok(());
    }

    let call = IOrderBook::cancelOrdersCall { orderIds: order_ids.clone() };
    let tx = TransactionRequest::default()
        .to(self.order_book_addr)
        .input(call.abi_encode().into());

    let pending = {
        let mut ns = nonce_sender.lock().await;
        ns.send(tx).await?
    };

    let receipt = pending.get_receipt().await?;
    info!(
        market_id,
        count = order_ids.len(),
        tx = %receipt.transaction_hash,
        gas_used = receipt.gas_used,
        "batch cancel confirmed"
    );

    // Clear local tracking
    self.active_orders.remove(&market_id);
    Ok(())
}
```

### Keep `cancel_local_orders()` as fallback

Keep the existing sequential method. Use batch by default; fall back to sequential if
the batch call reverts (e.g., some unexpected owner mismatch).

---

## ABI Update (`strike-mm/abi/OrderBook.json`)

After redeploying the contract, regenerate the ABI:
```bash
cd ~/dev/strike/contracts
forge build
cat out/OrderBook.sol/OrderBook.json | python3 -c "
import json, sys; d=json.load(sys.stdin); print(json.dumps(d['abi'], indent=2))
" > ~/dev/strike-mm/abi/OrderBook.json
```

---

## Deployment Steps

1. **Add function + tests** to `OrderBook.sol` / `OrderBook.t.sol`
2. **Run tests:** `forge test --match-contract OrderBookTest -v` — all 245+ existing + new tests must pass
3. **Redeploy contracts** (full redeploy needed — contract address changes):
   ```bash
   cd ~/dev/strike/contracts
   forge script script/Deploy.s.sol --rpc-url $BSC_TESTNET_RPC --broadcast
   ```
4. **Update all addresses** in:
   - `strike-infra/.env.testnet`
   - `strike-mm/config/default.toml`
   - `MEMORY.md` (BSC testnet addresses section)
5. **Regenerate OrderBook ABI** (step above)
6. **Update MM** — add `cancelOrders` to sol! bindings, implement `cancel_local_orders_batch()`
7. **Build + deploy MM:** `cargo build --release` → systemctl restart strike-mm
8. **Restart infra services** (indexer, keepers) with new addresses

---

## Expected Impact

| Metric | Before | After |
|--------|--------|-------|
| Cancel latency | ~28s (4 × 7s) | ~7s (1 tx) |
| Txs per requote cycle | 4 cancels + 4 places = 8 | 1 cancel + 4 places = 5 |
| Gas per requote | ~4 × 113k + ~4 × 280k = ~1.57M | ~200k + ~4 × 280k = ~1.32M |
| MM off-market time | ~28s | ~7s |

---

---

## Frontend Changes (`strike-frontend`)

### New component: `CancelAllOrdersButton`

Add to `src/app/portfolio/page.tsx`, alongside the existing `CancelOrderButton`:

```tsx
function CancelAllOrdersButton({ orderIds }: { orderIds: string[] }): React.ReactNode {
  const chainId = useChainId()
  const contracts = CONTRACTS[chainId as ChainId]
  const queryClient = useQueryClient()
  const { data: hash, writeContract, isPending } = useWriteContract()
  const { isLoading: isConfirming } = useWaitForTransactionReceipt({ hash })
  const { t } = useTranslation()

  function handleCancelAll(): void {
    if (!contracts) {
      toast.error(t('common.unsupportedChain'))
      return
    }

    writeContract(
      {
        address: contracts.ORDER_BOOK as `0x${string}`,
        abi: OrderBookAbi,
        functionName: 'cancelOrders',       // new batch function
        args: [orderIds.map(BigInt)],
      },
      {
        onSuccess: () => {
          toast.success(t('common.allOrdersCancelled'))
          void queryClient.invalidateQueries({ queryKey: ['orders'] })
          void queryClient.invalidateQueries({ queryKey: ['positions'] })
        },
        onError: (err) => toast.error(err.message.slice(0, 100)),
      },
    )
  }

  return (
    <Button
      variant="destructive"
      size="sm"
      disabled={isPending || isConfirming || orderIds.length === 0}
      onClick={handleCancelAll}
    >
      {isPending || isConfirming
        ? t('common.cancelling')
        : t('common.cancelAll')} ({orderIds.length})
    </Button>
  )
}
```

### Wire into the Open Orders tab

In the `TabsContent value="open"` section, add the button in the `CardHeader` next to the title:

```tsx
<TabsContent value="open">
  <Card>
    <CardHeader className="flex flex-row items-center justify-between">
      <CardTitle className="text-base">{t('portfolio.openOrders')}</CardTitle>
      {openOrders.length > 0 && (
        <CancelAllOrdersButton
          orderIds={openOrders.map((o) => o.id)}
        />
      )}
    </CardHeader>
    <CardContent>
      ...
    </CardContent>
  </Card>
</TabsContent>
```

The button only renders when there are open orders (`openOrders.length > 0`).

### New translation keys

Add to both `en.json` and `zh.json`:

```json
"common.cancelAll": "Cancel All",
"common.allOrdersCancelled": "All orders cancelled"
```

Chinese:
```json
"common.cancelAll": "全部取消",
"common.allOrdersCancelled": "所有订单已取消"
```

### ABI update

`cancelOrders` needs to be in `OrderBookAbi` (the frontend's ABI constant). After
redeploying, regenerate the ABI and update the frontend's ABI file at:
`src/lib/abi/OrderBook.ts` (or wherever it lives — check `grep -r "cancelOrder" src/lib/`).

The new entry to add to the ABI array:
```json
{
  "type": "function",
  "name": "cancelOrders",
  "inputs": [{ "name": "orderIds", "type": "uint256[]", "internalType": "uint256[]" }],
  "outputs": [],
  "stateMutability": "nonpayable"
}
```

---

## Note on Full Redeploy

This requires a full contract redeploy (not just an upgrade) since OrderBook has no
proxy pattern. All infra services (indexer, keepers) need their addresses updated.
Coordinate so nothing is pointing at the old contract while the new one goes live:
stop all services first, update all addresses, restart together.

---

## Address Update Checklist (post-redeploy)

After `forge script Deploy.s.sol --broadcast`, capture all new addresses from the
deploy output. Then update **every location below** — missing any one will cause
silent failures (old contract calls, wrong block scan start, stale ABIs).

### 1. `~/dev/strike-infra/.env.testnet`
All 8 addresses + `INDEXER_FROM_BLOCK` (set to the deploy tx block number):
```
ORDER_BOOK_ADDR=<new>
BATCH_AUCTION_ADDR=<new>
MARKET_FACTORY_ADDR=<new>
VAULT_ADDR=<new>
OUTCOME_TOKEN_ADDR=<new>
REDEMPTION_ADDR=<new>
FEE_MODEL_ADDR=<new>
PYTH_RESOLVER_ADDR=<new>
INDEXER_FROM_BLOCK=<deploy block>   ← CRITICAL: indexer must not scan from block 0
```

### 2. `~/dev/strike-infra/crates/strike-common/abi/OrderBook.json`
Regenerate from Foundry artifacts — `cancelOrders` must be present:
```bash
cat ~/dev/strike/contracts/out/OrderBook.sol/OrderBook.json \
  | python3 -c "import json,sys; d=json.load(sys.stdin); print(json.dumps(d['abi'], indent=2))" \
  > ~/dev/strike-infra/crates/strike-common/abi/OrderBook.json
```
Also regenerate the other ABIs if any changed (BatchAuction, MarketFactory):
```bash
for contract in BatchAuction MarketFactory; do
  cat ~/dev/strike/contracts/out/${contract}.sol/${contract}.json \
    | python3 -c "import json,sys; d=json.load(sys.stdin); print(json.dumps(d['abi'], indent=2))" \
    > ~/dev/strike-infra/crates/strike-common/abi/${contract}.json
done
```

### 3. `~/dev/strike-frontend/src/lib/contracts.ts`
Update the `97` (BSC testnet) chain ID entry — all 9 addresses:
```ts
97: {
  MARKET_FACTORY: '<new>',
  ORDER_BOOK:     '<new>',
  BATCH_AUCTION:  '<new>',
  VAULT:          '<new>',
  OUTCOME_TOKEN:  '<new>',
  PYTH_RESOLVER:  '<new>',
  REDEMPTION:     '<new>',
  FEE_MODEL:      '<new>',
  USDT:           '<new>',   // MockUSDT — new address on testnet
}
```

### 4. `~/dev/strike-frontend/src/lib/abi/OrderBook.json`
Regenerate — must include the new `cancelOrders` function:
```bash
cat ~/dev/strike/contracts/out/OrderBook.sol/OrderBook.json \
  | python3 -c "import json,sys; d=json.load(sys.stdin); print(json.dumps(d['abi'], indent=2))" \
  > ~/dev/strike-frontend/src/lib/abi/OrderBook.json
```
Also regenerate any other changed ABIs (BatchAuction, MarketFactory) into
`~/dev/strike-frontend/src/lib/abi/`.

### 5. `~/dev/strike-mm/config/default.toml`
All 7 addresses used by the MM:
```toml
[contracts]
order_book      = "<new>"
vault           = "<new>"
usdt            = "<new>"
redemption      = "<new>"
outcome_token   = "<new>"
batch_auction   = "<new>"
market_factory  = "<new>"
```

### 6. `~/dev/strike-mm/abi/OrderBook.json`
Same regeneration as above — the MM uses this for its sol! bindings:
```bash
cat ~/dev/strike/contracts/out/OrderBook.sol/OrderBook.json \
  | python3 -c "import json,sys; d=json.load(sys.stdin); print(json.dumps(d['abi'], indent=2))" \
  > ~/dev/strike-mm/abi/OrderBook.json
```
Also regenerate `abi/BatchAuction.json` and `abi/MarketFactory.json` if changed.

### 7. `MEMORY.md` — BSC Testnet Addresses section
Update the V4 → V5 address block so future sessions have the right addresses:
```
#### BSC Testnet Addresses (V5 — deployed YYYY-MM-DD, batchCancel)
- MockUSDT: <new>
- FeeModel: <new>
- OutcomeToken: <new>
- Vault: <new>
- OrderBook: <new>
- BatchAuction: <new>
- MarketFactory: <new>
- PythResolver: <new>
- Redemption: <new>
```

### 8. Service restarts (after all addresses updated)
Stop everything first, then restart in order:
```bash
# Stop
sudo systemctl stop strike-mm strike-keeper strike-indexer

# Verify all address files are updated (sanity check)
grep "ORDER_BOOK_ADDR" ~/dev/strike-infra/.env.testnet
grep "order_book" ~/dev/strike-mm/config/default.toml

# Restart infra (indexer must be up before keepers)
sudo systemctl start strike-indexer
sleep 5
sudo systemctl start strike-keeper
sudo systemctl start strike-mm

# Tail logs
journalctl -u strike-indexer -u strike-keeper -u strike-mm -f
```

### Summary table

| File | What changes |
|------|-------------|
| `strike-infra/.env.testnet` | All 8 addresses + `INDEXER_FROM_BLOCK` |
| `strike-infra/crates/strike-common/abi/OrderBook.json` | ABI (add `cancelOrders`) |
| `strike-infra/crates/strike-common/abi/BatchAuction.json` | ABI (if changed) |
| `strike-infra/crates/strike-common/abi/MarketFactory.json` | ABI (if changed) |
| `strike-frontend/src/lib/contracts.ts` | All 9 addresses |
| `strike-frontend/src/lib/abi/OrderBook.json` | ABI (add `cancelOrders`) |
| `strike-frontend/src/lib/abi/BatchAuction.json` | ABI (if changed) |
| `strike-frontend/src/lib/abi/MarketFactory.json` | ABI (if changed) |
| `strike-mm/config/default.toml` | All 7 addresses |
| `strike-mm/abi/OrderBook.json` | ABI (add `cancelOrders`) |
| `strike-mm/abi/BatchAuction.json` | ABI (if changed) |
| `strike-mm/abi/MarketFactory.json` | ABI (if changed) |
| `MEMORY.md` | BSC testnet addresses block |
