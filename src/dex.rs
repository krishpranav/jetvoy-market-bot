use anyhow::{Context, Result};
use alloy::{
    primitives::{Address, TxKind, U256},
    providers::Provider,
    rpc::types::TransactionInput,
    sol,
    sol_types::SolCall,
};
use std::str::FromStr;

use crate::{config::AppConfig, randomizer::{Side, TradeParams}};

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

/// Get the actual token balance for a wallet.
pub async fn get_token_balance(
    token: Address,
    owner: Address,
    provider: &impl Provider,
) -> Result<U256> {
    let call = IERC20::balanceOfCall { account: owner };
    let tx = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(token)),
        input: TransactionInput::new(call.abi_encode().into()),
        ..Default::default()
    };
    let result = provider.call(tx).await.context("balanceOf call failed")?;
    if result.len() < 32 {
        return Ok(U256::ZERO);
    }
    Ok(U256::from_be_slice(&result[..32]))
}

/// Get native ETH balance.
pub async fn get_eth_balance(wallet: Address, provider: &impl Provider) -> Result<U256> {
    Ok(provider.get_balance(wallet).await.context("get_balance failed")?)
}

/// Compute a safe trade amount based on actual wallet balance.
/// Uses at most 30% of available balance per trade, respecting config min/max.
pub async fn safe_trade_amount_usd(
    side: &Side,
    wallet: Address,
    cfg: &AppConfig,
    provider: &impl Provider,
) -> Result<f64> {
    let token_in = Address::from_str(&cfg.pair.token_in).context("Invalid token_in")?;
    let token_out = Address::from_str(&cfg.pair.token_out).context("Invalid token_out")?;

    let (sell_token, decimals) = match side {
        Side::Buy => (token_in, USDC_DECIMALS),
        Side::Sell => (token_out, WETH_DECIMALS),
    };

    let balance = get_token_balance(sell_token, wallet, provider).await?;
    let balance_units = balance.to::<u128>() as f64;
    let balance_in_base = balance_units / 10f64.powi(decimals as i32);

    // Convert balance to USD
    let balance_usd = match side {
        Side::Buy => balance_in_base, // USDC is 1:1 with USD
        Side::Sell => {
            let eth_price = fetch_eth_price_from_pool(provider).await.unwrap_or(3000.0);
            balance_in_base * eth_price
        }
    };

    if balance_usd < cfg.sizing.min_usd {
        anyhow::bail!(
            "Wallet balance ${:.2} is below minimum trade size ${:.2} — skipping",
            balance_usd,
            cfg.sizing.min_usd
        );
    }

    // Use at most 30% of balance per trade
    let max_tradeable = balance_usd * 0.30;
    let amount = max_tradeable.min(cfg.sizing.max_usd).max(cfg.sizing.min_usd);

    Ok(amount)
}

/// Build swap calldata. Amount is derived from actual balance.
pub async fn build_swap(
    params: &TradeParams,
    recipient: Address,
    cfg: &AppConfig,
    provider: &impl Provider,
) -> Result<SwapCalldata> {
    let token_in = Address::from_str(&cfg.pair.token_in).context("Invalid token_in")?;
    let token_out = Address::from_str(&cfg.pair.token_out).context("Invalid token_out")?;

    let (sell_token, buy_token, sell_decimals) = match params.side {
        Side::Buy => (token_in, token_out, USDC_DECIMALS),
        Side::Sell => (token_out, token_in, WETH_DECIMALS),
    };

    let amount_in = usd_to_token_units(params.amount_usd, sell_decimals, &params.side, provider).await?;

    let fee_u24: alloy::primitives::Uint<24, 1> =
        alloy::primitives::Uint::<24, 1>::from(cfg.pair.fee_tier);
    let sqrt_limit: alloy::primitives::Uint<160, 3> = alloy::primitives::Uint::<160, 3>::ZERO;

    let swap_params = ISwapRouter::ExactInputSingleParams {
        tokenIn: sell_token,
        tokenOut: buy_token,
        fee: fee_u24,
        recipient,
        amountIn: amount_in,
        amountOutMinimum: U256::ZERO,
        sqrtPriceLimitX96: sqrt_limit,
    };

    let call = ISwapRouter::exactInputSingleCall { params: swap_params };
    let router = Address::from_str(SWAP_ROUTER_BASE).unwrap();

    Ok(SwapCalldata {
        to: router,
        data: call.abi_encode(),
        value: U256::ZERO,
    })
}

/// Simulate the swap with eth_call — FREE, no gas spent.
/// Returns Err if the transaction would revert.
pub async fn simulate_swap(
    calldata: &SwapCalldata,
    from: Address,
    provider: &impl Provider,
) -> Result<()> {
    let tx = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(calldata.to)),
        from: Some(from),
        input: TransactionInput::new(calldata.data.clone().into()),
        value: Some(calldata.value),
        ..Default::default()
    };

    provider
        .call(tx)
        .await
        .context("Simulation failed — transaction would revert. Skipping to avoid gas burn.")?;

    Ok(())
}

/// Check ETH balance is enough to cover estimated gas cost.
pub async fn check_gas_funds(
    wallet: Address,
    provider: &impl Provider,
) -> Result<()> {
    let eth_balance = get_eth_balance(wallet, provider).await?;
    // Require at least 0.0005 ETH for gas headroom (~700 trades on Base)
    let min_eth = U256::from(500_000_000_000_000u128); // 0.0005 ETH in wei
    if eth_balance < min_eth {
        let eth_f = eth_balance.to::<u128>() as f64 / 1e18;
        anyhow::bail!(
            "Insufficient ETH for gas: {:.6} ETH — skipping until wallet is topped up",
            eth_f
        );
    }
    Ok(())
}

/// Check and return approve calldata if SwapRouter allowance is insufficient.
pub async fn ensure_approval(
    token: Address,
    owner: Address,
    spender: Address,
    _amount: U256,
    provider: &impl Provider,
) -> Result<Option<Vec<u8>>> {
    let call = IERC20::allowanceCall { owner, spender };
    let tx = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(token)),
        input: TransactionInput::new(call.abi_encode().into()),
        ..Default::default()
    };

    let result = provider.call(tx).await.context("allowance call failed")?;
    if result.len() < 32 {
        let approve = IERC20::approveCall { spender, amount: U256::MAX };
        return Ok(Some(approve.abi_encode()));
    }

    let allowance = U256::from_be_slice(&result[..32]);
    if allowance > U256::from(u128::MAX) {
        return Ok(None); // already fully approved
    }

    let approve = IERC20::approveCall { spender, amount: U256::MAX };
    Ok(Some(approve.abi_encode()))
}

async fn usd_to_token_units(
    usd: f64,
    decimals: u32,
    side: &Side,
    provider: &impl Provider,
) -> Result<U256> {
    match side {
        Side::Buy => {
            let units = (usd * 10f64.powi(decimals as i32)).round() as u64;
            Ok(U256::from(units))
        }
        Side::Sell => {
            let eth_price = fetch_eth_price_from_pool(provider).await.unwrap_or(3000.0);
            let eth_amount = usd / eth_price;
            let units = (eth_amount * 10f64.powi(decimals as i32)) as u128;
            Ok(U256::from(units))
        }
    }
}

pub async fn fetch_eth_price_from_pool(provider: &impl Provider) -> Result<f64> {
    let pool_addr = Address::from_str("0xd0b53D9277642d899DF5C87A3966A349A798F224")?;
    let call = IUniswapV3Pool::slot0Call {};
    let tx = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(pool_addr)),
        input: TransactionInput::new(call.abi_encode().into()),
        ..Default::default()
    };
    let result = provider.call(tx).await.context("slot0 call failed")?;
    if result.len() < 32 {
        anyhow::bail!("slot0 response too short");
    }
    let sqrt_price_x96 = U256::from_be_slice(&result[..32]);
    let q96 = 2f64.powi(96);
    let sqrt_price = sqrt_price_x96.to::<u128>() as f64 / q96;
    Ok(sqrt_price * sqrt_price * 1e12)
}
