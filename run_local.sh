#!/usr/bin/env bash
# Run MrOracle against the local validator.
# .env is loaded automatically via dotenvy (RUST_LOG, MAIN_POOL_ID, feed map paths).
# CLI args below cover what .env cannot: endpoint, payer, provider selection, switchboard config.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

PAYER_KEYPAIR="${PAYER_KEYPAIR:-../adrena-keypairs/localnet/payer.json}"
ENDPOINT="${ENDPOINT:-http://127.0.0.1:8899}"

cargo run --release -- \
  --endpoint "$ENDPOINT" \
  --payer-keypair "$PAYER_KEYPAIR" \
  --providers chaoslabs,switchboard \
  --switchboard-crossbar-url https://crossbar.switchboard.xyz \
  --switchboard-network mainnet \
  --switchboard-queue-pubkey A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w
