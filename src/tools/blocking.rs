//! Tokio blocking-pool boundary for synchronous tool operations.

/// Distinguishes an operation's domain error from a Tokio worker failure.
#[derive(Debug)]
pub(crate) enum BlockingError<E> {
    /// The synchronous operation returned its own error.
    Operation(E),
    /// Tokio could not complete the blocking worker.
    Worker(tokio::task::JoinError),
}

/// Runs a synchronous fallible operation on Tokio's blocking pool.
pub(crate) async fn run<T, E>(
    operation: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, BlockingError<E>>
where
    T: Send + 'static,
    E: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(BlockingError::Worker)?
        .map_err(BlockingError::Operation)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    use tokio::sync::oneshot;

    use super::run;

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_job_does_not_stall_the_current_thread_executor() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (entered_tx, mut entered_rx) = oneshot::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let blocking_job = run(move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok::<_, ()>(42)
                });
                tokio::pin!(blocking_job);

                let progressed = Arc::new(AtomicBool::new(false));
                let progressed_task = Arc::clone(&progressed);
                tokio::task::spawn_local(async move {
                    tokio::task::yield_now().await;
                    progressed_task.store(true, Ordering::Release);
                });

                tokio::select! {
                    entered = &mut entered_rx => entered.expect("blocking worker dropped entered signal"),
                    result = &mut blocking_job => panic!("blocking job completed before release: {result:?}"),
                }
                tokio::task::yield_now().await;
                assert!(progressed.load(Ordering::Acquire));

                release_tx.send(()).unwrap();
                assert_eq!(blocking_job.await.unwrap(), 42);
            })
            .await;
    }
}
