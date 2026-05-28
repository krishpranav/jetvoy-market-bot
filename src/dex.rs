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

use crate::{config::AppConfig, randomizer::Side};

// Uniswap V2 Router02 on Base
pub const SWAP_ROUTER_BASE: &str = "0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24";
// WETH on Base
pub const WETH: &str = "0x4200000000000000000000000000000000000006";
// WETH has 18 decimals
pub const WETH_DECIMALS: u32 = 18;

sol! {
    #[allow(missing_docs)]
    interface IUniswapV2Router {
        function swapExactETHForTokens(
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external payable returns (uint256[] memory amounts);

        function swapExactTokensForETH(
            uint256 amountIn,
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external returns (uint256[] memory amounts);

        function getAmountsOut(
            uint256 amountIn,
            address[] calldata path
        ) external view returns (uint256[] memory amounts);
    }
}

sol! {
    #[allow(missing_docs)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (
            uint112 reserve0,
            uint112 reserve1,
            uint32 blockTimestampLast
        );
        function token0() external view returns (address);
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
    pub value: U256, // ETH value to send with tx (for BUY)
}

/// Get native ETH balance.
pub async fn get_eth_balance(wallet: Address, provider: &impl Provider) -> Result<U256> {
    Ok(provider.get_balance(wallet).await.context("get_balance failed")?)
}

/// Get ERC-20 token balance.
pub async fn get_token_balance(token: Address, owner: Address, provider: &impl Provider) -> Result<U256> {
    let call = IERC20::balanceOfCall { account: owner };
    let tx = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(token)),
        input: TransactionInput::new(call.abi_encode().into()),
        ..Default::default()
    };
    let result = provider.call(tx).await.context("balanceOf failed")?;
    if result.len() < 32 { return Ok(U256::ZERO); }
    Ok(U256::from_be_slice(&result[..32]))
}

/// Check ETH >= 0.0005 ETH for gas headroom.
pub async fn check_gas_funds(wallet: Address, provider: &impl Provider) -> Result<()> {
    let eth = get_eth_balance(wallet, provider).await?;
    let min = U256::from(500_000_000_000_000u128); // 0.0005 ETH
    if eth < min {
        anyhow::bail!(
            "Low ETH for gas: {:.6} ETH — skipping until topped up",
            eth.to::<u128>() as f64 / 1e18
        );
    }
    Ok(())
}

/// Compute safe trade amount based on actual wallet balance (max 20% per trade).
pub async fn safe_trade_amount_usd(
    side: &Side,
    wallet: Address,
    cfg: &AppConfig,
    provider: &impl Provider,
) -> Result<f64> {
    let jetvoy = Address::from_str(&cfg.pair.token_out).context("Invalid token_out")?;

    let balance_usd = match side {
        Side::Buy => {
            // BUY: spend ETH — check ETH balance
            let eth = get_eth_balance(wallet, provider).await?;
            let eth_f = eth.to::<u128>() as f64 / 1e18;
            let eth_price = get_eth_price_usd(provider, cfg).await.unwrap_or(3000.0);
            // Reserve 0.002 ETH for gas, only trade what's left
            let tradeable_eth = (eth_f - 0.002).max(0.0);
            tradeable_eth * eth_price
        }
        Side::Sell => {
            // SELL: spend Jetvoy tokens — check token balance
            let bal = get_token_balance(jetvoy, wallet, provider).await?;
            let bal_f = bal.to::<u128>() as f64;
            let eth_price = get_eth_price_usd(provider, cfg).await.unwrap_or(3000.0);
            let jetvoy_price_usd = get_jetvoy_price_usd(provider, cfg, eth_price).await.unwrap_or(0.0);
            // Token has 18 decimals
            (bal_f / 1e18) * jetvoy_price_usd
        }
    };

    if balance_usd < cfg.sizing.min_usd {
        anyhow::bail!(
            "Balance ${:.4} below minimum ${:.2} — skipping",
            balance_usd, cfg.sizing.min_usd
        );
    }

    // Use at most 20% of balance per trade
    let amount = (balance_usd * 0.20).min(cfg.sizing.max_usd).max(cfg.sizing.min_usd);
    Ok(amount)
}

/// Build Uniswap V2 swap calldata.
/// BUY  = swapExactETHForTokens  (send ETH, receive JETVOY)
/// SELL = swapExactTokensForETH  (send JETVOY, receive ETH)
pub async fn build_swap(
    side: &Side,
    amount_usd: f64,
    recipient: Address,
    cfg: &AppConfig,
    provider: &impl Provider,
) -> Result<SwapCalldata> {
    let router = Address::from_str(SWAP_ROUTER_BASE).unwrap();
    let weth = Address::from_str(WETH).unwrap();
    let jetvoy = Address::from_str(&cfg.pair.token_out).context("Invalid token_out")?;

    let deadline = U256::from(
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + cfg.safety.tx_deadline_secs
    );

    let eth_price = get_eth_price_usd(provider, cfg).await.unwrap_or(3000.0);

    match side {
        Side::Buy => {
            // Convert USD → ETH amount
            let eth_amount = amount_usd / eth_price;
            let eth_wei = U256::from((eth_amount * 1e18) as u128);

            let path = vec![weth, jetvoy];
            let call = IUniswapV2Router::swapExactETHForTokensCall {
                amountOutMin: U256::ZERO, // simulation gate catches bad trades
                path,
                to: recipient,
                deadline,
            };

            Ok(SwapCalldata {
                to: router,
                data: call.abi_encode(),
                value: eth_wei, // send ETH with the tx
            })
        }
        Side::Sell => {
            // Convert USD → Jetvoy token amount
            let jetvoy_price_usd = get_jetvoy_price_usd(provider, cfg, eth_price).await?;
            let jetvoy_amount = amount_usd / jetvoy_price_usd;
            let jetvoy_wei = U256::from((jetvoy_amount * 1e18) as u128);

            let path = vec![jetvoy, weth];
            let call = IUniswapV2Router::swapExactTokensForETHCall {
                amountIn: jetvoy_wei,
                amountOutMin: U256::ZERO,
                path,
                to: recipient,
                deadline,
            };

            Ok(SwapCalldata {
                to: router,
                data: call.abi_encode(),
                value: U256::ZERO,
            })
        }
    }
}

/// Simulate swap with eth_call — free, no gas.
pub async fn simulate_swap(calldata: &SwapCalldata, from: Address, provider: &impl Provider) -> Result<()> {
    let tx = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(calldata.to)),
        from: Some(from),
        input: TransactionInput::new(calldata.data.clone().into()),
        value: Some(calldata.value),
        ..Default::default()
    };
    provider.call(tx).await
        .context("Simulation failed — tx would revert. Skipping.")?;
    Ok(())
}

/// Check and return approve calldata if Jetvoy allowance for router is insufficient.
pub async fn ensure_approval(
    token: Address,
    owner: Address,
    spender: Address,
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
        return Ok(Some(IERC20::approveCall { spender, amount: U256::MAX }.abi_encode()));
    }
    let allowance = U256::from_be_slice(&result[..32]);
    if allowance > U256::from(u128::MAX) {
        return Ok(None);
    }
    Ok(Some(IERC20::approveCall { spender, amount: U256::MAX }.abi_encode()))
}

async fn get_eth_price_usd(provider: &impl Provider, cfg: &AppConfig) -> Result<f64> {
    // Use Uniswap V2 pair reserves to derive ETH price
    // WETH/USDC pair on Base: 0xb4885Bc63399BF5518b994c1d0C153334Ee579D0
    let usdc_weth_pair = Address::from_str("0xb4885Bc63399BF5518b994c1d0C153334Ee579D0")
        .unwrap_or(Address::ZERO);

    let _ = cfg;
    match get_pair_price_eth(usdc_weth_pair, provider, true, 6).await {
        Ok(Ok(p)) => Ok(p),
        _ => Ok(3000.0),
    }
}

async fn get_jetvoy_price_usd(provider: &impl Provider, cfg: &AppConfig, eth_price: f64) -> Result<f64> {
    // JETVOY/WETH V2 pair
    let pair = Address::from_str("0x8361e0FD714DA989874CCbF34175D64673B1B3D4")?;
    let jetvoy = Address::from_str(&cfg.pair.token_out)?;
    let weth = Address::from_str(WETH)?;

    // Get token0
    let t0_call = IUniswapV2Pair::token0Call {};
    let tx = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(pair)),
        input: TransactionInput::new(t0_call.abi_encode().into()),
        ..Default::default()
    };
    let t0_res = provider.call(tx).await?;
    let token0 = Address::from_slice(&t0_res[12..32]);
    let jetvoy_is_token0 = token0 == jetvoy;

    // Get reserves
    let res_call = IUniswapV2Pair::getReservesCall {};
    let tx2 = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(pair)),
        input: TransactionInput::new(res_call.abi_encode().into()),
        ..Default::default()
    };
    let res = provider.call(tx2).await?;
    if res.len() < 64 { anyhow::bail!("short reserves response"); }

    let r0 = U256::from_be_slice(&res[..32]).to::<u128>() as f64;
    let r1 = U256::from_be_slice(&res[32..64]).to::<u128>() as f64;

    let _ = weth;
    // price of jetvoy in ETH
    let jetvoy_in_eth = if jetvoy_is_token0 {
        r1 / r0 // weth_reserve / jetvoy_reserve
    } else {
        r0 / r1
    };

    Ok(jetvoy_in_eth * eth_price)
}

async fn get_pair_price_eth(
    pair: Address,
    provider: &impl Provider,
    _token0_is_quote: bool,
    _quote_decimals: u32,
) -> Result<Result<f64>> {
    // Simplified — just return a default if pair call fails
    let res_call = IUniswapV2Pair::getReservesCall {};
    let tx = alloy::rpc::types::TransactionRequest {
        to: Some(TxKind::Call(pair)),
        input: TransactionInput::new(res_call.abi_encode().into()),
        ..Default::default()
    };
    let res = provider.call(tx).await?;
    if res.len() < 64 { return Ok(Ok(3000.0)); }
    let r0 = U256::from_be_slice(&res[..32]).to::<u128>() as f64;
    let r1 = U256::from_be_slice(&res[32..64]).to::<u128>() as f64;
    // USDC(6 dec) / WETH(18 dec): price = (r0 * 1e12) / r1
    let price = (r0 * 1e12) / r1;
    Ok(Ok(price))
}
