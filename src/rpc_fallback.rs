use {
    anchor_lang::AccountDeserialize,
    serde::de::DeserializeOwned,
    solana_client::{
        nonblocking::rpc_client::RpcClient,
        rpc_config::RpcSendTransactionConfig,
        rpc_request::RpcRequest,
    },
    solana_sdk::{
        commitment_config::CommitmentConfig,
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
        commitment: CommitmentConfig,
        payer: Arc<Keypair>,
    ) -> Self {
        let make_client = |url: &str| {
            RpcClient::new_with_timeout_and_commitment(url.to_string(), RPC_TIMEOUT, commitment)
        };

        let mut endpoints = vec![RpcEndpoint {
            label: "primary",
            client: make_client(primary_url),
        }];

        if let Some(url) = backup_url {
            endpoints.push(RpcEndpoint {
                label: "backup",
                client: make_client(url),
            });
        }

        endpoints.push(RpcEndpoint {
            label: "public",
            client: make_client(public_url),
        });

        Self { endpoints, payer }
    }

    pub fn payer_pubkey(&self) -> solana_sdk::pubkey::Pubkey {
        self.payer.pubkey()
    }

    /// Borrow the primary RPC client for one-off reads (e.g. startup validation).
    pub fn primary_client(&self) -> &RpcClient {
        &self.endpoints[0].client
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
        // Track whether we successfully sent a TX on any endpoint.
        // If primary sent but didn't confirm, we still return it — oracle updates
        // are idempotent and the next poll window will send fresh data anyway.
        // This prevents the backup from sending a DIFFERENT TX (new blockhash)
        // that could land alongside the primary's, wasting CU.
        let mut sent_sig: Option<(Signature, &str)> = None;

        for endpoint in &self.endpoints {
            let label_upper = endpoint.label.to_uppercase();

            // If a prior endpoint already sent a TX, don't re-send from a
            // different endpoint (different blockhash = different TX = potential
            // double-land). Just try to confirm the already-sent TX on this endpoint.
            if let Some((prior_sig, prior_label)) = &sent_sig {
                let mut confirmed = false;
                for _ in 0..CONFIRM_POLLS {
                    tokio::time::sleep(CONFIRM_INTERVAL).await;
                    if let Ok(Ok(response)) = timeout(
                        RPC_TIMEOUT,
                        endpoint.client.get_signature_statuses(&[*prior_sig]),
                    ).await {
                        if let Some(Some(status)) = response.value.first() {
                            if let Some(err) = &status.err {
                                return Err(anyhow::anyhow!(
                                    "[{}] tx {} (sent via {}) failed on-chain: {:?}",
                                    operation, prior_sig, prior_label, err
                                ));
                            }
                            confirmed = true;
                            break;
                        }
                    }
                }
                if confirmed {
                    log::info!(
                        "[{}] TX {} (sent via {}) confirmed via {} RPC",
                        operation, prior_sig, prior_label, label_upper
                    );
                    return Ok(*prior_sig);
                }
                continue;
            }

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

            // Mark as sent — subsequent endpoints will only try to confirm, not re-send.
            sent_sig = Some((sig, endpoint.label));

            // 4. Confirm
            let mut confirmed = false;
            for _ in 0..CONFIRM_POLLS {
                tokio::time::sleep(CONFIRM_INTERVAL).await;
                let status_result = timeout(
                    RPC_TIMEOUT,
                    endpoint.client.get_signature_statuses(&[sig]),
                )
                .await;
                match status_result {
                    Ok(Ok(response)) => {
                        if let Some(Some(status)) = response.value.first() {
                            if let Some(err) = &status.err {
                                log::error!(
                                    "[{}] {} RPC tx failed on-chain: {:?}",
                                    operation, label_upper, err
                                );
                                return Err(anyhow::anyhow!(
                                    "[{}] tx {} failed on-chain: {:?}",
                                    operation, sig, err
                                ));
                            }
                            confirmed = true;
                            break;
                        }
                    }
                    Ok(Err(_)) => {}
                    Err(_) => {}
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

            log::warn!(
                "[{}] {} RPC sent tx {} but not confirmed after {}ms — will try confirming on next endpoint",
                operation, label_upper, sig, CONFIRM_POLLS * CONFIRM_INTERVAL.as_millis() as u32
            );
        }

        // If we sent a TX but never confirmed it, return the signature anyway.
        // Oracle updates are idempotent; the TX is likely still propagating.
        if let Some((sig, label)) = sent_sig {
            log::warn!(
                "[{}] TX {} (sent via {}) not confirmed by any endpoint — returning as best-effort",
                operation, sig, label
            );
            return Ok(sig);
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
