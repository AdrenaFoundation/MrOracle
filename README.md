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

Default behavior starts all 3 providers in parallel:

- ChaosLabs (`update_pool_aum`)
- Autonom (`update_oracle` multi-provider batch)
- Switchboard (`update_oracle` native quote flow)

To run a single provider only, use one explicit flag:

- `--only-chaoslabs`
- `--only-autonom`
- `--only-switchboard`

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
- `--update-oracle-cu-limit` (default `1400000`)

Feed map format example is in `feed_map_examples/switchboard_feed_map.example.json`.

## Autonom Runtime (Signed Batch Flow)

Autonom in keeper is DB-backed and consumes batches already ingested in `adrena-data` (`oracle_batches` + `oracle_batch_prices`).

Autonom uses:

- `--autonom-feed-map-path <path>` or `AUTONOM_FEED_MAP_PATH`

Optional overrides:

- `--autonom-poll-ms` (default `3000`)
- `--update-oracle-cu-limit` (shared with switchboard/autonom update_oracle txs)

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
- Missing required config for any selected provider is a startup error (hard fail).
- `--db-string` and `--combined-cert` are required when ChaosLabs or Autonom is active.
- ChaosLabs feed map config is required only when ChaosLabs is active.
