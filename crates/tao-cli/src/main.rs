//! `tao` — command-line wallet and operator tool for the Tao chain.
//!
//! Talks to a node's Solana-compatible JSON-RPC: generate keys, check balances,
//! request faucet airdrops, and send transfers (built + signed locally).

use std::path::PathBuf;
use std::str::FromStr;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use base64::Engine as _;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;

const DEFAULT_RPC: &str = "http://127.0.0.1:8899";

#[derive(Parser)]
#[command(name = "tao", version, about = "Tao chain CLI wallet", long_about = None)]
struct Cli {
    /// RPC endpoint.
    #[arg(long, global = true, default_value = DEFAULT_RPC)]
    rpc_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a new keypair and write it to a file (Solana JSON format).
    Keygen {
        #[arg(short, long, default_value = "tao-keypair.json")]
        outfile: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Print the public key of a keypair file.
    Address {
        #[arg(short, long)]
        keypair: PathBuf,
    },
    /// Show an account's balance (lamports).
    Balance { pubkey: String },
    /// Request a faucet airdrop of `lamports` to `pubkey`.
    Airdrop { pubkey: String, lamports: u64 },
    /// Transfer `lamports` from a keypair to a recipient.
    Transfer {
        #[arg(short, long)]
        keypair: PathBuf,
        to: String,
        lamports: u64,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rpc = &cli.rpc_url;
    match cli.command {
        Command::Keygen { outfile, force } => keygen(&outfile, force),
        Command::Address { keypair } => {
            let kp = load_keypair(&keypair)?;
            println!("{}", kp.pubkey());
            Ok(())
        }
        Command::Balance { pubkey } => {
            let v = rpc_call(rpc, "getBalance", json!([pubkey]))?;
            let lamports = v["value"].as_u64().unwrap_or(0);
            println!("{lamports} lamports ({:.9} TAO)", lamports as f64 / 1e9);
            Ok(())
        }
        Command::Airdrop { pubkey, lamports } => {
            let sig = rpc_call(rpc, "requestAirdrop", json!([pubkey, lamports]))?;
            let sig = sig.as_str().ok_or_else(|| anyhow!("bad airdrop signature"))?;
            println!("airdrop signature: {sig}");
            confirm(rpc, sig)?;
            println!("confirmed");
            Ok(())
        }
        Command::Transfer { keypair, to, lamports } => transfer(rpc, &keypair, &to, lamports),
    }
}

fn keygen(outfile: &PathBuf, force: bool) -> anyhow::Result<()> {
    if outfile.exists() && !force {
        bail!("{} already exists (use --force to overwrite)", outfile.display());
    }
    let kp = Keypair::new();
    let bytes = kp.to_bytes().to_vec();
    std::fs::write(outfile, serde_json::to_string(&bytes)?)?;
    println!("wrote {}", outfile.display());
    println!("pubkey: {}", kp.pubkey());
    Ok(())
}

fn load_keypair(path: &PathBuf) -> anyhow::Result<Keypair> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let bytes: Vec<u8> = serde_json::from_str(&raw)?;
    Keypair::try_from(bytes.as_slice()).map_err(|e| anyhow!("invalid keypair: {e}"))
}

fn transfer(rpc: &str, keypair: &PathBuf, to: &str, lamports: u64) -> anyhow::Result<()> {
    let from = load_keypair(keypair)?;
    let to = Pubkey::from_str(to).map_err(|e| anyhow!("bad recipient: {e}"))?;

    let bh = rpc_call(rpc, "getLatestBlockhash", json!([]))?;
    let bh_str = bh["value"]["blockhash"].as_str().ok_or_else(|| anyhow!("no blockhash"))?;
    let bh_bytes: [u8; 32] = bs58::decode(bh_str)
        .into_vec()?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("blockhash not 32 bytes"))?;
    let blockhash = Hash::new_from_array(bh_bytes);

    let ix = solana_system_interface::instruction::transfer(&from.pubkey(), &to, lamports);
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&from.pubkey()), &[&from], blockhash);
    let raw = bincode::serialize(&tx)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);

    let sig = rpc_call(rpc, "sendTransaction", json!([b64, {"encoding": "base64"}]))?;
    let sig = sig.as_str().ok_or_else(|| anyhow!("bad signature"))?;
    println!("signature: {sig}");
    confirm(rpc, sig)?;
    println!("confirmed: transferred {lamports} lamports to {to}");
    Ok(())
}

/// Poll `getSignatureStatuses` until the signature is confirmed (or times out).
fn confirm(rpc: &str, sig: &str) -> anyhow::Result<()> {
    for _ in 0..100 {
        let r = rpc_call(rpc, "getSignatureStatuses", json!([[sig]]))?;
        if let Some(status) = r["value"].get(0) {
            if !status.is_null() {
                if let Some(err) = status.get("err") {
                    if !err.is_null() {
                        bail!("transaction failed: {err}");
                    }
                }
                return Ok(());
            }
        }
        sleep(Duration::from_millis(200));
    }
    bail!("transaction not confirmed in time")
}

fn rpc_call(url: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let resp: Value = ureq::post(url)
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| anyhow!("rpc request failed: {e}"))?
        .into_json()?;
    if let Some(err) = resp.get("error") {
        bail!("rpc error: {err}");
    }
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}
