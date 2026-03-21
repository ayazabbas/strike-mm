use alloy::primitives::Address;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use std::path::Path;

use strike_sdk::config::{ContractAddresses, StrikeConfig};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub rpc: RpcConfig,
    pub wallet: WalletConfig,
    pub contracts: ContractsConfig,
    pub indexer: IndexerConfig,
    pub quoting: QuotingConfig,
    pub risk: RiskConfig,
    pub volatility: VolatilityConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcConfig {
    pub url: String,
    pub wss_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletConfig {
    pub private_key_env: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContractsConfig {
    pub order_book: String,
    pub vault: String,
    pub usdt: String,
    pub redemption: String,
    pub batch_auction: Option<String>,
    pub market_factory: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IndexerConfig {
    pub url: String,
    pub poll_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuotingConfig {
    pub spread_ticks: u64,
    pub lots_per_level: u64,
    pub num_levels: u64,
    pub requote_cents: u64,
    pub requote_cooldown_secs: u64,
    pub min_expiry_secs: u64,
    /// Go one-sided when fair prob exceeds this (or is below 1 - this)
    #[serde(default = "default_one_sided_threshold")]
    pub one_sided_threshold: f64,
    /// Spread multiplier when <120s to expiry
    #[serde(default = "default_spread_mult_120")]
    pub expiry_spread_multiplier_120s: f64,
    /// Spread multiplier when <60s to expiry
    #[serde(default = "default_spread_mult_60")]
    pub expiry_spread_multiplier_60s: f64,
}

fn default_one_sided_threshold() -> f64 {
    0.90
}
fn default_spread_mult_120() -> f64 {
    1.5
}
fn default_spread_mult_60() -> f64 {
    2.0
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    /// Max USDT at risk per market (dollar-based budget)
    pub max_loss_budget_usdt: f64,
    /// Max tick shift from inventory skew
    #[serde(default = "default_max_skew_ticks")]
    pub max_skew_ticks: i64,
    pub stale_data_timeout_secs: u64,
}

fn default_max_skew_ticks() -> i64 {
    6
}

#[derive(Debug, Clone, Deserialize)]
pub struct VolatilityConfig {
    pub method: String,
    pub fixed_annual_vol: f64,
    pub realized_window_mins: u64,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).wrap_err_with(|| format!("reading config: {path:?}"))?;
        let config: Config =
            toml::from_str(&content).wrap_err_with(|| format!("parsing config: {path:?}"))?;
        Ok(config)
    }

    pub fn private_key(&self) -> Result<String> {
        std::env::var(&self.wallet.private_key_env).wrap_err_with(|| {
            format!(
                "env var {} not set — set it to the MM wallet private key",
                self.wallet.private_key_env
            )
        })
    }

    /// Build a StrikeConfig from the MM's contract addresses and URLs.
    pub fn strike_config(&self) -> Result<StrikeConfig> {
        let parse_addr = |s: &str, name: &str| -> Result<Address> {
            s.parse().wrap_err_with(|| format!("bad {name} address"))
        };

        let addresses = ContractAddresses {
            order_book: parse_addr(&self.contracts.order_book, "order_book")?,
            vault: parse_addr(&self.contracts.vault, "vault")?,
            usdt: parse_addr(&self.contracts.usdt, "usdt")?,
            redemption: parse_addr(&self.contracts.redemption, "redemption")?,
            outcome_token: Address::ZERO,
            batch_auction: self
                .contracts
                .batch_auction
                .as_ref()
                .map(|s| parse_addr(s, "batch_auction"))
                .transpose()?
                .unwrap_or(Address::ZERO),
            market_factory: self
                .contracts
                .market_factory
                .as_ref()
                .map(|s| parse_addr(s, "market_factory"))
                .transpose()?
                .unwrap_or(Address::ZERO),
            // Not used by MM — use zero addresses
            fee_model: Address::ZERO,
            pyth_resolver: Address::ZERO,
        };

        let mut sdk_config = StrikeConfig::custom(addresses, 97);
        sdk_config.rpc_url = self.rpc.url.clone();
        sdk_config.wss_url = self.rpc.wss_url.clone().unwrap_or_default();
        sdk_config.indexer_url = self.indexer.url.clone();

        Ok(sdk_config)
    }
}
