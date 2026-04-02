//! Multi-threaded async runtime.
//!
//! This module provides [`MultiThreadRuntime`], a thread-pool-based runtime
//! that distributes async tasks across multiple OS threads. It is the
//! multi-threaded counterpart to the single-threaded [`Runtime`](crate::Runtime).
//!
//! ## Quick Start
//!
//! ```
//! use mini_async_runtime::multi_thread::MultiThreadRuntime;
//!
//! let rt = MultiThreadRuntime::new(4).unwrap();
//! let result = rt.block_on(async { 2 + 2 });
//! assert_eq!(result, 4);
//! ```

pub(crate) mod executor;
pub(crate) mod task;

pub mod net;
mod runtime;
pub mod timer;

pub use runtime::MultiThreadRuntime;
pub use task::JoinHandle;
