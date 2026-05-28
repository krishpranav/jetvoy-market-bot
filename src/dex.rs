use anyhow::{Context, Result};
use alloy::{
    primitives::{Address, TxKind, U256},
    providers::Provider,
    rpc::types::TransactionInput,
    sol,
    sol_types::SolCall,
};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{config::AppConfig, randomizer::{Side, TradeParams}};

// Uniswap V3 SwapRouter02 on Base mainnet
pub const SWAP_ROUTER_BASE: &str = "0x2626664c2603336E57B271c5C0b26F421741e481";

pub const USDC_DECIMALS: u32 = 6;
pub const WETH_DECIMALS: u32 = 18;

sol! {
    #[allow(missing_docs)]
    interface ISwapRouter {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24  fee;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        function exactInputSingle(ExactInputSingleParams calldata params)
            external
            payable
            returns (uint256 amountOut);
    }
}

sol! {
    #[allow(missing_docs)]
    interface IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24  tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8  feeProtocol,
            bool   unlocked
        );
    }
}

sol! {
    #[allow(missing_docs)]
    interface IERC20 {
        function decimals() external view returns (uint8);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
    }
}

pub struct SwapCalldata {
    pub to: Address,
    pub data: Vec<u8>,
    pub value: U256,
}

/// Check if wallet has enough token balance for the trade. Returns error if insufficient.
pub async fn check_balance(
    params: &TradeParams,
    wallet: Address,
    cfg: &AppConfig,
    provider: &impl Provider,
) -> Result<()> {
    use crate::randomizer::Side;

    let token_in = Address::from_str(&cfg.pair.token_in).context("Invalid token_in")?;
    let token_out = Address::from_str(&cfg.pair.token_out).context("Invalid token_out")?;

    let (sell_token, sell_decimals) = match params.side {
        Side::Buy => (token_in, USDC_DECIMALS),
        Side::Sell => (token_out, WETH_DECIMALS),
    };

    let balance = get_token_balance(sell_token, wallet, provider).await?;
    let needed = match params.side {
        Side::Buy => U256::from((params.amount_usd * 10f64.powi(sell_decimals as i32)) as u64),
        Side::Sell => {
            let eth_price = fetch_eth_price_from_pool(provider, cfg).await.unwrap_or(3000.0);
            let eth_needed = params.amount_usd / eth_price;
            U256::from((eth_needed * 10f64.powi(sell_decimals as i32)) as u128)
        }
    };

    if balance < needed {
        let balance_usd = match params.side {
            Side::Buy => balance.to::<u64>() as f64 / 10f64.powi(sell_decimals as i32),
            Side::Sell => {
                let eth_price = fetch_eth_price_from_pool(provider, cfg).await.unwrap_or(3000.0);
                (balance.to::<u128>() as f64 / 10f64.powi(sell_decimals as i32)) * eth_price
            }
        };
        anyhow::bail!(
            "Insufficient balance: have ${:.2}, need ${:.2} — skipping trade",
            balance_usd,
            params.amount_usd
        );
    }

    Ok(())
}

async fn get_token_balance(token: Address, owner: Address, provider: &impl Provider) -> Result<U256> {
    let call = IERC20::balanceOfCall { account: owner };
    let encoded = call.abi_encode();
    let tx = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(token)),
        input: TransactionInput::new(encoded.into()),
        ..Default::default()
    };
    let result = provider.call(tx).await.context("balanceOf call failed")?;
    if result.len() < 32 {
        return Ok(U256::ZERO);
    }
    Ok(U256::from_be_slice(&result[..32]))
}

/// Build calldata for an exactInputSingle swap on Uniswap V3 / Base.
pub async fn build_swap(
    params: &TradeParams,
    recipient: Address,
    cfg: &AppConfig,
    provider: &impl Provider,
) -> Result<SwapCalldata> {
    let token_in = Address::from_str(&cfg.pair.token_in).context("Invalid token_in address")?;
    let token_out = Address::from_str(&cfg.pair.token_out).context("Invalid token_out address")?;

    let (sell_token, buy_token, sell_decimals) = match params.side {
        crate::randomizer::Side::Buy => (token_in, token_out, USDC_DECIMALS),
        crate::randomizer::Side::Sell => (token_out, token_in, WETH_DECIMALS),
    };

    let amount_in = usd_to_token_units(params.amount_usd, sell_decimals, &params.side, provider, cfg).await?;

    let fee_u24: alloy::primitives::Uint<24, 1> =
        alloy::primitives::Uint::<24, 1>::from(cfg.pair.fee_tier);
    let sqrt_limit: alloy::primitives::Uint<160, 3> = alloy::primitives::Uint::<160, 3>::ZERO;

    let swap_params = ISwapRouter::ExactInputSingleParams {
        tokenIn: sell_token,
        tokenOut: buy_token,
        fee: fee_u24,
        recipient,
        amountIn: amount_in,
        amountOutMinimum: U256::ZERO, // slippage handled by gas ceiling + tx deadline
        sqrtPriceLimitX96: sqrt_limit,
    };

    let call = ISwapRouter::exactInputSingleCall { params: swap_params };
    let encoded = call.abi_encode();
    let router = Address::from_str(SWAP_ROUTER_BASE).unwrap();

    Ok(SwapCalldata {
        to: router,
        data: encoded,
        value: U256::ZERO,
    })
}

/// Check allowance and return approve calldata if needed.
pub async fn ensure_approval(
    token: Address,
    owner: Address,
    spender: Address,
    _amount: U256,
    provider: &impl Provider,
) -> Result<Option<Vec<u8>>> {
    let call = IERC20::allowanceCall { owner, spender };
    let encoded = call.abi_encode();

    let tx = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(token)),
        input: TransactionInput::new(encoded.into()),
        ..Default::default()
    };

    let result = provider.call(tx).await.context("allowance call failed")?;
    if result.len() < 32 {
        let approve_call = IERC20::approveCall { spender, amount: U256::MAX };
        return Ok(Some(approve_call.abi_encode()));
    }

    let allowance = U256::from_be_slice(&result[..32]);
    if allowance > U256::from(u128::MAX) {
        return Ok(None);
    }

    let approve_call = IERC20::approveCall { spender, amount: U256::MAX };
    Ok(Some(approve_call.abi_encode()))
}

async fn usd_to_token_units(
    usd: f64,
    decimals: u32,
    side: &crate::randomizer::Side,
    provider: &impl Provider,
    cfg: &AppConfig,
) -> Result<U256> {
    match side {
        crate::randomizer::Side::Buy => {
            let units = (usd * 10f64.powi(decimals as i32)).round() as u64;
            Ok(U256::from(units))
        }
        crate::randomizer::Side::Sell => {
            let eth_price_usd = fetch_eth_price_from_pool(provider, cfg).await.unwrap_or(3000.0);
            let eth_amount = usd / eth_price_usd;
            let units = (eth_amount * 10f64.powi(decimals as i32)) as u128;
            Ok(U256::from(units))
        }
    }
}

async fn fetch_eth_price_from_pool(provider: &impl Provider, _cfg: &AppConfig) -> Result<f64> {
    let pool_addr = Address::from_str("0xd0b53D9277642d899DF5C87A3966A349A798F224")
        .context("Invalid pool address")?;

    let call = IUniswapV3Pool::slot0Call {};
    let encoded = call.abi_encode();

    let tx = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(pool_addr)),
        input: TransactionInput::new(encoded.into()),
        ..Default::default()
    };

    let result = provider.call(tx).await.context("slot0 call failed")?;

    if result.len() < 32 {
        anyhow::bail!("slot0 response too short");
    }

    let sqrt_price_x96 = U256::from_be_slice(&result[..32]);
    Ok(sqrt_price_x96_to_price(sqrt_price_x96))
}

fn sqrt_price_x96_to_price(sqrt_price_x96: U256) -> f64 {
    let q96 = 2f64.powi(96);
    let sqrt_price = sqrt_price_x96.to::<u128>() as f64 / q96;
    let raw_price = sqrt_price * sqrt_price;
    raw_price * 1e12
}
