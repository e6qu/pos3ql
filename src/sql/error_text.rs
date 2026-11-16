//! The text of an error's message, DETAIL and HINT. PostgreSQL reports these
//! in full however long they are, for example an invalid input value quoted
//! whole. Short text stays inline; longer text is copied into the error text
//! pool reserved at startup (`error_text_bytes`, `error_text_leases`). Text
//! that does not fit the pool raises SQLSTATE 54000 naming those settings
//! instead of being cut.

use core::cell::RefCell;
use core::fmt::{self, Write};

use std::sync::Arc;

use crate::mem::lease_pool::{LeaseError, LeasePool, MutableSlice, SharedSlice};
use crate::util::StackStr;

/// Bytes an error keeps inline: most messages, and every name-sized
/// message, need no pool lease.
pub const INLINE_BYTES: usize = 192;

std::thread_local! {
    /// The pool of the SQL engine running on this thread.
    static POOL: RefCell<Option<Arc<LeasePool>>> = const { RefCell::new(None) };
}

/// Makes `pool` hold this thread's long error text. Startup only: the SQL
/// engine installs its pool on the thread that runs it.
pub(crate) fn install(pool: Arc<LeasePool>) {
    POOL.with(|slot| *slot.borrow_mut() = Some(pool));
}

/// Reserves a pool on a test thread that runs no SQL engine.
#[cfg(test)]
pub(crate) fn reserve_for_test() {
    if POOL.with(|pool| pool.borrow().is_none()) {
        let mut budget = crate::mem::Budget::new(1 << 20);
        install(
            LeasePool::new(
                &mut budget,
                crate::mem::lease_pool::Purpose::ERROR_TEXT,
                64 * 1024,
                16,
            )
            .expect("test error text pool"),
        );
    }
}

/// Why text could not be kept whole.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TextExhausted {
    /// The pool is full, or the text is larger than all of it.
    Pool {
        requested_bytes: usize,
        error: LeaseError,
    },
    /// No SQL engine reserved a pool on this thread.
    Unreserved { requested_bytes: usize },
}

impl TextExhausted {
    /// The named `54000` error reported in place of the text. Its own
    /// message always fits inline.
    pub(crate) fn error(self) -> crate::sql::eval::SqlError {
        let mut message = StackStr::<INLINE_BYTES>::new();
        let _ = match self {
            Self::Pool {
                requested_bytes,
                error,
            } => write!(
                message,
                "error text of {} bytes does not fit error_text_bytes (largest free range {} bytes) or error_text_leases ({} available)",
                requested_bytes, error.largest_range_bytes, error.available_leases
            ),
            Self::Unreserved { requested_bytes } => write!(
                message,
                "error text of {requested_bytes} bytes needs error_text_bytes, which this thread has not reserved"
            ),
        };
        assert!(
            !message.is_truncated(),
            "the exhaustion message fits inline"
        );
        crate::sql::eval::SqlError {
            sqlstate: crate::sql::eval::SqlState::known(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
            ),
            message: ErrorText(Storage::Inline(message)),
        }
    }
}

#[derive(Clone)]
enum Storage {
    Inline(StackStr<INLINE_BYTES>),
    Pooled(SharedSlice<u8>),
}

/// Error text kept whole, inline or in the error text pool.
#[derive(Clone)]
pub struct ErrorText(Storage);

/// Writes error text whole: inline while it fits, then into a growing lease.
pub(crate) struct TextWriter {
    inline: StackStr<INLINE_BYTES>,
    pooled: Option<MutableSlice<u8>>,
    /// Bytes written, counted on after the text stopped fitting.
    written: usize,
    exhausted: Option<LeaseError>,
    unreserved: bool,
}

impl TextWriter {
    pub(crate) const fn new() -> Self {
        Self {
            inline: StackStr::new(),
            pooled: None,
            written: 0,
            exhausted: None,
            unreserved: false,
        }
    }

    fn spill(&mut self, text: &str) -> Result<(), LeaseError> {
        let needed = self.written + text.len();
        let lease = match &mut self.pooled {
            Some(lease) => lease,
            None => {
                let Some(pool) = POOL.with(|pool| pool.borrow().clone()) else {
                    self.unreserved = true;
                    return Ok(());
                };
                let mut lease = pool.lease::<u8>(needed)?;
                lease.extend_from_slice(self.inline.as_str().as_bytes())?;
                self.pooled.insert(lease)
            }
        };
        // Growth extends the reservation in place when the bytes after it
        // are free, so the frozen text holds no unused capacity.
        lease.extend_from_slice(text.as_bytes())
    }

    /// The text written, or why it could not be kept whole.
    pub(crate) fn finish(self) -> Result<ErrorText, TextExhausted> {
        let requested_bytes = self.written;
        if self.unreserved {
            return Err(TextExhausted::Unreserved { requested_bytes });
        }
        if let Some(error) = self.exhausted {
            return Err(TextExhausted::Pool {
                requested_bytes,
                error,
            });
        }
        Ok(ErrorText(match self.pooled {
            Some(lease) => Storage::Pooled(lease.freeze()),
            None => Storage::Inline(self.inline),
        }))
    }
}

impl Write for TextWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.pooled.is_none() && self.written + text.len() <= INLINE_BYTES {
            self.inline.write_str(text)?;
        } else if self.exhausted.is_none()
            && !self.unreserved
            && let Err(error) = self.spill(text)
        {
            self.exhausted = Some(error);
            self.pooled = None;
        }
        self.written += text.len();
        Ok(())
    }
}

impl ErrorText {
    pub const fn new() -> Self {
        Self(Storage::Inline(StackStr::new()))
    }

    /// Formats `args` whole.
    pub(crate) fn format(args: fmt::Arguments<'_>) -> Result<Self, TextExhausted> {
        let mut writer = TextWriter::new();
        let _ = writer.write_fmt(args);
        writer.finish()
    }

    /// Copies `text` whole.
    pub(crate) fn copy(text: &str) -> Result<Self, TextExhausted> {
        Self::format(format_args!("{text}"))
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            Storage::Inline(text) => text.as_str(),
            // Only whole formatted text, which is UTF-8, is ever stored.
            Storage::Pooled(bytes) => unsafe { core::str::from_utf8_unchecked(bytes.as_slice()) },
        }
    }

    pub fn len(&self) -> usize {
        self.as_str().len()
    }

    pub fn is_empty(&self) -> bool {
        self.as_str().is_empty()
    }
}

impl Default for ErrorText {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ErrorText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for ErrorText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl PartialEq for ErrorText {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for ErrorText {}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    #[test]
    fn long_text_is_kept_whole_in_the_pool_and_released() {
        let mut budget = crate::mem::Budget::new(1 << 20);
        let pool = LeasePool::new(
            &mut budget,
            crate::mem::lease_pool::Purpose::ERROR_TEXT,
            4096,
            2,
        )
        .unwrap();
        install(Arc::clone(&pool));
        let long = "x".repeat(1000);
        crate::mem::guard::forbid_alloc(|| {
            let short = ErrorText::copy("short").unwrap();
            assert_eq!(short.as_str(), "short");
            let whole = ErrorText::format(format_args!("value \"{long}\"")).unwrap();
            assert_eq!(whole.len(), 1008);
            assert!(whole.as_str().ends_with("x\""));
            let shared = whole.clone();
            drop(whole);
            assert_eq!(shared.len(), 1008);
            let second = ErrorText::copy(&long[..900]).unwrap();
            // Two leases are all the pool has.
            let exhausted = ErrorText::copy(&long[..800]).unwrap_err().error();
            assert_eq!(exhausted.sqlstate, "54000");
            assert!(
                exhausted
                    .message
                    .as_str()
                    .starts_with("error text of 800 bytes does not fit error_text_bytes"),
                "{}",
                exhausted.message
            );
            drop((shared, second));
            assert_eq!(ErrorText::copy(&long[..800]).unwrap().len(), 800);
        });
        POOL.with(|slot| slot.borrow_mut().take());
        let unreserved = ErrorText::copy(&long).unwrap_err().error();
        assert_eq!(
            unreserved.message.as_str(),
            "error text of 1000 bytes needs error_text_bytes, which this thread has not reserved"
        );
    }
}
