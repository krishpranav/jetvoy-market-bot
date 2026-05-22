use anyhow::{bail, Context, Result};
use rand::Rng;
use std::env;
use alloy::signers::local::PrivateKeySigner;

pub struct WalletPool {
    wallets: Vec<PrivateKeySigner>,
}

impl WalletPool {
    /// Load all WALLET_KEY_0, WALLET_KEY_1, ... from environment variables.
    pub fn load_from_env(prefix: &str) -> Result<Self> {
        let mut wallets = Vec::new();
        let mut index = 0;

        loop {
            let key = format!("{}{}", prefix, index);
            match env::var(&key) {
                Ok(val) => {
                    let signer: PrivateKeySigner = val
                        .trim()
                        .parse()
                        .with_context(|| format!("Failed to parse {key} as private key"))?;
                    tracing::info!("Loaded wallet {} → {}", index, signer.address());
                    wallets.push(signer);
                    index += 1;
                }
                Err(_) => break,
            }
        }

        if wallets.is_empty() {
            bail!(
                "No wallet keys found. Set {prefix}0, {prefix}1, ... in your .env file."
            );
        }

        tracing::info!("Loaded {} wallet(s)", wallets.len());
        Ok(Self { wallets })
    }

    /// Pick a random wallet from the pool.
    pub fn pick(&self, rng: &mut impl Rng) -> &PrivateKeySigner {
        let idx = rng.gen_range(0..self.wallets.len());
        &self.wallets[idx]
    }

    pub fn len(&self) -> usize {
        self.wallets.len()
    }
}
