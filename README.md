# MrOracle

OpenSource Rust keeper handling multi-provider oracle price updates on-chain for the Adrena protocol.

MrOracle reads the latest signed oracle batches from a PostgreSQL database (populated by adrena-data cron workers), formats them into the on-chain wire format, and pushes them via the `update_pool_aum` instruction. Three oracle providers are supported: ChaosLabs, Autonom, and Switchboard. A multi-layer RPC fallback chain (primary -> backup -> public) ensures price updates continue even if primary endpoints go down.

## Table of Contents

- [Architecture](#architecture)
- [Build](#build)
- [Configuration](#configuration)
- [Oracle Providers](#oracle-providers)
- [RPC Fallback Architecture](#rpc-fallback-architecture)
- [Running](#running)
- [Troubleshooting](#troubleshooting)

---

## Architecture

```
ChaosLabs  ──┐
Autonom     ──┤  adrena-data cron  ──►  oracle_batches + oracle_batch_prices (PostgreSQL)
Switchboard ──┘                                    │
                                                   │ 3 provider pollers
                                                   ▼
                                           +------------+
                                           |  MrOracle  |
                                           +-----+------+
                                                 │
                                                 │ update_pool_aum TX
                                                 ▼
                                  Primary RPC → Backup RPC → Public RPC
                                                 │
                                                 ▼
                                          Solana Blockchain (Oracle PDA)
```

Each cycle:
1. Three provider pollers read from `oracle_batches` + `oracle_batch_prices` tables
2. Each poller deduplicates by `oracle_batch_id` and validates timestamp freshness (5s TTL)
3. When all required providers for a pool have fresh data, the dispatcher fires `update_pool_aum`
4. ChaosLabs + Autonom go into `multi_oracle_prices` (MultiBatchPrices), Switchboard goes via `switchboard_oracle_prices` with ed25519 + quote_store instructions
5. Sign + send + confirm via RPC fallback chain

---

## Build

```bash
cargo build                # debug
cargo build --release      # production
```

The binary is at `target/release/mroracle`.

---

## Configuration

### CLI Arguments

| Flag | Required | Default | Description |
|------|----------|---------|-------------|
| `--rpc` | No | `http://127.0.0.1:10000` | Primary Solana JSON-RPC endpoint |
| `--rpc-backup` | No | — | Backup RPC endpoint |
| `--rpc-public` | No | `https://api.mainnet-beta.solana.com` | Last-resort public RPC |
| `--payer-keypair` | Yes | — | Path to funded payer keypair JSON |
| `--db-string` | Yes | — | PostgreSQL connection string for source DB |
| `--combined-cert` | Yes | — | Path to combined certificate file (for DB TLS) |
| `--commitment` | No | `processed` | Solana commitment level |
| `--manifest-path` | Yes | — | Path to `pools_config.json` (pool addresses, custodies, provider requirements) |

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RUST_LOG` | — | Log level (`debug`, `info`, `warn`, `error`) |
| `ORACLE_BATCH_MAX_AGE_SECONDS` | `5` | Max batch age before rejection (sits below on-chain 7s staleness) |
| `ORACLE_BATCH_FUTURE_DRIFT_SECONDS` | `2` | Max allowed clock drift for future-dated batches |

---

## Oracle Providers

| Provider | Wire format | Feed ID range | Signed batch field |
|----------|------------|---------------|-------------------|
| ChaosLabs | `BatchPricesWithProvider` in `multi_oracle_prices` | 0-29 | secp256k1 signature |
| Autonom | `BatchPricesWithProvider` in `multi_oracle_prices` | 30-59 | secp256k1 signature |
| Switchboard | `SwitchboardUpdateParams` in `switchboard_oracle_prices` | 142-199 | ed25519 via separate instruction |

The `oracle_prices` field (legacy single-batch) is always set to `None`.

Wire types are defined locally in `src/adrena_ix.rs` — byte-identical to the release/39 on-chain definitions but deliberately decoupled from `adrena-abi` for independent versioning.

---

## RPC Fallback Architecture

Every JSON-RPC call goes through a 3-layer fallback chain:

```
primary (--rpc)  →  backup (--rpc-backup)  →  public (--rpc-public)
```

Each layer has a 5-second timeout. On failure, the next layer is tried. On-chain errors short-circuit the chain (no retry on backup/public).

---

## Running

### Local

```bash
RUST_LOG=info ./target/release/mroracle \
  --rpc "https://your-primary-rpc.com/<KEY>" \
  --rpc-backup "https://your-backup-rpc.com/<KEY>" \
  --payer-keypair payer.json \
  --commitment processed \
  --db-string "postgresql://user:pass@host/db" \
  --combined-cert /path/to/combined.pem \
  --manifest-path pools_config.json
```

### On Render

```bash
./target/release/mroracle \
  --payer-keypair /etc/secrets/mr_oracle.json \
  --rpc "https://primary-rpc.example.com/<KEY>" \
  --rpc-backup "https://backup-rpc.example.com/<KEY>" \
  --commitment processed \
  --db-string "postgresql://adrena:<PASS>@<HOST>.singapore-postgres.render.com/transaction_db_celf" \
  --combined-cert /etc/secrets/combined.pem \
  --manifest-path pools_config.json
```

---

## Troubleshooting

### All RPCs failing

```
ERROR [update_pool_aum:main-pool] All RPCs failed
```

All 3 RPC layers failed. Check URLs and auth keys.

### DB connection errors

MrOracle reads from `oracle_batches` + `oracle_batch_prices` tables in the source DB. If the DB is unreachable or cron isn't populating these tables, pollers return `None` and no update is sent.

### Stale batch warnings

```
WARN chaoslabs oracle_batch 12345 stale: latest_timestamp age=8s > ttl=5s
```

The batch in the DB is older than the TTL. This means adrena-data cron stopped writing for that provider, or the provider's API is down.

### Priority Fees

If priority fees can't be fetched, MrOracle keeps the last known value and continues. Transactions still land but may be slow to confirm during congestion.
