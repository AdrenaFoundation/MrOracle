#!/usr/bin/env bash
# Run MrOracle against the local validator.
# .env is loaded automatically via dotenvy (RUST_LOG, MAIN_POOL_ID, feed map paths).
# CLI args below cover what .env cannot: endpoint, payer, provider selection, DB config.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

PAYER_KEYPAIR="${PAYER_KEYPAIR:-$HOME/.config/solana/localnet.json}"
ENDPOINT="${ENDPOINT:-http://127.0.0.1:8899}"
DB_STRING="${DB_STRING:-postgresql://adrena_local:adrena_local_pw@localhost/adrena_tx_local?sslmode=require}"

cargo run --release -- \
  --endpoint "$ENDPOINT" \
  --payer-keypair "$PAYER_KEYPAIR" \
  --providers chaoslabs,autonom,switchboard \
  --db-string "$DB_STRING" \
  --combined-cert combined.pem
