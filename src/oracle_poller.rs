use {
    crate::provider_updates::ProviderUpdate,
    std::{future::Future, pin::Pin, time::Duration},
    tokio::{sync::mpsc, time::Instant},
};

/// Return value from a provider's cycle function.
/// `dedup_key` prevents re-emitting the same data.
pub struct PollerPayload {
    pub dedup_key: String,
    pub update: ProviderUpdate,
}

/// Generic poll loop for all oracle providers.
///
/// `cycle_fn` is called each tick — it fetches from DB, transforms, and returns
/// the payload to emit (or `None` to skip, or `Err` to log and continue).
pub async fn run_provider_poller<F>(
    provider: &str,
    poll_interval: Duration,
    cycle_fn: F,
    update_sender: mpsc::Sender<ProviderUpdate>,
) -> Result<(), anyhow::Error>
where
    F: Fn() -> Pin<Box<dyn Future<Output = Result<Option<PollerPayload>, anyhow::Error>> + Send>>
        + Send
        + Sync,
{
    let mut last_dedup_key: Option<String> = None;

    loop {
        let start = Instant::now();

        match cycle_fn().await {
            Ok(Some(payload)) => {
                if last_dedup_key.as_deref() == Some(&payload.dedup_key) {
                    log::debug!(
                        "Skipping {} (already emitted, key={})",
                        provider,
                        payload.dedup_key
                    );
                } else {
                    update_sender
                        .send(payload.update)
                        .await
                        .map_err(|_| anyhow::anyhow!("{} channel closed", provider))?;
                    last_dedup_key = Some(payload.dedup_key);
                }
            }
            Ok(None) => {
                log::debug!("No {} data available", provider);
            }
            Err(e) => {
                log::error!("{} cycle error: {:?}", provider, e);
            }
        }

        let elapsed = start.elapsed();
        if let Some(remaining) = poll_interval.checked_sub(elapsed) {
            tokio::time::sleep(remaining).await;
        }
    }
}
