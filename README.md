# MrOracle

OpenSource rust client (Keeper) handling oracle and alp price updates onchain.

## Build

`$> cargo build`
`$> cargo build --release`

## Run

`$> cargo run -- --payer-keypair payer.json --endpoint https://adrena.rpcpool.com/xxx --commitment finalized --db-string "postgresql://adrena:YYY.singapore-postgres.render.com/transaction_db_celf" --combined-cert /etc/secrets/combined.pem`

Or on Render

`./target/release/mroracle --payer-keypair /etc/secrets/mroracle.json --endpoint https://adrena.rpcpool.com/xxx--x-token xxx --commitment finalized --db-string "postgresql://adrena:YYY.singapore-postgres.render.com/transaction_db_celf" --combined-cert /etc/secrets/combined.pem`

Ideally run that on a Render instance.

## Provider Mode

Default behavior starts all 3 providers in parallel, buffers each provider's latest update, then
sends a single coordinated `update_pool_aum` tx only when all selected providers are ready.

- ChaosLabs (DB-backed signed batch)
- Autonom (DB-backed signed batch)
- Switchboard (native quote flow)

Selection options:

- `--only-chaoslabs`
- `--only-autonom`
- `--only-switchboard`
- `--providers <list>` where `<list>` is comma-separated:
  - `chaoslabs`
  - `autonom`
  - `switchboard`
  - example: `--providers chaoslabs,autonom`

The `--providers` flag supports single, any two-provider combination, or all three providers.

### Payload shape by provider set

- ChaosLabs only OR Autonom only: `BatchPrices`
- ChaosLabs + Switchboard OR Autonom + Switchboard: `BatchPrices + SwitchboardPrices`
- ChaosLabs + Autonom: `MultiBatchPrices`
- ChaosLabs + Autonom + Switchboard: `MultiBatchPrices + SwitchboardPrices`

### CLI examples

Use your own values for:
- `RPC_URL`
- `PAYER_KEYPAIR`
- `MAIN_POOL_ID` (optional; defaults to main pool id baked in `adrena-abi`)
- `DB_STRING`
- `COMBINED_CERT`
- `CHAOSLABS_FEED_MAP_PATH`
- `AUTONOM_FEED_MAP_PATH`
- `SWITCHBOARD_API_KEY`
- `SWITCHBOARD_FEED_MAP_PATH`

#### 1 provider

ChaosLabs:

```bash
cargo run --bin mroracle -- \
  --endpoint "$RPC_URL" \
  --payer-keypair "$PAYER_KEYPAIR" \
  --providers chaoslabs \
  --db-string "$DB_STRING" \
  --combined-cert "$COMBINED_CERT" \
  --chaoslabs-feed-map-path "$CHAOSLABS_FEED_MAP_PATH"
```

Autonom:

```bash
cargo run --bin mroracle -- \
  --endpoint "$RPC_URL" \
  --payer-keypair "$PAYER_KEYPAIR" \
  --providers autonom \
  --db-string "$DB_STRING" \
  --combined-cert "$COMBINED_CERT" \
  --autonom-feed-map-path "$AUTONOM_FEED_MAP_PATH"
```

Switchboard:

```bash
cargo run --bin mroracle -- \
  --endpoint "$RPC_URL" \
  --payer-keypair "$PAYER_KEYPAIR" \
  --providers switchboard \
  --switchboard-api-key "$SWITCHBOARD_API_KEY" \
  --switchboard-feed-map-path "$SWITCHBOARD_FEED_MAP_PATH"
```

#### 2 providers

ChaosLabs + Autonom:

```bash
cargo run --bin mroracle -- \
  --endpoint "$RPC_URL" \
  --payer-keypair "$PAYER_KEYPAIR" \
  --providers chaoslabs,autonom \
  --db-string "$DB_STRING" \
  --combined-cert "$COMBINED_CERT" \
  --chaoslabs-feed-map-path "$CHAOSLABS_FEED_MAP_PATH" \
  --autonom-feed-map-path "$AUTONOM_FEED_MAP_PATH"
```

ChaosLabs + Switchboard:

```bash
cargo run --bin mroracle -- \
  --endpoint "$RPC_URL" \
  --payer-keypair "$PAYER_KEYPAIR" \
  --providers chaoslabs,switchboard \
  --db-string "$DB_STRING" \
  --combined-cert "$COMBINED_CERT" \
  --chaoslabs-feed-map-path "$CHAOSLABS_FEED_MAP_PATH" \
  --switchboard-api-key "$SWITCHBOARD_API_KEY" \
  --switchboard-feed-map-path "$SWITCHBOARD_FEED_MAP_PATH"
```

Autonom + Switchboard:

```bash
cargo run --bin mroracle -- \
  --endpoint "$RPC_URL" \
  --payer-keypair "$PAYER_KEYPAIR" \
  --providers autonom,switchboard \
  --db-string "$DB_STRING" \
  --combined-cert "$COMBINED_CERT" \
  --autonom-feed-map-path "$AUTONOM_FEED_MAP_PATH" \
  --switchboard-api-key "$SWITCHBOARD_API_KEY" \
  --switchboard-feed-map-path "$SWITCHBOARD_FEED_MAP_PATH"
```

#### 3 providers

```bash
cargo run --bin mroracle -- \
  --endpoint "$RPC_URL" \
  --payer-keypair "$PAYER_KEYPAIR" \
  --providers chaoslabs,autonom,switchboard \
  --db-string "$DB_STRING" \
  --combined-cert "$COMBINED_CERT" \
  --chaoslabs-feed-map-path "$CHAOSLABS_FEED_MAP_PATH" \
  --autonom-feed-map-path "$AUTONOM_FEED_MAP_PATH" \
  --switchboard-api-key "$SWITCHBOARD_API_KEY" \
  --switchboard-feed-map-path "$SWITCHBOARD_FEED_MAP_PATH"
```

## Switchboard Runtime (Native Quote Flow)

Switchboard uses:

- `--switchboard-feed-map-path <path>` or `SWITCHBOARD_FEED_MAP_PATH`
- `--switchboard-api-key <key>` or `SWITCHBOARD_API_KEY`

Optional overrides:

- `--switchboard-crossbar-url` (default `https://crossbar.switchboard.xyz`)
- `--switchboard-gateway-url` (dedicated endpoint)
- `--switchboard-network` (default `mainnet`)
- `--switchboard-queue-pubkey` (default mainnet queue)
- `--switchboard-cycle-ms` (default `5000`)
- `--switchboard-max-age-slots` (default `32`)
- `--update-oracle-cu-limit` (default `1400000`, compute limit for coordinated `update_pool_aum`)

Feed map format example is in `feed_map_examples/switchboard_feed_map.example.json`.

## Autonom Runtime (Signed Batch Flow)

Autonom in keeper is DB-backed and consumes batches already ingested in `adrena-data` (`oracle_batches` + `oracle_batch_prices`).

Autonom uses:

- `--autonom-feed-map-path <path>` or `AUTONOM_FEED_MAP_PATH`

Optional overrides:

- `--autonom-poll-ms` (default `3000`)
- `--update-oracle-cu-limit` (compute limit for coordinated `update_pool_aum`)

Autonom feed map format:
- `autonom_feed_map[].adrena_feed_id = <adrena provider feed id>`
- `autonom_feed_map[].autonom_feed_id = <autonom canonical feed id>`

Primary example file: `feed_map_examples/autonom_feed_map.example.json`.

## ChaosLabs Mapping

ChaosLabs uses:

- `--chaoslabs-feed-map-path <path>` or `CHAOSLABS_FEED_MAP_PATH`

For reference and keeper-side consistency, an example mapping file is provided at:
- `feed_map_examples/chaoslabs_feed_map.example.json`

Important: ChaosLabs signature verification is order-sensitive. Keep `chaoslabs_feed_map` entries
in the same signed order expected by your upstream signer.

ChaosLabs keeper consumption is DB-backed and prefers `oracle_batches` (`provider='chaoslabs'`).
It falls back to legacy `assets_price` if the new tables are unavailable.

## Provider Startup Behavior

- In default mode, all 3 providers are attempted.
- If providers are explicitly selected via CLI (`--providers` or `--only-*`), startup is strict:
  - Any selected provider init/config failure is a hard fail (process exits).
- Without explicit provider selection, startup is soft-fail by provider:
  - If one provider is misconfigured/unavailable, that provider is disabled.
  - Remaining healthy providers continue running.
- In non-explicit mode, keeper process does not exit just because one provider setup fails.
- `--db-string` and `--combined-cert` are only required for DB-backed providers (ChaosLabs/Autonom) to be active.
- In non-explicit mode, if all attempted providers fail startup checks, keeper stays alive in passive mode and logs errors.
