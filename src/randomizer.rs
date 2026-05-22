use crate::config::AppConfig;
use rand::Rng;
use rand_distr::{Distribution, Exp, LogNormal};

#[derive(Debug, Clone, PartialEq)]
pub enum Side {
    Buy,
    Sell,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Buy => write!(f, "BUY"),
            Side::Sell => write!(f, "SELL"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TradeParams {
    pub side: Side,
    pub amount_usd: f64,
    pub slippage_bps: u16,
    pub delay_secs: u64,
}

pub fn next_trade(cfg: &AppConfig, rng: &mut impl Rng) -> TradeParams {
    TradeParams {
        side: pick_side(cfg, rng),
        amount_usd: pick_amount(cfg, rng),
        slippage_bps: pick_slippage(cfg, rng),
        delay_secs: pick_delay(cfg, rng),
    }
}

fn pick_side(cfg: &AppConfig, rng: &mut impl Rng) -> Side {
    let total = cfg.bias.buy_weight + cfg.bias.sell_weight;
    let roll = rng.gen_range(0..total);
    if roll < cfg.bias.buy_weight {
        Side::Buy
    } else {
        Side::Sell
    }
}

fn pick_amount(cfg: &AppConfig, rng: &mut impl Rng) -> f64 {
    let dist = LogNormal::new(cfg.sizing.lognormal_mu, cfg.sizing.lognormal_sigma)
        .expect("invalid log-normal params");

    // Sample until we land in the allowed range (rejection sampling)
    loop {
        let sample = dist.sample(rng);
        if sample >= cfg.sizing.min_usd && sample <= cfg.sizing.max_usd {
            // Round to 2 decimal places but avoid exact round numbers
            return (sample * 100.0).round() / 100.0;
        }
    }
}

fn pick_slippage(cfg: &AppConfig, rng: &mut impl Rng) -> u16 {
    rng.gen_range(cfg.safety.min_slippage_bps..=cfg.safety.max_slippage_bps)
}

fn pick_delay(cfg: &AppConfig, rng: &mut impl Rng) -> u64 {
    let dist = Exp::new(cfg.timing.exp_lambda).expect("invalid exp lambda");
    loop {
        let sample = dist.sample(rng) as u64;
        if sample >= cfg.timing.min_delay_secs && sample <= cfg.timing.max_delay_secs {
            return sample;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn test_config() -> AppConfig {
        AppConfig {
            rpc: crate::config::RpcConfig { base_rpc_url: "https://mainnet.base.org".into() },
            pair: crate::config::PairConfig {
                token_in: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".into(),
                token_out: "0x4200000000000000000000000000000000000006".into(),
                fee_tier: 500,
            },
            sizing: crate::config::SizingConfig {
                min_usd: 50.0,
                max_usd: 800.0,
                lognormal_mu: 4.5,
                lognormal_sigma: 0.8,
            },
            timing: crate::config::TimingConfig {
                min_delay_secs: 45,
                max_delay_secs: 420,
                exp_lambda: 0.006,
            },
            bias: crate::config::BiasConfig { buy_weight: 55, sell_weight: 45 },
            wallets: crate::config::WalletsConfig {
                keys_env_prefix: "WALLET_KEY_".into(),
                use_smart_wallet: false,
                smart_wallet_address: None,
            },
            safety: crate::config::SafetyConfig {
                max_daily_volume_usd: 50000.0,
                max_gas_gwei: 10.0,
                min_slippage_bps: 50,
                max_slippage_bps: 150,
                tx_deadline_secs: 120,
            },
            logging: crate::config::LoggingConfig {
                log_file: "trades.csv".into(),
                alert_on_consecutive_failures: 3,
                telegram_bot_token: None,
                telegram_chat_id: None,
            },
        }
    }

    #[test]
    fn amounts_in_range() {
        let cfg = test_config();
        let mut rng = StdRng::seed_from_u64(42);
        for _ in 0..1000 {
            let p = next_trade(&cfg, &mut rng);
            assert!(p.amount_usd >= 50.0 && p.amount_usd <= 800.0);
        }
    }

    #[test]
    fn side_distribution_roughly_55_45() {
        let cfg = test_config();
        let mut rng = StdRng::seed_from_u64(99);
        let n = 10_000;
        let buys = (0..n)
            .filter(|_| next_trade(&cfg, &mut rng).side == Side::Buy)
            .count();
        let ratio = buys as f64 / n as f64;
        // Allow ±5% tolerance around 55%
        assert!(ratio > 0.50 && ratio < 0.60, "buy ratio was {:.2}", ratio);
    }

    #[test]
    fn delays_in_range() {
        let cfg = test_config();
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..500 {
            let p = next_trade(&cfg, &mut rng);
            assert!(p.delay_secs >= 45 && p.delay_secs <= 420);
        }
    }

    #[test]
    fn amounts_not_all_round_numbers() {
        let cfg = test_config();
        let mut rng = StdRng::seed_from_u64(13);
        let round_count = (0..200)
            .filter(|_| {
                let p = next_trade(&cfg, &mut rng);
                p.amount_usd.fract() == 0.0
            })
            .count();
        // Less than 5% should be exact whole numbers
        assert!(round_count < 10, "too many round amounts: {}", round_count);
    }
}
