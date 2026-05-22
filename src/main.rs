mod config;
mod dex;
mod executor;
mod monitor;
mod randomizer;
mod wallet;

use anyhow::Result;
use clap::Parser;
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::str::FromStr;
use tokio::time::{sleep, Duration};
use alloy::{primitives::Address, providers::ProviderBuilder};

#[derive(Parser, Debug)]
#[command(name = "market-bot", about = "Organic market-making bot for Base chain")]
struct Args {
    /// Run without broadcasting transactions (for testing)
    #[arg(long)]
    dry_run: bool,

    /// Override config file path
    #[arg(long, default_value = "config/default")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Init logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("market_bot=info".parse().unwrap()),
        )
        .init();

    // Load .env
    dotenv::dotenv().ok();

    let args = Args::parse();

    let cfg = config::load().expect("Failed to load config");
    tracing::info!(dry_run = args.dry_run, "Starting market bot");

    // Load wallet pool
    let pool = wallet::WalletPool::load_from_env(&cfg.wallets.keys_env_prefix)?;
    tracing::info!("Wallet pool size: {}", pool.len());

    // Connect provider (read-only, for price checks)
    let provider = ProviderBuilder::new()
        .connect_http(cfg.rpc.base_rpc_url.parse()?);

    // Init monitor
    let mut monitor = monitor::Monitor::new(
        cfg.logging.log_file.clone(),
        cfg.logging.alert_on_consecutive_failures,
        cfg.logging.telegram_bot_token.clone(),
        cfg.logging.telegram_chat_id.clone(),
    )?;

    let mut rng = StdRng::from_entropy();

    tracing::info!("Bot running — press Ctrl+C to stop");

    loop {
        // Check daily volume cap
        if monitor.total_volume_usd >= cfg.safety.max_daily_volume_usd {
            tracing::warn!(
                "Daily volume cap ${:.0} reached. Stopping.",
                cfg.safety.max_daily_volume_usd
            );
            break;
        }

        // Generate next trade params
        let params = randomizer::next_trade(&cfg, &mut rng);

        tracing::info!(
            delay_secs = params.delay_secs,
            side = %params.side,
            amount_usd = params.amount_usd,
            slippage_bps = params.slippage_bps,
            "Next trade scheduled"
        );

        // Wait the organic delay
        sleep(Duration::from_secs(params.delay_secs)).await;

        // Pick a random wallet
        let signer = pool.pick(&mut rng);
        let wallet_addr: Address = signer.address();

        if args.dry_run {
            tracing::info!(
                wallet = %wallet_addr,
                side = %params.side,
                amount_usd = params.amount_usd,
                "[DRY RUN] Would execute trade"
            );
            continue;
        }

        // Ensure token approval if needed
        let token_in = Address::from_str(&cfg.pair.token_in)?;
        let router = Address::from_str(dex::SWAP_ROUTER_BASE)?;

        match dex::ensure_approval(token_in, wallet_addr, router, alloy::primitives::U256::MAX, &provider).await {
            Ok(Some(approve_data)) => {
                tracing::info!(wallet = %wallet_addr, "Approving SwapRouter for token_in");
                let approve_calldata = dex::SwapCalldata {
                    to: token_in,
                    data: approve_data,
                    value: alloy::primitives::U256::ZERO,
                };
                if let Err(e) = executor::execute_with_retry(
                    approve_calldata,
                    signer,
                    &cfg.rpc.base_rpc_url,
                    cfg.safety.max_gas_gwei,
                    &mut rng,
                ).await {
                    tracing::error!("Approval failed: {}", e);
                    monitor.log_failure(&params, wallet_addr, &e.to_string());
                    continue;
                }
            }
            Ok(None) => {} // Already approved
            Err(e) => {
                tracing::warn!("Could not check approval: {}", e);
            }
        }

        // Build swap calldata
        let calldata = match dex::build_swap(&params, wallet_addr, &cfg, &provider).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to build swap calldata: {}", e);
                monitor.log_failure(&params, wallet_addr, &e.to_string());
                continue;
            }
        };

        // Execute
        match executor::execute_with_retry(
            calldata,
            signer,
            &cfg.rpc.base_rpc_url,
            cfg.safety.max_gas_gwei,
            &mut rng,
        ).await {
            Ok(result) => {
                monitor.log_success(&params, wallet_addr, result.tx_hash, result.gas_used);
            }
            Err(e) => {
                monitor.log_failure(&params, wallet_addr, &e.to_string());
            }
        }
    }

    tracing::info!("Bot stopped. Total volume: ${:.2}", monitor.total_volume_usd);
    Ok(())
}
