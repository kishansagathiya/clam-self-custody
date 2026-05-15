//! Solana wallet operations: load a local keypair, derive its USDC associated
//! token account, and read SOL + USDC balances.
//!
//! The keypair only ever exists in this process. Nothing in the MCP tool
//! surface exposes it to the agent — neither directly nor indirectly.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::signature::{Signature, SignerError};
use solana_sdk::signer::Signer;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use thiserror::Error;

use crate::config::{Config, Network};

/// One USDC = 10^6 base units.
pub const USDC_DECIMALS: u8 = 6;

#[derive(Debug, Error)]
pub enum WalletError {
    #[error("failed to read keypair at {path}: {source}")]
    ReadKeypair {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("keypair file at {path} is not a valid solana-keygen JSON array: {source}")]
    ParseKeypair {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("keypair file at {path} has the wrong byte length (expected 64, got {got})")]
    KeypairLength { path: String, got: usize },
    #[error("invalid pubkey {0}")]
    InvalidPubkey(String),
    #[error("Solana RPC error: {0}")]
    Rpc(#[from] solana_client::client_error::ClientError),
    #[error("invalid keypair bytes")]
    InvalidKeypair,
}

/// A loaded Solana wallet plus the network it's bound to and the USDC mint
/// for that network. Cheap to clone (the [`Keypair`] is held behind an `Arc`).
#[derive(Clone)]
pub struct Wallet {
    keypair: Arc<Keypair>,
    pubkey: Pubkey,
    network: Network,
    usdc_mint: Pubkey,
    usdc_ata: Pubkey,
}

impl Wallet {
    /// Loads the wallet using paths from [`Config`].
    pub fn from_config(config: &Config) -> Result<Self, WalletError> {
        let keypair = load_keypair_file(&config.keypair_path)?;
        let pubkey = keypair.pubkey();

        let usdc_mint = config
            .network
            .usdc_mint()
            .parse::<Pubkey>()
            .map_err(|_| WalletError::InvalidPubkey(config.network.usdc_mint().into()))?;

        let usdc_ata = get_associated_token_address_with_program_id(
            &pubkey,
            &usdc_mint,
            &spl_token::ID,
        );

        Ok(Self {
            keypair: Arc::new(keypair),
            pubkey,
            network: config.network,
            usdc_mint,
            usdc_ata,
        })
    }

    /// Buyer pubkey (Solana address).
    pub fn pubkey(&self) -> Pubkey {
        self.pubkey
    }

    /// Owned reference to the keypair. Used internally by the x402 client to
    /// sign payment transactions; never exposed via any MCP tool.
    pub fn keypair(&self) -> Arc<Keypair> {
        Arc::clone(&self.keypair)
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn usdc_mint(&self) -> Pubkey {
        self.usdc_mint
    }

    pub fn usdc_ata(&self) -> Pubkey {
        self.usdc_ata
    }

    /// Fetches the wallet's SOL and USDC balances over RPC and returns the
    /// data shaped for the `wallet_info` MCP tool.
    pub async fn info(&self, rpc: &RpcClient) -> Result<WalletInfo, WalletError> {
        let sol_lamports = rpc
            .get_balance_with_commitment(&self.pubkey, CommitmentConfig::confirmed())
            .await?
            .value;
        let sol_balance = lamports_to_sol(sol_lamports);

        let usdc_balance = match rpc.get_token_account_balance(&self.usdc_ata).await {
            Ok(b) => b.ui_amount.unwrap_or(0.0),
            Err(e) => {
                tracing::warn!(error = %e, "failed to fetch USDC balance");
                0.0
            }
        };

        Ok(WalletInfo {
            address: self.pubkey.to_string(),
            network: self.network,
            sol_balance,
            usdc_balance,
            usdc_ata: self.usdc_ata.to_string(),
            usdc_mint: self.usdc_mint.to_string(),
        })
    }
}

/// Public view of the wallet returned by the `wallet_info` MCP tool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WalletInfo {
    pub address: String,
    pub network: Network,
    pub sol_balance: f64,
    pub usdc_balance: f64,
    pub usdc_ata: String,
    pub usdc_mint: String,
}

fn load_keypair_file(path: &Path) -> Result<Keypair, WalletError> {
    let raw = fs::read_to_string(path).map_err(|source| WalletError::ReadKeypair {
        path: path.display().to_string(),
        source,
    })?;
    let bytes: Vec<u8> =
        serde_json::from_str(&raw).map_err(|source| WalletError::ParseKeypair {
            path: path.display().to_string(),
            source,
        })?;
    if bytes.len() != 64 {
        return Err(WalletError::KeypairLength {
            path: path.display().to_string(),
            got: bytes.len(),
        });
    }
    Keypair::try_from(bytes.as_slice()).map_err(|_| WalletError::InvalidKeypair)
}

fn lamports_to_sol(lamports: u64) -> f64 {
    lamports as f64 / LAMPORTS_PER_SOL as f64
}

/// A [`Clone`]-able `Signer` that wraps a shared `Keypair` behind an
/// [`Arc`]. The base [`Keypair`] intentionally does not implement `Clone`
/// (cloning ed25519 secret key material is rarely desirable), but the
/// x402 scheme client API requires `Clone + 'static`. This newtype lets us
/// satisfy that bound without copying the underlying key bytes.
#[derive(Clone)]
pub struct SharedKeypair(pub Arc<Keypair>);

impl SharedKeypair {
    pub fn new(kp: Arc<Keypair>) -> Self {
        Self(kp)
    }
}

impl Signer for SharedKeypair {
    fn try_pubkey(&self) -> Result<Pubkey, SignerError> {
        self.0.try_pubkey()
    }

    fn try_sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        self.0.try_sign_message(message)
    }

    fn is_interactive(&self) -> bool {
        self.0.is_interactive()
    }
}
