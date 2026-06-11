use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;

// want every connection to be able to do the following:
// log connection errors if it is the first one to occur since all connections were successfully established
// notify that an error occurred via a cancellation token (err_ct)
// terminate in case of shutdown (shutdown_ct) and log a single message upon shutdown (exited)
// allow one to wait for a connection to be re-established
#[derive(Clone)]
pub struct ConnectionState {
    exited: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    shutdown_ct: CancellationToken,
    err_ct: CancellationToken,
}

impl ConnectionState {
    pub fn new(shutdown_ct: CancellationToken, err_ct: CancellationToken) -> Self {
        Self {
            exited: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            shutdown_ct,
            err_ct,
        }
    }

    pub fn set_exited(&self) -> bool {
        self.exited
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn set_cancelled(&self) -> bool {
        self.cancelled
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn shutdown_ct(&self) -> CancellationToken {
        self.shutdown_ct.clone()
    }

    pub fn err_ct(&self) -> CancellationToken {
        self.err_ct.clone()
    }
}
