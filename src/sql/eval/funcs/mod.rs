//! Per-category dispatchers for the SQL scalar-function router.
//!
//! The central `call()` router in the parent module matches a function name
//! against a large `match`. To keep any one file from carrying the whole set,
//! each cohesive family of built-ins lives in a submodule here and exposes a
//! `dispatch(...)` that returns `Some(result)` when it recognizes `name`, or
//! `None` to let the router keep matching. Membership is decided by an explicit
//! `matches!` guard at the top of each `dispatch`, so a category's arms can be
//! relocated here without depending on where they sat in the original `match`.

pub(crate) mod acl;
pub(super) mod array;
pub(crate) mod bytea;
pub(super) mod conditional;
pub(super) mod datetime;
pub(super) mod full_text;
pub(crate) mod geometry;
pub(super) mod identity;
pub(crate) mod json;
pub(crate) mod lsn;
pub(super) mod math;
pub(super) mod misc;
pub(super) mod money;
pub(super) mod net;
pub(super) mod range;
pub(super) mod regex;
pub(crate) mod string;
pub mod system;
pub(super) mod uuid;
pub(super) mod xml;
