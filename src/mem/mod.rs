//! Static memory management.
//!
//! All memory the process will ever use is acquired during startup: each
//! component draws its allocation from a single [`budget::Budget`] computed
//! from config, and once startup completes the process is frozen via
//! [`guard::freeze`] — any later heap allocation aborts the process.
//! Fixed-capacity structures never grow; exhausting one is an explicit,
//! named error.

pub mod arena;
pub mod budget;
pub mod buf;
pub mod fixed_map;
pub mod fixed_vec;
pub mod guard;
pub mod pool;

pub use arena::{Arena, ArenaFull};
pub use budget::{Budget, BudgetError};
pub use buf::FixedBuf;
pub use fixed_map::{FixedMap, MapFull};
pub use fixed_vec::{CapacityError, FixedVec};
pub use pool::{Handle, Pool, PoolExhausted};
