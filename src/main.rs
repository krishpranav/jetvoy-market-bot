mod config;
mod dex;
mod executor;
mod monitor;
mod randomizer;
mod smart_wallet;
mod wallet;

use anyhow::Result;
use clap::Parser;
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::str::FromStr;
use tokio::time::{sleep, Duration};
use alloy::{primitives::Address, providers::ProviderBuilder};
use chrono;

#[derive(Parser, Debug)]
#[command(name = "market-bot", about = "Organic market-making bot for Base chain")]
struct Args {
    /// Run without broadcasting transactions
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("market_bot=info".parse().unwrap()),
        )
        .init();

    dotenv::dotenv().ok();

    let args = Args::parse();
    let cfg = config::load().expect("Failed to load config");

    tracing::info!(
        dry_run = args.dry_run,
        smart_wallet = cfg.wallets.use_smart_wallet,
        "Starting market bot"
    );

    let pool = wallet::WalletPool::load_from_env(&cfg.wallets.keys_env_prefix)?;

    // Read-only provider for price checks
    let provider = ProviderBuilder::new()
        .connect_http(cfg.rpc.base_rpc_url.parse()?);

    let mut monitor = monitor::Monitor::new(
        cfg.logging.log_file.clone(),
        cfg.logging.alert_on_consecutive_failures,
        cfg.logging.telegram_bot_token.clone(),
        cfg.logging.telegram_chat_id.clone(),
    )?;

    let mut rng = StdRng::from_entropy();

    // Validate smart wallet config upfront
    if cfg.wallets.use_smart_wallet {
        let addr = cfg.wallets.smart_wallet_address.as_deref()
            .expect("smart_wallet_address must be set in config when use_smart_wallet = true");
        let bundler = cfg.rpc.bundler_url.as_deref()
            .expect("bundler_url must be set in [rpc] when use_smart_wallet = true");
        tracing::info!(smart_wallet = addr, bundler = bundler, "Smart wallet mode active");
    }

    tracing::info!("Bot running — press Ctrl+C to stop");

    // Track current UTC date to reset daily volume at midnight
    let mut current_day = chrono::Utc::now().date_naive();

    loop {
        // Reset daily volume counter at midnight UTC
        let today = chrono::Utc::now().date_naive();
        if today != current_day {
            tracing::info!(
                previous_volume = monitor.total_volume_usd,
                "New day — resetting daily volume counter"
            );
            monitor.total_volume_usd = 0.0;
            current_day = today;
        }

        if monitor.total_volume_usd >= cfg.safety.max_daily_volume_usd {
            tracing::warn!(
                "Daily volume cap ${:.0} reached. Pausing until midnight UTC.",
                cfg.safety.max_daily_volume_usd
            );
            // Sleep 1 hour and recheck — systemd will NOT restart, bot stays alive
            sleep(Duration::from_secs(3600)).await;
            continue;
        }

        let params = randomizer::next_trade(&cfg, &mut rng);

        tracing::info!(
            delay_secs = params.delay_secs,
            side = %params.side,
            amount_usd = params.amount_usd,
            slippage_bps = params.slippage_bps,
            "Next trade scheduled"
        );

        sleep(Duration::from_secs(params.delay_secs)).await;

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

        // Check wallet has enough balance before attempting trade
        if let Err(e) = dex::check_balance(&params, wallet_addr, &cfg, &provider).await {
            tracing::warn!("{}", e);
            continue;
        }

        // Build swap calldata (same for both modes)
        let calldata = match dex::build_swap(&params, wallet_addr, &cfg, &provider).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to build swap calldata: {}", e);
                monitor.log_failure(&params, wallet_addr, &e.to_string());
                continue;
            }
        };

        if cfg.wallets.use_smart_wallet {
            // ── Smart wallet mode: ERC-4337 UserOperation via Coinbase Smart Wallet ──
            let smart_wallet_addr = Address::from_str(
                cfg.wallets.smart_wallet_address.as_deref().unwrap()
            ).expect("Invalid smart_wallet_address");
            let bundler_url = cfg.rpc.bundler_url.as_deref().unwrap();

            // Ensure token approval (calldata targets the smart wallet as spender)
            let token_in = Address::from_str(&cfg.pair.token_in).unwrap();
            let router = Address::from_str(dex::SWAP_ROUTER_BASE).unwrap();
            if let Ok(Some(approve_data)) = dex::ensure_approval(
                token_in, smart_wallet_addr, router, alloy::primitives::U256::MAX, &provider
            ).await {
                tracing::info!(wallet = %smart_wallet_addr, "Approving SwapRouter via smart wallet");
                let approve_calldata = dex::SwapCalldata {
                    to: token_in,
                    data: approve_data,
                    value: alloy::primitives::U256::ZERO,
                };
                if let Err(e) = smart_wallet::execute_via_smart_wallet(
                    approve_calldata,
                    smart_wallet_addr,
                    signer,
                    bundler_url,
                    &cfg.rpc.base_rpc_url,
                    cfg.safety.max_gas_gwei,
                ).await {
                    tracing::error!("Smart wallet approval failed: {}", e);
                    monitor.log_failure(&params, smart_wallet_addr, &e.to_string());
                    continue;
                }
            }

            match smart_wallet::execute_via_smart_wallet(
                calldata,
                smart_wallet_addr,
                signer,
                bundler_url,
                &cfg.rpc.base_rpc_url,
                cfg.safety.max_gas_gwei,
            ).await {
                Ok(op_hash) => {
                    tracing::info!(op_hash, "Smart wallet trade confirmed");
                    monitor.log_success_smart(&params, smart_wallet_addr, &op_hash);
                }
                Err(e) => {
                    monitor.log_failure(&params, smart_wallet_addr, &e.to_string());
                }
            }
        } else {
            // ── Standard EOA mode ──────────────────────────────────────────────────
            let token_in = Address::from_str(&cfg.pair.token_in).unwrap();
            let router = Address::from_str(dex::SWAP_ROUTER_BASE).unwrap();
            if let Ok(Some(approve_data)) = dex::ensure_approval(
                token_in, wallet_addr, router, alloy::primitives::U256::MAX, &provider
            ).await {
                let approve_calldata = dex::SwapCalldata {
                    to: token_in,
                    data: approve_data,
                    value: alloy::primitives::U256::ZERO,
                };
                if let Err(e) = executor::execute_with_retry(
                    approve_calldata, signer, &cfg.rpc.base_rpc_url,
                    cfg.safety.max_gas_gwei, &mut rng,
                ).await {
                    monitor.log_failure(&params, wallet_addr, &e.to_string());
                    continue;
                }
            }

            match executor::execute_with_retry(
                calldata, signer, &cfg.rpc.base_rpc_url,
                cfg.safety.max_gas_gwei, &mut rng,
            ).await {
                Ok(result) => {
                    monitor.log_success(&params, wallet_addr, result.tx_hash, result.gas_used);
                }
                Err(e) => {
                    monitor.log_failure(&params, wallet_addr, &e.to_string());
                }
            }
        }
    }

    tracing::info!("Bot shut down. Session volume: ${:.2}", monitor.total_volume_usd);
    Ok(())
}
