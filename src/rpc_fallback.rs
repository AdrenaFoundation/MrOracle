use {
    anchor_lang::AccountDeserialize,
    serde::de::DeserializeOwned,
    solana_client::{
        nonblocking::rpc_client::RpcClient,
        rpc_config::RpcSendTransactionConfig,
        rpc_request::RpcRequest,
    },
    solana_sdk::{
        instruction::Instruction,
        pubkey::Pubkey,
        signature::{Keypair, Signature},
        signer::Signer,
        transaction::Transaction,
    },
    std::{sync::Arc, time::Duration},
    tokio::time::timeout,
};

const RPC_TIMEOUT: Duration = Duration::from_secs(5);
const CONFIRM_POLLS: u32 = 4;
const CONFIRM_INTERVAL: Duration = Duration::from_millis(500);

pub const DEFAULT_PUBLIC_RPC: &str = "https://api.mainnet-beta.solana.com";

struct RpcEndpoint {
    label: &'static str,
    client: RpcClient,
}

pub struct RpcFallback {
    endpoints: Vec<RpcEndpoint>,
    payer: Arc<Keypair>,
}

impl RpcFallback {
    pub fn new(
        primary_url: &str,
        backup_url: Option<&str>,
        public_url: &str,
        payer: Arc<Keypair>,
    ) -> Self {
        let mut endpoints = vec![RpcEndpoint {
            label: "primary",
            client: RpcClient::new_with_timeout(primary_url.to_string(), RPC_TIMEOUT),
        }];

        if let Some(url) = backup_url {
            endpoints.push(RpcEndpoint {
                label: "backup",
                client: RpcClient::new_with_timeout(url.to_string(), RPC_TIMEOUT),
            });
        }

        endpoints.push(RpcEndpoint {
            label: "public",
            client: RpcClient::new_with_timeout(public_url.to_string(), RPC_TIMEOUT),
        });

        Self { endpoints, payer }
    }

    pub fn payer_pubkey(&self) -> solana_sdk::pubkey::Pubkey {
        self.payer.pubkey()
    }

    /// Build, sign, send, and confirm a transaction with RPC fallback.
    /// Tries each RPC in order (primary -> backup -> public).
    /// Per endpoint: get blockhash -> sign -> send -> confirm.
    /// Only moves to the next endpoint if send or confirm fails.
    pub async fn sign_and_send(
        &self,
        instructions: Vec<Instruction>,
        config: RpcSendTransactionConfig,
        operation: &str,
    ) -> Result<Signature, anyhow::Error> {
        for endpoint in &self.endpoints {
            let label_upper = endpoint.label.to_uppercase();

            // 1. Get blockhash
            let blockhash = match timeout(RPC_TIMEOUT, endpoint.client.get_latest_blockhash()).await
            {
                Ok(Ok(h)) => h,
                Ok(Err(e)) => {
                    log::error!("[{}] {} RPC failed: {}", operation, label_upper, e);
                    continue;
                }
                Err(_) => {
                    log::error!("[{}] {} RPC failed: timed out", operation, label_upper);
                    continue;
                }
            };

            // 2. Sign
            let tx = Transaction::new_signed_with_payer(
                &instructions,
                Some(&self.payer.pubkey()),
                &[self.payer.as_ref()],
                blockhash,
            );

            // 3. Send
            let sig = match timeout(
                RPC_TIMEOUT,
                endpoint.client.send_transaction_with_config(&tx, config.clone()),
            )
            .await
            {
                Ok(Ok(sig)) => sig,
                Ok(Err(e)) => {
                    log::error!("[{}] {} RPC failed: {}", operation, label_upper, e);
                    continue;
                }
                Err(_) => {
                    log::error!("[{}] {} RPC failed: timed out", operation, label_upper);
                    continue;
                }
            };

            // 4. Confirm — poll for on-chain status
            let mut confirmed = false;
            for _ in 0..CONFIRM_POLLS {
                tokio::time::sleep(CONFIRM_INTERVAL).await;
                match endpoint.client.get_signature_statuses(&[sig]).await {
                    Ok(response) => {
                        if let Some(Some(status)) = response.value.first() {
                            if status.err.is_none() {
                                confirmed = true;
                                break;
                            } else {
                                // TX landed but failed on-chain
                                log::error!(
                                    "[{}] {} RPC tx failed on-chain: {:?}",
                                    operation, label_upper, status.err
                                );
                                break;
                            }
                        }
                        // None = not yet visible, keep polling
                    }
                    Err(_) => {
                        // Can't check status, keep polling
                    }
                }
            }

            if confirmed {
                if endpoint.label != "primary" {
                    log::warn!(
                        "[{}] TX confirmed via {} RPC fallback: {}",
                        operation, label_upper, sig
                    );
                }
                return Ok(sig);
            }

            log::error!(
                "[{}] {} RPC tx not confirmed after {}ms",
                operation, label_upper, CONFIRM_POLLS * CONFIRM_INTERVAL.as_millis() as u32
            );
        }

        log::error!("[{}] All RPCs failed. Please handle ASAP. Critical Priority.", operation);

        Err(anyhow::anyhow!(
            "[{}] All RPCs failed. Please handle ASAP. Critical Priority.",
            operation
        ))
    }

    /// Fetch and deserialize a typed account with RPC fallback.
    /// Tries each RPC in order. Deserialization errors are NOT retried
    /// (data is the same on all RPCs).
    pub async fn get_account<T: AccountDeserialize>(
        &self,
        pubkey: &Pubkey,
        operation: &str,
    ) -> Result<T, anyhow::Error> {
        for endpoint in &self.endpoints {
            let label_upper = endpoint.label.to_uppercase();

            let account = match timeout(RPC_TIMEOUT, endpoint.client.get_account(pubkey)).await {
                Ok(Ok(account)) => account,
                Ok(Err(e)) => {
                    log::error!("[{}] {} RPC failed: {}", operation, label_upper, e);
                    continue;
                }
                Err(_) => {
                    log::error!("[{}] {} RPC failed: timed out", operation, label_upper);
                    continue;
                }
            };

            let mut data: &[u8] = &account.data;
            return T::try_deserialize(&mut data)
                .map_err(|e| anyhow::anyhow!("[{}] deserialization failed: {}", operation, e));
        }

        log::error!(
            "[{}] All RPCs failed. Please handle ASAP. Critical Priority.",
            operation
        );
        Err(anyhow::anyhow!(
            "[{}] All RPCs failed. Please handle ASAP. Critical Priority.",
            operation
        ))
    }

    /// Send a raw RPC request with fallback.
    pub async fn send_rpc_request<T: DeserializeOwned>(
        &self,
        request: RpcRequest,
        params: serde_json::Value,
        operation: &str,
    ) -> Result<T, anyhow::Error> {
        for endpoint in &self.endpoints {
            let label_upper = endpoint.label.to_uppercase();

            match timeout(
                RPC_TIMEOUT,
                endpoint.client.send::<T>(request, params.clone()),
            )
            .await
            {
                Ok(Ok(result)) => return Ok(result),
                Ok(Err(e)) => {
                    log::error!("[{}] {} RPC failed: {}", operation, label_upper, e);
                    continue;
                }
                Err(_) => {
                    log::error!("[{}] {} RPC failed: timed out", operation, label_upper);
                    continue;
                }
            }
        }

        log::error!(
            "[{}] All RPCs failed. Please handle ASAP. Critical Priority.",
            operation
        );
        Err(anyhow::anyhow!(
            "[{}] All RPCs failed. Please handle ASAP. Critical Priority.",
            operation
        ))
    }
}
