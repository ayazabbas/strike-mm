use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::Result;
use std::collections::HashSet;
use tracing::{info, warn};

use crate::market_manager::Market;

sol!(
    #[sol(rpc)]
    RedemptionContract,
    "abi/Redemption.json"
);

sol!(
    #[sol(rpc)]
    OutcomeToken,
    "abi/OutcomeToken.json"
);

/// Multicall3 at canonical address
const MULTICALL3: Address = Address::new([
    0xca, 0x11, 0xbd, 0xe0, 0x59, 0x77, 0xb3, 0x63, 0x11, 0x67,
    0x02, 0x88, 0x62, 0xbE, 0x2a, 0x17, 0x39, 0x76, 0xCA, 0x11,
]);

sol! {
    struct Call3 {
        address target;
        bool allowFailure;
        bytes callData;
    }

    struct MulticallResult {
        bool success;
        bytes returnData;
    }

    function aggregate3(Call3[] calldata calls) external payable returns (MulticallResult[] memory returnData);
}

#[derive(Debug, serde::Deserialize)]
struct MarketsResponse {
    markets: Vec<Market>,
}

/// Fetch resolved markets from the indexer.
async fn fetch_resolved_markets(
    client: &reqwest::Client,
    indexer_url: &str,
) -> Result<Vec<Market>> {
    let url = format!("{indexer_url}/markets");
    let resp: MarketsResponse = client
        .get(&url)
        .send()
        .await?
        .json()
        .await?;

    let resolved: Vec<Market> = resp
        .markets
        .into_iter()
        .filter(|m| m.status == "resolved")
        .collect();

    Ok(resolved)
}

/// Background task that redeems resolved market positions every 10 minutes.
pub async fn run_redeem_loop<P>(
    provider: P,
    redemption_addr: Address,
    outcome_token_addr: Address,
    mm_address: Address,
    indexer_url: String,
) where
    P: Provider + Clone,
{
    let client = reqwest::Client::new();
    let mut redeemed: HashSet<u64> = HashSet::new();
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(600));

    // Skip the first immediate tick — let the bot settle
    interval.tick().await;

    info!("redeemer: background task started (10-min interval)");

    loop {
        interval.tick().await;

        if let Err(e) = redeem_cycle(
            &provider,
            &client,
            redemption_addr,
            outcome_token_addr,
            mm_address,
            &indexer_url,
            &mut redeemed,
        )
        .await
        {
            warn!(err = %e, "redeemer: cycle failed");
        }
    }
}

async fn redeem_cycle<P>(
    provider: &P,
    client: &reqwest::Client,
    redemption_addr: Address,
    outcome_token_addr: Address,
    mm_address: Address,
    indexer_url: &str,
    redeemed: &mut HashSet<u64>,
) -> Result<()>
where
    P: Provider + Clone,
{
    let markets = fetch_resolved_markets(client, indexer_url).await?;

    if markets.is_empty() {
        return Ok(());
    }

    info!(count = markets.len(), "redeemer: found resolved markets");

    for market in &markets {
        let market_id = market.id as u64;

        if redeemed.contains(&market_id) {
            continue;
        }

        match try_redeem_market(
            provider,
            redemption_addr,
            outcome_token_addr,
            mm_address,
            market_id,
        )
        .await
        {
            Ok(did_redeem) => {
                if did_redeem {
                    info!(market_id, "redeemer: successfully redeemed");
                }
                // Mark as redeemed regardless — no point rechecking markets with 0 balance
                redeemed.insert(market_id);
            }
            Err(e) => {
                warn!(market_id, err = %e, "redeemer: failed to redeem, will retry next cycle");
            }
        }
    }

    Ok(())
}

async fn try_redeem_market<P>(
    provider: &P,
    redemption_addr: Address,
    outcome_token_addr: Address,
    mm_address: Address,
    market_id: u64,
) -> Result<bool>
where
    P: Provider + Clone,
{
    let market_id_u256 = U256::from(market_id);
    let outcome_token = OutcomeToken::new(outcome_token_addr, provider.clone());

    // Get token IDs
    let yes_token_id = outcome_token.yesTokenId(market_id_u256).call().await?;
    let no_token_id = outcome_token.noTokenId(market_id_u256).call().await?;

    // Check balances
    let yes_balance = outcome_token
        .balanceOf(mm_address, yes_token_id)
        .call()
        .await?;
    let no_balance = outcome_token
        .balanceOf(mm_address, no_token_id)
        .call()
        .await?;

    if yes_balance.is_zero() && no_balance.is_zero() {
        info!(market_id, "redeemer: no outcome tokens to redeem");
        return Ok(false);
    }

    info!(
        market_id,
        yes_balance = %yes_balance,
        no_balance = %no_balance,
        "redeemer: found outcome tokens, attempting redemption"
    );

    // Build redeem calls — try both sides via multicall with allowFailure.
    // The winning side will succeed, the losing side will revert harmlessly.
    let mut calls: Vec<Call3> = Vec::new();

    if !yes_balance.is_zero() {
        let calldata =
            RedemptionContract::redeemCall {
                factoryMarketId: market_id_u256,
                amount: yes_balance,
            }
            .abi_encode();
        calls.push(Call3 {
            target: redemption_addr,
            allowFailure: true,
            callData: Bytes::from(calldata),
        });
    }

    if !no_balance.is_zero() {
        let calldata =
            RedemptionContract::redeemCall {
                factoryMarketId: market_id_u256,
                amount: no_balance,
            }
            .abi_encode();
        calls.push(Call3 {
            target: redemption_addr,
            allowFailure: true,
            callData: Bytes::from(calldata),
        });
    }

    // Sync nonce from chain
    let nonce = provider.get_transaction_count(mm_address).await?;

    let multicall_data = aggregate3Call { calls }.abi_encode();
    let mut tx = alloy::rpc::types::TransactionRequest::default()
        .to(MULTICALL3)
        .input(Bytes::from(multicall_data).into())
        .nonce(nonce);
    tx.gas = Some(500_000);

    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        async {
            let pending = provider.send_transaction(tx).await?;
            let receipt = pending.get_receipt().await?;
            Ok::<_, eyre::Report>(receipt)
        },
    )
    .await
    {
        Ok(Ok(receipt)) => {
            info!(
                market_id,
                tx = %receipt.transaction_hash,
                "redeemer: redemption tx confirmed"
            );
            Ok(true)
        }
        Ok(Err(e)) => {
            warn!(market_id, err = %e, "redeemer: redemption tx failed");
            Err(e)
        }
        Err(_) => {
            warn!(market_id, "redeemer: redemption tx timed out after 30s");
            eyre::bail!("redemption tx timed out");
        }
    }
}
