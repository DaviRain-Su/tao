#!/usr/bin/env bash
# Launch a single-node Tao devnet with mining, RPC, and a funded faucet.
#
# Usage:  ./scripts/devnet.sh [DATA_DIR]
# Then, in another terminal, use the `tao` CLI against http://127.0.0.1:8899:
#   tao keygen -o wallet.json
#   tao airdrop <PUBKEY> 2000000000
#   tao balance <PUBKEY>
#   tao transfer -k wallet.json <RECIPIENT> 500000000
set -euo pipefail

DATA="${1:-.tao-devnet}"
NODE="${TAO_NODE:-./target/debug/tao-node}"
TAO="${TAO_CLI:-./target/debug/tao}"
mkdir -p "$DATA"

# Faucet + miner keypairs (generated once, via the tao CLI — no external tools).
[ -f "$DATA/faucet.json" ] || "$TAO" keygen -o "$DATA/faucet.json" >/dev/null
[ -f "$DATA/miner.json" ]  || "$TAO" keygen -o "$DATA/miner.json"  >/dev/null
FAUCET="$("$TAO" address -k "$DATA/faucet.json")"
MINER="$("$TAO" address -k "$DATA/miner.json")"

# Genesis funds the faucet (1,000,000 TAO).
cat > "$DATA/genesis.toml" <<EOF
network = "tao-devnet"
creation_time = 1750000000

[[allocations]]
address = "$FAUCET"
lamports = 1000000000000000

[pow]
target_block_time_secs = 1
lwma_window = 30
initial_target = "00000fffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"

[reward]
initial_lamports = 1000000000
halving_interval = 2100000
EOF

echo "Tao devnet"
echo "  data dir : $DATA"
echo "  faucet   : $FAUCET"
echo "  miner    : $MINER"
echo "  RPC      : http://127.0.0.1:8899"
echo "  P2P      : 127.0.0.1:9001"
echo
exec "$NODE" run --mine --miner "$MINER" --data-dir "$DATA" \
  --listen 127.0.0.1:9001 --rpc --rpc-port 8899 \
  --faucet-keypair "$DATA/faucet.json"
