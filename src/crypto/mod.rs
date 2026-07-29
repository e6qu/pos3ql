//! Allocation-free cryptographic primitives shared by protocol, storage, and
//! authentication code.
//!
//! These implementations are small fixed-memory building blocks, not an
//! object-provider subsystem. Keeping them here prevents block identities,
//! PostgreSQL authentication, and SQL digest functions from depending on the
//! first object-store adapter.

pub(crate) mod hmac;
pub(crate) mod sha256;
