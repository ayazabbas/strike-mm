use alloy::network::Ethereum;
use alloy::primitives::Address;
use alloy::providers::{DynProvider, PendingTransactionBuilder, Provider};
use alloy::rpc::types::TransactionRequest;
use eyre::{Result, WrapErr};
use tracing::{info, warn};

/// Concrete pending tx type — re-exported for callers.
pub type PendingTx = PendingTransactionBuilder<Ethereum>;

/// Shared nonce manager — all tx sends go through this to avoid nonce collisions.
/// Wraps a type-erased provider (`DynProvider`) so it's a plain concrete type.
pub struct NonceSender {
    provider: DynProvider,
    signer_addr: Address,
    nonce: u64,
}

impl NonceSender {
    /// Create a new NonceSender, fetching the current nonce from chain.
    pub async fn new(provider: DynProvider, signer_addr: Address) -> Result<Self> {
        let nonce = provider
            .get_transaction_count(signer_addr)
            .await
            .wrap_err("failed to get initial nonce")?;
        info!(nonce, "NonceSender initialized");
        Ok(Self {
            provider,
            signer_addr,
            nonce,
        })
    }

    /// Re-fetch nonce from chain (use after errors).
    pub async fn sync(&mut self) -> Result<()> {
        let n = self
            .provider
            .get_transaction_count(self.signer_addr)
            .await
            .wrap_err("failed to sync nonce")?;
        info!(old_nonce = self.nonce, new_nonce = n, "nonce synced from chain");
        self.nonce = n;
        Ok(())
    }

    /// Send a transaction, stamping it with the next nonce.
    /// On nonce-related errors: syncs from chain and retries once.
    /// The returned PendingTransactionBuilder can be awaited for the receipt
    /// AFTER releasing the Mutex lock.
    pub async fn send(
        &mut self,
        tx: TransactionRequest,
    ) -> Result<PendingTx> {
        let attempt = tx.clone().nonce(self.nonce);
        match self.provider.send_transaction(attempt).await {
            Ok(pending) => {
                self.nonce += 1;
                Ok(pending)
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("nonce")
                    || err_str.contains("replacement")
                    || err_str.contains("already known")
                {
                    warn!(nonce = self.nonce, err = %e, "nonce error — syncing and retrying");
                    self.sync().await?;
                    let retry = tx.nonce(self.nonce);
                    let pending = self
                        .provider
                        .send_transaction(retry)
                        .await
                        .wrap_err("retry after nonce sync failed")?;
                    self.nonce += 1;
                    Ok(pending)
                } else {
                    Err(e.into())
                }
            }
        }
    }
}
