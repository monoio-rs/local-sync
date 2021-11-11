use std::cell::UnsafeCell;

pub trait Semaphore {
    fn add_permits(&self, n: usize);
    fn close(&self);
    fn is_closed(&self) -> bool;
}

impl Semaphore for crate::semaphore::Inner {
    fn add_permits(&self, n: usize) {
        self.release(n);
    }

    fn close(&self) {
        crate::semaphore::Inner::close(self);
    }

    fn is_closed(&self) -> bool {
        crate::semaphore::Inner::is_closed(self)
    }
}

pub struct Unlimited {
    closed: UnsafeCell<bool>,
}

impl Unlimited {
    pub fn new() -> Self {
        Self {
            closed: UnsafeCell::new(false),
        }
    }
}

impl Semaphore for Unlimited {
    fn add_permits(&self, _: usize) {}

    fn close(&self) {
        unsafe {
            *self.closed.get() = true;
        }
    }

    fn is_closed(&self) -> bool {
        unsafe { *self.closed.get() }
    }
}
