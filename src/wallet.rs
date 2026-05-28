use anyhow::{bail, Context, Result};
use rand::Rng;
use std::env;
use alloy::signers::local::{MnemonicBuilder, PrivateKeySigner};

pub struct WalletPool {
    wallets: Vec<PrivateKeySigner>,
}

impl WalletPool {
    /// Load wallets from environment.
    ///
    /// Priority:
    ///   1. WALLET_MNEMONIC — derives accounts 0..N (default 5) via BIP-44 m/44'/60'/0'/0/i
    ///   2. WALLET_KEY_0, WALLET_KEY_1, ... — raw 0x-prefixed private keys (fallback)
    pub fn load_from_env(prefix: &str) -> Result<Self> {
        // Try mnemonic first
        if let Ok(phrase) = env::var("WALLET_MNEMONIC") {
            return Self::load_from_mnemonic(phrase.trim());
        }

        // Fall back to individual private keys
        Self::load_from_keys(prefix)
    }

    fn load_from_mnemonic(phrase: &str) -> Result<Self> {
        // How many derived accounts to use (default 5, override with WALLET_MNEMONIC_COUNT)
        let count: u32 = env::var("WALLET_MNEMONIC_COUNT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        let mut wallets = Vec::new();

        for i in 0..count {
            let signer = MnemonicBuilder::<alloy::signers::local::coins_bip39::English>::from_phrase_nth(phrase, i);
            tracing::info!("Derived wallet {} → {}", i, signer.address());
            wallets.push(signer);
        }

        tracing::info!("Loaded {} wallet(s) from mnemonic (BIP-44 m/44'/60'/0'/0/0..{})", wallets.len(), count - 1);
        Ok(Self { wallets })
    }

    fn load_from_keys(prefix: &str) -> Result<Self> {
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
                "No wallets found. Set WALLET_MNEMONIC or {prefix}0, {prefix}1, ... in your .env"
            );
        }

        tracing::info!("Loaded {} wallet(s) from private keys", wallets.len());
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
