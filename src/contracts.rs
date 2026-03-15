use alloy::sol;

sol!(
    #[sol(rpc)]
    BatchAuction,
    "abi/BatchAuction.json"
);

sol!(
    #[sol(rpc)]
    MarketFactory,
    "abi/MarketFactory.json"
);
