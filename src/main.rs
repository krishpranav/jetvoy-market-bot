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

    tracing::info!(dry_run = args.dry_run, "Starting market bot");

    let pool = wallet::WalletPool::load_from_env(&cfg.wallets.keys_env_prefix)?;

    let provider = ProviderBuilder::new()
        .connect_http(cfg.rpc.base_rpc_url.parse()?);

    let mut monitor = monitor::Monitor::new(
        cfg.logging.log_file.clone(),
        cfg.logging.alert_on_consecutive_failures,
        cfg.logging.telegram_bot_token.clone(),
        cfg.logging.telegram_chat_id.clone(),
    )?;

    let mut rng = StdRng::from_entropy();
    let mut current_day = chrono::Utc::now().date_naive();

    tracing::info!("Bot running — press Ctrl+C to stop");

    loop {
        // Reset daily volume at midnight UTC
        let today = chrono::Utc::now().date_naive();
        if today != current_day {
            tracing::info!(previous_volume = monitor.total_volume_usd, "New day — resetting volume");
            monitor.total_volume_usd = 0.0;
            current_day = today;
        }

        if monitor.total_volume_usd >= cfg.safety.max_daily_volume_usd {
            tracing::warn!("Daily cap reached. Pausing 1hr.");
            sleep(Duration::from_secs(3600)).await;
            continue;
        }

        // Pick a wallet that actually has ETH — try all before giving up
        let (signer, wallet_addr) = {
            let mut found = None;
            for _ in 0..pool.len() {
                let s = pool.pick(&mut rng);
                let addr: Address = s.address();
                if dex::check_gas_funds(addr, &provider).await.is_ok() {
                    found = Some((s, addr));
                    break;
                }
            }
            match found {
                Some(pair) => pair,
                None => {
                    tracing::warn!("No wallet has enough ETH for gas — waiting 60s");
                    sleep(Duration::from_secs(60)).await;
                    continue;
                }
            }
        };

        // ── Gate 2: Determine trade side & compute amount from actual balance ─
        // Generate side first, then compute safe amount from real balance
        let mut params = randomizer::next_trade(&cfg, &mut rng);

        match dex::safe_trade_amount_usd(&params.side, wallet_addr, &cfg, &provider).await {
            Ok(safe_amount) => {
                params.amount_usd = safe_amount;
            }
            Err(e) => {
                tracing::warn!("{}", e);
                // Wait before retrying — no funds yet
                sleep(Duration::from_secs(params.delay_secs)).await;
                continue;
            }
        }

        tracing::info!(
            delay_secs = params.delay_secs,
            side = %params.side,
            amount_usd = params.amount_usd,
            wallet = %wallet_addr,
            "Next trade scheduled"
        );

        sleep(Duration::from_secs(params.delay_secs)).await;

        if args.dry_run {
            tracing::info!(wallet = %wallet_addr, side = %params.side, amount_usd = params.amount_usd, "[DRY RUN]");
            continue;
        }

        // For SELL trades, ensure Jetvoy token is approved for the router
        if matches!(params.side, randomizer::Side::Sell) {
            let jetvoy = Address::from_str(&cfg.pair.token_out).unwrap();
            let router = Address::from_str(dex::SWAP_ROUTER_BASE).unwrap();
            if let Ok(Some(approve_data)) = dex::ensure_approval(
                jetvoy, wallet_addr, router, &provider
            ).await {
                tracing::info!(wallet = %wallet_addr, "Approving router to spend Jetvoy");
                let approve_calldata = dex::SwapCalldata {
                    to: jetvoy,
                    data: approve_data,
                    value: alloy::primitives::U256::ZERO,
                };
                if let Err(e) = executor::execute_with_retry(
                    approve_calldata, signer, &cfg.rpc.base_rpc_url,
                    cfg.safety.max_gas_gwei, &mut rng,
                ).await {
                    tracing::error!("Approval failed: {}", e);
                    monitor.log_failure(&params, wallet_addr, &e.to_string());
                    continue;
                }
            }
        }

        // Build calldata
        let calldata = match dex::build_swap(&params.side, params.amount_usd, wallet_addr, &cfg, &provider).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to build calldata: {}", e);
                monitor.log_failure(&params, wallet_addr, &e.to_string());
                continue;
            }
        };

        // ── Gate 3: Simulate for FREE before broadcasting ─────────────────────
        if let Err(e) = dex::simulate_swap(&calldata, wallet_addr, &provider).await {
            tracing::warn!("Simulation says tx would fail — skipping (no gas burned). Reason: {}", e);
            continue;
        }

        // All gates passed — execute for real
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
