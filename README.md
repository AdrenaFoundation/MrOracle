# MrOracle

OpenSource Rust keeper handling oracle and ALP price updates on-chain for the Adrena protocol.

MrOracle reads oracle price batches from the adrena-data PostgreSQL database and submits coordinated `update_pool_aum` transactions to the Solana blockchain.

## Table of Contents

- [Architecture](#architecture)
- [Prerequisites](#prerequisites)
- [Build](#build)
- [Configuration](#configuration)
- [Provider Mode](#provider-mode)
- [Multi-Pool Mode](#multi-pool-mode)
- [Feed Maps](#feed-maps)
- [Localnet vs Devnet Deployment](#localnet-vs-devnet-deployment)
- [CLI Reference](#cli-reference)
- [Environment Variables](#environment-variables)
- [Troubleshooting](#troubleshooting)

---

## Architecture

```
adrena-data DB (oracle_batches)
        |
        v
   +-----------+        +-------------+
   | ChaosLabs |        | Switchboard |
   | (DB batch)|        | (crossbar)  |
   +-----+-----+        +------+------+
         |                      |
   +-----+-----+               |
   |  Autonom   |               |
   | (DB batch) |               |
   +-----+------+               |
         |                      |
         +----------+-----------+
                    |
          [Coordinated Payload]
                    |
                    v
         update_pool_aum TX
                    |
                    v
            Solana Blockchain
```

- **ChaosLabs & Autonom**: DB-backed signed batch flow. Reads from `oracle_batches` table.
- **Switchboard**: Native quote flow via Crossbar API. No DB dependency.

---

## Prerequisites

- Rust toolchain (stable)
- PostgreSQL running with adrena-data tables populated
- Solana CLI tools
- `adrena-abi` dependency (local path or git)
- Funded payer keypair

---

## Build

```bash
cargo build                # debug
cargo build --release      # production
```

The binary is at `target/release/mroracle`.

---

## Configuration

MrOracle accepts configuration via CLI flags, environment variables, or `.env` file. CLI flags take precedence.

### Required

| Flag | Env Var | Description |
|------|---------|-------------|
| `--endpoint` | — | Solana RPC URL |
| `--payer-keypair` | — | Path to funded payer keypair JSON |

### Required for DB-backed providers (ChaosLabs, Autonom)

| Flag | Env Var | Description |
|------|---------|-------------|
| `--db-string` | — | PostgreSQL connection string |
| `--combined-cert` | — | SSL certificate for DB connection |

### Optional

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--commitment` | — | `finalized` | Solana commitment level |
| `--main-pool-id` | `MAIN_POOL_ID` | from adrena-abi | Override pool PDA (single-pool mode) |
| `--providers` | — | all three | Comma-separated provider list (single-pool mode) |
| `--pools-config` | — | — | Path to multi-pool config JSON (see [Multi-Pool Mode](#multi-pool-mode)) |
| `--dry-run` | `DRY_RUN` | `false` | Build payloads but skip on-chain sends |
| `--update-oracle-cu-limit` | — | `1400000` | Compute limit for update tx |

---

## Provider Mode

Default behavior starts all 3 providers in parallel, buffers each provider's latest update, then sends a single coordinated `update_pool_aum` tx only when all selected providers are ready.

### Selection

- `--providers chaoslabs` — single provider
- `--providers chaoslabs,autonom` — two providers
- `--providers chaoslabs,autonom,switchboard` — all three (default)
- `--only-chaoslabs` / `--only-autonom` / `--only-switchboard` — shorthand

### Payload Shape

| Providers | Payload Type |
|-----------|-------------|
| ChaosLabs only OR Autonom only | `BatchPrices` |
| ChaosLabs + Switchboard OR Autonom + Switchboard | `BatchPrices + SwitchboardPrices` |
| ChaosLabs + Autonom | `MultiBatchPrices` |
| All three | `MultiBatchPrices + SwitchboardPrices` |

### Startup Behavior

- **Explicit selection** (`--providers` or `--only-*`): any selected provider failure is a hard fail (process exits)
- **Default mode** (no selection): soft-fail per provider — misconfigured providers are disabled, healthy ones continue
- If all providers fail startup, keeper stays alive in passive mode and logs errors

---

## Multi-Pool Mode

When multiple pools exist with different oracle provider requirements, use `--pools-config` to define per-pool provider mappings. MrOracle will update each pool's AUM independently with only the providers it needs.

### Config File

Create a `pools_config.json` (see `pools_config.example.json`):

```json
{
  "pools": [
    {
      "name": "main-pool",
      "address": "PoolPubkeyBase58...",
      "providers": ["chaoslabs", "autonom", "switchboard"],
      "cu_limit": 1400000
    },
    {
      "name": "autonom-pool",
      "address": "AnotherPoolPubkey...",
      "providers": ["autonom"],
      "cu_limit": 120000
    },
    {
      "name": "sb-pool",
      "address": "ThirdPoolPubkey...",
      "providers": ["switchboard"],
      "cu_limit": 1400000
    }
  ]
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Human-readable label (used in logs) |
| `address` | yes | Pool account pubkey (base58) |
| `providers` | yes | Array of `"chaoslabs"`, `"autonom"`, `"switchboard"` |
| `cu_limit` | no | Compute unit limit (defaults to 1,400,000 if switchboard is included, 120,000 otherwise) |

### Usage

```bash
cargo run --bin mroracle -- --endpoint "$RPC" --payer-keypair kp.json \
  --pools-config pools_config.json \
  --db-string "$DB" --combined-cert cert.pem \
  --chaoslabs-feed-map-path config/chaoslabs_feed_map.json \
  --autonom-feed-map-path config/autonom_feed_map.json \
  --switchboard-feed-map-path config/switchboard_feed_map.json
```

### How It Works

1. MrOracle starts the global provider pollers based on the **union** of all pools' provider requirements. If any pool needs ChaosLabs, the ChaosLabs poller starts. Same for Autonom and Switchboard.
2. When a provider update arrives, it's distributed to every pool that declared that provider.
3. Each pool independently tracks readiness. A pool fires its `update_pool_aum` transaction only when **all** of its declared providers have delivered fresh data.
4. Each pool's transaction is self-contained with its own custody accounts, CU limit, and provider payloads.

### Backward Compatibility

`--pools-config` conflicts with `--providers`, `--only-chaoslabs`, `--only-autonom`, and `--only-switchboard`. Without `--pools-config`, MrOracle behaves exactly as before: single pool via `MAIN_POOL_ID` env var, provider selection via CLI flags.

---

## Feed Maps

Each provider uses a JSON feed map in `config/`. These must match the feed maps in `adrena-data/cron/config/` for prices to flow correctly.

| File | Provider | Env/Flag |
|------|----------|----------|
| `config/chaoslabs_feed_map.json` | ChaosLabs | `--chaoslabs-feed-map-path` / `CHAOSLABS_FEED_MAP_PATH` |
| `config/autonom_feed_map.json` | Autonom | `--autonom-feed-map-path` / `AUTONOM_FEED_MAP_PATH` |
| `config/switchboard_feed_map.json` | Switchboard | `--switchboard-feed-map-path` / `SWITCHBOARD_FEED_MAP_PATH` |

Example files are in `feed_map_examples/`.

ChaosLabs signature verification is order-sensitive. Keep feed map entries in the same signed order expected by the upstream signer.

---

## Localnet vs Devnet Deployment

### Comparison

| Setting | Localnet | Devnet |
|---------|----------|--------|
| RPC | `http://127.0.0.1:8899` | Devnet RPC (Helius, Triton, etc.) |
| DB | `adrena_tx_local` on localhost | `adrena_tx_devnet` on localhost |
| SSL cert | Self-signed (`generated/combined.pem`) | Self-signed (`generated/combined.pem`) |
| Switchboard queue | `A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w` | `EYiAmGSdsQTuCw413V5BzaruWuCCSDgTPtBGvLkXHbe7` |
| Pool ID | From localnet deployment manifest | From devnet deployment manifest |
| Payer keypair | `adrena-keypairs/localnet/payer.json` | `adrena-keypairs/devnet/payer.json` |
| tmux session | `adrena-local` | `adrena-devnet` |

### Manual Localnet Setup

1. Deploy the Adrena program (`setup-adrena.sh --mode localnet`)
2. Patch adrena-abi with correct PDAs (`setup-adrena-abi.sh --mode localnet`)
3. Set up adrena-data DB + crons (`setup-adrena-data.sh --mode localnet`)
4. Build MrOracle: `cargo build --release`
5. Run:

```bash
POOL_ID="<from manifest>"
PAYER_KP="adrena-keypairs/localnet/payer.json"
DB_STRING="host=localhost port=5432 user=adrena_local password=adrena_local_pw dbname=adrena_tx_local sslmode=prefer"
SSL_CERT="../local-execution/generated/combined.pem"

RUST_LOG=info ./target/release/mroracle \
  --endpoint http://127.0.0.1:8899 \
  --payer-keypair "$PAYER_KP" \
  --db-string "$DB_STRING" \
  --combined-cert "$SSL_CERT" \
  --switchboard-queue-pubkey A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w \
  --providers chaoslabs,autonom,switchboard \
  --commitment confirmed
```

### Manual Devnet Setup

Same flow but with devnet values:

```bash
RUST_LOG=info ./target/release/mroracle \
  --endpoint "$DEVNET_RPC_URL" \
  --payer-keypair adrena-keypairs/devnet/payer.json \
  --db-string "host=localhost port=5432 user=adrena_devnet password=adrena_devnet_pw dbname=adrena_tx_devnet sslmode=prefer" \
  --combined-cert "../local-execution/generated/combined.pem" \
  --switchboard-queue-pubkey EYiAmGSdsQTuCw413V5BzaruWuCCSDgTPtBGvLkXHbe7 \
  --providers chaoslabs,autonom,switchboard \
  --commitment confirmed
```

### Automated (via local-execution)

```bash
cd local-execution
npm run start -- --mode localnet   # full deploy + start
npm run start -- --mode devnet     # devnet deploy + start
```

The orchestrator runs `scripts/setup-mroracle.sh` which:
1. Reads pool ID and payer keypair from the deployment manifest
2. Generates `.env` with feed map paths
3. Builds MrOracle (`cargo build --release`)
4. Starts in tmux with correct DB string, SSL cert, and Switchboard queue

### DRY_RUN Mode

Set `DRY_RUN=true` to build oracle payloads without submitting transactions. Useful for testing the full pipeline without affecting on-chain state.

---

## CLI Reference

### Single Provider

```bash
# ChaosLabs only
cargo run --bin mroracle -- --endpoint "$RPC" --payer-keypair kp.json \
  --providers chaoslabs --db-string "$DB" --combined-cert cert.pem \
  --chaoslabs-feed-map-path config/chaoslabs_feed_map.json

# Autonom only
cargo run --bin mroracle -- --endpoint "$RPC" --payer-keypair kp.json \
  --providers autonom --db-string "$DB" --combined-cert cert.pem \
  --autonom-feed-map-path config/autonom_feed_map.json

# Switchboard only
cargo run --bin mroracle -- --endpoint "$RPC" --payer-keypair kp.json \
  --providers switchboard \
  --switchboard-api-key "$KEY" --switchboard-feed-map-path config/switchboard_feed_map.json
```

### All Three Providers

```bash
cargo run --bin mroracle -- --endpoint "$RPC" --payer-keypair kp.json \
  --providers chaoslabs,autonom,switchboard \
  --db-string "$DB" --combined-cert cert.pem \
  --chaoslabs-feed-map-path config/chaoslabs_feed_map.json \
  --autonom-feed-map-path config/autonom_feed_map.json \
  --switchboard-api-key "$KEY" --switchboard-feed-map-path config/switchboard_feed_map.json
```

---

## Environment Variables

Can be set in `.env` file at project root or via shell environment.

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Log level (`debug`, `info`, `warn`, `error`) |
| `DRY_RUN` | `true` to skip on-chain sends |
| `MAIN_POOL_ID` | Override pool PDA (otherwise from adrena-abi) |
| `CHAOSLABS_FEED_MAP_PATH` | Path to ChaosLabs feed map JSON |
| `AUTONOM_FEED_MAP_PATH` | Path to Autonom feed map JSON |
| `SWITCHBOARD_FEED_MAP_PATH` | Path to Switchboard feed map JSON |
| `SWITCHBOARD_API_KEY` | Switchboard API key |

---

## Troubleshooting

### `adrena-abi` build errors after branch switch

If you switch branches on adrena-abi, MrOracle won't build because the cached dependency is stale:

```bash
cd MrOracle && cargo clean && cargo build --release
```

### "Pool not found" or wrong pool ID

The pool ID is baked into `adrena-abi/src/lib.rs`. If you redeployed the program, re-run `setup-adrena-abi.sh` to patch the correct PDAs, then rebuild MrOracle.

### DB connection refused

Ensure PostgreSQL is running and the DB/user exist:

```bash
sudo systemctl start postgresql
pg_isready
PGPASSWORD=adrena_local_pw psql -h localhost -U adrena_local -d adrena_tx_local -c "SELECT 1"
```

### No oracle batches in DB

MrOracle reads from `oracle_batches` table populated by adrena-data cron. Ensure the cron processes are running:

```bash
tmux attach -t adrena-local  # check cron windows
```

### Cargo.toml uses git dependency instead of local path

For local development, the setup script patches `adrena-abi = { git = "..." }` to `adrena-abi = { path = "../adrena-abi" }`. If you see git dependency errors, either:
- Run `setup-mroracle.sh --mode localnet` (patches automatically), or
- Manually edit `Cargo.toml`:
  ```toml
  adrena-abi = { path = "../adrena-abi" }
  ```

### Feed map mismatch

MrOracle's feed maps must match adrena-data's. The `local-execution validate` command checks this automatically:

```bash
cd local-execution && npm run validate -- --mode localnet
```

Look for "Feed map" results in the static analysis output.
