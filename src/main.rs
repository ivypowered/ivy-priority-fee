use rouille::{Response, router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::io::Read;

const LISTEN_URL: &str = "127.0.0.1:43278";
const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const JUPITER_AGGREGATOR_V6: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
const MAX_RESPONSE_LEN: u64 = 100_000_000;
const MAX_RETRIES: usize = 10;

fn main() {
    let rpc_url = env::var("RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());

    eprintln!("Starting ivy-priority-fee on http://{}", LISTEN_URL);
    eprintln!("RPC: {}", rpc_url);

    rouille::start_server(LISTEN_URL, move |request| {
        let rpc_url = rpc_url.clone();

        router!(request,
            (GET) (/) => {
                match get_reasonable_priority_fee(&rpc_url) {
                    Ok(fee) => Response::json(&json!({ "reasonablePriorityFee": fee })),
                    Err(err) => {
                        Response::from_data("application/json", json!({
                            "error": err.to_string()
                        }).to_string()).with_status_code(500)
                    }
                }
            },
            (GET) (/health) => {
                Response::text("ok")
            },
            _ => Response::empty_404()
        )
    });
}

fn get_reasonable_priority_fee(rpc_url: &str) -> Result<u64, Box<dyn std::error::Error>> {
    // 1) Fetch last 1,000 confirmed Jupiter transactions' signatures
    let signatures: Vec<String> = get_signatures_for_address(rpc_url, JUPITER_AGGREGATOR_V6, 1000)?;
    if signatures.is_empty() {
        return Ok(0);
    }

    // 2) Call getTransaction for those signatures, and compute per-tx priority fees
    let mut priority_fees: Vec<u64> = Vec::new();
    let mut priority_fee_error: Option<Box<dyn std::error::Error>> = None;
    for _ in 0..MAX_RETRIES {
        match get_priority_fees_for_signatures(rpc_url, &signatures) {
            Ok(v) => {
                priority_fees = v;
                break;
            }
            Err(e) => priority_fee_error = Some(e),
        }
    }
    if let Some(e) = priority_fee_error {
        return Err(e);
    }

    // 3) Take median, clamp at [0, 5_000_000]
    if priority_fees.is_empty() {
        return Ok(0);
    }
    priority_fees.sort_unstable();
    let median = priority_fees[priority_fees.len() / 2];
    Ok(median.min(5_000_000))
}

// --------------------------- JSON-RPC plumbing ---------------------------

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: serde_json::Value, // allow numeric or string
    method: &'static str,
    params: serde_json::Value,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct SingleResponse<T> {
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct BatchItem<T> {
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    error: Option<JsonRpcError>,
    id: serde_json::Value,
}

// --------------------------- getSignaturesForAddress ---------------------------

#[derive(Deserialize)]
struct SignatureInfo {
    signature: String,
    // other fields available but not required here
}

fn get_signatures_for_address(
    rpc_url: &str,
    address: &str,
    limit: usize,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let limit = limit.min(1000); // RPC max
    let req = JsonRpcRequest {
        jsonrpc: "2.0",
        id: json!(1),
        method: "getSignaturesForAddress",
        params: json!([
            address,
            {
                "commitment": "confirmed",
                "limit": limit
            }
        ]),
    };

    let resp = ureq::post(rpc_url).send_json(&req)?;
    if resp.status() != 200 {
        return Err(format!("got status {}: {}", resp.status(), resp.into_string()?).into());
    }
    let resp: SingleResponse<Vec<SignatureInfo>> = resp.into_json()?;

    if let Some(err) = resp.error {
        Err(format!(
            "getSignaturesForAddress error (code {}): {}",
            err.code, err.message
        ))?
    }

    let result = resp
        .result
        .ok_or("getSignaturesForAddress: missing result")?;

    Ok(result.into_iter().map(|s| s.signature).collect())
}

// --------------------------- getTransaction (batch) ---------------------------

#[derive(Deserialize, Debug)]
struct TransactionMeta {
    fee: u64,
    #[serde(rename = "computeUnitsConsumed")]
    compute_units_consumed: Option<u64>,
}

#[derive(Deserialize, Debug, Default)]
struct TransactionResult {
    meta: Option<TransactionMeta>,
}

fn get_priority_fees_for_signatures(
    rpc_url: &str,
    signatures: &[String],
) -> Result<Vec<u64>, Box<dyn std::error::Error>> {
    // Build a JSON-RPC batch
    let mut batch: Vec<JsonRpcRequest> = Vec::with_capacity(signatures.len());
    for (i, sig) in signatures.iter().enumerate() {
        batch.push(JsonRpcRequest {
            jsonrpc: "2.0",
            id: json!(i as u64),
            method: "getTransaction",
            params: json!([
                sig,
                {
                    "commitment": "confirmed",
                    "encoding": "json",
                    "maxSupportedTransactionVersion": 0
                }
            ]),
        });
    }

    // Send the batch
    let resp = ureq::post(rpc_url).send_json(&batch)?;
    if resp.status() != 200 {
        return Err(format!("got status {}: {}", resp.status(), resp.into_string()?).into());
    }
    let mut s = String::new();
    resp.into_reader()
        .take(MAX_RESPONSE_LEN)
        .read_to_string(&mut s)?;
    let responses: Vec<BatchItem<TransactionResult>> = serde_json::from_str(&s)?;
    if responses.len() == 0 && signatures.len() != 0 {
        return Err(format!("batch size too large for destination RPC, try again!").into());
    }

    // Extract per-transaction priority fee (in micro-lamports), assume 1 signature
    let mut out = Vec::with_capacity(responses.len());
    for item in responses {
        if let Some(err) = item.error {
            // Skip errored items (e.g., not found / too old)
            eprintln!(
                "getTransaction error (id {:?}, code {}): {}",
                item.id, err.code, err.message
            );
            continue;
        }

        let tr = match item.result {
            Some(r) => r,
            None => continue,
        };

        let meta = match tr.meta {
            Some(m) => m,
            None => continue,
        };

        let compute_units = meta.compute_units_consumed.unwrap_or(0) as i64;
        if compute_units <= 0 {
            continue;
        }

        // we're assuming 1 signature for simplicity here
        // priority_fee_micro_lamports = ((fee_lamports - (5000 * n_signatures)) * 1_000_000) / compute_units
        let fee_lamports = meta.fee as u128;
        let priority_fee = (((fee_lamports - 5000) * 1_000_000) / (compute_units as u128)) as u64;

        out.push(priority_fee);
    }

    Ok(out)
}
