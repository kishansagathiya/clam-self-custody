//! `clam-core`: wallet, x402 payment client, and payment ledger primitives
//! for the Clam Self Custody MCP server.

pub mod config;
pub mod ledger;
pub mod wallet;
pub mod x402_client;

pub use config::{Config, Network};
pub use ledger::{Ledger, LedgerEntry, PaymentStatus};
pub use wallet::{SharedKeypair, Wallet, WalletInfo};
pub use x402_client::{
    ApprovalDecision, ApprovalFn, ApprovalRequest, ClamX402Client, PayAndFetchOutcome,
    PayAndFetchRequest, PaymentReceipt,
};
