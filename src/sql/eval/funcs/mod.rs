//! Per-category dispatchers for the SQL scalar-function router.
//!
//! The central `call()` router in the parent module matches a function name
//! against a large `match`. To keep any one file from carrying the whole set,
//! each cohesive family of built-ins lives in a submodule here and exposes a
//! `dispatch(...)` that returns `Some(result)` when it recognizes `name`, or
//! `None` to let the router keep matching. Membership is decided by an explicit
//! `matches!` guard at the top of each `dispatch`, so a category's arms can be
//! relocated here without depending on where they sat in the original `match`.

pub(super) mod bytea;
pub(super) mod datetime;
pub(super) mod math;
pub(super) mod string;
