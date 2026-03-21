use eyre::Result;
use std::collections::HashSet;
use tracing::{info, warn};

use strike_sdk::prelude::*;

/// Background task that redeems resolved market positions every 10 minutes.
pub async fn run_redeem_loop(client: StrikeClient) {
    let mut redeemed: HashSet<u64> = HashSet::new();
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(600));

    // Skip the first immediate tick — let the bot settle
    interval.tick().await;

    info!("redeemer: background task started (10-min interval)");

    loop {
        interval.tick().await;

        if let Err(e) = redeem_cycle(&client, &mut redeemed).await {
            warn!(err = %e, "redeemer: cycle failed");
        }
    }
}

async fn redeem_cycle(client: &StrikeClient, redeemed: &mut HashSet<u64>) -> Result<()> {
    let markets = client
        .indexer()
        .get_markets()
        .await
        .map_err(|e| eyre::eyre!("indexer error: {e}"))?;

    let resolved: Vec<_> = markets
        .into_iter()
        .filter(|m| m.status == "resolved")
        .collect();

    if resolved.is_empty() {
        return Ok(());
    }

    info!(count = resolved.len(), "redeemer: found resolved markets");

    let owner = client
        .signer_address()
        .ok_or_else(|| eyre::eyre!("no signer address"))?;

    for market in &resolved {
        let market_id = market.id as u64;

        if redeemed.contains(&market_id) {
            continue;
        }

        match try_redeem_market(client, owner, market_id).await {
            Ok(did_redeem) => {
                if did_redeem {
                    info!(market_id, "redeemer: successfully redeemed");
                }
                redeemed.insert(market_id);
            }
            Err(e) => {
                warn!(market_id, err = %e, "redeemer: failed to redeem, will retry next cycle");
            }
        }
    }

    Ok(())
}

async fn try_redeem_market(
    client: &StrikeClient,
    owner: alloy::primitives::Address,
    market_id: u64,
) -> Result<bool> {
    let (yes_lots, no_lots) = client
        .redeem()
        .internal_positions(market_id, owner)
        .await
        .map_err(|e| eyre::eyre!("position check failed: {e}"))?;

    if yes_lots == 0 && no_lots == 0 {
        info!(market_id, "redeemer: no positions to redeem");
        return Ok(false);
    }

    let amount = alloy::primitives::U256::from(yes_lots.max(no_lots));
    info!(
        market_id,
        yes_lots,
        no_lots,
        "redeemer: found positions, attempting redemption"
    );

    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.redeem().redeem(market_id, amount),
    )
    .await
    {
        Ok(Ok(())) => {
            info!(market_id, "redeemer: redemption confirmed");
            Ok(true)
        }
        Ok(Err(e)) => {
            warn!(market_id, err = %e, "redeemer: redemption failed");
            Err(eyre::eyre!("redemption failed: {e}"))
        }
        Err(_) => {
            warn!(market_id, "redeemer: redemption timed out");
            Err(eyre::eyre!("redemption timed out"))
        }
    }
}
