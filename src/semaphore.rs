//! Semaphore borrowed from tokio.

#![allow(unused)]

use core::future::Future;
use std::{
    cell::{RefCell, UnsafeCell},
    cmp, fmt,
    marker::PhantomPinned,
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll, Waker},
};

use crate::{
    linked_list::{self, LinkedList},
    wake_list::WakeList,
};

/// Low level semaphore.
pub(crate) struct Inner {
    waiters: RefCell<Waitlist>,
    /// The current number of available permits in the semaphore.
    permits: RefCell<usize>,
}

struct Waitlist {
    queue: LinkedList<Waiter, <Waiter as linked_list::Link>::Target>,
    closed: bool,
}

/// Error returned from the [`Semaphore::try_acquire`] function.
///
/// [`Semaphore::try_acquire`]: crate::sync::Semaphore::try_acquire
#[derive(Debug, PartialEq)]
pub enum TryAcquireError {
    /// The semaphore has been [closed] and cannot issue new permits.
    ///
    /// [closed]: crate::sync::Semaphore::close
    Closed,

    /// The semaphore has no available permits.
    NoPermits,
}

/// Error returned from the [`Semaphore::acquire`] function.
///
/// An `acquire` operation can only fail if the semaphore has been
/// [closed].
///
/// [closed]: crate::sync::Semaphore::close
/// [`Semaphore::acquire`]: crate::sync::Semaphore::acquire
#[derive(Debug)]
pub struct AcquireError(());

pub(crate) struct Acquire<'a> {
    node: Waiter,
    semaphore: &'a Inner,
    num_permits: u32,
    queued: bool,
}

struct Waiter {
    /// The current state of the waiter.
    ///
    /// This is either the number of remaining permits required by
    /// the waiter, or a flag indicating that the waiter is not yet queued.
    state: RefCell<usize>,

    /// The waker to notify the task awaiting permits.
    ///
    /// # Safety
    ///
    /// This may only be accessed while the wait queue is locked.
    waker: UnsafeCell<Option<Waker>>,

    /// Intrusive linked-list pointers.
    ///
    /// # Safety
    ///
    /// This may only be accessed while the wait queue is locked.
    ///
    /// TODO: Ideally, we would be able to use loom to enforce that
    /// this isn't accessed concurrently. However, it is difficult to
    /// use a `UnsafeCell` here, since the `Link` trait requires _returning_
    /// references to `Pointers`, and `UnsafeCell` requires that checked access
    /// take place inside a closure. We should consider changing `Pointers` to
    /// use `UnsafeCell` internally.
    pointers: linked_list::Pointers<Waiter>,

    /// Should not be `Unpin`.
    _p: PhantomPinned,
}

impl Waiter {
    fn new(num_permits: u32) -> Self {
        Waiter {
            waker: UnsafeCell::new(None),
            state: RefCell::new(num_permits as usize),
            pointers: linked_list::Pointers::new(),
            _p: PhantomPinned,
        }
    }

    /// Assign permits to the waiter.
    ///
    /// Returns `true` if the waiter should be removed from the queue
    fn assign_permits(&self, n: &mut usize) -> bool {
        let mut curr = self.state.borrow_mut();
        let assign = cmp::min(*curr, *n);
        *curr -= assign;
        *n -= assign;

        *curr == 0
    }
}

unsafe impl linked_list::Link for Waiter {
    // XXX: ideally, we would be able to use `Pin` here, to enforce the
    // invariant that list entries may not move while in the list. However, we
    // can't do this currently, as using `Pin<&'a mut Waiter>` as the `Handle`
    // type would require `Semaphore` to be generic over a lifetime. We can't
    // use `Pin<*mut Waiter>`, as raw pointers are `Unpin` regardless of whether
    // or not they dereference to an `!Unpin` target.
    type Handle = NonNull<Waiter>;
    type Target = Waiter;

    fn as_raw(handle: &Self::Handle) -> NonNull<Waiter> {
        *handle
    }

    unsafe fn from_raw(ptr: NonNull<Waiter>) -> NonNull<Waiter> {
        ptr
    }

    unsafe fn pointers(mut target: NonNull<Waiter>) -> NonNull<linked_list::Pointers<Waiter>> {
        NonNull::from(&mut target.as_mut().pointers)
    }
}

impl Inner {
    /// The maximum number of permits which a semaphore can hold.
    ///
    /// Note that this reserves three bits of flags in the permit counter, but
    /// we only actually use one of them. However, the previous semaphore
    /// implementation used three bits, so we will continue to reserve them to
    /// avoid a breaking change if additional flags need to be added in the
    /// future.
    pub(crate) const MAX_PERMITS: usize = std::usize::MAX >> 3;
    const CLOSED: usize = 1;
    // The least-significant bit in the number of permits is reserved to use
    // as a flag indicating that the semaphore has been closed. Consequently
    // PERMIT_SHIFT is used to leave that bit for that purpose.
    const PERMIT_SHIFT: usize = 1;

    /// Creates a new semaphore with the initial number of permits
    ///
    /// Maximum number of permits on 32-bit platforms is `1<<29`.
    pub(crate) const fn new(mut permits: usize) -> Self {
        permits &= Self::MAX_PERMITS;

        Self {
            permits: RefCell::new(permits << Self::PERMIT_SHIFT),
            waiters: RefCell::new(Waitlist {
                queue: LinkedList::new(),
                closed: false,
            }),
        }
    }

    /// Returns the current number of available permits
    pub(crate) fn available_permits(&self) -> usize {
        *self.permits.borrow() >> Self::PERMIT_SHIFT
    }

    /// Adds `added` new permits to the semaphore.
    ///
    /// The maximum number of permits is `usize::MAX >> 3`, and this function will panic if the limit is exceeded.
    pub(crate) fn release(&self, added: usize) {
        if added == 0 {
            return;
        }

        // Assign permits to the wait queue
        self.add_permits(added);
    }

    /// Closes the semaphore. This prevents the semaphore from issuing new
    /// permits and notifies all pending waiters.
    pub(crate) fn close(&self) {
        *self.permits.borrow_mut() |= Self::CLOSED;
        (*self.waiters.borrow_mut()).closed = true;

        let mut waiters = self.waiters.borrow_mut();

        while let Some(mut waiter) = waiters.queue.pop_back() {
            let waker = unsafe { (*waiter.as_mut().waker.get()).take() };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    /// Returns true if the semaphore is closed
    pub(crate) fn is_closed(&self) -> bool {
        *self.permits.borrow() & Self::CLOSED != 0
    }

    pub(crate) fn try_acquire(&self, num_permits: u32) -> Result<(), TryAcquireError> {
        assert!(
            num_permits as usize <= Self::MAX_PERMITS,
            "a semaphore may not have more than MAX_PERMITS permits ({})",
            Self::MAX_PERMITS
        );
        let num_permits = (num_permits as usize) << Self::PERMIT_SHIFT;
        let mut curr = self.permits.borrow_mut();

        // Has the semaphore closed?
        if *curr & Self::CLOSED == Self::CLOSED {
            return Err(TryAcquireError::Closed);
        }

        // Are there enough permits remaining?
        if *curr < num_permits {
            return Err(TryAcquireError::NoPermits);
        }

        *curr -= num_permits;
        Ok(())
    }

    pub(crate) fn acquire(&self, num_permits: u32) -> Acquire<'_> {
        Acquire::new(self, num_permits)
    }

    /// Release `rem` permits to the semaphore's wait list, starting from the
    /// end of the queue.
    ///
    /// If `rem` exceeds the number of permits needed by the wait list, the
    /// remainder are assigned back to the semaphore.
    fn add_permits(&self, mut rem: usize) {
        let mut waiters = self.waiters.borrow_mut();
        let mut wakers = WakeList::new();
        let mut is_empty = false;
        while rem > 0 {
            'inner: while wakers.can_push() {
                // Was the waiter assigned enough permits to wake it?
                match waiters.queue.last() {
                    Some(waiter) => {
                        if !waiter.assign_permits(&mut rem) {
                            break 'inner;
                        }
                    }
                    None => {
                        is_empty = true;
                        // If we assigned permits to all the waiters in the queue, and there are
                        // still permits left over, assign them back to the semaphore.
                        break 'inner;
                    }
                };
                let mut waiter = waiters.queue.pop_back().unwrap();
                if let Some(waker) = unsafe { (*waiter.as_mut().waker.get()).take() } {
                    wakers.push(waker);
                }
            }

            if rem > 0 && is_empty {
                let permits = rem;
                assert!(
                    permits <= Self::MAX_PERMITS,
                    "cannot add more than MAX_PERMITS permits ({})",
                    Self::MAX_PERMITS
                );
                *self.permits.borrow_mut() += rem << Self::PERMIT_SHIFT;
                rem = 0;
            }

            wakers.wake_all();
        }

        assert_eq!(rem, 0);
    }

    fn poll_acquire(
        &self,
        cx: &mut Context<'_>,
        num_permits: u32,
        node: Pin<&mut Waiter>,
        queued: bool,
    ) -> Poll<Result<(), AcquireError>> {
        let needed = if queued {
            *node.state.borrow() << Self::PERMIT_SHIFT
        } else {
            (num_permits as usize) << Self::PERMIT_SHIFT
        };

        let mut curr = self.permits.borrow_mut();

        // If closed, return error immediately.
        if *curr & Self::CLOSED > 0 {
            return Poll::Ready(Err(AcquireError::closed()));
        }
        // If the current permits is enough and not queued, assign permit
        // and return ok immediately.
        if *curr >= needed && !queued {
            *curr -= needed;
            return Poll::Ready(Ok(()));
        }

        // Clear permits and assign it.
        let mut permits = *curr;
        *curr = 0;
        if node.assign_permits(&mut permits) {
            // TODO: may never be here?
            self.add_permits(permits);
            return Poll::Ready(Ok(()));
        }

        // Replace waker if needed.
        let waker = unsafe { &mut *node.waker.get() };
        // Do we need to register the new waker?
        if waker
            .as_ref()
            .map(|waker| !waker.will_wake(cx.waker()))
            .unwrap_or(true)
        {
            *waker = Some(cx.waker().clone());
        }

        // If the waiter is not already in the wait queue, enqueue it.
        if !queued {
            let node = unsafe {
                let node = Pin::into_inner_unchecked(node) as *mut _;
                NonNull::new_unchecked(node)
            };

            self.waiters.borrow_mut().queue.push_front(node);
        }

        Poll::Pending
    }
}

impl fmt::Debug for Inner {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Semaphore")
            .field("permits", &self.available_permits())
            .finish()
    }
}

impl Future for Acquire<'_> {
    type Output = Result<(), AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let (node, semaphore, needed, queued) = self.project();

        match semaphore.poll_acquire(cx, needed, node, *queued) {
            Poll::Pending => {
                *queued = true;
                Poll::Pending
            }
            Poll::Ready(r) => {
                r?;
                *queued = false;
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl<'a> Acquire<'a> {
    fn new(semaphore: &'a Inner, num_permits: u32) -> Self {
        Self {
            node: Waiter::new(num_permits),
            semaphore,
            num_permits,
            queued: false,
        }
    }

    fn project(self: Pin<&mut Self>) -> (Pin<&mut Waiter>, &Inner, u32, &mut bool) {
        fn is_unpin<T: Unpin>() {}
        unsafe {
            // Safety: all fields other than `node` are `Unpin`

            is_unpin::<&Inner>();
            is_unpin::<&mut bool>();
            is_unpin::<u32>();

            let this = self.get_unchecked_mut();
            (
                Pin::new_unchecked(&mut this.node),
                this.semaphore,
                this.num_permits,
                &mut this.queued,
            )
        }
    }
}

impl Drop for Acquire<'_> {
    fn drop(&mut self) {
        // If the future is completed, there is no node in the wait list, so we
        // can skip acquiring the lock.
        if !self.queued {
            return;
        }

        // This is where we ensure safety. The future is being dropped,
        // which means we must ensure that the waiter entry is no longer stored
        // in the linked list.
        let mut waiters = self.semaphore.waiters.borrow_mut();

        // remove the entry from the list
        let node = NonNull::from(&mut self.node);
        // Safety: we have locked the wait list.
        unsafe { waiters.queue.remove(node) };

        let acquired_permits = self.num_permits as usize - *self.node.state.borrow();
        if acquired_permits > 0 {
            self.semaphore.add_permits(acquired_permits);
        }
    }
}

impl AcquireError {
    fn closed() -> AcquireError {
        AcquireError(())
    }
}

impl fmt::Display for AcquireError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "semaphore closed")
    }
}

impl std::error::Error for AcquireError {}

impl TryAcquireError {
    /// Returns `true` if the error was caused by a closed semaphore.
    #[allow(dead_code)] // may be used later!
    pub(crate) fn is_closed(&self) -> bool {
        matches!(self, TryAcquireError::Closed)
    }

    /// Returns `true` if the error was caused by calling `try_acquire` on a
    /// semaphore with no available permits.
    #[allow(dead_code)] // may be used later!
    pub(crate) fn is_no_permits(&self) -> bool {
        matches!(self, TryAcquireError::NoPermits)
    }
}

impl fmt::Display for TryAcquireError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryAcquireError::Closed => write!(fmt, "semaphore closed"),
            TryAcquireError::NoPermits => write!(fmt, "no permits available"),
        }
    }
}

impl std::error::Error for TryAcquireError {}

/// Counting semaphore performing asynchronous permit acquisition.
///
/// A semaphore maintains a set of permits. Permits are used to synchronize
/// access to a shared resource. A semaphore differs from a mutex in that it
/// can allow more than one concurrent caller to access the shared resource at a
/// time.
///
/// When `acquire` is called and the semaphore has remaining permits, the
/// function immediately returns a permit. However, if no remaining permits are
/// available, `acquire` (asynchronously) waits until an outstanding permit is
/// dropped. At this point, the freed permit is assigned to the caller.
///
/// This `Semaphore` is fair, which means that permits are given out in the order
/// they were requested. This fairness is also applied when `acquire_many` gets
/// involved, so if a call to `acquire_many` at the front of the queue requests
/// more permits than currently available, this can prevent a call to `acquire`
/// from completing, even if the semaphore has enough permits complete the call
/// to `acquire`.
///
/// To use the `Semaphore` in a poll function, you can use the [`PollSemaphore`]
/// utility.
///
/// # Examples
///
/// Basic usage:
///
/// ```
/// use tokio::sync::{Semaphore, TryAcquireError};
///
/// #[tokio::main]
/// async fn main() {
///     let semaphore = Semaphore::new(3);
///
///     let a_permit = semaphore.acquire().await.unwrap();
///     let two_permits = semaphore.acquire_many(2).await.unwrap();
///
///     assert_eq!(semaphore.available_permits(), 0);
///
///     let permit_attempt = semaphore.try_acquire();
///     assert_eq!(permit_attempt.err(), Some(TryAcquireError::NoPermits));
/// }
/// ```
///
/// Use [`Semaphore::acquire_owned`] to move permits across tasks:
///
/// ```
/// use std::sync::Arc;
/// use tokio::sync::Semaphore;
///
/// #[tokio::main]
/// async fn main() {
///     let semaphore = Arc::new(Semaphore::new(3));
///     let mut join_handles = Vec::new();
///
///     for _ in 0..5 {
///         let permit = semaphore.clone().acquire_owned().await.unwrap();
///         join_handles.push(tokio::spawn(async move {
///             // perform task...
///             // explicitly own `permit` in the task
///             drop(permit);
///         }));
///     }
///
///     for handle in join_handles {
///         handle.await.unwrap();
///     }
/// }
/// ```
///
/// [`PollSemaphore`]: https://docs.rs/tokio-util/0.6/tokio_util/sync/struct.PollSemaphore.html
/// [`Semaphore::acquire_owned`]: crate::sync::Semaphore::acquire_owned
#[derive(Debug)]
pub struct Semaphore(Inner);

/// A permit from the semaphore.
///
/// This type is created by the [`acquire`] method.
///
/// [`acquire`]: crate::sync::Semaphore::acquire()
#[must_use]
#[derive(Debug)]
pub struct SemaphorePermit<'a> {
    sem: &'a Semaphore,
    permits: u32,
}

/// An owned permit from the semaphore.
///
/// This type is created by the [`acquire_owned`] method.
///
/// [`acquire_owned`]: crate::sync::Semaphore::acquire_owned()
#[must_use]
#[derive(Debug)]
pub struct OwnedSemaphorePermit {
    sem: std::rc::Rc<Semaphore>,
    permits: u32,
}

pub struct AcquireResult<'a>(Acquire<'a>, &'a Semaphore, u32);

impl<'a> Future for AcquireResult<'a> {
    type Output = Result<SemaphorePermit<'a>, AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let sem = self.1;
        let permits = self.2;
        let inner = unsafe { self.map_unchecked_mut(|me| &mut me.0) };
        futures_util::ready!(inner.poll(cx))?;
        Poll::Ready(Ok(SemaphorePermit { sem, permits }))
    }
}

impl Semaphore {
    /// Creates a new semaphore with the initial number of permits.
    pub const fn new(permits: usize) -> Self {
        Self(Inner::new(permits))
    }

    /// Returns the current number of available permits.
    pub fn available_permits(&self) -> usize {
        self.0.available_permits()
    }

    /// Adds `n` new permits to the semaphore.
    ///
    /// The maximum number of permits is `usize::MAX >> 3`, and this function will panic if the limit is exceeded.
    pub fn add_permits(&self, n: usize) {
        self.0.release(n);
    }

    /// Acquires a permit from the semaphore.
    ///
    /// If the semaphore has been closed, this returns an [`AcquireError`].
    /// Otherwise, this returns a [`SemaphorePermit`] representing the
    /// acquired permit.
    ///
    /// # Cancel safety
    ///
    /// This method uses a queue to fairly distribute permits in the order they
    /// were requested. Cancelling a call to `acquire` makes you lose your place
    /// in the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::Semaphore;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let semaphore = Semaphore::new(2);
    ///
    ///     let permit_1 = semaphore.acquire().await.unwrap();
    ///     assert_eq!(semaphore.available_permits(), 1);
    ///
    ///     let permit_2 = semaphore.acquire().await.unwrap();
    ///     assert_eq!(semaphore.available_permits(), 0);
    ///
    ///     drop(permit_1);
    ///     assert_eq!(semaphore.available_permits(), 1);
    /// }
    /// ```
    ///
    /// [`AcquireError`]: crate::sync::AcquireError
    /// [`SemaphorePermit`]: crate::sync::SemaphorePermit
    pub fn acquire(&self) -> AcquireResult<'_> {
        let acq = self.0.acquire(1);
        AcquireResult(acq, self, 1)
    }

    /// Acquires `n` permits from the semaphore.
    ///
    /// If the semaphore has been closed, this returns an [`AcquireError`].
    /// Otherwise, this returns a [`SemaphorePermit`] representing the
    /// acquired permits.
    ///
    /// # Cancel safety
    ///
    /// This method uses a queue to fairly distribute permits in the order they
    /// were requested. Cancelling a call to `acquire_many` makes you lose your
    /// place in the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::Semaphore;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let semaphore = Semaphore::new(5);
    ///
    ///     let permit = semaphore.acquire_many(3).await.unwrap();
    ///     assert_eq!(semaphore.available_permits(), 2);
    /// }
    /// ```
    ///
    /// [`AcquireError`]: crate::sync::AcquireError
    /// [`SemaphorePermit`]: crate::sync::SemaphorePermit
    pub fn acquire_many(&self, n: u32) -> AcquireResult<'_> {
        let acq = self.0.acquire(n);
        AcquireResult(acq, self, n)
    }

    /// Tries to acquire a permit from the semaphore.
    ///
    /// If the semaphore has been closed, this returns a [`TryAcquireError::Closed`]
    /// and a [`TryAcquireError::NoPermits`] if there are no permits left. Otherwise,
    /// this returns a [`SemaphorePermit`] representing the acquired permits.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::{Semaphore, TryAcquireError};
    ///
    /// # fn main() {
    /// let semaphore = Semaphore::new(2);
    ///
    /// let permit_1 = semaphore.try_acquire().unwrap();
    /// assert_eq!(semaphore.available_permits(), 1);
    ///
    /// let permit_2 = semaphore.try_acquire().unwrap();
    /// assert_eq!(semaphore.available_permits(), 0);
    ///
    /// let permit_3 = semaphore.try_acquire();
    /// assert_eq!(permit_3.err(), Some(TryAcquireError::NoPermits));
    /// # }
    /// ```
    ///
    /// [`TryAcquireError::Closed`]: crate::sync::TryAcquireError::Closed
    /// [`TryAcquireError::NoPermits`]: crate::sync::TryAcquireError::NoPermits
    /// [`SemaphorePermit`]: crate::sync::SemaphorePermit
    pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        match self.0.try_acquire(1) {
            Ok(_) => Ok(SemaphorePermit {
                sem: self,
                permits: 1,
            }),
            Err(e) => Err(e),
        }
    }

    /// Tries to acquire `n` permits from the semaphore.
    ///
    /// If the semaphore has been closed, this returns a [`TryAcquireError::Closed`]
    /// and a [`TryAcquireError::NoPermits`] if there are not enough permits left.
    /// Otherwise, this returns a [`SemaphorePermit`] representing the acquired permits.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::{Semaphore, TryAcquireError};
    ///
    /// # fn main() {
    /// let semaphore = Semaphore::new(4);
    ///
    /// let permit_1 = semaphore.try_acquire_many(3).unwrap();
    /// assert_eq!(semaphore.available_permits(), 1);
    ///
    /// let permit_2 = semaphore.try_acquire_many(2);
    /// assert_eq!(permit_2.err(), Some(TryAcquireError::NoPermits));
    /// # }
    /// ```
    ///
    /// [`TryAcquireError::Closed`]: crate::sync::TryAcquireError::Closed
    /// [`TryAcquireError::NoPermits`]: crate::sync::TryAcquireError::NoPermits
    /// [`SemaphorePermit`]: crate::sync::SemaphorePermit
    pub fn try_acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        match self.0.try_acquire(n) {
            Ok(_) => Ok(SemaphorePermit {
                sem: self,
                permits: n,
            }),
            Err(e) => Err(e),
        }
    }

    /// Acquires a permit from the semaphore.
    ///
    /// The semaphore must be wrapped in an [`Arc`] to call this method.
    /// If the semaphore has been closed, this returns an [`AcquireError`].
    /// Otherwise, this returns a [`OwnedSemaphorePermit`] representing the
    /// acquired permit.
    ///
    /// # Cancel safety
    ///
    /// This method uses a queue to fairly distribute permits in the order they
    /// were requested. Cancelling a call to `acquire_owned` makes you lose your
    /// place in the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use tokio::sync::Semaphore;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let semaphore = Arc::new(Semaphore::new(3));
    ///     let mut join_handles = Vec::new();
    ///
    ///     for _ in 0..5 {
    ///         let permit = semaphore.clone().acquire_owned().await.unwrap();
    ///         join_handles.push(tokio::spawn(async move {
    ///             // perform task...
    ///             // explicitly own `permit` in the task
    ///             drop(permit);
    ///         }));
    ///     }
    ///
    ///     for handle in join_handles {
    ///         handle.await.unwrap();
    ///     }
    /// }
    /// ```
    ///
    /// [`Arc`]: std::sync::Arc
    /// [`AcquireError`]: crate::sync::AcquireError
    /// [`OwnedSemaphorePermit`]: crate::sync::OwnedSemaphorePermit
    pub async fn acquire_owned(
        self: std::rc::Rc<Self>,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        self.0.acquire(1).await?;
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: 1,
        })
    }

    /// Acquires `n` permits from the semaphore.
    ///
    /// The semaphore must be wrapped in an [`Arc`] to call this method.
    /// If the semaphore has been closed, this returns an [`AcquireError`].
    /// Otherwise, this returns a [`OwnedSemaphorePermit`] representing the
    /// acquired permit.
    ///
    /// # Cancel safety
    ///
    /// This method uses a queue to fairly distribute permits in the order they
    /// were requested. Cancelling a call to `acquire_many_owned` makes you lose
    /// your place in the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use tokio::sync::Semaphore;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let semaphore = Arc::new(Semaphore::new(10));
    ///     let mut join_handles = Vec::new();
    ///
    ///     for _ in 0..5 {
    ///         let permit = semaphore.clone().acquire_many_owned(2).await.unwrap();
    ///         join_handles.push(tokio::spawn(async move {
    ///             // perform task...
    ///             // explicitly own `permit` in the task
    ///             drop(permit);
    ///         }));
    ///     }
    ///
    ///     for handle in join_handles {
    ///         handle.await.unwrap();
    ///     }
    /// }
    /// ```
    ///
    /// [`Arc`]: std::sync::Arc
    /// [`AcquireError`]: crate::sync::AcquireError
    /// [`OwnedSemaphorePermit`]: crate::sync::OwnedSemaphorePermit
    pub async fn acquire_many_owned(
        self: std::rc::Rc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        self.0.acquire(n).await?;
        Ok(OwnedSemaphorePermit {
            sem: self,
            permits: n,
        })
    }

    /// Tries to acquire a permit from the semaphore.
    ///
    /// The semaphore must be wrapped in an [`Arc`] to call this method. If
    /// the semaphore has been closed, this returns a [`TryAcquireError::Closed`]
    /// and a [`TryAcquireError::NoPermits`] if there are no permits left.
    /// Otherwise, this returns a [`OwnedSemaphorePermit`] representing the
    /// acquired permit.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use tokio::sync::{Semaphore, TryAcquireError};
    ///
    /// # fn main() {
    /// let semaphore = Arc::new(Semaphore::new(2));
    ///
    /// let permit_1 = Arc::clone(&semaphore).try_acquire_owned().unwrap();
    /// assert_eq!(semaphore.available_permits(), 1);
    ///
    /// let permit_2 = Arc::clone(&semaphore).try_acquire_owned().unwrap();
    /// assert_eq!(semaphore.available_permits(), 0);
    ///
    /// let permit_3 = semaphore.try_acquire_owned();
    /// assert_eq!(permit_3.err(), Some(TryAcquireError::NoPermits));
    /// # }
    /// ```
    ///
    /// [`Arc`]: std::sync::Arc
    /// [`TryAcquireError::Closed`]: crate::sync::TryAcquireError::Closed
    /// [`TryAcquireError::NoPermits`]: crate::sync::TryAcquireError::NoPermits
    /// [`OwnedSemaphorePermit`]: crate::sync::OwnedSemaphorePermit
    pub fn try_acquire_owned(
        self: std::rc::Rc<Self>,
    ) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        match self.0.try_acquire(1) {
            Ok(_) => Ok(OwnedSemaphorePermit {
                sem: self,
                permits: 1,
            }),
            Err(e) => Err(e),
        }
    }

    /// Tries to acquire `n` permits from the semaphore.
    ///
    /// The semaphore must be wrapped in an [`Arc`] to call this method. If
    /// the semaphore has been closed, this returns a [`TryAcquireError::Closed`]
    /// and a [`TryAcquireError::NoPermits`] if there are no permits left.
    /// Otherwise, this returns a [`OwnedSemaphorePermit`] representing the
    /// acquired permit.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use tokio::sync::{Semaphore, TryAcquireError};
    ///
    /// # fn main() {
    /// let semaphore = Arc::new(Semaphore::new(4));
    ///
    /// let permit_1 = Arc::clone(&semaphore).try_acquire_many_owned(3).unwrap();
    /// assert_eq!(semaphore.available_permits(), 1);
    ///
    /// let permit_2 = semaphore.try_acquire_many_owned(2);
    /// assert_eq!(permit_2.err(), Some(TryAcquireError::NoPermits));
    /// # }
    /// ```
    ///
    /// [`Arc`]: std::sync::Arc
    /// [`TryAcquireError::Closed`]: crate::sync::TryAcquireError::Closed
    /// [`TryAcquireError::NoPermits`]: crate::sync::TryAcquireError::NoPermits
    /// [`OwnedSemaphorePermit`]: crate::sync::OwnedSemaphorePermit
    pub fn try_acquire_many_owned(
        self: std::rc::Rc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        match self.0.try_acquire(n) {
            Ok(_) => Ok(OwnedSemaphorePermit {
                sem: self,
                permits: n,
            }),
            Err(e) => Err(e),
        }
    }

    /// Closes the semaphore.
    ///
    /// This prevents the semaphore from issuing new permits and notifies all pending waiters.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::Semaphore;
    /// use std::sync::Arc;
    /// use tokio::sync::TryAcquireError;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let semaphore = Arc::new(Semaphore::new(1));
    ///     let semaphore2 = semaphore.clone();
    ///
    ///     tokio::spawn(async move {
    ///         let permit = semaphore.acquire_many(2).await;
    ///         assert!(permit.is_err());
    ///         println!("waiter received error");
    ///     });
    ///
    ///     println!("closing semaphore");
    ///     semaphore2.close();
    ///
    ///     // Cannot obtain more permits
    ///     assert_eq!(semaphore2.try_acquire().err(), Some(TryAcquireError::Closed))
    /// }
    /// ```
    pub fn close(&self) {
        self.0.close();
    }

    /// Returns true if the semaphore is closed
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

impl<'a> SemaphorePermit<'a> {
    /// Forgets the permit **without** releasing it back to the semaphore.
    /// This can be used to reduce the amount of permits available from a
    /// semaphore.
    pub fn forget(mut self) {
        self.permits = 0;
    }
}

impl OwnedSemaphorePermit {
    /// Forgets the permit **without** releasing it back to the semaphore.
    /// This can be used to reduce the amount of permits available from a
    /// semaphore.
    pub fn forget(mut self) {
        self.permits = 0;
    }
}

impl<'a> Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        self.sem.add_permits(self.permits as usize);
    }
}

impl Drop for OwnedSemaphorePermit {
    fn drop(&mut self) {
        self.sem.add_permits(self.permits as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::{Inner, Semaphore};

    #[frosty::test]
    async fn inner_works() {
        let s = Inner::new(10);
        for _ in 0..10 {
            s.acquire(1).await.unwrap();
        }
    }

    #[frosty::test]
    async fn inner_release_after_acquire() {
        let s = std::rc::Rc::new(Inner::new(0));

        let s_move = s.clone();
        let join = frosty::spawn(async move {
            let _ = s_move.acquire(1).await.unwrap();
            let _ = s_move.acquire(1).await.unwrap();
        });
        s.release(2);
        join.await;
    }

    #[frosty::test]
    async fn it_works() {
        let s = Semaphore::new(0);
        s.add_permits(1);
        let _ = s.acquire().await.unwrap();
    }
}
