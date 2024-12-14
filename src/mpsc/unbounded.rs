use super::{
    chan::{self, SendError, TryRecvError},
    semaphore::Unlimited,
};
use futures_core::Stream;
use futures_util::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct Tx<T>(chan::Tx<T, Unlimited>);

pub struct Rx<T>(chan::Rx<T, Unlimited>);

pub fn channel<T>() -> (Tx<T>, Rx<T>) {
    let semaphore = Unlimited::new();
    let (tx, rx) = chan::channel(semaphore);
    (Tx(tx), Rx(rx))
}

impl<T> Tx<T> {
    pub fn send(&self, value: T) -> Result<(), SendError> {
        self.0.send(value)
    }

    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    pub fn same_channel(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
}

impl<T> Clone for Tx<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
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

impl<T> Stream for Rx<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::channel;

    #[monoio::test]
    async fn tets_unbounded_channel() {
        let (tx, mut rx) = channel();
        tx.send(1).unwrap();
        assert_eq!(rx.recv().await.unwrap(), 1);

        drop(tx);
        assert_eq!(rx.recv().await, None);
    }
}
