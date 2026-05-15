# Clam Self Custody

An MCP server that lets AI agents pay for **x402**-protected APIs from a
locally-held **Solana** wallet, in **USDC**, with every payment confirmed
interactively by the user via MCP elicitation.

- Agents never see the private key.
- No "approve once, run wild" — every payment pops a confirmation prompt
  through whichever MCP client the user is running.
- Devnet-by-default, so the first connection cannot accidentally spend real
  money.

## How it works

```
Agent (LLM) ──tools/call──▶  clam-mcp  ──HTTP──▶  paid API
                                │
                                ├─ 402 Payment Required
                                │
                                ├─ elicitation/create ──▶  user approves
                                │
                                ├─ sign USDC TransferChecked
                                ├─ retry with X-PAYMENT header
                                ▼
                            x402 facilitator settles on Solana
```

The server exposes exactly three tools:

| Tool             | Purpose                                                                  | Triggers a prompt? |
| ---------------- | ------------------------------------------------------------------------ | ------------------ |
| `wallet_info`    | Address, network, SOL + USDC balances                                    | No                 |
| `pay_and_fetch`  | Call a URL; if it returns 402, pay USDC after the user approves          | **Yes, every time**|
| `list_payments`  | Recent entries from the local JSONL payment ledger                       | No                 |

Deliberately omitted to preserve self-custody: `transfer`, `sign_transaction`,
`export_keypair`. The keypair only ever signs x402 payment payloads whose
`PaymentRequirements` the user just approved.

## Prerequisites

- Rust **1.88+** (this project pins `stable` in `rust-toolchain.toml`; runs on
  1.95 as of writing).
- A Solana keypair file (the standard `solana-keygen` JSON array format).
  Install the Solana CLI if you don't have it: <https://docs.solanalabs.com/cli/install>.

## Setup

```bash
# 1. Build
cargo build --release

# 2. Create the config directory and a fresh keypair
mkdir -p ~/.config/clam-self-custody
solana-keygen new --outfile ~/.config/clam-self-custody/keypair.json --no-bip39-passphrase

# 3. (Devnet) airdrop a little SOL for transaction fees and request test USDC
solana airdrop 1 --url https://api.devnet.solana.com \
    "$(solana-keygen pubkey ~/.config/clam-self-custody/keypair.json)"
# Then mint devnet USDC from Circle's faucet: https://faucet.circle.com/
```

## Configuration

All configuration is via environment variables — the agent **cannot** override
any of these at runtime.

| Variable                  | Default                                                | Purpose                                  |
| ------------------------- | ------------------------------------------------------ | ---------------------------------------- |
| `CLAM_KEYPAIR_PATH`       | `~/.config/clam-self-custody/keypair.json`             | Path to a `solana-keygen` JSON keypair   |
| `CLAM_NETWORK`            | `devnet`                                               | `devnet` or `mainnet-beta`               |
| `CLAM_RPC_URL`            | Public RPC for the chosen network                      | Custom Solana JSON-RPC endpoint          |
| `CLAM_FACILITATOR_URL`    | `https://api.cdp.coinbase.com/platform/v2/x402`        | x402 facilitator base URL                |
| `CLAM_FACILITATOR_API_KEY`| _(unset)_                                              | CDP API key (omit for keyless providers) |
| `CLAM_LEDGER_PATH`        | `~/.config/clam-self-custody/payments.jsonl`           | Append-only JSONL payment log            |
| `CLAM_LOG`                | `info`                                                 | `tracing_subscriber::EnvFilter` syntax   |

## Register with an MCP client

### Cursor

Add to `~/.cursor/mcp.json` (or per-workspace `.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "clam": {
      "command": "/absolute/path/to/clam-self-custody/target/release/clam-mcp",
      "env": {
        "CLAM_NETWORK": "devnet"
      }
    }
  }
}
```

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "clam": {
      "command": "/absolute/path/to/clam-self-custody/target/release/clam-mcp",
      "env": {
        "CLAM_NETWORK": "devnet"
      }
    }
  }
}
```

Once registered, restart the client and you should see `wallet_info`,
`pay_and_fetch`, and `list_payments` show up in the tool list.

> **Client must support MCP elicitation.** If your client doesn't,
> `pay_and_fetch` will fail closed: the elicitation call returns an error and
> the payment is never signed.

## Tool reference

### `wallet_info`

Read-only. No input.

```json
{
  "address": "9xQe…",
  "network": "devnet",
  "sol_balance": 0.987,
  "usdc_balance": 4.500000,
  "usdc_ata": "FwY1…",
  "usdc_mint": "4zMM…"
}
```

### `pay_and_fetch`

Input:

```json
{
  "url": "https://api.example.com/premium",
  "method": "GET",
  "headers": { "Accept": "application/json" },
  "body": null,
  "max_usdc": 0.05,
  "reason": "Fetching today's premium weather data"
}
```

- `max_usdc` is a **hard cap**: if the server demands more, the call fails
  before the user is ever prompted.
- `reason` is shown verbatim in the user's approval prompt; pass something
  meaningful so a human can decide.

Output:

```json
{
  "status": 200,
  "headers": { "content-type": "application/json", "x-payment-response": "…" },
  "body": "…response body…",
  "payment": {
    "tx_signature": "5kJ…",
    "amount_usdc": 0.001,
    "pay_to": "9xQe…",
    "network": "devnet"
  }
}
```

### `list_payments`

Input:

```json
{ "limit": 50, "since": "2026-05-01T00:00:00Z" }
```

Both fields are optional (defaults: `limit=100`, no time filter). Output is an
array of ledger entries, newest first.

## Repository layout

```
clam-self-custody/
├── Cargo.toml                 # workspace manifest + spl-token-2022 patch
├── rust-toolchain.toml        # pins stable Rust
├── crates/
│   ├── clam-core/             # wallet, x402 client, ledger (library)
│   └── clam-mcp/              # MCP stdio server (binary)
└── vendor/
    └── spl-token-2022/        # locally-patched copy; see Cargo.toml comment
```

## Why is `spl-token-2022` vendored?

`x402-chain-solana 1.4.6` transitively pulls `spl-token-2022 v10.0.0`, which
hits an API mismatch with its own peer dep `spl-token-group-interface 0.7.1+`
(`OptionalNonZeroPubkey` was replaced by `MaybeNull<Pubkey>` in the latter,
but v10's `processor.rs` still uses the old type and won't compile). Upstream
fixed this in v11, but we can't upgrade across that major boundary without
breaking `x402-chain-solana`'s pin on `^10`. The vendored copy under
`vendor/spl-token-2022/` is an exact mirror of v10.0.0 with a one-function
patch in `extension/token_group/processor.rs::check_update_authority` to
accept either type. The patch is applied via `[patch.crates-io]` in the
workspace `Cargo.toml`. Once `x402-chain-solana` releases a version that
pins v11+, the patch and vendor directory can be removed.

## GitHub issue automation

This repo includes a GitHub Actions workflow that runs when a new issue is
opened. The workflow starts a Cursor Cloud Agent, asks it to verify whether the
issue is real, and only starts a fixing run with PR creation enabled when the
issue is actionable.

To enable it:

1. Create a Cursor API key from the Cursor dashboard. Use a user key or service
   account that has access to this GitHub repo.
2. Add the key as a GitHub Actions repository secret named `CURSOR_API_KEY`.
3. Ensure the Cursor GitHub integration can create branches and pull requests
   for `kishansagathiya/clam-self-custody`.

The automation comments on the issue with either the created PR link or the
reason no PR was opened. It is defined in
`.github/workflows/issue-ai-fix.yml`, with the SDK runner in
`.github/automation/issue-ai-fix.mjs`.

## Security model

- The keypair lives on local disk under the user's chosen path. The MCP
  process is the only thing that loads it; the agent never receives the bytes
  through any tool.
- Every payment requires an explicit `accept` from the user via MCP
  elicitation. There is no per-domain allowlist, daily budget, or
  "small payments auto-approve" shortcut.
- `max_usdc` is a defensive ceiling: a buggy or hostile agent that somehow
  causes the server to respond `402` with an outrageous amount can't even ask
  the user — the call short-circuits.
- The ledger is append-only and lives alongside the keypair so audits stay
  local.

Out of scope (for now): passphrase-encrypted keypair files, daily spend
caps, multi-recipient `accepts` arbitration. See the design plan for
follow-ups.

## License

Apache-2.0.
