use std::collections::VecDeque;
use std::future::Future;
use tokio::task::JoinHandle;

/// We define a AsyncStream to replace futures::Stream since we don't want to implement
/// poll_next nor using async_stream.
/// Although we use GAT, we don't want the future to capture self's ref. We did like
/// that before, and this makes it hard to load stream in parallel like Buffered.
/// Also, our AsyncStream is not like Stream in signature. We return `Option<Future>`
/// instead of `Future<Output = Option<_>>`.
pub trait AsyncStream {
    type Item;
    type Future: Future<Output = Self::Item>;
    fn next(&mut self) -> Option<Self::Future>;

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

/// Buffered Stream.
/// By decorating Buffered, the output future of stream will be polled
/// concurrently.
/// Here I implement it by spawning tasks. It is indeed not efficient as
/// `FuturesOrdered` which is used by `futures-util::stream::Buffered`.
/// As a decorator of an async trait, it is hard to implement it in a poll
/// way. We can do that, but it breaks the safety boundary which requires
/// user to make sure that the AsyncStream exists when polling the future
/// since in our trait definition, the future has no relation with self.
/// And without poll, we can not drive multiple futures by one future.
pub struct Buffered<St>
where
    St: AsyncStream,
{
    stream: Option<St>,
    queue: VecDeque<JoinHandle<St::Item>>,
    max: usize,
}

impl<St> Buffered<St>
where
    St: AsyncStream,
{
    pub fn new(stream: St, buffer_size: usize) -> Self {
        assert!(buffer_size > 0, "buffer_size must be greater than 0");

        Self {
            stream: Some(stream),
            queue: VecDeque::with_capacity(buffer_size),
            max: buffer_size,
        }
    }
}

impl<St> AsyncStream for Buffered<St>
where
    St: AsyncStream,
    St::Item: Send + 'static,
    St::Future: Send + 'static,
{
    type Item = St::Item;
    type Future = impl Future<Output = Self::Item>;

    fn next(&mut self) -> Option<Self::Future> {
        while self.queue.len() < self.max {
            let Some(st) = self.stream.as_mut() else {
                break;
            };

            let Some(fut) = st.next() else {
                self.stream = None;
                break;
            };

            self.queue.push_back(tokio::spawn(fut));
        }

        self.queue.pop_front().map(|handle| async move {
            match handle.await {
                Ok(item) => item,
                Err(e) => {
                    panic!("buffered task failed: {e}");
                }
            }
        })
    }
}