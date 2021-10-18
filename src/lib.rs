//! Local Sync is a crate that providing non-thread-safe data structures useful
//! for async programming.
//! If you use a runtime with thread-per-core model(for example the Frosty), you
//! may use this crate to avoid the cost of communicating across threads.

// shared basic data structure
mod linked_list;
mod wake_list;

// Semaphore
pub mod semaphore;
// BoundedChannel and UnboundedChannel
pub mod mpsc;

// OneshotChannel
pub mod oneshot;

// OnceCell
mod once_cell;
pub use once_cell::{OnceCell, SetError};
