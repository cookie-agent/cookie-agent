use tokio::sync::Semaphore;

use crate::ToolError;

// Admission happens before spawn_blocking, so even Tokio's blocking queue contains
// at most this many artifact/capture jobs. A dropped awaiter does not release its slot.
const MAX_BLOCKING_IO_JOBS: usize = 8;
static IO_SLOTS: Semaphore = Semaphore::const_new(MAX_BLOCKING_IO_JOBS);

pub(crate) async fn run<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ToolError> {
    let slot = IO_SLOTS
        .acquire()
        .await
        .map_err(|_| ToolError::execution("artifact I/O unavailable"))?;
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        work()
    })
    .await
    .map_err(|error| ToolError::execution(format!("artifact I/O task failed: {error}")))
}

#[cfg(test)]
pub(crate) fn gate(
    store: &super::artifacts::ArtifactStore,
    operation: &'static str,
    key: Option<String>,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let pending = std::sync::Mutex::new(Some((entered, wait)));
    store
        .io_test_hook
        .set(std::sync::Arc::new(move |current, current_key| {
            if current == operation && key.as_ref().is_none_or(|key| key == current_key) {
                let gate = pending.lock().unwrap().take();
                if let Some((entered, wait)) = gate {
                    let _ = entered.send(());
                    wait.recv_timeout(std::time::Duration::from_secs(30))
                        .map_err(|error| {
                            std::io::Error::other(format!("I/O test gate: {error}"))
                        })?;
                }
            }
            Ok(())
        }));
    (observed, release)
}
