# Strike MM — Quoting Improvements Plan

## Problem

The market maker posts a symmetric two-sided market regardless of how extreme the probability is. Near expiry with a strong directional view, this causes two issues:

1. **Adverse selection on the losing side** — e.g. if NO is 99% likely to win, the MM is still selling NO at 0.07–0.09 cents (i.e. posting YES bids at tick ~91–93), giving away cheap options to informed traders who know the outcome is almost certain.

2. **Missing the profitable trade** — instead of accumulating the winning side cheaply, the MM is providing two-sided liquidity it doesn't need to.

## Root Causes

- `fair_value()` hard-clamps output to `[0.01, 0.99]` — extreme probabilities never get reflected fully
- `compute_ticks()` applies a fixed 6-tick spread regardless of time-to-expiry or confidence
- The quoting loop always places both bid and ask at all levels, no one-sided logic exists
- No size reduction near expiry — same lots placed with 2 seconds left as with 5 minutes left

## Planned Improvements

### 1. One-Sided Quoting (High Impact)

**Threshold:** configurable `one_sided_threshold` (default: `0.90`)

- If `fair_prob > threshold` → only post YES bids (buy YES cheap). Cancel all YES asks.
- If `fair_prob < (1 - threshold)` → only post YES asks (buy NO cheap). Cancel all YES bids.
- Between the thresholds → normal two-sided market

This is the highest-impact fix. Stops the MM from gifting away near-certain winners.

**Config addition:**
```toml
[quoting]
one_sided_threshold = 0.90   # go one-sided when fair prob > 90% or < 10%
```

---

### 2. Time-Decay Spread Widening (Medium Impact)

Near expiry, adverse selection risk is highest (informed traders know more). Widen the spread as time runs out.

**Logic:**
- `secs_left > 120` → normal spread (6 ticks)
- `60 < secs_left <= 120` → 1.5× spread (9 ticks)
- `30 < secs_left <= 60` → 2× spread (12 ticks)
- `secs_left <= 30` → stop quoting entirely (cancel all orders, don't place new ones)

**Config additions:**
```toml
[quoting]
expiry_spread_multiplier_120s = 1.5   # spread multiplier when <120s left
expiry_spread_multiplier_60s  = 2.0   # spread multiplier when <60s left
min_quote_secs = 30                   # stop quoting below this threshold
```

---

### 3. Raise Fair Value Clamp (Low Impact, Easy)

Change `p.clamp(0.01, 0.99)` → `p.clamp(0.001, 0.999)` in `fair_value()`.

Allows extreme B-S probabilities to be reflected more accurately in tick placement. At very short time-to-expiry with large price deviation, B-S naturally goes to 0.999+ — let it.

---

### 4. Size Reduction Near Expiry (Medium Impact)

Scale `lots_per_level` down linearly as time runs out to reduce risk exposure when adverse selection is highest.

**Logic:**
- `secs_left > 120` → 100% of configured lots
- `60 < secs_left <= 120` → 50% of configured lots
- `secs_left <= 60` → 25% of configured lots (or 0 if combined with improvement #2)

---

## Implementation Order

1. **One-sided quoting** — implement first, highest ROI
2. **Raise fair value clamp** — trivial, do alongside #1
3. **Time-decay spread widening + stop quoting near expiry** — second pass
4. **Size reduction near expiry** — can be folded into #3

## Files to Modify

- `src/pricing.rs` — clamp change (#3), spread widening helper (#3)
- `src/main.rs` — one-sided logic in quoting loop (#1), size scaling (#4), early exit near expiry (#3)
- `src/quoter.rs` — cancel only one side (bids or asks) without cancelling both (#1)
- `config/default.toml` — new config keys
- `src/config.rs` — new config fields

## Notes

- All thresholds should be configurable via `config/default.toml`, not hardcoded
- One-sided mode should log clearly when it activates/deactivates so we can monitor it
- The `requote` path in `quoter.rs` needs to support placing only bids, only asks, or both — currently it always does both
- When transitioning from one-sided back to two-sided (e.g. price moves back toward strike), the cancelled side needs to be re-quoted immediately
