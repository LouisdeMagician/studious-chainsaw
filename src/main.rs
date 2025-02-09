use std::{collections::{HashMap, HashSet}, str::FromStr, sync::Arc};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::RpcTransactionConfig,
    rpc_request::TokenAccountsFilter,
};
use solana_sdk::{
    commitment_config::CommitmentConfig, 
    pubkey::Pubkey, 
    signature::Signature,
    program_pack::Pack
};
use solana_transaction_status::UiTransactionEncoding;
use solana_account_decoder::UiAccountData;
use log::{info, error};
use chrono::{DateTime, Utc, NaiveDateTime};
use regex::Regex;
use spl_token::state::{Account, Mint};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::{sink::SinkExt, StreamExt};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;

const SOLANA_RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const SOLANA_WS_URL: &str = "wss://api.mainnet-beta.solana.com/";
const STABLECOIN_MINTS: [&str; 3] = [
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
    "USDH1SM1ojwWUga67PGrgFWUHibbjqMvuMaDkRJTgkX",
];

struct SwapEvent {
    timestamp: DateTime<Utc>,
    input_mint: String,
    output_mint: String,
    in_amount: f64,
    out_amount: f64,
    signature: String,
    dex: String,
}

struct PortfolioTracker {
    wallet: Pubkey,
    rpc_client: Arc<RpcClient>,
    token_balances: HashMap<String, (f64, u8)>,
    swap_history: Vec<SwapEvent>,
}

impl PortfolioTracker {
    async fn new(wallet: Pubkey) -> Self {
        let client = Arc::new(RpcClient::new(SOLANA_RPC_URL.to_string()));
        let mut tracker = Self {
            wallet,
            rpc_client: client.clone(),
            token_balances: HashMap::new(),
            swap_history: Vec::new(),
        };
        tracker.refresh_portfolio().await;
        tracker
    }

    async fn refresh_portfolio(&mut self) {
        if let Ok(balance) = self.rpc_client.get_balance(&self.wallet) {
            let sol_balance = balance as f64 / 1e9;
            self.token_balances.insert("SOL".to_string(), (sol_balance, 9));
        }

        if let Ok(accounts) = self.rpc_client.get_token_accounts_by_owner(
            &self.wallet,
            TokenAccountsFilter::ProgramId(spl_token::id()),
        ) {
            for account in accounts.iter() {
                if let UiAccountData::Binary(encoded_data, _) = &account.data {
                    let decoded_data = STANDARD.decode(encoded_data).expect("Failed to decode data");
                    if let Ok(token_account) = Account::unpack(&decoded_data) {
                        let mint = token_account.mint.to_string();
                        if let Ok(mint_account) = self.rpc_client.get_account(&token_account.mint) {
                            if let UiAccountData::Binary(encoded_mint_data, _) = &mint_account.data {
                                let decoded_mint_data = STANDARD.decode(encoded_mint_data).unwrap_or_default();
                                if let Ok(mint_data) = Mint::unpack(&decoded_mint_data) {
                                    let balance = token_account.amount as f64 / 10f64.powi(mint_data.decimals as i32);
                                    self.token_balances.insert(mint, (balance, mint_data.decimals));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    
    async fn process_transaction(&mut self, signature: &str) {
        let config = RpcTransactionConfig {
            commitment: Some(CommitmentConfig::finalized()),
            encoding: Some(UiTransactionEncoding::JsonParsed),
            ..Default::default()
        };

        if let Ok(tx) = self.rpc_client.get_transaction_with_config(&Signature::from_str(signature).unwrap(), config) {
            let timestamp = NaiveDateTime::from_timestamp_opt(tx.block_time.unwrap(), 0)
                .expect("Invalid timestamp")
                .and_utc();
            let logs = tx.transaction.meta
                .and_then(|meta| meta.log_messages.clone())
                .unwrap_or_else(Vec::new);
            if let Some(swap) = self.detect_swap(&logs, signature).await {
                self.handle_swap_event(swap, timestamp).await;
                self.refresh_portfolio().await;
            }
        } else {
            error!("Failed to process tx: {}", signature);
        }
    }

    fn extract_signature(text: &str) -> Option<String> {
        text.split_whitespace().find(|s| s.len() == 88).map(|s| s.to_string())
    }

    async fn detect_swap(&self, logs: &[String], signature: &str) -> Option<SwapEvent> {
        let amount_regex = Regex::new(r"amount[:=](\d+)").unwrap();
        let mint_regex = Regex::new(r"(\w{32})").unwrap();
        let mut mints = HashSet::new();
        let dex = "Unknown".to_string();

        for log in logs {
            for cap in mint_regex.captures_iter(log) {
                if let Some(mint) = cap.get(0) {
                    mints.insert(mint.as_str().to_string());
                }
            }
        }

        let amounts: Vec<f64> = amount_regex
            .captures_iter(&logs.join("\n"))
            .filter_map(|c| c.get(1).and_then(|m| m.as_str().parse::<u64>().ok()))
            .map(|v| v as f64)
            .collect();

        if mints.len() < 2 || amounts.len() < 2 {
            return None;
        }

        Some(SwapEvent {
            timestamp: Utc::now(),
            input_mint: mints.iter().next().unwrap().clone(),
            output_mint: mints.iter().nth(1).unwrap().clone(),
            in_amount: amounts[0] / 1e9,
            out_amount: amounts[1] / 1e9,
            signature: signature.to_string(),
            dex,
        })
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let wallet_pubkey = Pubkey::from_str("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1").unwrap();
    let mut tracker = PortfolioTracker::new(wallet_pubkey).await;
    let (mut ws_stream, _) = connect_async(SOLANA_WS_URL).await.unwrap();
    ws_stream.send(Message::Text(format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"logsSubscribe","params":[{{"mentions": ["{}"]}}]}}"#,
        wallet_pubkey
    ))).await.unwrap();

    if let Some(sig) = extract_signature(&text) {
        println!("Transaction Signature: {}", sig);
    }

    let logs = tx.transaction.meta
        .and_then(|meta| meta.log_messages.clone())
        .unwrap_or_else(Vec::new);

    let timestamp = NaiveDateTime::from_timestamp_opt(tx.block_time.unwrap(), 0)
        .expect("Invalid timestamp")
        .and_utc();

}
