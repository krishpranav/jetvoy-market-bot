/// ERC-4337 integration for Coinbase Smart Wallet on Base.
///
/// Flow:
///   1. Build a UserOperation with the swap calldata wrapped in executeBatch()
///   2. Compute the UserOp hash per ERC-4337 v0.6 spec
///   3. Sign the hash with the owner EOA (WALLET_KEY_0)
///   4. Submit to the bundler via eth_sendUserOperation
///   5. Poll bundler for receipt
use anyhow::{Context, Result};
use alloy::{
    primitives::{keccak256, Address, Bytes, B256, U256},
    signers::{local::PrivateKeySigner, SignerSync},
    sol,
    sol_types::SolValue,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::str::FromStr;

use crate::dex::SwapCalldata;

// CoinbaseSmartWallet uses EntryPoint v0.6 on Base
pub const ENTRY_POINT_V06: &str = "0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789";

sol! {
    #[allow(missing_docs)]
    interface ICoinbaseSmartWallet {
        struct Call {
            address target;
            uint256 value;
            bytes data;
        }
        function executeBatch(Call[] calldata calls) external payable;
        function addOwnerAddress(address owner) external;
        function isOwnerAddress(address owner) external view returns (bool);
        function getNonce(uint192 key) external view returns (uint256);
    }
}

/// ERC-4337 UserOperation (v0.6)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserOperation {
    pub sender: String,
    pub nonce: String,
    pub init_code: String,
    pub call_data: String,
    pub call_gas_limit: String,
    pub verification_gas_limit: String,
    pub pre_verification_gas: String,
    pub max_fee_per_gas: String,
    pub max_priority_fee_per_gas: String,
    pub paymaster_and_data: String,
    pub signature: String,
}

/// JSON-RPC response wrapper
#[derive(Deserialize)]
struct RpcResponse {
    result: Option<Value>,
    error: Option<Value>,
}

/// Build, sign, and submit a UserOperation through the Coinbase Smart Wallet.
pub async fn execute_via_smart_wallet(
    swap: SwapCalldata,
    smart_wallet: Address,
    signer: &PrivateKeySigner,
    bundler_url: &str,
    rpc_url: &str,
    max_gas_gwei: f64,
) -> Result<String> {
    let client = Client::new();
    let chain_id: u64 = 8453; // Base mainnet

    // 1. Build executeBatch calldata wrapping the swap
    let call = ICoinbaseSmartWallet::Call {
        target: swap.to,
        value: swap.value,
        data: Bytes::from(swap.data),
    };
    let calls = vec![call];
    use alloy::sol_types::SolCall;
    let call_data = ICoinbaseSmartWallet::executeBatchCall { calls }.abi_encode();

    // 2. Get nonce from smart wallet (key=0 for sequential)
    let nonce = get_smart_wallet_nonce(&client, rpc_url, smart_wallet).await?;

    // 3. Estimate gas via bundler
    let gas_price = get_gas_price(&client, rpc_url).await?;
    let gas_gwei = gas_price as f64 / 1e9;
    if gas_gwei > max_gas_gwei {
        anyhow::bail!("Gas {:.2} gwei exceeds ceiling {:.2} gwei", gas_gwei, max_gas_gwei);
    }

    // 4. Build unsigned UserOp (signature = dummy 65-byte for gas estimation)
    let dummy_sig = "0x".to_string() + &"00".repeat(65);
    let mut userop = UserOperation {
        sender: format!("{:?}", smart_wallet),
        nonce: format!("{:#x}", nonce),
        init_code: "0x".into(),
        call_data: format!("0x{}", hex::encode(&call_data)),
        call_gas_limit: "0x0".into(),
        verification_gas_limit: "0x0".into(),
        pre_verification_gas: "0x0".into(),
        max_fee_per_gas: format!("{:#x}", gas_price),
        max_priority_fee_per_gas: format!("{:#x}", gas_price / 10),
        paymaster_and_data: "0x".into(),
        signature: dummy_sig,
    };

    // 5. Estimate gas limits via bundler
    estimate_user_op_gas(&client, bundler_url, &mut userop).await?;

    // 6. Sign the UserOp hash
    let userop_hash = compute_userop_hash(&userop, smart_wallet, chain_id)?;
    let sig = signer.sign_hash_sync(&userop_hash).context("Failed to sign UserOp")?;
    userop.signature = format!("0x{}", hex::encode(sig.as_bytes()));

    // 7. Submit
    let hash = send_user_operation(&client, bundler_url, &userop).await?;
    tracing::info!(op_hash = %hash, wallet = %smart_wallet, "UserOperation submitted");

    // 8. Wait for receipt
    wait_for_receipt(&client, bundler_url, &hash).await?;

    Ok(hash)
}

async fn get_smart_wallet_nonce(client: &Client, rpc_url: &str, wallet: Address) -> Result<U256> {
    use alloy::sol_types::SolCall;
    let call_data = ICoinbaseSmartWallet::getNonceCall { key: alloy::primitives::Uint::<192, 3>::ZERO }.abi_encode();

    let resp: RpcResponse = client
        .post(rpc_url)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_call",
            "params": [{
                "to": format!("{:?}", wallet),
                "data": format!("0x{}", hex::encode(&call_data))
            }, "latest"]
        }))
        .send().await?.json().await?;

    let hex = resp.result
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "0x0".into());

    Ok(U256::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO))
}

async fn get_gas_price(client: &Client, rpc_url: &str) -> Result<u128> {
    let resp: RpcResponse = client
        .post(rpc_url)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"eth_gasPrice","params":[]}))
        .send().await?.json().await?;

    let hex = resp.result
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .context("Missing gasPrice")?;

    Ok(u128::from_str_radix(hex.trim_start_matches("0x"), 16).context("Bad gas price hex")?)
}

async fn estimate_user_op_gas(client: &Client, bundler_url: &str, op: &mut UserOperation) -> Result<()> {
    let resp: RpcResponse = client
        .post(bundler_url)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_estimateUserOperationGas",
            "params": [op, ENTRY_POINT_V06]
        }))
        .send().await?.json().await?;

    if let Some(err) = resp.error {
        anyhow::bail!("Gas estimation failed: {}", err);
    }

    if let Some(result) = resp.result {
        op.call_gas_limit = result["callGasLimit"].as_str().unwrap_or("0x30000").to_string();
        op.verification_gas_limit = result["verificationGasLimit"].as_str().unwrap_or("0x30000").to_string();
        op.pre_verification_gas = result["preVerificationGas"].as_str().unwrap_or("0xc000").to_string();
    }

    Ok(())
}

fn compute_userop_hash(op: &UserOperation, _sender: Address, chain_id: u64) -> Result<B256> {
    // ERC-4337 v0.6 UserOp hash:
    // keccak256(abi.encode(keccak256(packed(op fields without sig)), entryPoint, chainId))

    let sender = Address::from_str(&op.sender).context("bad sender")?;
    let nonce = U256::from_str_radix(op.nonce.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO);
    let init_code = hex::decode(op.init_code.trim_start_matches("0x")).unwrap_or_default();
    let call_data = hex::decode(op.call_data.trim_start_matches("0x")).unwrap_or_default();
    let call_gas_limit = U256::from_str_radix(op.call_gas_limit.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO);
    let verification_gas_limit = U256::from_str_radix(op.verification_gas_limit.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO);
    let pre_verification_gas = U256::from_str_radix(op.pre_verification_gas.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO);
    let max_fee_per_gas = U256::from_str_radix(op.max_fee_per_gas.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO);
    let max_priority_fee_per_gas = U256::from_str_radix(op.max_priority_fee_per_gas.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO);
    let paymaster_and_data = hex::decode(op.paymaster_and_data.trim_start_matches("0x")).unwrap_or_default();

    // Pack fields for inner hash
    let packed = (
        sender,
        nonce,
        keccak256(&init_code),
        keccak256(&call_data),
        call_gas_limit,
        verification_gas_limit,
        pre_verification_gas,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        keccak256(&paymaster_and_data),
    ).abi_encode();

    let inner_hash = keccak256(&packed);
    let entry_point = Address::from_str(ENTRY_POINT_V06).unwrap();

    let outer = (inner_hash, entry_point, U256::from(chain_id)).abi_encode();
    Ok(keccak256(&outer))
}

async fn send_user_operation(client: &Client, bundler_url: &str, op: &UserOperation) -> Result<String> {
    let resp: RpcResponse = client
        .post(bundler_url)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_sendUserOperation",
            "params": [op, ENTRY_POINT_V06]
        }))
        .send().await?.json().await?;

    if let Some(err) = &resp.error {
        anyhow::bail!("eth_sendUserOperation failed: {}", err);
    }

    resp.result
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .context("Missing UserOp hash in response")
}

async fn wait_for_receipt(client: &Client, bundler_url: &str, op_hash: &str) -> Result<()> {
    use tokio::time::{sleep, Duration};

    for _ in 0..30 {
        let resp: RpcResponse = client
            .post(bundler_url)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_getUserOperationReceipt",
                "params": [op_hash]
            }))
            .send().await?.json().await?;

        if let Some(result) = resp.result {
            if !result.is_null() {
                let success = result["success"].as_bool().unwrap_or(false);
                if !success {
                    anyhow::bail!("UserOperation {} failed on-chain", op_hash);
                }
                tracing::info!(op_hash, "UserOperation confirmed");
                return Ok(());
            }
        }

        sleep(Duration::from_secs(2)).await;
    }

    anyhow::bail!("UserOperation {} receipt timeout", op_hash)
}
