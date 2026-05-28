use anyhow::Result;
use chrono::Utc;
use alloy::primitives::{Address, TxHash};
use std::fs::OpenOptions;
use std::io::Write;

use crate::randomizer::{Side, TradeParams};

pub struct Monitor {
    log_file: String,
    pub total_volume_usd: f64,
    pub consecutive_failures: u32,
    alert_threshold: u32,
    telegram_bot_token: Option<String>,
    telegram_chat_id: Option<String>,
}

impl Monitor {
    pub fn new(
        log_file: String,
        alert_threshold: u32,
        telegram_bot_token: Option<String>,
        telegram_chat_id: Option<String>,
    ) -> Result<Self> {
        // Write CSV header if file doesn't exist
        if !std::path::Path::new(&log_file).exists() {
            let mut f = OpenOptions::new().create(true).append(true).open(&log_file)?;
            writeln!(f, "timestamp,side,amount_usd,wallet_addr,tx_hash,gas_used,status")?;
        }

        Ok(Self {
            log_file,
            total_volume_usd: 0.0,
            consecutive_failures: 0,
            alert_threshold,
            telegram_bot_token,
            telegram_chat_id,
        })
    }

    pub fn log_success(&mut self, params: &TradeParams, wallet: Address, hash: TxHash, gas_used: u64) {
        self.total_volume_usd += params.amount_usd;
        self.consecutive_failures = 0;

        let row = format!(
            "{},{},{:.2},{},{},{},ok",
            Utc::now().to_rfc3339(),
            params.side,
            params.amount_usd,
            wallet,
            hash,
            gas_used,
        );

        tracing::info!(
            side = %params.side,
            amount_usd = params.amount_usd,
            wallet = %wallet,
            tx = %hash,
            gas_used,
            total_volume_usd = self.total_volume_usd,
            "Trade executed"
        );

        self.append_csv(&row);
    }

    pub fn log_success_smart(&mut self, params: &TradeParams, wallet: Address, op_hash: &str) {
        self.total_volume_usd += params.amount_usd;
        self.consecutive_failures = 0;

        let row = format!(
            "{},{},{:.2},{},{},0,ok",
            Utc::now().to_rfc3339(),
            params.side,
            params.amount_usd,
            wallet,
            op_hash,
        );

        tracing::info!(
            side = %params.side,
            amount_usd = params.amount_usd,
            wallet = %wallet,
            op_hash,
            total_volume_usd = self.total_volume_usd,
            "Smart wallet trade executed"
        );

        self.append_csv(&row);
    }

    pub fn log_failure(&mut self, params: &TradeParams, wallet: Address, reason: &str) {
        self.consecutive_failures += 1;

        let row = format!(
            "{},{},{:.2},{},,,failed:{}",
            Utc::now().to_rfc3339(),
            params.side,
            params.amount_usd,
            wallet,
            reason.replace(',', ";"),
        );

        tracing::error!(
            side = %params.side,
            amount_usd = params.amount_usd,
            wallet = %wallet,
            consecutive_failures = self.consecutive_failures,
            reason,
            "Trade failed"
        );

        self.append_csv(&row);

        if self.alert_threshold > 0 && self.consecutive_failures >= self.alert_threshold {
            self.send_alert(reason);
        }
    }

    fn append_csv(&self, row: &str) {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.log_file) {
            let _ = writeln!(f, "{}", row);
        }
    }

    fn send_alert(&self, reason: &str) {
        if let (Some(token), Some(chat_id)) = (&self.telegram_bot_token, &self.telegram_chat_id) {
            let token = token.clone();
            let chat_id = chat_id.clone();
            let msg = format!(
                "⚠️ Market bot: {} consecutive failures. Last error: {}",
                self.consecutive_failures, reason
            );
            // Fire-and-forget async alert
            tokio::spawn(async move {
                let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
                let _ = reqwest::Client::new()
                    .post(&url)
                    .json(&serde_json::json!({
                        "chat_id": chat_id,
                        "text": msg,
                    }))
                    .send()
                    .await;
            });
        }
    }
}
