//! Solana-compatible JSON-RPC server.
//!
//! Implements the core method subset that `@solana/web3.js` and Phantom use to
//! read state and submit + confirm transactions. Account reads come straight
//! from the shared `AccountsDb`; `sendTransaction` enqueues into the mempool the
//! miner drains. Responses match Solana's `{context, value}` shapes.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{extract::State, routing::post, Json, Router};
use base64::Engine as _;
use serde_json::{json, Value};
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;

use crate::shared::Shared;

type RpcResult = Result<Value, (i64, String)>;

const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
const INTERNAL_ERROR: i64 = -32603;

/// Serve the RPC until `shutdown` is set.
pub async fn serve(
    shared: Arc<Shared>,
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let app = Router::new().route("/", post(handle)).with_state(shared);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "JSON-RPC listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            while !shutdown.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await?;
    Ok(())
}

async fn handle(State(shared): State<Arc<Shared>>, Json(body): Json<Value>) -> Json<Value> {
    if let Some(batch) = body.as_array() {
        Json(Value::Array(batch.iter().map(|r| handle_one(&shared, r)).collect()))
    } else {
        Json(handle_one(&shared, &body))
    }
}

fn handle_one(shared: &Shared, req: &Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    match dispatch(shared, method, &params) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    }
}

fn context(slot: u64) -> Value {
    json!({"slot": slot, "apiVersion": "2.0.0"})
}

fn param_str(params: &Value, i: usize) -> Result<String, (i64, String)> {
    params
        .get(i)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or((INVALID_PARAMS, format!("expected string param at {i}")))
}

fn parse_pubkey(s: &str) -> Result<Pubkey, (i64, String)> {
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|_| (INVALID_PARAMS, "invalid base58 pubkey".into()))?;
    let arr: [u8; 32] =
        bytes.as_slice().try_into().map_err(|_| (INVALID_PARAMS, "pubkey must be 32 bytes".into()))?;
    Ok(Pubkey::from(arr))
}

fn dispatch(shared: &Shared, method: &str, params: &Value) -> RpcResult {
    let slot = shared.slot();
    match method {
        "getHealth" => Ok(json!("ok")),
        "getVersion" => Ok(json!({"solana-core": "2.0.0-tao", "feature-set": 0u32})),
        "getSlot" => Ok(json!(slot)),
        "getBlockHeight" => Ok(json!(slot)),
        "getGenesisHash" => Ok(json!(bs58::encode(shared.genesis_hash).into_string())),
        "getEpochInfo" => Ok(json!({
            "absoluteSlot": slot, "blockHeight": slot, "epoch": 0,
            "slotIndex": slot, "slotsInEpoch": 432_000u64, "transactionCount": slot
        })),

        "getLatestBlockhash" => Ok(json!({
            "context": context(slot),
            "value": {
                "blockhash": bs58::encode(shared.latest_blockhash()).into_string(),
                "lastValidBlockHeight": slot + 150
            }
        })),

        "getMinimumBalanceForRentExemption" => {
            let space = params.get(0).and_then(Value::as_u64).unwrap_or(0) as usize;
            Ok(json!(solana_rent::Rent::default().minimum_balance(space)))
        }

        "getFeeForMessage" => Ok(json!({"context": context(slot), "value": 5_000u64})),

        "getBalance" => {
            let pubkey = parse_pubkey(&param_str(params, 0)?)?;
            let lamports = shared
                .accounts
                .get(&pubkey)
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
                .map(|a| a.lamports())
                .unwrap_or(0);
            Ok(json!({"context": context(slot), "value": lamports}))
        }

        "getAccountInfo" => {
            let pubkey = parse_pubkey(&param_str(params, 0)?)?;
            let value = match shared
                .accounts
                .get(&pubkey)
                .map_err(|e| (INTERNAL_ERROR, e.to_string()))?
            {
                Some(acct) => {
                    let data_b64 = base64::engine::general_purpose::STANDARD.encode(acct.data());
                    json!({
                        "lamports": acct.lamports(),
                        "owner": bs58::encode(acct.owner().to_bytes()).into_string(),
                        "data": [data_b64, "base64"],
                        "executable": acct.executable(),
                        "rentEpoch": acct.rent_epoch(),
                        "space": acct.data().len(),
                    })
                }
                None => Value::Null,
            };
            Ok(json!({"context": context(slot), "value": value}))
        }

        "sendTransaction" => {
            let encoded = param_str(params, 0)?;
            let encoding = params
                .get(1)
                .and_then(|c| c.get("encoding"))
                .and_then(Value::as_str)
                .unwrap_or("base58");
            let raw = match encoding {
                "base64" => base64::engine::general_purpose::STANDARD
                    .decode(encoded.as_bytes())
                    .map_err(|e| (INVALID_PARAMS, format!("base64: {e}")))?,
                _ => bs58::decode(encoded)
                    .into_vec()
                    .map_err(|e| (INVALID_PARAMS, format!("base58: {e}")))?,
            };
            let tx: Transaction = bincode::deserialize(&raw)
                .map_err(|e| (INVALID_PARAMS, format!("decode transaction: {e}")))?;
            let sig = tx
                .signatures
                .first()
                .ok_or((INVALID_PARAMS, "transaction has no signature".into()))?;
            let sig_b58 = bs58::encode(sig.as_ref()).into_string();
            shared.submit(tx);
            Ok(json!(sig_b58))
        }

        "getSignatureStatuses" => {
            let sigs = params
                .get(0)
                .and_then(Value::as_array)
                .ok_or((INVALID_PARAMS, "expected array of signatures".into()))?;
            let values: Vec<Value> = sigs
                .iter()
                .map(|s| {
                    let Some(s) = s.as_str() else { return Value::Null };
                    let Ok(bytes) = bs58::decode(s).into_vec() else { return Value::Null };
                    let Ok(arr) = <[u8; 64]>::try_from(bytes.as_slice()) else { return Value::Null };
                    match shared.signature_status(&arr) {
                        Some(result) => json!({
                            "slot": slot,
                            "confirmations": Value::Null,
                            "err": result.map(|e| json!(e)).unwrap_or(Value::Null),
                            "confirmationStatus": "finalized",
                        }),
                        None => Value::Null,
                    }
                })
                .collect();
            Ok(json!({"context": context(slot), "value": values}))
        }

        other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
    }
}
