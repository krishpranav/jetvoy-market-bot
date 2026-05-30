use anyhow::{Context, Result};
use alloy::{
    consensus::{SignableTransaction, Signed, TxEip1559, TxEnvelope},
    eips::eip2718::Encodable2718,
    network::{TransactionBuilder, TxSignerSync},
    primitives::{Address, Bytes, TxHash},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
};
use rand::Rng;
use std::time::Duration;
use tokio::time::sleep;

use crate::dex::SwapCalldata;

pub struct ExecutionResult {
    pub tx_hash: TxHash,
    pub gas_used: u64,
}

pub async fn execute(
    calldata: SwapCalldata,
    signer: &PrivateKeySigner,
    rpc_url: &str,
    max_gas_gwei: f64,
    rng: &mut impl Rng,
) -> Result<ExecutionResult> {
    let url: reqwest::Url = rpc_url.parse().context("Invalid RPC URL")?;
    let provider = ProviderBuilder::new().connect_http(url);

    let gas_price = provider.get_gas_price().await.context("Failed to fetch gas price")?;
    let gas_gwei = gas_price as f64 / 1e9;

    if gas_gwei > max_gas_gwei {
        anyhow::bail!(
            "Gas price {:.2} gwei exceeds ceiling {:.2} gwei — skipping trade",
            gas_gwei,
            max_gas_gwei
        );
    }

    let gas_multiplier = rng.gen_range(1.05f64..1.25f64);
    let adjusted_gas = (gas_price as f64 * gas_multiplier) as u128;

    let from: Address = signer.address();
    let chain_id = provider.get_chain_id().await.context("Failed to fetch chain id")?;
    let nonce = provider
        .get_transaction_count(from)
        .await
        .context("Failed to fetch nonce")?;

    // Estimate gas limit. estimate_gas IS a full on-chain simulation of THIS
    // exact tx — if it errors, the tx is guaranteed to revert, so we abort and
    // spend ZERO gas instead of broadcasting a doomed transaction.
    let estimate_req = TransactionRequest::default()
        .with_to(calldata.to)
        .with_from(from)
        .with_input(calldata.data.clone())
        .with_value(calldata.value);

    let estimated = match provider.estimate_gas(estimate_req).await {
        Ok(g) => g,
        Err(e) => {
            anyhow::bail!(
                "Gas estimation failed — tx would revert. Skipping, no gas spent. ({})",
                e
            );
        }
    };
    let gas_limit = (estimated as f64 * 1.2) as u64; // 20% safety buffer

    // Pre-broadcast balance guard: must cover (gas_limit * gas_price) + value.
    let eth_balance = provider
        .get_balance(from)
        .await
        .context("Failed to fetch ETH balance")?;
    let max_gas_cost = alloy::primitives::U256::from(gas_limit as u128 * adjusted_gas);
    let required = max_gas_cost + calldata.value;
    if eth_balance < required {
        anyhow::bail!(
            "Insufficient ETH for gas + value: have {:.6}, need {:.6} — skipping, no gas spent",
            eth_balance.to::<u128>() as f64 / 1e18,
            required.to::<u128>() as f64 / 1e18
        );
    }

    let mut tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit,
        max_fee_per_gas: adjusted_gas,
        max_priority_fee_per_gas: adjusted_gas / 10,
        to: alloy::primitives::TxKind::Call(calldata.to),
        value: calldata.value,
        input: Bytes::from(calldata.data),
        access_list: Default::default(),
    };

    let sig = signer
        .sign_transaction_sync(&mut tx)
        .context("Failed to sign transaction")?;

    let signed: Signed<TxEip1559> = tx.into_signed(sig);
    let envelope = TxEnvelope::Eip1559(signed);

    let mut raw_tx: Vec<u8> = Vec::new();
    envelope.encode_2718(&mut raw_tx);

    tracing::debug!(wallet = %from, gas_gwei, "Broadcasting transaction");

    let pending = provider
        .send_raw_transaction(&raw_tx)
        .await
        .context("Failed to broadcast transaction")?;

    let hash = *pending.tx_hash();
    tracing::info!(tx_hash = %hash, "Transaction submitted");

    // Poll for receipt (60s timeout)
    let receipt = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Ok(Some(r)) = provider.get_transaction_receipt(hash).await {
                return Ok::<_, anyhow::Error>(r);
            }
            sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .context("Transaction receipt timeout (60s)")??;

    if !receipt.status() {
        anyhow::bail!("Transaction {} reverted", hash);
    }

    let gas_used = receipt.gas_used;
    tracing::info!(tx_hash = %hash, gas_used, "Transaction confirmed");

    Ok(ExecutionResult { tx_hash: hash, gas_used })
}

pub async fn execute_with_retry(
    calldata: SwapCalldata,
    signer: &PrivateKeySigner,
    rpc_url: &str,
    max_gas_gwei: f64,
    rng: &mut impl Rng,
) -> Result<ExecutionResult> {
    match execute(calldata, signer, rpc_url, max_gas_gwei, rng).await {
        Ok(r) => Ok(r),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("exceeds ceiling") {
                let wait = rng.gen_range(30u64..90);
                tracing::warn!("Gas too high, backing off {}s", wait);
                sleep(Duration::from_secs(wait)).await;
                Err(e)
            } else if msg.contains("reverted") {
                tracing::error!("Swap reverted: {}", msg);
                Err(e)
            } else {
                tracing::warn!("Execution error: {}", msg);
                Err(e)
            }
        }
    }
}
