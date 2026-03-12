use eyre::{Result, WrapErr};
use serde::Deserialize;
use std::collections::HashSet;
use tracing::{info, warn};

#[derive(Debug, Clone, Deserialize)]
pub struct Market {
    pub id: i64,
    pub expiry_time: i64,
    pub status: String,
    pub pyth_feed_id: Option<String>,
    pub strike_price: Option<i64>,
    pub batch_interval: i64,
}

#[derive(Debug, Deserialize)]
struct MarketsResponse {
    markets: Vec<Market>,
}

/// BTC/USD Pyth feed ID (mainnet).
const BTC_USD_FEED: &str = "0xe62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";

/// Fetches active BTC/USD markets from the Strike indexer.
pub async fn fetch_active_markets(
    client: &reqwest::Client,
    indexer_url: &str,
    min_expiry_secs: u64,
) -> Result<Vec<Market>> {
    let url = format!("{indexer_url}/markets");
    let resp: MarketsResponse = client
        .get(&url)
        .send()
        .await
        .wrap_err("fetching markets")?
        .json()
        .await
        .wrap_err("parsing markets response")?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let active: Vec<Market> = resp
        .markets
        .into_iter()
        .filter(|m| {
            m.status == "active"
                && m.pyth_feed_id
                    .as_ref()
                    .map(|f| f == BTC_USD_FEED)
                    .unwrap_or(false)
                && (m.expiry_time - now) > min_expiry_secs as i64
        })
        .collect();

    Ok(active)
}

/// Manages which markets the bot is actively quoting.
pub struct MarketManager {
    /// Set of market IDs we're currently quoting.
    active_ids: HashSet<u64>,
}

impl MarketManager {
    pub fn new() -> Self {
        Self {
            active_ids: HashSet::new(),
        }
    }

    /// Reconcile with latest active markets from indexer.
    /// Returns (new_markets, expired_markets).
    pub fn reconcile(&mut self, active_markets: &[Market]) -> (Vec<Market>, Vec<u64>) {
        let new_ids: HashSet<u64> = active_markets.iter().map(|m| m.id as u64).collect();

        // Markets we should start quoting
        let new_markets: Vec<Market> = active_markets
            .iter()
            .filter(|m| !self.active_ids.contains(&(m.id as u64)))
            .cloned()
            .collect();

        // Markets that have expired / gone inactive
        let expired: Vec<u64> = self
            .active_ids
            .iter()
            .filter(|id| !new_ids.contains(id))
            .copied()
            .collect();

        // Update our tracking
        for m in &new_markets {
            self.active_ids.insert(m.id as u64);
            info!(market_id = m.id, expiry = m.expiry_time, "joining market");
        }
        for id in &expired {
            self.active_ids.remove(id);
            info!(market_id = id, "leaving expired market");
        }

        (new_markets, expired)
    }

    /// Check if we're tracking a market.
    pub fn is_active(&self, market_id: u64) -> bool {
        self.active_ids.contains(&market_id)
    }

    /// Get all active market IDs.
    pub fn active_market_ids(&self) -> Vec<u64> {
        self.active_ids.iter().copied().collect()
    }

    /// Remove a specific market (e.g., on error).
    pub fn remove(&mut self, market_id: u64) {
        self.active_ids.remove(&market_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_market(id: i64, status: &str) -> Market {
        Market {
            id,
            expiry_time: 9999999999,
            status: status.to_string(),
            pyth_feed_id: Some(BTC_USD_FEED.to_string()),
            strike_price: Some(8_000_000_000_000),
            batch_interval: 10,
        }
    }

    #[test]
    fn test_reconcile_new_markets() {
        let mut mm = MarketManager::new();
        let markets = vec![make_market(1, "active"), make_market(2, "active")];
        let (new, expired) = mm.reconcile(&markets);
        assert_eq!(new.len(), 2);
        assert!(expired.is_empty());
        assert!(mm.is_active(1));
        assert!(mm.is_active(2));
    }

    #[test]
    fn test_reconcile_expired() {
        let mut mm = MarketManager::new();
        let markets = vec![make_market(1, "active"), make_market(2, "active")];
        mm.reconcile(&markets);

        // Market 1 disappears
        let markets = vec![make_market(2, "active")];
        let (new, expired) = mm.reconcile(&markets);
        assert!(new.is_empty());
        assert_eq!(expired, vec![1]);
        assert!(!mm.is_active(1));
        assert!(mm.is_active(2));
    }

    #[test]
    fn test_reconcile_rolling() {
        let mut mm = MarketManager::new();
        let markets = vec![make_market(1, "active")];
        mm.reconcile(&markets);

        // Market 1 expires, market 3 appears
        let markets = vec![make_market(3, "active")];
        let (new, expired) = mm.reconcile(&markets);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].id, 3);
        assert_eq!(expired, vec![1]);
    }
}
