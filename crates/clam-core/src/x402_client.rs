//! Policy-gated x402 client.
//!
//! Wraps [`x402_reqwest::X402Client`] with:
//! - a [`V2SolanaExactClient`] scheme registration scoped to USDC on the
//!   configured Solana network,
//! - a [`PaymentSelector`] that filters incoming `PaymentCandidate`s to that
//!   USDC scope, applies an absolute USDC cap, and forwards an approval
//!   request to a user-supplied async callback,
//! - ledger writes for every settled payment.
//!
//! The approval callback is async at the public boundary but runs inside a
//! synchronous `PaymentSelector::select` call by way of
//! [`tokio::task::block_in_place`]; this requires a multi-thread tokio
//! runtime (configured in `clam-mcp`'s `main`).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use solana_client::nonblocking::rpc_client::RpcClient;
use thiserror::Error;
use x402_chain_solana::v2_solana_exact::client::V2SolanaExactClient;
use x402_reqwest::{ReqwestWithPayments, ReqwestWithPaymentsBuild, X402Client};
use x402_types::scheme::client::{PaymentCandidate, PaymentSelector};

use crate::config::{Config, Network};
use crate::ledger::{Ledger, LedgerEntry, PaymentStatus};
use crate::wallet::{SharedKeypair, USDC_DECIMALS, Wallet};

/// Header set by an x402 resource server on a 200 response to communicate
/// settlement metadata (base64-encoded `SettleResponse` per the x402 spec).
pub const X_PAYMENT_RESPONSE_HEADER: &str = "x-payment-response";

#[derive(Debug, Error)]
pub enum X402ClientError {
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("unsupported HTTP method: {0}")]
    UnsupportedMethod(String),
    #[error("invalid header name '{name}': {source}")]
    InvalidHeaderName {
        name: String,
        #[source]
        source: reqwest::header::InvalidHeaderName,
    },
    #[error("invalid header value for '{name}'")]
    InvalidHeaderValue { name: String },
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP middleware error: {0}")]
    HttpMiddleware(#[from] reqwest_middleware::Error),
    #[error("payment was required but no USDC-on-Solana option matched (max_usdc={max_usdc:?})")]
    NoMatchingPayment { max_usdc: Option<f64> },
    #[error(
        "payment was required but no USDC-on-Solana candidates matched (max_usdc={max_usdc:?})"
    )]
    NoMatchingCandidates { max_usdc: Option<f64> },
    #[error("payment amount {amount_usdc} USDC exceeds max_usdc cap {max_usdc}")]
    ExceedsMaxUsdc { amount_usdc: f64, max_usdc: f64 },
    #[error("user declined the payment")]
    UserDeclined,
    #[error("user cancelled the payment prompt")]
    UserCancelled,
    #[error("ledger error: {0}")]
    Ledger(#[from] crate::ledger::LedgerError),
}

/// Approval request passed to the user via the consumer-supplied callback.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApprovalRequest {
    pub url: String,
    pub method: String,
    pub amount_usdc: f64,
    pub pay_to: String,
    pub asset_mint: String,
    pub network: Network,
    pub reason: Option<String>,
}

/// User's response to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Reject,
    Cancel,
}

/// Boxed async approval callback. Receives an [`ApprovalRequest`] describing
/// the proposed payment and returns the user's decision.
pub type ApprovalFn = Arc<
    dyn Fn(ApprovalRequest) -> Pin<Box<dyn Future<Output = ApprovalDecision> + Send>> + Send + Sync,
>;

/// Input to [`ClamX402Client::pay_and_fetch`].
#[derive(Debug, Clone)]
pub struct PayAndFetchRequest {
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    pub max_usdc: Option<f64>,
    pub reason: Option<String>,
}

/// Successful settlement metadata for a paid request.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PaymentReceipt {
    pub tx_signature: Option<String>,
    pub amount_usdc: f64,
    pub pay_to: String,
    pub network: Network,
}

/// Result of [`ClamX402Client::pay_and_fetch`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct PayAndFetchOutcome {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
    pub payment: Option<PaymentReceipt>,
}

/// The policy-gated x402 client. Cheap to clone.
#[derive(Clone)]
pub struct ClamX402Client {
    wallet: Wallet,
    config: Config,
    ledger: Ledger,
    rpc: Arc<RpcClient>,
}

impl ClamX402Client {
    pub fn new(wallet: Wallet, config: Config, ledger: Ledger) -> Self {
        let rpc = Arc::new(RpcClient::new(config.rpc_url.clone()));
        Self {
            wallet,
            config,
            ledger,
            rpc,
        }
    }

    pub fn rpc(&self) -> Arc<RpcClient> {
        Arc::clone(&self.rpc)
    }

    /// Performs the x402 dance for a single URL:
    /// 1. Build a reqwest client wrapped with `X402Client`, registering a
    ///    USDC-on-Solana scheme client and the user-approval selector.
    /// 2. Send the request. If the resource returns 402, the middleware will
    ///    call our selector. The selector filters to USDC-on-Solana within
    ///    `max_usdc`, asks the user for approval via `approval`, and (if
    ///    approved) signs the payment.
    /// 3. On 2xx, the response's `X-PAYMENT-RESPONSE` header (if any) is
    ///    parsed for a settlement tx signature and recorded in the ledger.
    pub async fn pay_and_fetch(
        &self,
        req: PayAndFetchRequest,
        approval: ApprovalFn,
    ) -> Result<PayAndFetchOutcome, X402ClientError> {
        let url = reqwest::Url::parse(&req.url)
            .map_err(|_| X402ClientError::InvalidUrl(req.url.clone()))?;
        let method = parse_method(&req.method)?;
        let header_map = build_header_map(&req.headers)?;

        let last_selection: Arc<Mutex<Option<SelectedPayment>>> = Arc::new(Mutex::new(None));
        let selection_failure: Arc<Mutex<Option<PaymentSelectionFailure>>> =
            Arc::new(Mutex::new(None));

        // V2SolanaExactClient's trait bounds require both signer and RPC client
        // to be `Clone + Send + Sync + 'static`. We satisfy this with `Arc`
        // wrappers: `SharedKeypair` (Arc<Keypair>, impls Signer + Clone) and
        // `Arc<RpcClient>` (impls AsRef<RpcClient>, hence `RpcClientLike`).
        let scheme_signer = SharedKeypair::new(self.wallet.keypair());
        let scheme_client = V2SolanaExactClient::new(scheme_signer, Arc::clone(&self.rpc));

        let selector = InteractiveSelector {
            usdc_mint: self.config.network.usdc_mint().to_string(),
            network_caip2: self.config.network.caip2().to_string(),
            network: self.config.network,
            max_usdc: req.max_usdc,
            method: req.method.to_uppercase(),
            url: req.url.clone(),
            reason: req.reason.clone(),
            approval: Arc::clone(&approval),
            last_selection: Arc::clone(&last_selection),
            selection_failure: Arc::clone(&selection_failure),
        };

        let x402_client = X402Client::new()
            .register(scheme_client)
            .with_selector(selector);

        let http = reqwest::Client::new().with_payments(x402_client).build();

        let mut builder = http
            .request(method.clone(), url.clone())
            .headers(header_map);
        if let Some(body) = req.body.clone() {
            builder = builder.body(body);
        }

        let response = builder.send().await?;
        let status = response.status();
        let resp_headers = serialize_headers(response.headers());
        let tx_signature = response
            .headers()
            .get(X_PAYMENT_RESPONSE_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = response.text().await.unwrap_or_default();

        if status == StatusCode::PAYMENT_REQUIRED {
            let failure = {
                let guard = selection_failure
                    .lock()
                    .expect("selector failure mutex poisoned");
                guard.clone()
            };
            return Err(payment_required_error(failure, req.max_usdc));
        }

        let payment_receipt = {
            let guard = last_selection.lock().expect("selector mutex poisoned");
            guard.clone()
        }
        .map(|sel| PaymentReceipt {
            tx_signature: tx_signature.clone(),
            amount_usdc: sel.amount_usdc,
            pay_to: sel.pay_to,
            network: sel.network,
        });

        if let Some(ref receipt) = payment_receipt {
            let entry = LedgerEntry {
                ts: Utc::now(),
                url: req.url.clone(),
                method: req.method.to_uppercase(),
                amount_usdc: receipt.amount_usdc,
                pay_to: receipt.pay_to.clone(),
                network: receipt.network,
                tx_signature: receipt.tx_signature.clone(),
                status: if status.is_success() {
                    PaymentStatus::Settled
                } else {
                    PaymentStatus::Failed
                },
                error: if status.is_success() {
                    None
                } else {
                    Some(format!("upstream status {status}"))
                },
            };
            self.ledger.append(&entry).await?;
        }

        Ok(PayAndFetchOutcome {
            status: status.as_u16(),
            headers: resp_headers,
            body,
            payment: payment_receipt,
        })
    }
}

#[derive(Debug, Clone)]
struct SelectedPayment {
    amount_usdc: f64,
    pay_to: String,
    network: Network,
}

#[derive(Debug, Clone)]
enum PaymentSelectionFailure {
    NoMatchingCandidates,
    ExceedsMaxUsdc { amount_usdc: f64, max_usdc: f64 },
    UserDeclined,
    UserCancelled,
}

/// The custom [`PaymentSelector`] that gates payments behind a user-supplied
/// async approval callback.
struct InteractiveSelector {
    usdc_mint: String,
    network_caip2: String,
    network: Network,
    max_usdc: Option<f64>,
    method: String,
    url: String,
    reason: Option<String>,
    approval: ApprovalFn,
    last_selection: Arc<Mutex<Option<SelectedPayment>>>,
    selection_failure: Arc<Mutex<Option<PaymentSelectionFailure>>>,
}

impl PaymentSelector for InteractiveSelector {
    fn select<'a>(&self, candidates: &'a [PaymentCandidate]) -> Option<&'a PaymentCandidate> {
        *self
            .selection_failure
            .lock()
            .expect("selector failure mutex poisoned") = None;

        let matching: Vec<&PaymentCandidate> = candidates
            .iter()
            .filter(|c| {
                c.scheme == "exact"
                    && c.chain_id.to_string() == self.network_caip2
                    && c.asset.eq_ignore_ascii_case(&self.usdc_mint)
            })
            .collect();

        if matching.is_empty() {
            *self
                .selection_failure
                .lock()
                .expect("selector failure mutex poisoned") =
                Some(PaymentSelectionFailure::NoMatchingCandidates);
            return None;
        }

        let chosen = matching.into_iter().min_by(|a, b| {
            a.amount
                .to_string()
                .len()
                .cmp(&b.amount.to_string().len())
                .then_with(|| a.amount.to_string().cmp(&b.amount.to_string()))
        })?;

        let amount_usdc = base_units_to_usdc(&chosen.amount.to_string());
        if let Some(max) = self.max_usdc {
            if amount_usdc > max {
                tracing::warn!(amount_usdc, max, "payment exceeds max_usdc; refusing");
                *self
                    .selection_failure
                    .lock()
                    .expect("selector failure mutex poisoned") =
                    Some(PaymentSelectionFailure::ExceedsMaxUsdc {
                        amount_usdc,
                        max_usdc: max,
                    });
                return None;
            }
        }

        let req = ApprovalRequest {
            url: self.url.clone(),
            method: self.method.clone(),
            amount_usdc,
            pay_to: chosen.pay_to.clone(),
            asset_mint: chosen.asset.clone(),
            network: self.network,
            reason: self.reason.clone(),
        };

        let approval = Arc::clone(&self.approval);
        let decision = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move { (approval)(req).await })
        });

        match decision {
            ApprovalDecision::Approve => {
                *self.last_selection.lock().expect("selector mutex poisoned") =
                    Some(SelectedPayment {
                        amount_usdc,
                        pay_to: chosen.pay_to.clone(),
                        network: self.network,
                    });
                *self
                    .selection_failure
                    .lock()
                    .expect("selector failure mutex poisoned") = None;
                Some(chosen)
            }
            ApprovalDecision::Reject => {
                *self
                    .selection_failure
                    .lock()
                    .expect("selector failure mutex poisoned") =
                    Some(PaymentSelectionFailure::UserDeclined);
                None
            }
            ApprovalDecision::Cancel => {
                *self
                    .selection_failure
                    .lock()
                    .expect("selector failure mutex poisoned") =
                    Some(PaymentSelectionFailure::UserCancelled);
                None
            }
        }
    }
}

fn payment_required_error(
    failure: Option<PaymentSelectionFailure>,
    max_usdc: Option<f64>,
) -> X402ClientError {
    match failure {
        Some(PaymentSelectionFailure::ExceedsMaxUsdc {
            amount_usdc,
            max_usdc,
        }) => X402ClientError::ExceedsMaxUsdc {
            amount_usdc,
            max_usdc,
        },
        Some(PaymentSelectionFailure::UserDeclined) => X402ClientError::UserDeclined,
        Some(PaymentSelectionFailure::UserCancelled) => X402ClientError::UserCancelled,
        Some(PaymentSelectionFailure::NoMatchingCandidates) | None => {
            X402ClientError::NoMatchingCandidates { max_usdc }
        }
    }
}

fn parse_method(s: &str) -> Result<Method, X402ClientError> {
    let upper = s.to_uppercase();
    match upper.as_str() {
        "GET" => Ok(Method::GET),
        "POST" => Ok(Method::POST),
        "PUT" => Ok(Method::PUT),
        "PATCH" => Ok(Method::PATCH),
        "DELETE" => Ok(Method::DELETE),
        "HEAD" => Ok(Method::HEAD),
        "OPTIONS" => Ok(Method::OPTIONS),
        _ => Err(X402ClientError::UnsupportedMethod(s.into())),
    }
}

fn build_header_map(input: &HashMap<String, String>) -> Result<HeaderMap, X402ClientError> {
    let mut headers = HeaderMap::new();
    for (k, v) in input {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|source| {
            X402ClientError::InvalidHeaderName {
                name: k.clone(),
                source,
            }
        })?;
        let value = HeaderValue::from_str(v)
            .map_err(|_| X402ClientError::InvalidHeaderValue { name: k.clone() })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn serialize_headers(map: &HeaderMap) -> HashMap<String, String> {
    map.iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.to_string(), s.to_string())))
        .collect()
}

/// Converts a USDC base-unit amount (decimal string) into a UI-friendly f64.
/// USDC has 6 decimals; we accept the amount as a string because the
/// upstream type is an arbitrary-precision unsigned integer.
fn base_units_to_usdc(raw: &str) -> f64 {
    let raw = raw.trim();
    let n: u128 = raw.parse().unwrap_or(0);
    let divisor = 10u128.pow(USDC_DECIMALS as u32) as f64;
    n as f64 / divisor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_required_error_preserves_selector_reason() {
        assert!(matches!(
            payment_required_error(Some(PaymentSelectionFailure::UserDeclined), Some(1.0)),
            X402ClientError::UserDeclined
        ));
        assert!(matches!(
            payment_required_error(Some(PaymentSelectionFailure::UserCancelled), Some(1.0)),
            X402ClientError::UserCancelled
        ));
        assert!(matches!(
            payment_required_error(
                Some(PaymentSelectionFailure::ExceedsMaxUsdc {
                    amount_usdc: 2.0,
                    max_usdc: 1.0,
                }),
                Some(1.0),
            ),
            X402ClientError::ExceedsMaxUsdc {
                amount_usdc: 2.0,
                max_usdc: 1.0,
            }
        ));
        assert!(matches!(
            payment_required_error(
                Some(PaymentSelectionFailure::NoMatchingCandidates),
                Some(1.0)
            ),
            X402ClientError::NoMatchingCandidates {
                max_usdc: Some(1.0)
            }
        ));
    }
}
