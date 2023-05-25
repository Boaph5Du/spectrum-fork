use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_std::channel::Sender;
use async_std::future::{timeout, TimeoutError};
use futures::stream::FuturesUnordered;
use futures::{FutureExt, Stream};
use log::warn;

/// Tasks talk to protocol behaviour.
pub enum FromTask<TIn, TOut> {
    /// Inject the input into protocol behaviour.
    ToBehaviour(TIn),
    /// Instructs protocol behaviour to emit the event.
    ToHandler(TOut),
}

pub struct TaskPool<TIn, TOut, R> {
    /// Name of the pool.
    name: String,
    /// Timeout for each task.
    timeout: Duration,
    /// Communication channel with parental behaviour.
    channel: Sender<FromTask<TIn, TOut>>,
    tasks: FuturesUnordered<Pin<Box<dyn Future<Output = Result<R, TimeoutError>> + Send + 'static>>>,
}

impl<TIn, TOut, R> TaskPool<TIn, TOut, R> {
    pub fn spawn<F, T>(&mut self, task: F)
    where
        F: FnOnce(Sender<FromTask<TIn, TOut>>) -> T,
        T: Future<Output = R> + Send + 'static,
        R: 'static,
    {
        self.tasks
            .push(timeout(self.timeout, task(self.channel.clone())).boxed())
    }
}

impl<TIn, TOut, R> Stream for TaskPool<TIn, TOut, R> {
    type Item = R;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<R>> {
        loop {
            match Stream::poll_next(Pin::new(&mut self.tasks), cx) {
                Poll::Ready(Some(Ok(res))) => {
                    return Poll::Ready(Some(res));
                }
                Poll::Ready(Some(Err(_timeout))) => {
                    warn!("[{}] Operation timeout", self.name);
                    continue;
                }
                Poll::Pending | Poll::Ready(None) => {}
            }
            return Poll::Pending;
        }
    }
}
