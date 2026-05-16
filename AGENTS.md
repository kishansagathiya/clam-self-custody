# Project Overview

Clam Self Custody let's agents pay for pay-per-use APIs using x402 via local wallets like Solana.

# Tech Stack

Rust, Solana

# Chains

Only Solana for now


## Vendored Dependencies

- `spl-token-2022` is currently vendored in `vendor/spl-token-2022` due to a bug in v10.0.0 regarding `spl-token-group-interface`. This should be removed once `x402-chain-solana` upgrades to v11+ (Tracked in issue #22).
