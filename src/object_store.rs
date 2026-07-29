//! Provider-neutral durable-object contract.
//!
//! WAL upload, manifests, garbage collection, checkpoints, and the block cache
//! all speak this module's six semantics: immutable or conditional PUT,
//! whole/ranged GET, LIST, DELETE, and compare-and-swap through an ETag.  A
//! backend may be AWS S3, MinIO, a GCS/Azure compatibility gateway, a future
//! native adapter, or the deterministic simulator; code above this boundary
//! cannot observe which one it is.
//!
//! The concrete adapter choice is an enum rather than a trait object.  That
//! keeps dispatch allocation-free and makes the fixed startup memory budget
//! explicit while preserving one semantic interface for every durable object.

use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::s3::S3Client;
use crate::util::StackStr;

pub(crate) mod sim;

/// A condition attached to a write.
#[derive(Debug, Clone, Copy)]
pub enum Precondition<'a> {
    /// Store regardless of whether the key exists.
    None,
    /// Create only; fail if the key already exists.
    IfNoneMatchAny,
    /// Replace only the exact generation named by this ETag.
    IfMatch(&'a str),
}

/// Metadata returned with a successful object read.
#[derive(Debug)]
pub struct GetResult {
    pub len: usize,
    pub etag: StackStr<80>,
}

/// A provider-neutral operation failure.
#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "error text is carried inline on the stack; boxing would heap-allocate"
)]
pub enum Error {
    /// The provider rejected the operation with an HTTP-like status.
    Status { code: u16, message: StackStr<256> },
    /// Connection-level failure after retries.
    Io {
        context: &'static str,
        kind: std::io::ErrorKind,
        detail: StackStr<160>,
    },
    /// Response exceeded the fixed response buffer.
    ResponseTooLarge { content_length: usize, capacity: usize },
    /// The adapter received a malformed response.
    Protocol(&'static str),
}

impl Error {
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Status { code: 404, .. })
    }

    pub fn is_precondition_failed(&self) -> bool {
        matches!(self, Self::Status { code: 412 | 409, .. })
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status { code, message } => {
                write!(formatter, "object store returned {code}: {}", message.as_str())
            }
            Self::Io { context, kind, detail } => {
                if detail.as_str().is_empty() {
                    write!(formatter, "object store i/o ({context}): {kind:?}")
                } else {
                    write!(formatter, "object store i/o ({context}): {}", detail.as_str())
                }
            }
            Self::ResponseTooLarge {
                content_length,
                capacity,
            } => write!(
                formatter,
                "object store response of {content_length} bytes exceeds buffer of {capacity}"
            ),
            Self::Protocol(what) => write!(formatter, "object store protocol error: {what}"),
        }
    }
}

/// Startup failed before the provider-neutral client became usable.
#[derive(Debug)]
pub(crate) enum SetupError {
    Budget(BudgetError),
    Credentials(&'static str),
    Adapter(crate::s3::S3SetupError),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(error) => write!(formatter, "{error}"),
            Self::Credentials(what) => {
                write!(formatter, "object storage is enabled but credentials are missing ({what})")
            }
            Self::Adapter(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for SetupError {}

impl From<BudgetError> for SetupError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<crate::s3::S3SetupError> for SetupError {
    fn from(error: crate::s3::S3SetupError) -> Self {
        Self::Adapter(error)
    }
}

/// One durable-object client.
///
/// Provider-specific signing, endpoint behavior, and retry dialects terminate
/// inside an adapter variant.  Adding a native provider therefore changes this
/// module and that adapter only; storage, WAL, cache, and query code remain
/// unchanged.
#[allow(
    clippy::large_enum_variant,
    reason = "two long-lived instances per process; boxing buys nothing"
)]
pub(crate) enum Client {
    S3(S3Client),
    Simulator(sim::SimClient),
}

impl Client {
    pub(crate) fn budget_bytes(config: &Config) -> usize {
        S3Client::budget_bytes(config)
    }

    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, SetupError> {
        let mut config = config.clone();
        // Credentials and their conventional environment fallbacks are an
        // adapter concern. The simulator signs nothing.
        if config.object_store_access_key.is_empty() && !config.object_store_sim {
            config.object_store_access_key = std::env::var("AWS_ACCESS_KEY_ID")
                .map_err(|_| {
                    SetupError::Credentials("object_store_access_key / AWS_ACCESS_KEY_ID")
                })?;
        }
        if config.object_store_secret_key.is_empty() && !config.object_store_sim {
            config.object_store_secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
                .map_err(|_| {
                    SetupError::Credentials("object_store_secret_key / AWS_SECRET_ACCESS_KEY")
                })?;
        }
        if config.object_store_sim {
            Ok(Self::Simulator(sim::SimClient::new(&config, budget)?))
        } else {
            Ok(Self::S3(S3Client::new(&config, budget)?))
        }
    }

    pub(crate) fn put(
        &mut self,
        key: &str,
        body: &[u8],
        precondition: Precondition<'_>,
    ) -> Result<StackStr<80>, Error> {
        match self {
            Self::S3(client) => client.put(key, body, precondition),
            Self::Simulator(client) => client.put(key, body, precondition),
        }
    }

    pub(crate) fn get(
        &mut self,
        key: &str,
        range: Option<(u64, u64)>,
    ) -> Result<GetResult, Error> {
        match self {
            Self::S3(client) => client.get(key, range),
            Self::Simulator(client) => client.get(key, range),
        }
    }

    /// Bytes returned by the most recent successful GET.
    pub(crate) fn body_bytes(&self) -> &[u8] {
        match self {
            Self::S3(client) => client.body_bytes(),
            Self::Simulator(client) => client.body_bytes(),
        }
    }

    /// Maximum whole or ranged response the adapter can return.
    pub(crate) fn response_capacity(&self) -> usize {
        match self {
            Self::S3(client) => client.response_capacity(),
            Self::Simulator(client) => client.response_capacity(),
        }
    }

    pub(crate) fn delete(&mut self, key: &str) -> Result<(), Error> {
        match self {
            Self::S3(client) => client.delete(key),
            Self::Simulator(client) => client.delete(key),
        }
    }

    pub(crate) fn list(
        &mut self,
        prefix: &str,
        each: impl FnMut(&str),
    ) -> Result<usize, Error> {
        match self {
            Self::S3(client) => client.list(prefix, each),
            Self::Simulator(client) => client.list(prefix, each),
        }
    }
}

/// Stable process-writer identity derived from the durable namespace and local
/// journal identity. Ambiguous manifest CAS recovery uses this to distinguish
/// its own lost response from another writer's publish.
pub(crate) fn writer_id(config: &Config) -> u64 {
    use crate::wal::crc32c::Crc32c;

    let mut low = Crc32c::new();
    low.update(config.object_store_endpoint.as_bytes());
    low.update(config.object_store_bucket.as_bytes());
    low.update(config.object_store_prefix.as_bytes());
    low.update(config.data_dir.as_bytes());
    let mut high = Crc32c::new();
    high.update(config.data_dir.as_bytes());
    high.update(config.object_store_prefix.as_bytes());
    high.update(config.object_store_bucket.as_bytes());
    high.update(config.object_store_endpoint.as_bytes());
    (u64::from(high.finish()) << 32) | u64::from(low.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simulated() -> Client {
        let mut config = Config::default_dev();
        config.object_store_sim = true;
        config.object_store_bucket = format!("object-contract-{}", std::process::id());
        sim::drop_bucket(&config.object_store_bucket);
        let _bucket = sim::open_bucket(&config.object_store_bucket, 7);
        let mut budget = Budget::new(Client::budget_bytes(&config) + 4096);
        Client::new(&config, &mut budget).unwrap()
    }

    #[test]
    fn simulator_obeys_the_provider_neutral_contract() {
        let mut client = simulated();
        let first = client
            .put("prefix/a", b"abcdef", Precondition::IfNoneMatchAny)
            .unwrap();
        assert!(client
            .put("prefix/a", b"wrong", Precondition::IfNoneMatchAny)
            .unwrap_err()
            .is_precondition_failed());

        let whole = client.get("prefix/a", None).unwrap();
        assert_eq!(whole.len, 6);
        assert_eq!(client.body_bytes(), b"abcdef");
        assert_eq!(whole.etag.as_str(), first.as_str());

        let range = client.get("prefix/a", Some((2, 4))).unwrap();
        assert_eq!(range.len, 3);
        assert_eq!(client.body_bytes(), b"cde");

        let second = client
            .put("prefix/a", b"updated", Precondition::IfMatch(first.as_str()))
            .unwrap();
        assert_ne!(second.as_str(), first.as_str());
        assert!(client
            .put("prefix/a", b"stale", Precondition::IfMatch(first.as_str()))
            .unwrap_err()
            .is_precondition_failed());

        client.put("prefix/b", b"b", Precondition::None).unwrap();
        let mut listed = [StackStr::<32>::new(), StackStr::<32>::new()];
        let mut count = 0;
        let returned = client
            .list("prefix/", |key| {
                use core::fmt::Write;
                write!(listed[count], "{key}").unwrap();
                count += 1;
            })
            .unwrap();
        assert_eq!(returned, 2);
        assert_eq!(listed[0].as_str(), "prefix/a");
        assert_eq!(listed[1].as_str(), "prefix/b");

        client.delete("prefix/a").unwrap();
        assert!(client.get("prefix/a", None).unwrap_err().is_not_found());
    }

    #[test]
    fn writer_identity_covers_endpoint_bucket_prefix_and_journal() {
        let base = Config::default_dev();
        let baseline = writer_id(&base);
        for mutate in [
            |config: &mut Config| config.object_store_endpoint.push('x'),
            |config: &mut Config| config.object_store_bucket.push('x'),
            |config: &mut Config| config.object_store_prefix.push('x'),
            |config: &mut Config| config.data_dir.push('x'),
        ] {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert_ne!(writer_id(&changed), baseline);
        }
    }
}
