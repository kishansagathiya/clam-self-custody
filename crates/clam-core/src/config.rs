//! Configuration is driven entirely by environment variables — never by tool
//! inputs from the agent. This is intentional: the agent must not be able to
//! point the wallet at a different keypair, network, or facilitator at runtime.

use std::env;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use thiserror::Error;

/// USDC mint on Solana mainnet (Circle's canonical mint).
pub const USDC_MAINNET_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// USDC mint on Solana devnet (Circle's faucet-backed mint).
pub const USDC_DEVNET_MINT: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";

/// Default CDP facilitator endpoint.
pub const DEFAULT_FACILITATOR_URL: &str = "https://api.cdp.coinbase.com/platform/v2/x402";

/// Solana network targeted by this server instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Network {
    MainnetBeta,
    Devnet,
}

impl Network {
    pub fn as_str(self) -> &'static str {
        match self {
            Network::MainnetBeta => "mainnet-beta",
            Network::Devnet => "devnet",
        }
    }

    /// CAIP-2 chain identifier used by the x402 V2 protocol.
    pub fn caip2(self) -> &'static str {
        match self {
            Network::MainnetBeta => "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
            Network::Devnet => "solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1",
        }
    }

    /// Default Solana JSON-RPC endpoint for this network.
    pub fn default_rpc_url(self) -> &'static str {
        match self {
            Network::MainnetBeta => "https://api.mainnet-beta.solana.com",
            Network::Devnet => "https://api.devnet.solana.com",
        }
    }

    /// USDC mint address for this network.
    pub fn usdc_mint(self) -> &'static str {
        match self {
            Network::MainnetBeta => USDC_MAINNET_MINT,
            Network::Devnet => USDC_DEVNET_MINT,
        }
    }
}

impl FromStr for Network {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mainnet" | "mainnet-beta" | "solana-mainnet" => Ok(Network::MainnetBeta),
            "devnet" | "solana-devnet" => Ok(Network::Devnet),
            other => Err(ConfigError::InvalidNetwork(other.to_string())),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("CLAM_NETWORK '{0}' is not a supported Solana network (use 'devnet' or 'mainnet-beta')")]
    InvalidNetwork(String),
    #[error("unable to determine a default config directory; set CLAM_KEYPAIR_PATH and CLAM_LEDGER_PATH explicitly")]
    NoConfigDir,
}

/// All runtime configuration. Built once at process start; immutable afterward.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path to a `solana-keygen`-format JSON keypair file.
    pub keypair_path: PathBuf,
    /// Solana network this server signs for.
    pub network: Network,
    /// JSON-RPC endpoint used for balance lookups.
    pub rpc_url: String,
    /// x402 facilitator base URL.
    pub facilitator_url: String,
    /// Append-only JSONL file recording every settled payment.
    pub ledger_path: PathBuf,
}

impl Config {
    /// Builds a [`Config`] from environment variables, falling back to safe
    /// defaults (devnet, public RPC, CDP facilitator, `~/.config/clam-self-custody/`
    /// on every platform — mirroring `solana-cli`'s `~/.config/solana/` convention
    /// rather than the OS-idiomatic `directories` crate output).
    pub fn from_env() -> Result<Self, ConfigError> {
        let network = match env::var("CLAM_NETWORK") {
            Ok(s) => s.parse()?,
            Err(_) => Network::Devnet,
        };

        let default_dirs = default_config_dir();

        let keypair_path = env::var("CLAM_KEYPAIR_PATH")
            .ok()
            .map(PathBuf::from)
            .or_else(|| default_dirs.as_ref().map(|d| d.join("keypair.json")))
            .ok_or(ConfigError::NoConfigDir)?;

        let ledger_path = env::var("CLAM_LEDGER_PATH")
            .ok()
            .map(PathBuf::from)
            .or_else(|| default_dirs.as_ref().map(|d| d.join("payments.jsonl")))
            .ok_or(ConfigError::NoConfigDir)?;

        let rpc_url = env::var("CLAM_RPC_URL").unwrap_or_else(|_| network.default_rpc_url().into());

        let facilitator_url =
            env::var("CLAM_FACILITATOR_URL").unwrap_or_else(|_| DEFAULT_FACILITATOR_URL.into());

        Ok(Self {
            keypair_path,
            network,
            rpc_url,
            facilitator_url,
            ledger_path,
        })
    }

    /// Ensures the parent directories for the keypair and ledger paths exist.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        if let Some(parent) = self.keypair_path.parent() {
            create_dir(parent)?;
        }
        if let Some(parent) = self.ledger_path.parent() {
            create_dir(parent)?;
        }
        Ok(())
    }
}

fn default_config_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".config").join("clam-self-custody"))
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn create_dir(p: &Path) -> std::io::Result<()> {
    if !p.as_os_str().is_empty() && !p.exists() {
        std::fs::create_dir_all(p)?;
    }
    Ok(())
}
