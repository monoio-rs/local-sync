//! A multi-producer, multi-consumer broadcast channel. Each sent value is received by
//! all consumers.
//!
//! A [`Sender`] is used to broadcast values to **all** connected [`Receiver`]
//! instances. [`Sender`] handles are cloneable, allowing concurrent send and
//! receive actions. [`Sender`] and [`Receiver`] are both non-thread-safe and should be
//! used within the same thread.
//!
//! When a value is sent, **all** [`Receiver`] handles are notified and will
//! receive the value. The value is stored once inside the channel and cloned on
//! demand for each receiver. Once all receivers have received a clone of the value,
//! the value is released from the channel.
//!
//! A channel is created by calling [`channel`], specifying the maximum number
//! of messages the channel can retain at any given time.
//!
//! New [`Receiver`] handles are created by calling [`Sender::subscribe`]. The
//! returned [`Receiver`] will receive values sent **after** the call to
//! `subscribe`.
//!
//! This channel is also suitable for the single-producer multi-consumer
//! use-case, where a single sender broadcasts values to many receivers.
//!
//! # Lagging
//!
//! As sent messages must be retained until **all** [`Receiver`] handles receive
//! a clone, broadcast channels are susceptible to the "slow receiver" problem.
//! In this case, all but one receiver are able to receive values at the rate
//! they are sent. Because one receiver is stalled, the channel starts to fill
//! up.
//!
//! This broadcast channel implementation handles this case by setting a hard
//! upper bound on the number of values the channel may retain at any given
//! time. This upper bound is passed to the [`channel`] function as an argument.
//!
//! If a value is sent when the channel is at capacity, the oldest value
//! currently held by the channel is released. This frees up space for the new
//! value. Any receiver that has not yet seen the released value will return
//! [`RecvError::Lagged`] the next time [`recv`] is called.
//!
//! Once [`RecvError::Lagged`] is returned, the lagging receiver's position is
//! updated to the oldest value contained by the channel. The next call to
//! [`recv`] will return this value.
//!
//! This behavior enables a receiver to detect when it has lagged so far behind
//! that data has been dropped. The caller may decide how to respond to this:
//! either by aborting its task or by tolerating lost messages and resuming
//! consumption of the channel.
//!
//! # Closing
//!
//! When **all** [`Sender`] handles have been dropped, no new values may be
//! sent. At this point, the channel is "closed". Once a receiver has received
//! all values retained by the channel, the next call to [`recv`] will return
//! with [`RecvError::Closed`].
//!
//! When a [`Receiver`] handle is dropped, any messages not read by the receiver
//! will be marked as read. If this receiver was the only one not to have read
//! that message, the message will be dropped at this point.
//!
//! # Examples
//!
//! Basic usage:
//!
//! ```
//! use local_sync::broadcast;
//!
//! #[monoio::main]
//! async fn main() {
//!     let (tx, mut rx1) = broadcast::channel(16);
//!     let mut rx2 = tx.subscribe();
//!
//!     monoio::spawn(async move {
//!         assert_eq!(rx1.recv().await.unwrap(), 10);
//!         assert_eq!(rx1.recv().await.unwrap(), 20);
//!     });
//!
//!     monoio::spawn(async move {
//!         assert_eq!(rx2.recv().await.unwrap(), 10);
//!         assert_eq!(rx2.recv().await.unwrap(), 20);
//!     });
//!
//!     tx.send(10).unwrap();
//!     tx.send(20).unwrap();
//! }
//! ```
//!
//! Handling lag:
//!
//! ```
//! use local_sync::broadcast::{self, error::RecvError};
//!
//! #[monoio::main]
//! async fn main() {
//!     let (tx, mut rx) = broadcast::channel(2);
//!
//!     tx.send(10).unwrap();
//!     tx.send(20).unwrap();
//!     tx.send(30).unwrap();
//!
//!     // The receiver lagged behind
//!     assert!(matches!(rx.recv().await, Err(RecvError::Lagged(_))));
//!
//!     // At this point, we can abort or continue with lost messages
//!
//!     assert_eq!(rx.recv().await.unwrap(), 20);
//!     assert_eq!(rx.recv().await.unwrap(), 30);
//! }
//! ```

use crate::wake_list::WakeList;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

/// Create a bounded, multi-producer, multi-consumer channel where each sent
/// value is broadcasted to all active receivers.
///
/// **Note:** The actual capacity will be rounded up to the next power of 2.
///
/// All data sent on [`Sender`] will become available on every active
/// [`Receiver`] in the same order as it was sent.
///
/// The `Sender` can be cloned to `send` to the same channel from multiple
/// points in the process. New `Receiver` handles are created by calling
/// [`Sender::subscribe`].
///
/// If all [`Receiver`] handles are dropped, the `send` method will return a
/// [`SendError`]. Similarly, if all [`Sender`] handles are dropped, the [`recv`]
/// method will return a [`RecvError`].
///
/// # Examples
///
/// ```
/// use local_sync::broadcast;
///
/// #[monoio::main]
/// async fn main() {
///     let (tx, mut rx1) = broadcast::channel(16);
///     let mut rx2 = tx.subscribe();
///
///     monoio::spawn(async move {
///         assert_eq!(rx1.recv().await.unwrap(), 10);
///         assert_eq!(rx1.recv().await.unwrap(), 20);
///     });
///
///     monoio::spawn(async move {
///         assert_eq!(rx2.recv().await.unwrap(), 10);
///         assert_eq!(rx2.recv().await.unwrap(), 20);
///     });
///
///     tx.send(10).unwrap();
///     tx.send(20).unwrap();
/// }
/// ```
///
/// # Panics
///
/// This will panic if `capacity` is equal to `0`.
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0, "broadcast channel capacity cannot be zero");

    // Round up to the next power of 2
    let cap = capacity.next_power_of_two();
    let mut buffer = Vec::with_capacity(cap);

    for _ in 0..cap {
        buffer.push(RefCell::new(Slot {
            rem: Cell::new(0),
            pos: 0,
            val: RefCell::new(None),
        }));
    }

    let shared = Rc::new(Shared {
        buffer,
        mask: cap - 1,
        tail: RefCell::new(Tail {
            pos: 0,
            rx_cnt: 1,
            closed: false,
            wakers: WakeList::new(),
        }),
        num_tx: Cell::new(1),
    });

    let tx = Sender {
        shared: shared.clone(),
    };

    let rx = Receiver { shared, next: 0 };

    (tx, rx)
}

/// Sending-half of the [`broadcast`] channel.
/// Must only be used from the same thread. Messages can be sent with
/// [`send`][Sender::send].
///
/// # Examples
///
/// ```
/// use local_sync::broadcast;
///
/// #[monoio::main]
/// async fn main() {
///     let (tx, mut rx1) = broadcast::channel(16);
///     let mut rx2 = tx.subscribe();
///
///     monoio::spawn(async move {
///         assert_eq!(rx1.recv().await.unwrap(), 10);
///         assert_eq!(rx1.recv().await.unwrap(), 20);
///     });
///
///     monoio::spawn(async move {
///         assert_eq!(rx2.recv().await.unwrap(), 10);
///         assert_eq!(rx2.recv().await.unwrap(), 20);
///     });
///
///     tx.send(10).unwrap();
///     tx.send(20).unwrap();
/// }
/// ```
///
/// [`broadcast`]: crate::broadcast
pub struct Sender<T> {
    shared: Rc<Shared<T>>,
}

/// Receiving-half of the [`broadcast`] channel.
///
/// Must not be used concurrently. Messages may be retrieved using
/// [`recv`][Receiver::recv].
///
/// # Examples
///
/// ```
/// use local_sync::broadcast;
///
/// #[monoio::main]
/// async fn main() {
///     let (tx, mut rx1) = broadcast::channel(16);
///     let mut rx2 = tx.subscribe();
///
///     monoio::spawn(async move {
///         assert_eq!(rx1.recv().await.unwrap(), 10);
///         assert_eq!(rx1.recv().await.unwrap(), 20);
///     });
///
///     monoio::spawn(async move {
///         assert_eq!(rx2.recv().await.unwrap(), 10);
///         assert_eq!(rx2.recv().await.unwrap(), 20);
///     });
///
///     tx.send(10).unwrap();
///     tx.send(20).unwrap();
/// }
/// ```
///
/// [`broadcast`]: crate::broadcast
pub struct Receiver<T> {
    shared: Rc<Shared<T>>,
    next: u64,
}

pub mod error {
    //! Broadcast error types
    //!
    //! This module contains the error types for the broadcast channel.

    use std::fmt;

    /// Error returned by the [`send`] function on a [`Sender`].
    ///
    /// A **send** operation can only fail if there are no active receivers,
    /// implying that the message could never be received. The error contains the
    /// message being sent as a payload so it can be recovered.
    ///
    /// [`send`]: crate::broadcast::Sender::send
    /// [`Sender`]: crate::broadcast::Sender
    #[derive(Debug)]
    pub struct SendError<T>(pub T);

    impl<T> fmt::Display for SendError<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "channel closed")
        }
    }

    impl<T: fmt::Debug> std::error::Error for SendError<T> {}

    /// An error returned from the [`recv`] function on a [`Receiver`].
    ///
    /// [`recv`]: crate::broadcast::Receiver::recv
    /// [`Receiver`]: crate::broadcast::Receiver
    #[derive(Debug, PartialEq, Eq, Clone)]
    pub enum RecvError {
        /// There are no more active senders implying no further messages will ever
        /// be sent.
        Closed,

        /// The receiver lagged too far behind. Attempting to receive again will
        /// return the oldest message still retained by the channel.
        ///
        /// Includes the number of skipped messages.
        Lagged(u64),
    }

    impl fmt::Display for RecvError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                RecvError::Closed => write!(f, "channel closed"),
                RecvError::Lagged(amt) => write!(f, "channel lagged by {}", amt),
            }
        }
    }

    impl std::error::Error for RecvError {}

    /// An error returned from the [`try_recv`] function on a [`Receiver`].
    ///
    /// [`try_recv`]: crate::broadcast::Receiver::try_recv
    /// [`Receiver`]: crate::broadcast::Receiver
    #[derive(Debug, PartialEq, Eq, Clone)]
    pub enum TryRecvError {
        /// The channel is currently empty. There are still active
        /// [`Sender`] handles, so data may yet become available.
        ///
        /// [`Sender`]: crate::broadcast::Sender
        Empty,

        /// There are no more active senders implying no further messages will ever
        /// be sent.
        Closed,

        /// The receiver lagged too far behind and has been forcibly disconnected.
        /// Attempting to receive again will return the oldest message still
        /// retained by the channel.
        ///
        /// Includes the number of skipped messages.
        Lagged(u64),
    }

    impl fmt::Display for TryRecvError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                TryRecvError::Empty => write!(f, "channel empty"),
                TryRecvError::Closed => write!(f, "channel closed"),
                TryRecvError::Lagged(amt) => write!(f, "channel lagged by {}", amt),
            }
        }
    }

    impl std::error::Error for TryRecvError {}
}

use error::{RecvError, SendError, TryRecvError};

/// Data shared between senders and receivers.
struct Shared<T> {
    /// slots in the channel.
    buffer: Vec<RefCell<Slot<T>>>,

    /// Mask a position -> index.
    mask: usize,

    /// Tail of the queue.
    tail: RefCell<Tail>,

    /// Number of outstanding Sender handles.
    num_tx: Cell<usize>,
}

/// Next position to write a value.
struct Tail {
    /// Next position to write to.
    pos: u64,

    /// Number of active receivers.
    rx_cnt: usize,

    /// True if the channel is closed.
    closed: bool,

    /// Receivers waiting for a value.
    wakers: WakeList,
}

/// Slot in the buffer.
struct Slot<T> {
    /// Remaining number of receivers that are expected to see this value.
    ///
    /// When this goes to zero, the value is released.
    rem: Cell<usize>,

    /// Uniquely identifies the `send` stored in the slot.
    pos: u64,

    /// The value being broadcast.
    ///
    /// The value is set by `send`. When a reader drops, `rem` is decremented.
    /// When it hits zero, the value is dropped.
    val: RefCell<Option<T>>,
}

impl<T> Sender<T> {
    /// Attempts to send a value to all active [`Receiver`] handles.
    ///
    /// If this function returns an error, the value was not sent to any receivers
    /// and the channel has been closed.
    ///
    /// # Examples
    ///
    /// ```
    /// use local_sync::broadcast;
    ///
    /// #[monoio::main]
    /// async fn main() {
    ///     let (tx, mut rx1) = broadcast::channel(16);
    ///     let mut rx2 = tx.subscribe();
    ///
    ///     tx.send(10).unwrap();
    ///     tx.send(20).unwrap();
    ///
    ///     assert_eq!(rx1.recv().await.unwrap(), 10);
    ///     assert_eq!(rx2.recv().await.unwrap(), 10);
    ///     assert_eq!(rx1.recv().await.unwrap(), 20);
    ///     assert_eq!(rx2.recv().await.unwrap(), 20);
    /// }
    /// ```
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        let mut tail = self.shared.tail.borrow_mut();
        if tail.rx_cnt == 0 || tail.closed {
            return Err(SendError(value));
        }

        let idx = tail.pos as usize & self.shared.mask;
        let slot = &self.shared.buffer[idx];
        let mut slot = slot.borrow_mut();

        slot.pos = tail.pos;
        slot.rem.set(tail.rx_cnt);
        *slot.val.borrow_mut() = Some(value);

        tail.pos = tail.pos.wrapping_add(1);

        // Wake all waiting receivers
        tail.wakers.wake_all();

        Ok(())
    }

    /// Creates a new [`Receiver`] handle that will receive values sent **after**
    /// this call to `subscribe`.
    ///
    /// # Examples
    ///
    /// ```
    /// use local_sync::broadcast;
    ///
    /// #[monoio::main]
    /// async fn main() {
    ///     let (tx, _rx) = broadcast::channel(16);
    ///
    ///     // Will not be seen
    ///     tx.send(10).unwrap();
    ///
    ///     let mut rx = tx.subscribe();
    ///
    ///     tx.send(20).unwrap();
    ///
    ///     let value = rx.recv().await.unwrap();
    ///     assert_eq!(20, value);
    /// }
    /// ```
    pub fn subscribe(&self) -> Receiver<T> {
        let shared = self.shared.clone();
        new_receiver(shared)
    }

    /// Returns the number of active receivers.
    ///
    /// An active receiver is a [`Receiver`] handle returned from [`channel`] or
    /// [`subscribe`]. These are the handles that will receive values sent on
    /// this [`Sender`].
    ///
    /// # Note
    ///
    /// It is not guaranteed that a sent message will reach this number of
    /// receivers. Active receivers may never call [`recv`] again before
    /// dropping.
    ///
    /// # Examples
    ///
    /// ```
    /// use local_sync::broadcast;
    ///
    /// #[monoio::main]
    /// async fn main() {
    ///     let (tx, _rx1) = broadcast::channel(16);
    ///
    ///     assert_eq!(1, tx.receiver_count());
    ///
    ///     let mut _rx2 = tx.subscribe();
    ///
    ///     assert_eq!(2, tx.receiver_count());
    ///
    ///     tx.send(10).unwrap();
    /// }
    /// ```
    pub fn receiver_count(&self) -> usize {
        self.shared.tail.borrow().rx_cnt
    }

    /// Returns whether the channel is closed without needing to await.
    ///
    /// This happens when all receivers have been dropped.
    ///
    /// A return value of `true` means that a subsequent [`send`](Sender::send)
    /// will return an error.
    ///
    /// # Examples
    ///
    /// ```
    /// use local_sync::broadcast;
    ///
    /// let (tx, rx) = broadcast::channel::<()>(100);
    ///
    /// assert!(!tx.is_closed());
    ///
    /// drop(rx);
    ///
    /// assert!(tx.is_closed());
    /// ```
    pub fn is_closed(&self) -> bool {
        self.shared.tail.borrow().rx_cnt == 0
    }

    /// Closes the channel without sending a message.
    ///
    /// This prevents the channel from sending any new messages. Current
    /// receivers may still receive any values buffered, but will receive
    /// an error when attempting to receive additional messages after the buffer
    /// has been drained.
    ///
    /// # Examples
    ///
    /// ```
    /// use local_sync::broadcast;
    /// use local_sync::broadcast::error::RecvError;
    ///
    /// #[monoio::main]
    /// async fn main() {
    ///     let (tx, mut rx) = broadcast::channel(16);
    ///
    ///     // Close the channel
    ///     tx.close();
    ///
    ///     // After closing, receivers should get a Closed error
    ///     assert_eq!(rx.recv().await, Err(RecvError::Closed));
    ///
    ///     // Sending after close should fail
    ///     assert!(tx.send(10).is_err());
    /// }
    /// ```
    pub fn close(&self) {
        let mut tail = self.shared.tail.borrow_mut();
        tail.closed = true;
        tail.wakers.wake_all();
    }

    /// Returns `true` if senders belong to the same channel.
    ///
    /// # Examples
    ///
    /// ```
    /// use local_sync::broadcast;
    ///
    /// let (tx1, _) = broadcast::channel::<()>(16);
    /// let tx2 = tx1.clone();
    ///
    /// assert!(tx1.same_channel(&tx2));
    ///
    /// let (tx3, _) = broadcast::channel::<()>(16);
    ///
    /// assert!(!tx1.same_channel(&tx3));
    /// ```
    pub fn same_channel(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.shared, &other.shared)
    }
}

/// Create a new `Receiver` which reads starting from the tail.
fn new_receiver<T>(shared: Rc<Shared<T>>) -> Receiver<T> {
    let mut tail = shared.tail.borrow_mut();

    tail.rx_cnt = tail.rx_cnt.checked_add(1).expect("overflow");

    let next = tail.pos;

    drop(tail);

    Receiver { shared, next }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.num_tx.set(self.shared.num_tx.get() + 1);
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let count = self.shared.num_tx.get();
        self.shared.num_tx.set(count - 1);
        if count == 1 {
            let mut tail = self.shared.tail.borrow_mut();
            tail.closed = true;
            // Wake all waiting receivers
            tail.wakers.wake_all();
        }
    }
}

impl<T: Clone> Receiver<T> {
    pub fn resubscribe(&self) -> Self {
        let shared = self.shared.clone();
        new_receiver(shared)
    }

    /// Returns the number of messages that were sent into the channel and that
    /// this [`Receiver`] has yet to receive.
    ///
    /// If the returned value from `len` is larger than the next largest power of 2
    /// of the capacity of the channel any call to [`recv`] will return an
    /// `Err(RecvError::Lagged)` and any call to [`try_recv`] will return an
    /// `Err(TryRecvError::Lagged)`, e.g. if the capacity of the channel is 10,
    /// [`recv`] will start to return `Err(RecvError::Lagged)` once `len` returns
    /// values larger than 16.
    ///
    /// # Examples
    ///
    /// ```
    /// use local_sync::broadcast;
    ///
    /// #[monoio::main]
    /// async fn main() {
    ///     let (tx, mut rx1) = broadcast::channel(16);
    ///
    ///     tx.send(10).unwrap();
    ///     tx.send(20).unwrap();
    ///
    ///     assert_eq!(rx1.len(), 2);
    ///     assert_eq!(rx1.recv().await.unwrap(), 10);
    ///     assert_eq!(rx1.len(), 1);
    ///     assert_eq!(rx1.recv().await.unwrap(), 20);
    ///     assert_eq!(rx1.len(), 0);
    /// }
    /// ```
    pub fn len(&self) -> usize {
        let tail = self.shared.tail.borrow();
        (tail.pos - self.next) as usize
    }

    /// Returns true if there aren't any messages in the channel that the [`Receiver`]
    /// has yet to receive.
    ///
    /// # Examples
    ///
    /// ```
    /// use local_sync::broadcast;
    ///
    /// #[monoio::main]
    /// async fn main() {
    ///     let (tx, mut rx1) = broadcast::channel(16);
    ///
    ///     assert!(rx1.is_empty());
    ///
    ///     tx.send(10).unwrap();
    ///     tx.send(20).unwrap();
    ///
    ///     assert!(!rx1.is_empty());
    ///     assert_eq!(rx1.recv().await.unwrap(), 10);
    ///     assert_eq!(rx1.recv().await.unwrap(), 20);
    ///     assert!(rx1.is_empty());
    /// }
    /// ```
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` if receivers belong to the same channel.
    ///
    /// # Examples
    ///
    /// ```
    /// use local_sync::broadcast;
    ///
    /// #[monoio::main]
    /// async fn main() {
    ///     let (tx, rx) = broadcast::channel::<()>(16);
    ///     let rx2 = tx.subscribe();
    ///
    ///     assert!(rx.same_channel(&rx2));
    ///
    ///     let (_tx3, rx3) = broadcast::channel::<()>(16);
    ///
    ///     assert!(!rx3.same_channel(&rx2));
    /// }
    /// ```
    pub fn same_channel(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.shared, &other.shared)
    }

    pub async fn recv(&mut self) -> Result<T, RecvError> {
        Recv { receiver: self }.await
    }

    /// Attempts to return a pending value on this receiver without awaiting.
    ///
    /// This is useful for a flavor of "optimistic check" before deciding to
    /// await on a receiver.
    ///
    /// Compared with [`recv`], this function has three failure cases instead of two
    /// (one for closed, one for an empty buffer, one for a lagging receiver).
    ///
    /// `Err(TryRecvError::Closed)` is returned when all `Sender` halves have
    /// dropped, indicating that no further values can be sent on the channel.
    ///
    /// If the [`Receiver`] handle falls behind, once the channel is full, newly
    /// sent values will overwrite old values. At this point, a call to [`recv`]
    /// will return with `Err(TryRecvError::Lagged)` and the [`Receiver`]'s
    /// internal cursor is updated to point to the oldest value still held by
    /// the channel. A subsequent call to [`try_recv`] will return this value
    /// **unless** it has been since overwritten. If there are no values to
    /// receive, `Err(TryRecvError::Empty)` is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// use local_sync::broadcast;
    ///
    /// #[monoio::main]
    /// async fn main() {
    ///     let (tx, mut rx) = broadcast::channel(16);
    ///
    ///     assert!(rx.try_recv().is_err());
    ///
    ///     tx.send(10).unwrap();
    ///
    ///     let value = rx.try_recv().unwrap();
    ///     assert_eq!(10, value);
    /// }
    /// ```
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let tail = self.shared.tail.borrow();
        if self.next == tail.pos {
            if tail.closed || self.shared.num_tx.get() == 0 {
                return Err(TryRecvError::Closed);
            }
            return Err(TryRecvError::Empty);
        }

        let idx = self.next as usize & self.shared.mask;
        let slot = &self.shared.buffer[idx];
        let slot = slot.borrow();

        if slot.pos != self.next {
            // We've lagged behind, calculate by how much
            let next = tail.pos.wrapping_sub(self.shared.buffer.len() as u64);
            let missed = next.wrapping_sub(self.next);
            self.next = next;
            return Err(TryRecvError::Lagged(missed));
        }

        let value = slot.val.borrow().clone();
        if let Some(value) = value {
            let rem = slot.rem.get();
            if rem > 1 {
                slot.rem.set(rem - 1);
            } else {
                *slot.val.borrow_mut() = None;
            }
            self.next = self.next.wrapping_add(1);
            Ok(value)
        } else {
            Err(TryRecvError::Closed)
        }
    }
}

/// Receive a value future.
struct Recv<'a, T> {
    /// Receiver being waited on.
    receiver: &'a mut Receiver<T>,
}

impl<'a, T: Clone> Future for Recv<'a, T> {
    type Output = Result<T, RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        match self.receiver.try_recv() {
            Ok(value) => Poll::Ready(Ok(value)),
            Err(TryRecvError::Empty) => {
                // Register waker
                if self.receiver.shared.tail.borrow_mut().wakers.can_push() {
                    self.receiver
                        .shared
                        .tail
                        .borrow_mut()
                        .wakers
                        .push(cx.waker().clone());
                } else {
                }
                Poll::Pending
            }
            Err(TryRecvError::Lagged(n)) => Poll::Ready(Err(RecvError::Lagged(n))),
            Err(TryRecvError::Closed) => Poll::Ready(Err(RecvError::Closed)),
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.drop_receiver(self.next);
    }
}

impl<T> Shared<T> {
    fn drop_receiver(&self, next: u64) {
        let mut tail = self.tail.borrow_mut();
        tail.rx_cnt -= 1;

        // Iterate from 'next' to 'tail.pos' to decrement 'rem' counters.
        for pos in next..tail.pos {
            let idx = (pos as usize) & self.mask;
            let slot = &self.buffer[idx];
            let slot = slot.borrow();

            if slot.pos == pos {
                if slot.rem.get() > 0 {
                    slot.rem.set(slot.rem.get() - 1);
                }

                // If no receivers are waiting for this slot, drop the value.
                if slot.rem.get() == 0 {
                    *slot.val.borrow_mut() = None;
                }
            }
        }

        // If no receivers are left and the channel is not closed, mark it as closed.
        if tail.rx_cnt == 0 && !tail.closed {
            tail.closed = true;
            tail.wakers.wake_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[monoio::test]
    async fn basic_usage() {
        let (tx, mut rx1) = channel(16);
        let mut rx2 = tx.subscribe();

        tx.send(10).unwrap();
        tx.send(20).unwrap();

        assert_eq!(rx1.recv().await.unwrap(), 10);
        assert_eq!(rx2.recv().await.unwrap(), 10);
        assert_eq!(rx1.recv().await.unwrap(), 20);
        assert_eq!(rx2.recv().await.unwrap(), 20);
    }

    #[monoio::test]
    async fn lagged_receiver() {
        let (tx, mut rx) = channel(2);

        tx.send(10).unwrap();
        tx.send(20).unwrap();
        tx.send(30).unwrap();

        assert!(matches!(rx.recv().await, Err(RecvError::Lagged(_))));
        assert_eq!(rx.recv().await.unwrap(), 20);
        assert_eq!(rx.recv().await.unwrap(), 30);
    }

    #[test]
    fn receiver_count_on_channel_constructor() {
        let (sender, _) = channel::<i32>(16);
        assert_eq!(sender.receiver_count(), 0);

        let rx_1 = sender.subscribe();
        assert_eq!(sender.receiver_count(), 1);

        let rx_2 = rx_1.resubscribe();
        assert_eq!(sender.receiver_count(), 2);

        let rx_3 = sender.subscribe();
        assert_eq!(sender.receiver_count(), 3);

        drop(rx_3);
        drop(rx_1);
        assert_eq!(sender.receiver_count(), 1);

        drop(rx_2);
        assert_eq!(sender.receiver_count(), 0);
    }
}
