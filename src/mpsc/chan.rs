use std::{
    cell::{Cell, RefCell},
    error::Error,
    fmt,
    rc::Rc,
    task::{Context, Poll, Waker},
};

use super::{block::Queue, semaphore::Semaphore};

pub(crate) fn channel<T, S>(semaphore: S) -> (Tx<T, S>, Rx<T, S>)
where
    S: Semaphore,
{
    let chan = Rc::new(Chan::new(semaphore));
    let tx = Tx::new(chan.clone());
    let rx = Rx::new(chan);
    (tx, rx)
}

pub(crate) struct Chan<T, S: Semaphore> {
    queue: RefCell<Queue<T>>,
    pub(crate) semaphore: S,
    rx_waker: Cell<Option<Waker>>,
    tx_count: Cell<usize>,
}

/// Error returned by `try_recv`.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum TryRecvError {
    /// This **channel** is currently empty, but the **Sender**(s) have not yet
    /// disconnected, so data may yet become available.
    Empty,
    /// The **channel**'s sending half has become disconnected, and there will
    /// never be any more data received on it.
    Disconnected,
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TryRecvError::Empty => "receiving on an empty channel".fmt(fmt),
            TryRecvError::Disconnected => "receiving on a closed channel".fmt(fmt),
        }
    }
}

impl Error for TryRecvError {}

impl<T, S> Chan<T, S>
where
    S: Semaphore,
{
    pub(crate) fn new(semaphore: S) -> Self {
        let queue = RefCell::new(Queue::new());
        Self {
            queue,
            semaphore,
            rx_waker: Cell::new(None),
            tx_count: Cell::new(0),
        }
    }
}

impl<T, S> Drop for Chan<T, S>
where
    S: Semaphore,
{
    fn drop(&mut self) {
        // consume all elements:
        // we cleared all elements on Rx drop, but there may still some
        // values sent after permits added.
        let mut queue = self.queue.borrow_mut();
        while !queue.is_empty() {
            drop(unsafe { queue.pop_unchecked() });
        }
        // drop all blocks of queue
        unsafe { queue.free_blocks() }
    }
}

pub(crate) struct Tx<T, S>
where
    S: Semaphore,
{
    pub(crate) chan: Rc<Chan<T, S>>,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum SendError {
    RxClosed,
}

pub(crate) struct Rx<T, S>
where
    S: Semaphore,
{
    chan: Rc<Chan<T, S>>,
}

impl<T, S> Tx<T, S>
where
    S: Semaphore,
{
    pub(crate) fn new(chan: Rc<Chan<T, S>>) -> Self {
        chan.tx_count.set(chan.tx_count.get() + 1);
        Self { chan }
    }

    // caller must make sure the chan has spaces
    pub(crate) fn send(&self, value: T) -> Result<(), SendError> {
        // check if the semaphore is closed
        if self.chan.semaphore.is_closed() {
            return Err(SendError::RxClosed);
        }

        // put data into the queue
        unsafe {
            self.chan.queue.borrow_mut().push_unchecked(value);
        }
        // if rx waker is set, wake it
        if let Some(w) = self.chan.rx_waker.replace(None) {
            w.wake();
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.chan.semaphore.is_closed()
    }

    /// Returns `true` if senders belong to the same channel.
    pub(crate) fn same_channel(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.chan, &other.chan)
    }
}

impl<T, S> Clone for Tx<T, S>
where
    S: Semaphore,
{
    fn clone(&self) -> Self {
        self.chan.tx_count.set(self.chan.tx_count.get() + 1);
        Self {
            chan: self.chan.clone(),
        }
    }
}

impl<T, S> Drop for Tx<T, S>
where
    S: Semaphore,
{
    fn drop(&mut self) {
        self.chan.tx_count.set(self.chan.tx_count.get() - 1);
    }
}

impl<T, S> Rx<T, S>
where
    S: Semaphore,
{
    pub(crate) fn new(chan: Rc<Chan<T, S>>) -> Self {
        Self { chan }
    }

    pub(crate) fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut queue = self.chan.queue.borrow_mut();
        if !queue.is_empty() {
            let val = unsafe { queue.pop_unchecked() };
            self.chan.semaphore.add_permits(1);
            return Ok(val);
        }
        if self.chan.tx_count.get() == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub(crate) fn recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut queue = self.chan.queue.borrow_mut();
        if !queue.is_empty() {
            let val = unsafe { queue.pop_unchecked() };
            self.chan.semaphore.add_permits(1);
            return Poll::Ready(Some(val));
        }
        if self.chan.tx_count.get() == 0 {
            return Poll::Ready(None);
        }
        self.chan.rx_waker.replace(Some(cx.waker().clone()));
        Poll::Pending
    }

    pub(crate) fn close(&mut self) {
        self.chan.semaphore.close();
    }
}

impl<T, S> Drop for Rx<T, S>
where
    S: Semaphore,
{
    fn drop(&mut self) {
        // close semaphore on close, this will make tx send await return.
        self.chan.semaphore.close();
        // consume all elements
        let mut queue = self.chan.queue.borrow_mut();
        let len = queue.len();
        while !queue.is_empty() {
            drop(unsafe { queue.pop_unchecked() });
        }
        self.chan.semaphore.add_permits(len);
    }
}

#[cfg(test)]
mod tests {
    use super::channel;
    use crate::semaphore::Inner;
    use futures_util::future::poll_fn;

    #[monoio::test]
    async fn test_chan() {
        let semaphore = Inner::new(1);
        let (tx, mut rx) = channel::<u32, _>(semaphore);
        assert!(tx.send(1).is_ok());
        assert_eq!(poll_fn(|cx| rx.recv(cx)).await, Some(1));

        // close rx
        rx.close();
        assert!(tx.is_closed());
    }
}
