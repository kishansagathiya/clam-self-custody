//! MCP `ServerHandler` exposing three tools:
//!
//! - `wallet_info`: read-only balance lookup
//! - `pay_and_fetch`: x402-protected HTTP request, with mandatory
//!   per-call user approval via [`rmcp::Peer::elicit`]
//! - `list_payments`: read-only audit of the local payment ledger

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use clam_core::{
    ApprovalDecision, ApprovalFn, ApprovalRequest, ClamX402Client, Ledger, LedgerEntry, Network,
    PayAndFetchOutcome, PayAndFetchRequest, PaymentStatus, Wallet, WalletInfo,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    ErrorData as McpError, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// State shared across all tool invocations on this server. Cheap to clone.
#[derive(Clone)]
pub struct ClamServer {
    wallet: Wallet,
    ledger: Ledger,
    x402: ClamX402Client,
    // Used by the `#[tool_handler]`-generated `call_tool` impl via macro
    // expansion; the compiler can't see that usage.
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<ClamServer>,
}

impl ClamServer {
    pub fn new(wallet: Wallet, ledger: Ledger, x402: ClamX402Client) -> Self {
        Self {
            wallet,
            ledger,
            x402,
            tool_router: Self::tool_router(),
        }
    }
}

// ----- Tool input/output schemas -----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct WalletInfoParams {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PayAndFetchParams {
    /// URL to call. Must be a fully-qualified http(s) URL.
    #[schemars(description = "URL to call (http or https).")]
    pub url: String,
    /// HTTP method; defaults to GET.
    #[serde(default = "default_method")]
    #[schemars(description = "HTTP method (GET, POST, PUT, PATCH, DELETE). Defaults to GET.")]
    pub method: String,
    /// Optional HTTP headers to include on both the probe and the paid retry.
    #[serde(default)]
    #[schemars(description = "Additional HTTP headers to send with the request.")]
    pub headers: HashMap<String, String>,
    /// Optional request body (pre-serialized). Set Content-Type via headers.
    #[serde(default)]
    #[schemars(description = "Optional request body; the agent must pre-serialize JSON, etc.")]
    pub body: Option<String>,
    /// Hard cap on payable amount. If the server demands more, the call
    /// fails without prompting the user.
    #[serde(default)]
    #[schemars(
        description = "Hard upper bound in USDC. If the server demands more, the call fails without prompting."
    )]
    pub max_usdc: Option<f64>,
    /// Free-form justification shown to the user in the approval prompt.
    #[serde(default)]
    #[schemars(description = "Reason shown verbatim to the user in the approval prompt.")]
    pub reason: Option<String>,
}

fn default_method() -> String {
    "GET".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListPaymentsParams {
    /// Maximum number of entries to return (newest first). Default 100.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Inclusive lower-bound timestamp (RFC 3339).
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
}

// ----- Elicitation schema -----

/// The user's response when prompted to approve a payment. The MCP
/// elicitation spec requires property values to be primitives, so we
/// constrain `decision` to a string-enum.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PaymentDecision {
    #[schemars(description = "approve or reject the proposed USDC payment")]
    pub decision: Decision,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Approve,
    Reject,
}

rmcp::elicit_safe!(PaymentDecision);

// ----- Tool router impl -----

#[tool_router]
impl ClamServer {
    /// Read-only: returns the wallet's address, network, and SOL + USDC
    /// balances. No elicitation.
    #[tool(
        description = "Get the local Solana wallet's address, network, and SOL + USDC balances. Read-only."
    )]
    pub async fn wallet_info(
        &self,
        _params: Parameters<WalletInfoParams>,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let rpc = self.x402.rpc();
        let info: WalletInfo = self
            .wallet
            .info(&rpc)
            .await
            .map_err(|e| internal_error("wallet_info", e))?;
        json_result(&info)
    }

    /// Calls `url`. If the server responds 402 per x402, prompts the user
    /// to approve a USDC payment, then signs and retries automatically.
    #[tool(
        description = "Call an x402-protected URL. If the server responds 402, the user is prompted to approve a USDC-on-Solana payment; on approval the request is signed, retried, and the response is returned with a settlement receipt."
    )]
    pub async fn pay_and_fetch(
        &self,
        Parameters(params): Parameters<PayAndFetchParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let peer = ctx.peer.clone();
        let approval: ApprovalFn = Arc::new(move |req: ApprovalRequest| {
            let peer = peer.clone();
            let fut: Pin<Box<dyn Future<Output = ApprovalDecision> + Send>> =
                Box::pin(async move { elicit_approval(&peer, req).await });
            fut
        });

        let outcome: PayAndFetchOutcome = self
            .x402
            .pay_and_fetch(
                PayAndFetchRequest {
                    url: params.url,
                    method: params.method,
                    headers: params.headers,
                    body: params.body,
                    max_usdc: params.max_usdc,
                    reason: params.reason,
                },
                approval,
            )
            .await
            .map_err(|e| internal_error("pay_and_fetch", e))?;

        json_result(&outcome)
    }

    /// Read-only: returns recent payment ledger entries.
    #[tool(
        description = "List recent payment ledger entries (newest first). Read-only and never triggers a payment."
    )]
    pub async fn list_payments(
        &self,
        Parameters(params): Parameters<ListPaymentsParams>,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let entries: Vec<LedgerEntry> = self
            .ledger
            .list(params.limit, params.since)
            .await
            .map_err(|e| internal_error("list_payments", e))?;
        let view: Vec<LedgerEntryView> = entries.into_iter().map(LedgerEntryView::from).collect();
        json_result(&view)
    }
}

/// Calls `peer.elicit::<PaymentDecision>(message)` with a human-readable
/// prompt and maps the result to a [`ApprovalDecision`].
async fn elicit_approval(
    peer: &rmcp::service::Peer<RoleServer>,
    req: ApprovalRequest,
) -> ApprovalDecision {
    let reason = req
        .reason
        .as_deref()
        .map(|r| format!(" Reason: {r}"))
        .unwrap_or_default();
    let message = format!(
        "Approve x402 payment of {:.6} USDC ({}) to {} for {} {}?{}",
        req.amount_usdc,
        req.network.as_str(),
        req.pay_to,
        req.method,
        req.url,
        reason,
    );

    match peer.elicit::<PaymentDecision>(message).await {
        Ok(Some(PaymentDecision {
            decision: Decision::Approve,
        })) => ApprovalDecision::Approve,
        Ok(Some(PaymentDecision {
            decision: Decision::Reject,
        })) => ApprovalDecision::Reject,
        Ok(None) => ApprovalDecision::Cancel,
        Err(err) => {
            tracing::warn!(error = %err, "elicitation error; treating as cancel");
            ApprovalDecision::Cancel
        }
    }
}

#[derive(Debug, Serialize)]
struct LedgerEntryView {
    ts: DateTime<Utc>,
    url: String,
    method: String,
    amount_usdc: f64,
    pay_to: String,
    network: Network,
    tx_signature: Option<String>,
    status: PaymentStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl From<LedgerEntry> for LedgerEntryView {
    fn from(e: LedgerEntry) -> Self {
        Self {
            ts: e.ts,
            url: e.url,
            method: e.method,
            amount_usdc: e.amount_usdc,
            pay_to: e.pay_to,
            network: e.network,
            tx_signature: e.tx_signature,
            status: e.status,
            error: e.error,
        }
    }
}

fn json_result<T: Serialize>(v: &T) -> Result<rmcp::model::CallToolResult, McpError> {
    let value = serde_json::to_value(v).map_err(|e| internal_error("serialize", e))?;
    Ok(rmcp::model::CallToolResult::structured(value))
}

fn internal_error<E: std::fmt::Display>(scope: &'static str, err: E) -> McpError {
    ErrorData::internal_error(format!("{scope}: {err}"), None)
}

#[tool_handler]
impl ServerHandler for ClamServer {
    fn get_info(&self) -> ServerInfo {
        let mut implementation = Implementation::default();
        implementation.name = "clam-mcp".to_string();
        implementation.version = env!("CARGO_PKG_VERSION").to_string();

        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = implementation;
        info.instructions = Some(
            "Pay-per-use API gateway: call wallet_info to see your local Solana wallet, \
             pay_and_fetch to call any URL (the user is prompted to approve any USDC \
             payment), and list_payments to audit past spends."
                .to_string(),
        );
        info
    }
}
