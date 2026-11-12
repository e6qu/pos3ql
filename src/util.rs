//! Small allocation-free helpers.

use core::fmt;

/// Writes one already formatted diagnostic without touching the heap. Runtime
/// failure reporting remains usable after the startup allocation freeze.
pub(crate) fn stderr_line(message: &str) {
    crate::logging::warn("runtime_warning", message.trim_end());
}

/// Sorts in place without allocating (heap sort, not stable); the first
/// comparison error stops it and is returned. Use this rather than
/// `sort_unstable_by` whenever a comparison can fail, so a failure is
/// reported instead of being folded into an ordering.
pub(crate) fn try_sort_unstable_by<T, E>(
    items: &mut [T],
    mut compare: impl FnMut(&T, &T) -> Result<core::cmp::Ordering, E>,
) -> Result<(), E> {
    let mut sift = |items: &mut [T], mut root: usize, end: usize| -> Result<(), E> {
        loop {
            let mut child = 2 * root + 1;
            if child >= end {
                return Ok(());
            }
            if child + 1 < end && compare(&items[child], &items[child + 1])?.is_lt() {
                child += 1;
            }
            if !compare(&items[root], &items[child])?.is_lt() {
                return Ok(());
            }
            items.swap(root, child);
            root = child;
        }
    };
    let len = items.len();
    for root in (0..len / 2).rev() {
        sift(items, root, len)?;
    }
    for end in (1..len).rev() {
        items.swap(0, end);
        sift(items, 0, end)?;
    }
    Ok(())
}

/// A fixed-capacity string on the stack; `fmt::Write` that truncates
/// (marked) instead of failing, for error messages and number formatting
/// on paths that must not allocate.
#[derive(Clone, Copy)]
pub struct StackStr<const N: usize> {
    buffer: [u8; N],
    // The high bit records truncation; the other bits hold the byte length.
    length_and_truncation: u32,
}

impl<const N: usize> PartialEq for StackStr<N> {
    fn eq(&self, other: &Self) -> bool {
        self.is_truncated() == other.is_truncated() && self.as_str() == other.as_str()
    }
}

impl<const N: usize> Eq for StackStr<N> {}

impl<const N: usize> fmt::Debug for StackStr<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const N: usize> fmt::Display for StackStr<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const N: usize> StackStr<N> {
    pub const fn new() -> Self {
        assert!(
            N <= 0x7fff_ffff,
            "fixed string capacity exceeds its length field"
        );
        Self {
            buffer: [0; N],
            length_and_truncation: 0,
        }
    }

    /// Builds a `StackStr` from `s`, writing it in full. `is_truncated()` is set
    /// if `s` did not fit.
    pub fn from_str(s: &str) -> Self {
        let mut out = Self::new();
        let _ = core::fmt::Write::write_str(&mut out, s);
        out
    }

    pub fn as_str(&self) -> &str {
        // Only whole UTF-8 sequences are ever appended.
        unsafe { core::str::from_utf8_unchecked(&self.buffer[..self.len()]) }
    }

    pub fn is_truncated(&self) -> bool {
        self.length_and_truncation & 0x8000_0000 != 0
    }

    pub fn clear(&mut self) {
        self.length_and_truncation = 0;
    }

    pub const fn len(&self) -> usize {
        (self.length_and_truncation & 0x7fff_ffff) as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn truncate(&mut self, len: usize) {
        assert!(len <= self.len() && self.as_str().is_char_boundary(len));
        self.length_and_truncation = len as u32;
    }
}

impl<const N: usize> Default for StackStr<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> fmt::Write for StackStr<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let len = self.len();
        let space = N - len;
        if s.len() <= space {
            self.buffer[len..len + s.len()].copy_from_slice(s.as_bytes());
            self.length_and_truncation += s.len() as u32;
        } else {
            // Take the longest prefix that is still valid UTF-8.
            let mut cut = space;
            while cut > 0 && !s.is_char_boundary(cut) {
                cut -= 1;
            }
            self.buffer[len..len + cut].copy_from_slice(&s.as_bytes()[..cut]);
            self.length_and_truncation = (len + cut) as u32 | 0x8000_0000;
        }
        Ok(())
    }
}

/// Formats any Display value into a StackStr.
#[macro_export]
macro_rules! stack_format {
    ($n:expr, $($arg:tt)*) => {{
        let mut s = $crate::util::StackStr::<{ $n }>::new();
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!($($arg)*));
        s
    }};
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use core::fmt::Write;

    #[test]
    fn formats_and_truncates() {
        let mut s = StackStr::<8>::new();
        write!(s, "{}", 12345).unwrap();
        assert_eq!(s.as_str(), "12345");
        assert!(!s.is_truncated());
        write!(s, "yyyy").unwrap();
        assert_eq!(s.as_str(), "12345yyy");
        assert!(s.is_truncated());
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        let mut s = StackStr::<5>::new();
        write!(s, "aé€x").unwrap(); // 1 + 2 + 3 + 1 bytes
        assert_eq!(s.as_str(), "aé");
        assert!(s.is_truncated());
    }

    #[test]
    fn equality_uses_the_value_not_unused_capacity() {
        let mut reused = StackStr::<8>::from_str("long");
        reused.clear();
        reused.write_str("ok").unwrap();
        assert_eq!(reused, StackStr::from_str("ok"));
    }

    #[test]
    fn compact_length_preserves_truncation_until_an_explicit_reset() {
        assert_eq!(core::mem::size_of::<StackStr<120>>(), 124);
        crate::mem::guard::forbid_alloc(|| {
            let mut text = StackStr::<5>::from_str("aé€");
            assert_eq!(text.len(), 3);
            text.write_str("x").unwrap();
            assert_eq!(text.as_str(), "aéx");
            assert!(text.is_truncated());
            text.truncate(3);
            assert!(!text.is_truncated());
            text.write_str("é").unwrap();
            assert_eq!(text.as_str(), "aéé");
            assert_eq!(text.len(), 5);
            text.clear();
            assert!(text.is_empty());
            assert!(!text.is_truncated());
            let mut empty = StackStr::<0>::new();
            empty.write_str("x").unwrap();
            assert!(empty.is_empty());
            assert!(empty.is_truncated());
            empty.truncate(0);
            assert!(!empty.is_truncated());
        });
    }

    #[test]
    fn macro_does_not_allocate() {
        crate::mem::guard::forbid_alloc(|| {
            let s = stack_format!(32, "row {} of {}", 5, 10);
            assert_eq!(s.as_str(), "row 5 of 10");
        });
    }
}

/// Displays an I/O error without allocating, unlike `io::Error`'s own
/// `Display`, which copies the OS message into a `String` and so cannot run
/// after the startup memory freeze.
pub(crate) struct IoErrorText<'a>(pub &'a std::io::Error);

impl core::fmt::Display for IoErrorText<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0.raw_os_error() {
            Some(code) => write!(formatter, "{} (os error {code})", self.0.kind()),
            None => write!(formatter, "{}", self.0.kind()),
        }
    }
}
