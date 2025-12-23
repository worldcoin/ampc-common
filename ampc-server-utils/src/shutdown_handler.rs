use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::signal;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ShutdownHandler {
    ct: CancellationToken,
    network_ct: CancellationToken,
    n_batches_pending_completion: Arc<AtomicUsize>,
    last_results_sync_timeout: Duration,
}

#[derive(Error, Debug)]
pub enum ShutdownError {
    #[error("timeout waiting for shutdown")]
    Timeout { batches_pending_completion: usize },
}

impl ShutdownHandler {
    pub fn new(shutdown_last_results_sync_timeout_secs: u64) -> Self {
        Self {
            ct: CancellationToken::new(),
            network_ct: CancellationToken::new(),
            n_batches_pending_completion: Arc::new(AtomicUsize::new(0)),
            last_results_sync_timeout: Duration::from_secs(shutdown_last_results_sync_timeout_secs),
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        self.ct.is_cancelled()
    }

    pub fn trigger_manual_shutdown(&self) {
        self.ct.cancel()
    }

    pub async fn wait_for_shutdown(&self) {
        self.ct.cancelled().await
    }

    pub fn get_network_cancellation_token(&self) -> CancellationToken {
        self.network_ct.clone()
    }

    pub async fn register_signal_handler(&self) {
        let ct = self.ct.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            ct.cancel();
            tracing::info!("Shutdown signal received.");
        });
    }

    pub fn increment_batches_pending_completion(&self) {
        tracing::debug!("Incrementing pending batches count");
        self.n_batches_pending_completion
            .fetch_add(1, Ordering::SeqCst);
    }

    pub fn decrement_batches_pending_completion(&self) {
        tracing::debug!("Decrementing pending batches count");
        self.n_batches_pending_completion
            .fetch_sub(1, Ordering::SeqCst);
    }

    pub async fn wait_for_pending_batches_completion(&self) -> Result<(), ShutdownError> {
        let check_interval = Duration::from_millis(100);
        let start = Instant::now();

        while self.n_batches_pending_completion.load(Ordering::SeqCst) > 0 {
            if start.elapsed() >= self.last_results_sync_timeout {
                let pending = self.get_batches_pending_completion();
                tracing::warn!(
                    "Shutdown timeout reached with {} batches still pending",
                    pending
                );
                return Err(ShutdownError::Timeout {
                    batches_pending_completion: pending,
                });
            }

            tokio::time::sleep(check_interval).await;
        }

        if self.ct.is_cancelled() {
            self.network_ct.cancel();
        }
        tracing::info!("Pending batches count reached zero.");
        Ok(())
    }

    fn get_batches_pending_completion(&self) -> usize {
        self.n_batches_pending_completion.load(Ordering::SeqCst)
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
        tracing::info!("Ctrl+C received.");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
        tracing::info!("SIGTERM received.");
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_shutdown_handler() {
        // Use a shorter timeout for testing (100ms instead of 1s)
        let handler = ShutdownHandler::new(0);

        // Start work.
        assert!(!handler.is_shutting_down());
        handler.increment_batches_pending_completion();

        // Initiate a shutdown.
        handler.trigger_manual_shutdown();
        assert!(handler.is_shutting_down());

        // If batches do not complete, return timeout error.
        // Since timeout is 0 seconds, it should return immediately with error
        let result = handler.wait_for_pending_batches_completion().await;
        assert!(matches!(
            result,
            Err(ShutdownError::Timeout {
                batches_pending_completion: 1
            })
        ));

        // Complete the batch.
        handler.decrement_batches_pending_completion();

        // Should return quickly since no batches are pending
        let result = handler.wait_for_pending_batches_completion().await;
        assert!(result.is_ok());

        // Test quick completion again
        let quick = timeout(
            Duration::from_millis(10),
            handler.wait_for_pending_batches_completion(),
        );
        assert!(quick.await.unwrap().is_ok());
    }
}
