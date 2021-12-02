use super::chan::{self, SendError, TryRecvError};
use crate::semaphore::Inner;
use futures_util::future::poll_fn;
use std::task::{Context, Poll};

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
        self.0
            .chan
            .semaphore
            .acquire(1)
            .await
            .map_err(|_| SendError::RxClosed)?;
        self.0.send(value)
    }

    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    pub fn same_channel(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
}

impl<T> Rx<T> {
    pub async fn recv(&mut self) -> Option<T> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.recv(cx)
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.0.try_recv()
    }

    pub fn close(&mut self) {
        self.0.close()
    }
}

#[cfg(test)]
mod tests {
    use super::channel;

    #[monoio::test]
    async fn tets_bounded_channel() {
        let (tx, mut rx) = channel(1);
        tx.send(1).await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), 1);

        drop(tx);
        assert_eq!(rx.recv().await, None);
    }
}
