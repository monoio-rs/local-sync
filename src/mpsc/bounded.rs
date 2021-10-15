use std::task::{Context, Poll};

use futures_util::future::poll_fn;

use crate::semaphore::Inner;

use super::chan::{self, SendError};

#[derive(Clone)]
pub struct Tx<T>(chan::Tx<T, Inner>);

pub struct Rx<T>(chan::Rx<T, Inner>);

pub fn channel<T>(buffer: usize) -> (Tx<T>, Rx<T>) {
    let semaphore = Inner::new(buffer);
    let (tx, rx) = chan::channel(semaphore);
    (Tx(tx), Rx(rx))
}

impl<T> Tx<T> {
    pub async fn send(&self, value: T) -> Result<(), SendError> {
        // acquire semaphore first
        if self.0.chan.semaphore.acquire(1).await.is_err() {
            return Err(SendError::RxClosed);
        }
        self.0.send(value)
    }
}

impl<T> Rx<T> {
    pub async fn recv(&mut self) -> Option<T> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::channel;

    #[frosty::test]
    async fn tets_bounded_channel() {
        let (tx, mut rx) = channel(1);
        tx.send(1).await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), 1);

        drop(tx);
        assert_eq!(rx.recv().await, None);
    }
}
