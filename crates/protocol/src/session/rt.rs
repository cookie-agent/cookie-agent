//! The executor services the client needs: spawning a task, a timeout, a
//! sleep, and an interval.
//!
//! Natively they are tokio's. On wasm32 tasks run on the browser's event loop
//! (`wasm_bindgen_futures::spawn_local`) and timers are the browser's, so a
//! spawned future need not be `Send` there; natively it still must be.

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::*;
#[cfg(target_arch = "wasm32")]
pub(crate) use wasm::*;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::{future::Future, time::Duration};

    pub(crate) use tokio::{
        task::JoinHandle,
        time::{Interval, sleep, timeout},
    };

    /// Runs `future` on the tokio runtime.
    pub(crate) fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::spawn(future)
    }

    /// Ticks every `period`, first at once; a late tick delays the ones after
    /// it rather than bursting to catch up.
    pub(crate) fn interval(period: Duration) -> Interval {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
        time::Duration,
    };

    use futures_util::{FutureExt as _, StreamExt as _};
    use gloo_timers::future::IntervalStream;
    use tokio::sync::oneshot;

    pub(crate) use gloo_timers::future::sleep;

    /// A spawned task's output. It resolves to an error when the task ended
    /// without producing one (it panicked).
    pub(crate) struct JoinHandle<T>(oneshot::Receiver<T>);

    /// The task behind a [`JoinHandle`] ended without an output.
    #[derive(Debug)]
    pub(crate) struct JoinError;

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.poll_unpin(context).map_err(|_| JoinError)
        }
    }

    /// Runs `future` on the browser's event loop. Dropping the handle
    /// detaches the task, as it does natively.
    pub(crate) fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (sender, receiver) = oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = sender.send(future.await);
        });
        JoinHandle(receiver)
    }

    /// `future` did not complete within its timeout.
    #[derive(Debug)]
    pub(crate) struct Elapsed;

    /// Runs `future` for at most `duration`.
    pub(crate) async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, Elapsed>
    where
        F: Future,
    {
        let future = std::pin::pin!(future);
        match futures_util::future::select(future, sleep(duration)).await {
            futures_util::future::Either::Left((output, _)) => Ok(output),
            futures_util::future::Either::Right(_) => Err(Elapsed),
        }
    }

    /// Ticks every period. Waiting for a tick and giving up keeps the
    /// period running, so a tick is never lost to a cancelled wait.
    pub(crate) struct Interval(IntervalStream);

    impl Interval {
        pub(crate) async fn tick(&mut self) {
            // The stream never ends while it is held.
            let _ = self.0.next().await;
        }
    }

    /// Ticks every `period`, first after one period; the browser delays a
    /// late tick rather than bursting to catch up.
    pub(crate) fn interval(period: Duration) -> Interval {
        let millis = u32::try_from(period.as_millis()).unwrap_or(u32::MAX);
        Interval(IntervalStream::new(millis))
    }
}
