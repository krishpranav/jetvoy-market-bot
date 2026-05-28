use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub rpc: RpcConfig,
    pub pair: PairConfig,
    pub sizing: SizingConfig,
    pub timing: TimingConfig,
    pub bias: BiasConfig,
    pub wallets: WalletsConfig,
    pub safety: SafetyConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RpcConfig {
    pub base_rpc_url: String,
    pub bundler_url: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PairConfig {
    pub token_in: String,
    pub token_out: String,
    pub fee_tier: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SizingConfig {
    pub min_usd: f64,
    pub max_usd: f64,
    pub lognormal_mu: f64,
    pub lognormal_sigma: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TimingConfig {
    pub min_delay_secs: u64,
    pub max_delay_secs: u64,
    pub exp_lambda: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BiasConfig {
    pub buy_weight: u32,
    pub sell_weight: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WalletsConfig {
    pub keys_env_prefix: String,
    // Smart wallet mode — routes trades through Coinbase Smart Wallet via ERC-4337
    pub use_smart_wallet: bool,
    pub smart_wallet_address: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SafetyConfig {
    pub max_daily_volume_usd: f64,
    pub max_gas_gwei: f64,
    pub min_slippage_bps: u16,
    pub max_slippage_bps: u16,
    pub tx_deadline_secs: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    pub log_file: String,
    pub alert_on_consecutive_failures: u32,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
}

pub fn load() -> Result<AppConfig> {
    let cfg = config::Config::builder()
        .add_source(config::File::with_name("config/default"))
        .add_source(config::Environment::default().separator("__"))
        .build()?;

    Ok(cfg.try_deserialize()?)
}
