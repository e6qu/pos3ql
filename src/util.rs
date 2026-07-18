//! Small allocation-free helpers.

use core::fmt;

/// A fixed-capacity string on the stack; `fmt::Write` that truncates
/// (marked) instead of failing, for error messages and number formatting
/// on paths that must not allocate.
#[derive(Clone, Copy)]
pub struct StackStr<const N: usize> {
    buf: [u8; N],
    len: usize,
    truncated: bool,
}

impl<const N: usize> fmt::Debug for StackStr<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const N: usize> StackStr<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
            truncated: false,
        }
    }

    pub fn as_str(&self) -> &str {
        // Only whole UTF-8 sequences are ever appended.
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.truncated = false;
    }
}

impl<const N: usize> Default for StackStr<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> fmt::Write for StackStr<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let space = N - self.len;
        if s.len() <= space {
            self.buf[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
            self.len += s.len();
        } else {
            // Take the longest prefix that is still valid UTF-8.
            let mut cut = space;
            while cut > 0 && !s.is_char_boundary(cut) {
                cut -= 1;
            }
            self.buf[self.len..self.len + cut].copy_from_slice(&s.as_bytes()[..cut]);
            self.len += cut;
            self.truncated = true;
        }
        Ok(())
    }
}

/// Formats any Display value into a StackStr.
#[macro_export]
macro_rules! stack_format {
    ($n:literal, $($arg:tt)*) => {{
        let mut s = $crate::util::StackStr::<$n>::new();
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!($($arg)*));
        s
    }};
}

#[cfg(test)]
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
    fn macro_does_not_allocate() {
        crate::mem::guard::forbid_alloc(|| {
            let s = stack_format!(32, "row {} of {}", 5, 10);
            assert_eq!(s.as_str(), "row 5 of 10");
        });
    }
}
