use std::collections::HashMap;

use crate::market_manager::Market;

/// A fill detected from an OrderSettled event.
#[derive(Debug, Clone)]
pub struct FillEvent {
    pub order_id: u64,
    pub market_id: u64,
    pub filled_lots: u64,
    /// "bid" or "ask" — looked up from quoter active_orders at event time
    pub side: String,
}

/// Shared state fed by WS event subscriptions and read by the main loop.
#[derive(Debug, Default)]
pub struct EventState {
    /// Active markets discovered via MarketCreated events (and initial snapshot).
    /// orderBookMarketId → Market
    pub active_markets: HashMap<u64, Market>,

    /// Accumulated fill events since last drain.
    pub fills: Vec<FillEvent>,

    /// True once the initial market snapshot has been loaded.
    pub initialized: bool,
}
