//! Local Sync is a crate that providing non-thread-safe data structures useful
//! for async programming.
//! If you use a runtime with thread-per-core model(for example the Monoio), you
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

// stream wrapper for BoundedChannel and UnboundedChannel
pub mod stream_wrappers;

// OnceCell
mod once_cell;
pub use once_cell::{OnceCell, SetError};
