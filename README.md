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
