# Strike MM — Risk-Reward Model Plan

## Problem with Current Lot-Based Limits

The current risk model caps position in **lots**, which is a poor proxy for actual risk:

- 25k lots YES at tick 5 → costs **$12.50**, wins **$237.50** if YES resolves
- 25k lots YES at tick 85 → costs **$212.50**, wins **$37.50** if YES resolves

Same lot count, completely different dollar exposure and risk profile. The MM is currently blind to this distinction.

Additionally, subsequent quotes after fills don't scale down — the MM just places the same `lots_per_level` again regardless of what it's already accumulated, meaning risk grows unboundedly until the hard lot cap is hit.

---

## Core Concepts

### LOT_SIZE Conversion

On-chain LOT_SIZE = 1e16. Dollar value of a position:

```
cost_usdt = clearing_tick / 100.0 * lots * LOT_SIZE / 1e18
          = clearing_tick / 100.0 * lots * 0.01
```

Example: 25,000 lots filled at clearing_tick 50 = `50/100 * 25000 * 0.01 = $125`

A helper function `lots_to_usdt(tick, lots)` should encapsulate this and be unit tested.

### Per-Fill Accounting

For each fill, track using the **clearing tick** from `BatchCleared` events (uniform FBA price):
- `side`: YES or NO (Bid = buying YES, Ask = selling YES = buying NO)
- `clearing_tick`: the batch's uniform clearing price
- `lots`: quantity filled
- `cost`: `clearing_tick / 100.0 * lots * 0.01` in USDT (what was spent)
- `max_loss`: same as cost — what you lose if position goes to zero
- `max_gain`: `(1.0 - clearing_tick / 100.0) * lots * 0.01` — what you win if position resolves in your favour

**Fill data flow:**
1. `BatchCleared` event fires → store `clearing_tick` for that market/batch
2. Fill events arrive for that batch → use stored `clearing_tick` as cost basis
3. Pass `(clearing_tick, lots, side)` to `PositionState::record_fill()`

### Aggregate Position Metrics

Sum across all fills for a market:

```
total_cost      = Σ fill.cost                          # USDT spent
total_max_loss  = Σ fill.max_loss                      # worst case drawdown
total_max_gain  = Σ fill.max_gain                      # best case profit
```

Expected P&L is directional:
```
If net long YES:
  expected_pnl = total_max_gain * fair_prob - total_max_loss * (1 - fair_prob)
If net long NO (short YES):
  expected_pnl = total_max_gain * (1 - fair_prob) - total_max_loss * fair_prob
```

### Risk Budget

Replace `max_position_per_market` (lots) with `max_loss_budget` (USDT) per market.

**Default:** `max_loss_budget = $500` per market (configurable)

This means:
- At tick 10: can fill up to $500 / (0.10 * 0.01) = 500,000 lots of YES
- At tick 90: can fill up to $500 / (0.90 * 0.01) = 55,556 lots of YES
- The MM naturally takes larger positions when they're cheap (high edge) and smaller when expensive

---

## Quote Sizing Algorithm

Replace the current fixed `lots_per_level` with dynamic sizing based on remaining risk budget:

```
remaining_budget   = max_loss_budget - total_max_loss
quote_lots         = remaining_budget / (tick / 100.0 * 0.01) / num_levels
quote_lots         = min(quote_lots, base_lots_per_level)   # cap at configured max
quote_lots         = max(quote_lots, 0)                     # floor at 0
```

**Directional asymmetry:**
- On the **same side** as current exposure → use `remaining_budget` sizing (reduce)
- On the **opposite side** → use full `base_lots_per_level` (encourage flattening)

Example: MM is long YES (spent $400, max_loss=$400, budget=$500):
- Next YES bid → only $100 remaining → reduced lots
- Next YES ask (selling YES) → full base lots, no reduction (flattening the position)

**Implementation:** `build_order_params` needs to accept `bid_lots: u64, ask_lots: u64` instead of a single `lots_override: u64`. The caller computes each based on position side and remaining budget.

---

## Inventory Skew Improvement

Replace lot-based skew with **dollar-exposure-based skew**:

```
exposure_ratio  = total_max_loss / max_loss_budget      # 0.0 to 1.0
skew_ticks      = round(exposure_ratio * max_skew_ticks)
```

Directional: positive skew (shift quotes down) when long YES, negative when short YES.

This means:
- At tick 90 with 5k lots filled: high dollar exposure → large skew
- At tick 10 with 5k lots filled: low dollar exposure → small skew

The skew reflects actual dollar risk, not arbitrary lot count.

---

## Expected P&L Signal

Use `expected_pnl` for **logging only in v1** — observe the signal before feeding it into quoting decisions.

| expected_pnl | Interpretation | Action (v1) |
|---|---|---|
| Strongly positive | Winning position, good edge | Log only |
| Near zero | Break-even | Log only |
| Negative | Underwater | Log only |
| Deeply negative | Bad fill sequence | Log only |

Could feed into spread widening or size reduction in v2.

---

## New Config Keys

```toml
[risk]
max_loss_budget_usdt = 500.0       # max USDT at risk per market (replaces max_position_per_market)
max_skew_ticks       = 6           # max tick shift from inventory skew (unchanged)
```

---

## Data Structures

### `PositionState` (new struct, replaces raw lot tracking in RiskManager)

```rust
pub struct PositionState {
    pub total_cost: f64,        // USDT spent (sum of all fills)
    pub total_max_loss: f64,    // worst case loss (sum of all fills)
    pub total_max_gain: f64,    // best case gain (sum of all fills)
    pub net_lots: i64,          // signed lot count (positive = long YES)
}

impl PositionState {
    /// Record a fill using clearing tick from BatchCleared event.
    pub fn record_fill(&mut self, clearing_tick: u64, lots: u64, is_bid: bool) { ... }

    /// Remaining USDT budget before hitting max_loss_budget.
    pub fn remaining_budget(&self, max_loss_budget: f64) -> f64 { ... }

    /// Expected P&L given current fair probability.
    pub fn expected_pnl(&self, fair_prob: f64) -> f64 { ... }

    /// Exposure ratio (0.0 to 1.0) for skew calculation.
    pub fn exposure_ratio(&self, max_loss_budget: f64) -> f64 { ... }

    /// Compute lots to quote on the same side as current position (reduced by budget).
    pub fn quote_lots_same_side(&self, tick: u64, base_lots: u64, num_levels: u64,
                                 max_loss_budget: f64) -> u64 { ... }
}
```

### Clearing tick storage

```rust
/// In EventState or main loop:
/// market_id → most recent clearing_tick from BatchCleared event
clearing_ticks: HashMap<u64, u64>
```

Updated on each `BatchCleared` event. Consumed when fills arrive for that market.

---

## Files to Modify

- `src/risk.rs` — replace `RiskManager` internals with `PositionState`-based model; `record_fill` takes `(clearing_tick, lots, is_bid)`; `inventory_skew` uses exposure ratio; `can_place` uses dollar budget
- `src/main.rs` — store clearing tick from `BatchCleared` events; pass clearing tick + side to `record_fill`; compute `bid_lots` / `ask_lots` separately based on position; log `expected_pnl` and `exposure_ratio` in REQUOTING line
- `src/quoter.rs` — `build_order_params` takes `bid_lots: u64, ask_lots: u64` instead of `lots_override: u64`; same for `place_quotes`, `requote`
- `src/event_state.rs` — add `clearing_tick` to `FillEvent`; add clearing tick storage
- `src/config.rs` — new config fields (`max_loss_budget_usdt`)
- `config/default.toml` — new config values

---

## Implementation Order

1. `lots_to_usdt()` helper + unit tests
2. `PositionState` struct + unit tests in `risk.rs`
3. Store clearing tick from `BatchCleared` events in main loop
4. Wire up fill tracking with clearing tick + side
5. `build_order_params` → split into `bid_lots` / `ask_lots`
6. Compute directional quote sizing in main loop
7. Update `inventory_skew` to use exposure ratio
8. Add `expected_pnl` + `exposure_ratio` to REQUOTING log line
9. Update config + defaults

## Notes

- Keep `net_lots` in `PositionState` for compatibility with existing cancel/cleanup logic
- `expected_pnl` is logging-only in v1 — don't feed it into quoting decisions yet
- Unit test edge cases: tick=1 (near-free YES), tick=99 (near-certain YES), zero budget remaining, budget exactly exhausted
- **Restart behaviour:** `PositionState` is in-memory. If MM restarts mid-market, fill history resets and risk budget goes back to full. Acceptable for v1 with 5-min markets — at most one market of oversized quotes. Can add indexer-based recovery in v2.
