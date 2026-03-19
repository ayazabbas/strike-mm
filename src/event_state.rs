use std::collections::{HashMap, HashSet};

use strike_sdk::indexer::types::Market;

/// A fill detected from an OrderSettled event.
#[derive(Debug, Clone)]
pub struct FillEvent {
    pub order_id: u64,
    pub market_id: u64,
    pub filled_lots: u64,
    /// "bid" or "ask" — looked up from quoter active_orders at event time
    pub side: String,
    /// Clearing tick from the BatchCleared event for this batch
    pub clearing_tick: u64,
}

/// Shared state fed by WS event subscriptions and read by the main loop.
#[derive(Debug, Default)]
pub struct EventState {
    /// Active markets discovered via MarketCreated events (and initial snapshot).
    /// orderBookMarketId → Market
    pub active_markets: HashMap<u64, Market>,

    /// Accumulated fill events since last drain.
    pub fills: Vec<FillEvent>,

    /// Markets that had a batch cleared since last drain.
    /// The quoter should invalidate its local orders and re-place immediately.
    pub cleared_markets: HashSet<u64>,

    /// True once the initial market snapshot has been loaded.
    pub initialized: bool,

    /// Most recent clearing tick per market from BatchCleared events.
    /// market_id → clearing_tick
    pub clearing_ticks: HashMap<u64, u64>,
}
