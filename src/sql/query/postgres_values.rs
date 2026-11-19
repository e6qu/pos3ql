//! The bytes PostgreSQL 18.6 stores for values whose binary form pos3ql keeps
//! as text or a private encoding: `jsonb` (`jsonb_util.c`'s
//! `convertToJsonb`), arrays (`construct_md_array`), `bit`, geometric paths
//! and polygons. Each size counts the data after the four-byte varlena header,
//! as a tuple holds the value.

use crate::sql::types::ArrElem;

use super::postgres_memory::{Stored, maxalign, stored};

/// The bytes of a `jsonb` value after its varlena header. `text` is the
/// canonical rendering, whose members come in storage order. `None` for text
/// that is not canonical `jsonb`.
pub(crate) fn jsonb_payload(text: &str) -> Option<usize> {
    let mut parser = Jsonb {
        text: text.as_bytes(),
        at: 0,
        len: 4,
    };
    parser.skip_space();
    // A top-level scalar is stored as a one-element array flagged raw.
    let scalar = !matches!(parser.peek()?, b'[' | b'{');
    if scalar {
        parser.pad();
        parser.len += 4 + 4;
    }
    parser.value()?;
    parser.skip_space();
    (parser.at == parser.text.len()).then_some(parser.len - 4)
}

/// `convertJsonbValue`'s running buffer length over the canonical text.
struct Jsonb<'t> {
    text: &'t [u8],
    at: usize,
    /// `buffer->len`: the varlena header, then the root container.
    len: usize,
}

impl<'t> Jsonb<'t> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    /// A walker over the same text from here, whose length is its own.
    fn probe(&self) -> Jsonb<'t> {
        Jsonb {
            text: self.text,
            at: self.at,
            len: 0,
        }
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        self.skip_space();
        (self.peek()? == byte).then(|| self.at += 1)
    }

    /// `padBufferToInt`.
    fn pad(&mut self) {
        self.len = self.len.next_multiple_of(4);
    }

    fn value(&mut self) -> Option<()> {
        self.skip_space();
        match self.peek()? {
            b'[' => self.array(),
            b'{' => self.object(),
            b'"' => {
                self.len += self.string()?;
                Some(())
            }
            b't' | b'f' | b'n' => {
                let word: &[u8] = match self.peek()? {
                    b't' => b"true",
                    b'f' => b"false",
                    _ => b"null",
                };
                self.text
                    .get(self.at..self.at + word.len())?
                    .eq(word)
                    .then(|| {
                        self.at += word.len();
                    })
            }
            _ => self.number(),
        }
    }

    /// `convertJsonbArray`: the header, the entries, then each element.
    fn array(&mut self) -> Option<()> {
        self.expect(b'[')?;
        self.pad();
        self.len += 4;
        self.skip_space();
        if self.peek()? == b']' {
            self.at += 1;
            return Some(());
        }
        // The entries are reserved before the first element's data.
        let mut probe = self.probe();
        let mut count = 0usize;
        loop {
            probe.value()?;
            count += 1;
            probe.skip_space();
            match probe.peek()? {
                b',' => probe.at += 1,
                b']' => break,
                _ => return None,
            }
        }
        self.len += 4 * count;
        loop {
            self.value()?;
            self.skip_space();
            match self.peek()? {
                b',' => self.at += 1,
                b']' => {
                    self.at += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// `convertJsonbObject`: the header, the key and value entries, every
    /// key, then every value.
    fn object(&mut self) -> Option<()> {
        self.expect(b'{')?;
        self.pad();
        self.len += 4;
        self.skip_space();
        if self.peek()? == b'}' {
            self.at += 1;
            return Some(());
        }
        let mut pairs = 0usize;
        let mut probe = self.probe();
        loop {
            probe.skip_space();
            probe.string()?;
            probe.expect(b':')?;
            probe.value()?;
            pairs += 1;
            probe.skip_space();
            match probe.peek()? {
                b',' => probe.at += 1,
                b'}' => break,
                _ => return None,
            }
        }
        self.len += 8 * pairs;
        // Keys first: strings only, so the walk skips the values.
        let mut keys = self.probe();
        loop {
            keys.skip_space();
            self.len += keys.string()?;
            keys.expect(b':')?;
            keys.skip_value()?;
            keys.skip_space();
            match keys.peek()? {
                b',' => keys.at += 1,
                _ => break,
            }
        }
        // Then the values, whose alignment follows the keys' bytes.
        loop {
            self.skip_space();
            self.string()?;
            self.expect(b':')?;
            self.value()?;
            self.skip_space();
            match self.peek()? {
                b',' => self.at += 1,
                b'}' => {
                    self.at += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// Steps over one value without sizing it.
    fn skip_value(&mut self) -> Option<()> {
        let mut skipped = self.probe();
        skipped.value()?;
        self.at = skipped.at;
        Some(())
    }

    /// One string's decoded UTF-8 length, stepping over it.
    fn string(&mut self) -> Option<usize> {
        self.skip_space();
        if self.peek()? != b'"' {
            return None;
        }
        self.at += 1;
        let mut bytes = 0usize;
        loop {
            let byte = self.peek()?;
            self.at += 1;
            match byte {
                b'"' => return Some(bytes),
                b'\\' => {
                    let escape = self.peek()?;
                    self.at += 1;
                    if escape == b'u' {
                        let unit = self.hex4()?;
                        bytes += if (0xd800..0xdc00).contains(&unit) {
                            // A surrogate pair is one four-byte character.
                            if self.peek()? != b'\\' || self.text.get(self.at + 1) != Some(&b'u') {
                                return None;
                            }
                            self.at += 2;
                            self.hex4()?;
                            4
                        } else if unit < 0x80 {
                            1
                        } else if unit < 0x800 {
                            2
                        } else {
                            3
                        };
                    } else {
                        bytes += 1;
                    }
                }
                _ => bytes += 1,
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.text.get(self.at..self.at + 4)?;
        self.at += 4;
        let digits = core::str::from_utf8(digits).ok()?;
        u32::from_str_radix(digits, 16).ok()
    }

    /// A number: padded to the next four bytes, then its `numeric` datum.
    fn number(&mut self) -> Option<()> {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'))
        {
            self.at += 1;
        }
        let digits = core::str::from_utf8(&self.text[start..self.at]).ok()?;
        self.pad();
        self.len += 4 + decimal_payload(digits)?;
        Some(())
    }
}

/// The bytes of a `numeric` after its header, from its plain decimal text
/// (`numeric_in`'s result: base-10000 digits after a short or long header).
pub(crate) fn decimal_payload(text: &str) -> Option<usize> {
    let text = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    let dscale = fraction.len();
    let int_groups = whole.len().div_ceil(4);
    let frac_groups = fraction.len().div_ceil(4);
    let group = |index: usize| -> u32 {
        // Group `index` of the digits, aligned at the decimal point.
        let mut value = 0u32;
        for place in 0..4 {
            let position = index * 4 + place;
            let digit = if position < int_groups * 4 {
                let pad = int_groups * 4 - whole.len();
                position
                    .checked_sub(pad)
                    .map_or(0, |at| u32::from(whole.as_bytes()[at] - b'0'))
            } else {
                fraction
                    .as_bytes()
                    .get(position - int_groups * 4)
                    .map_or(0, |&byte| u32::from(byte - b'0'))
            };
            value = value * 10 + digit;
        }
        value
    };
    let total = int_groups + frac_groups;
    let mut first = 0usize;
    while first < total && group(first) == 0 {
        first += 1;
    }
    let mut last = total;
    while last > first && group(last - 1) == 0 {
        last -= 1;
    }
    let digits = last - first;
    // The weight of the first stored group; zero stores no digits.
    let weight = int_groups as i64 - 1 - first as i64;
    let short = dscale <= 63 && (-64..=63).contains(&weight);
    Some(if digits == 0 {
        2
    } else {
        (if short { 2 } else { 4 }) + 2 * digits
    })
}

/// The bytes of a `bit` or `bit varying` value after its header: the bit
/// count, then the bits packed eight to a byte.
pub(crate) fn bit_payload(bits: &str) -> usize {
    4 + bits.len().div_ceil(8)
}

/// The points of a path or polygon in its canonical text: each is the
/// parenthesis pair that holds two coordinates.
fn point_count(text: &str, closed_or_polygon: bool) -> usize {
    let opens = text.bytes().filter(|&byte| byte == b'(').count();
    if closed_or_polygon {
        opens.saturating_sub(1)
    } else {
        opens
    }
}

/// A path's bytes after its header: the point count, the closed flag and
/// padding, then sixteen bytes per point.
pub(crate) fn path_payload(text: &str) -> usize {
    12 + 16 * point_count(text, !text.starts_with('['))
}

/// A polygon's bytes after its header: the point count, the bounding box,
/// then sixteen bytes per point.
pub(crate) fn polygon_payload(text: &str) -> usize {
    36 + 16 * point_count(text, true)
}

/// The bytes of an array after its header (`ARR_OVERHEAD_*`: the type, flags,
/// dimensions and bounds, a null bitmap when an element is `NULL`, then each
/// non-null element at its type's alignment). `None` when an element has no
/// PostgreSQL size here.
pub(crate) fn array_payload(element: ArrElem, raw: &[u8]) -> Option<usize> {
    if matches!(
        element,
        ArrElem::Record | ArrElem::Composite(_) | ArrElem::Nested { .. }
    ) {
        return None;
    }
    let layout = crate::sql::types::ColType::Array(element).physical_layout();
    let element_layout = element.to_coltype().physical_layout();
    let align = match element_layout.alignment {
        b'd' => 8,
        b'i' => 4,
        b's' => 2,
        _ => 1,
    };
    let shape = crate::sql::array::shape(raw)?;
    let count = shape.element_count();
    let mut nulls = false;
    let mut data = 0usize;
    for value in crate::sql::array::elements(raw, element) {
        if value.is_null() {
            nulls = true;
            continue;
        }
        let Stored { payload, .. } = stored(None, &value)?;
        data = data.next_multiple_of(align)
            + if element_layout.length > 0 {
                element_layout.length as usize
            } else {
                // `construct_md_array` detoasts each element to a four-byte
                // header.
                payload + 4
            };
        data = data.next_multiple_of(align);
    }
    let _ = layout;
    let header =
        maxalign(16 + 8 * shape.dimension_count() + if nulls { count.div_ceil(8) } else { 0 });
    Some(header - 4 + data)
}

/// The bytes of an `int2vector` or `oidvector` after its header: a
/// one-dimensional array of `elements` values of `width` bytes.
pub(crate) fn vector_payload(elements: usize, width: usize) -> usize {
    maxalign(16 + 8) - 4 + (elements * width).next_multiple_of(width)
}

#[cfg(all(
    test,
    not(all(miri, feature = "miri-ownership")),
    not(feature = "sql-fixture-suite")
))]
mod tests {
    use super::*;

    // Sizes from PostgreSQL 18.6's `pg_column_size(<value>::jsonb)`.
    #[test]
    fn jsonb_sizes_follow_the_container_layout() {
        for (text, total) in [
            ("{}", 8),
            ("[]", 8),
            ("1", 20),
            ("\"a\"", 13),
            ("true", 12),
            ("null", 12),
            ("[1, 2]", 32),
            ("{\"a\": 1}", 28),
            ("[\"ab\", 12.5, [true, null]]", 48),
            (
                "{\"c\": 12345678.123, \"ab\": \"x\", \"dd\": [1,\"\u{e9}\",{\"k\":0.00001}]}",
                108,
            ),
            ("-0.5", 20),
            ("123456789012345678901234567890.123456789", 40),
            ("\"\\u00e9\\ud83d\\ude00\"", 18),
            (
                "{\"k\": 1, \"n\": 0.33333333333333333333, \"v\": \"z\"}",
                61,
            ),
            (
                "{\"k\": 3, \"n\": 1.00000000000000000000, \"v\": \"zzz\"}",
                55,
            ),
            ("{\"k\": 4, \"n\": 1.3333333333333333, \"v\": \"zzzz\"}", 64),
            (
                "{\"k\": 6, \"n\": 2.0000000000000000, \"v\": \"zzzzzz\"}",
                58,
            ),
            ("{\"k\": 2, \"n\": 3.0000000000000000, \"v\": \"\"}", 52),
        ] {
            assert_eq!(
                jsonb_payload(text).map(|payload| payload + 4),
                Some(total),
                "{text}"
            );
        }
    }

    #[test]
    fn decimals_size_as_base_10000_groups() {
        assert_eq!(decimal_payload("0"), Some(2));
        assert_eq!(decimal_payload("1"), Some(4));
        assert_eq!(decimal_payload("12345"), Some(6));
        assert_eq!(decimal_payload("-0.5"), Some(4));
        assert_eq!(decimal_payload("0.00001"), Some(4));
    }
}
