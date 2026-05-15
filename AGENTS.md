# Project Overview

Clam Self Custody let's agents pay for pay-per-use APIs using x402 via local wallets like Solana.

# Tech Stack

Rust, Solana

# Chains

Only Solana for now

# Vendored Dependencies

## spl-token-2022

**Location:** `vendor/spl-token-2022`  
**Reason:** Patched to fix compilation issue in v10.0.0 against `spl-token-group-interface ^0.7.1`  
**Removal plan:** Remove when `x402-chain-solana` upgrades to `spl-token-2022 >= v11`  
**Tracking:** Issue #22
