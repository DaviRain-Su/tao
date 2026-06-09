//! `tao` — command-line wallet and operator tool for the Tao chain.
//!
//! M0 scaffold: CLI surface only. Keygen, balance, transfer, and faucet are
//! implemented in milestone **M6**.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tao", version, about = "Tao chain CLI wallet", long_about = None)]
struct Cli {
    /// RPC endpoint of a tao-node.
    #[arg(long, global = true, default_value = "http://127.0.0.1:8899")]
    rpc_url: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the RPC endpoint the CLI is configured to use.
    Info,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Info => {
            println!("tao-cli {}", tao_core::VERSION);
            println!("rpc-url: {}", cli.rpc_url);
            println!("(scaffold — wallet commands land in M6)");
        }
    }
    Ok(())
}
