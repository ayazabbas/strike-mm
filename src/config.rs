use eyre::{Result, WrapErr};
use serde::Deserialize;
use std::path::Path;

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
    pub chain_id: u64,
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
    pub outcome_token: String,
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
    /// Stop quoting entirely below this many seconds to expiry
    #[serde(default = "default_min_quote_secs")]
    pub min_quote_secs: u64,
}

fn default_one_sided_threshold() -> f64 { 0.90 }
fn default_spread_mult_120() -> f64 { 1.5 }
fn default_spread_mult_60() -> f64 { 2.0 }
fn default_min_quote_secs() -> u64 { 30 }

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    pub max_position_per_market: i64,
    pub max_total_exposure: i64,
    pub stale_data_timeout_secs: u64,
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
}
