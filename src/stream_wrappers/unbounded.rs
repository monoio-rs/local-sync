use crate::mpsc::unbounded::Rx;
use futures_core::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A wrapper around [`local_sync::mpsc::unbounded::Receiver`] that implements [`Stream`].
///
/// [`local_sync::mpsc::unbounded::Receiver`]: struct@local_sync::mpsc::unbounded::Receiver
/// [`Stream`]: trait@futures_core::Stream
pub struct ReceiverStream<T> {
    inner: Rx<T>,
}

impl<T> std::fmt::Debug for ReceiverStream<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiverStream").finish()
    }
}

impl<T> ReceiverStream<T> {
    /// Create a new `ReceiverStream`.
    pub fn new(recv: Rx<T>) -> Self {
        Self { inner: recv }
    }

    /// Get back the inner `Receiver`.
    #[allow(dead_code)]
    pub fn into_inner(self) -> Rx<T> {
        self.inner
    }

    /// Closes the receiving half of a channel without dropping it.
    ///
    /// This prevents any further messages from being sent on the channel while
    /// still enabling the receiver to drain messages that are buffered. Any
    /// outstanding [`Permit`] values will still be able to send messages.
    ///
    /// To guarantee no messages are dropped, after calling `close()`, you must
    /// receive all items from the stream until `None` is returned.
    ///
    /// [`Permit`]: struct@tokio::sync::mpsc::Permit
    #[allow(dead_code)]
    pub fn close(&mut self) {
        self.inner.close()
    }
}

impl<T> Stream for ReceiverStream<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_recv(cx)
    }
}

impl<T> AsRef<Rx<T>> for ReceiverStream<T> {
    fn as_ref(&self) -> &Rx<T> {
        &self.inner
    }
}

impl<T> AsMut<Rx<T>> for ReceiverStream<T> {
    fn as_mut(&mut self) -> &mut Rx<T> {
        &mut self.inner
    }
}

impl<T> From<Rx<T>> for ReceiverStream<T> {
    fn from(recv: Rx<T>) -> Self {
        Self::new(recv)
    }
}
