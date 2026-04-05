# MrOracle

OpenSource Rust keeper handling oracle and ALP price updates on-chain for the Adrena protocol.

MrOracle reads the latest ChaosLabs oracle prices from a PostgreSQL database, formats them into `ChaosLabsBatchPrices`, and pushes them on-chain via the `updatePoolAum` instruction every 5 seconds (longer during RPC fallback). A multi-layer RPC fallback chain (primary → backup → public) ensures price updates continue even if primary endpoints go down.

## Table of Contents

- [Architecture](#architecture)
- [Build](#build)
- [Configuration](#configuration)
- [RPC Fallback Architecture](#rpc-fallback-architecture)
- [Running](#running)
- [Troubleshooting](#troubleshooting)

---

## Architecture

```
ChaosLabs  →  adrena-data cron  →  PostgreSQL
                                        │
                                        │ every 5s
                                        ▼
                                 +------------+
                                 |  MrOracle  |
                                 +-----+------+
                                       │
                                       │ updatePoolAum TX
                                       ▼
                        Primary RPC → Backup RPC → Public RPC
                                       │
                                       ▼
                                Solana Blockchain
```

Each cycle:
1. Fetch latest prices from PostgreSQL (`assets_price` table)
2. Format into ChaosLabs batch format
3. Build `updatePoolAum` instruction
4. Sign + send + confirm on primary RPC (falls through to backup → public on failure)

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
| `--rpc` | No | `http://127.0.0.1:10000` | Primary Solana JSON-RPC endpoint (with auth key in URL path) |
| `--rpc-backup` | No | — | Backup RPC endpoint (used when primary fails) |
| `--rpc-public` | No | `https://api.mainnet-beta.solana.com` | Last-resort public RPC |
| `--payer-keypair` | Yes | — | Path to funded payer keypair JSON |
| `--db-string` | Yes | — | PostgreSQL connection string for price DB |
| `--combined-cert` | Yes | — | Path to combined certificate file (for DB TLS) |
| `--commitment` | No | `processed` | Solana commitment level |

### Log Levels

Control via `RUST_LOG` environment variable:

```bash
RUST_LOG=info ./target/release/mroracle ...
RUST_LOG=debug ./target/release/mroracle ...
```

---

## RPC Fallback Architecture

Every JSON-RPC call in MrOracle goes through a 3-layer fallback chain:

```
primary (--rpc)  →  backup (--rpc-backup)  →  public (--rpc-public)
```

Each layer has a 5-second timeout. On failure, the next layer is tried. Per endpoint: get blockhash → sign → send → confirm (4 polls @ 500ms, up to 2s). On-chain errors short-circuit the chain (no retry on backup/public).

**What's covered by fallback:**
- Pool account fetch at startup
- Priority fee polling (every 5s)
- Oracle price update transaction signing + sending + confirmation

**RPC fallback is stateless** — each RPC call starts fresh from primary. If primary recovers, the very next call hits it first. No circuit breaker, no stickiness.

**Log prefixes:** `[Pool Fetch]`, `[Priority Fees]`, `[Price Update]`. Failures show as:

```
ERROR [Price Update] PRIMARY RPC failed: <reason>
ERROR [Price Update] BACKUP RPC failed: <reason>
WARN  [Price Update] TX confirmed via PUBLIC RPC fallback: <signature>
```

On total failure:
```
ERROR [Price Update] All RPCs failed. Please handle ASAP. Critical Priority.
```

---

## Running

### Local

```bash
./target/release/mroracle \
  --rpc "https://your-primary-rpc.com/<KEY>" \
  --rpc-backup "https://your-backup-rpc.com/<KEY>" \
  --rpc-public "https://api.mainnet-beta.solana.com" \
  --payer-keypair payer.json \
  --commitment processed \
  --db-string "postgresql://user:pass@host/db" \
  --combined-cert /path/to/combined.pem
```

### On Render

```bash
./target/release/mroracle \
  --payer-keypair /etc/secrets/mr_oracle.json \
  --rpc "https://primary-rpc.example.com/<KEY>" \
  --rpc-backup "https://backup-rpc.example.com/<KEY>" \
  --rpc-public "https://api.mainnet-beta.solana.com" \
  --commitment processed \
  --db-string "postgresql://adrena:<PASS>@<HOST>.singapore-postgres.render.com/transaction_db_celf" \
  --combined-cert /etc/secrets/combined.pem
```

Without `--rpc-backup`: the fallback chain is primary → public. With it: primary → backup → public.

---

## Troubleshooting

### All RPCs failing / "Critical Priority" log

```
ERROR [Price Update] All RPCs failed. Please handle ASAP. Critical Priority.
```

All 3 RPC layers failed. Check the individual layer errors above the summary line. Common causes:
- All three URLs misconfigured
- Network outage on your side
- Solana mainnet congestion causing public RPC to throttle (100 req/10s limit)

### Pool fetch crashes at startup

If `[Pool Fetch] All RPCs failed` fires at startup, the service can't boot. Primary, backup, and public RPCs all failed to return the pool account. Verify URLs and auth keys.

### Priority Fees stale

If priority fees can't be fetched (e.g., public RPC rate-limited), MrOracle keeps the last known value (0 at startup if never fetched) and continues. Transactions still land but may be slow to confirm during congestion.

### DB connection errors

MrOracle depends on adrena-data writing prices to the PostgreSQL `assets_price` table. If the DB is unreachable, MrOracle retries with exponential backoff (50ms → 100ms → 200ms, 3 attempts) before logging an error and moving to the next cycle.
