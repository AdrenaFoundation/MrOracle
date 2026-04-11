//! Generic polling loop for provider cycles.
//!
//! Each provider supplies a `cycle_fn` that:
//!   1. Reads the latest batch row from `oracle_batches` (via the DB pool)
//!   2. Checks whether it's newer than the last emit (via `dedup_key`)
//!   3. If new, constructs the appropriate `ProviderUpdate` variant
//!   4. Returns `Some(PollerPayload)` on new data, `None` otherwise
//!
//! The poller task just calls `cycle_fn` every `poll_interval`, dedups via
//! the key, and forwards new updates onto an mpsc channel consumed by the
//! main dispatch loop.

use {
    crate::provider_updates::ProviderUpdate,
    std::{future::Future, pin::Pin, time::Duration},
    tokio::{
        sync::mpsc,
        time::sleep,
    },
};

pub struct PollerPayload {
    /// Unique-per-batch string used for provider-side dedup. Typically the
    /// `oracle_batch_id` as a string. If two consecutive cycles return the
    /// same `dedup_key`, the second is silently skipped.
    pub dedup_key: String,
    pub update: ProviderUpdate,
}

pub type CycleFn = Box<
    dyn Fn() -> Pin<
            Box<dyn Future<Output = Result<Option<PollerPayload>, anyhow::Error>> + Send>,
        > + Send
        + Sync,
>;

/// Run an infinite polling loop for one provider. The loop is resilient to
/// transient errors (logs + sleeps), so the only thing that stops it is the
/// downstream channel closing (caller dropped the receiver).
pub async fn run_provider_poller(
    provider_label: &'static str,
    poll_interval: Duration,
    cycle_fn: CycleFn,
    sender: mpsc::Sender<ProviderUpdate>,
) {
    let mut last_dedup_key: Option<String> = None;

    log::info!(
        "[{provider_label}] poller started (interval={}ms)",
        poll_interval.as_millis()
    );

    loop {
        match cycle_fn().await {
            Ok(Some(payload)) => {
                if last_dedup_key.as_deref() == Some(payload.dedup_key.as_str()) {
                    log::trace!(
                        "[{provider_label}] cycle returned same dedup_key={}, skipping",
                        payload.dedup_key
                    );
                } else {
                    last_dedup_key = Some(payload.dedup_key.clone());
                    if let Err(err) = sender.send(payload.update).await {
                        log::error!(
                            "[{provider_label}] dispatch channel closed, stopping poller: {err}"
                        );
                        break;
                    }
                }
            }
            Ok(None) => {
                log::trace!("[{provider_label}] cycle returned no data");
            }
            Err(err) => {
                log::error!("[{provider_label}] cycle error: {err:#}");
            }
        }

        sleep(poll_interval).await;
    }
}
