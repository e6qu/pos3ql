//! `jsonb`'s btree order (`compareJsonbContainers`, `compareJsonbScalarValue`
//! in `jsonb_util.c`), read from the canonical text of two documents without
//! building them: values of different kinds order `null` < string < number <
//! boolean < array < object; arrays and objects compare their sizes first,
//! then member by member (an object's pairs in storage order, its key before
//! its value); a scalar document is a one-element "raw scalar" array that
//! sorts before any real array of at least one element.

use core::cmp::Ordering;

/// A cursor over canonical `jsonb` text.
#[derive(Clone, Copy)]
struct Scan<'t> {
    text: &'t [u8],
    at: usize,
}

/// A value's kind in `jsonb`'s own order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Null,
    String,
    Number,
    Bool,
    Array,
    Object,
}

impl<'t> Scan<'t> {
    fn skip_space(&mut self) {
        while self.text.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> u8 {
        self.skip_space();
        self.text.get(self.at).copied().unwrap_or(0)
    }

    fn kind(&mut self) -> Kind {
        match self.peek() {
            b'[' => Kind::Array,
            b'{' => Kind::Object,
            b'"' => Kind::String,
            b't' | b'f' => Kind::Bool,
            b'n' => Kind::Null,
            _ => Kind::Number,
        }
    }

    /// Steps over one byte the caller saw.
    fn bump(&mut self) {
        self.at += 1;
    }

    /// Steps over one scalar, returning its text (a string's without its
    /// quotes).
    fn scalar(&mut self) -> &'t str {
        self.skip_space();
        let start = self.at;
        if self.text[start] == b'"' {
            self.at += 1;
            while self.text[self.at] != b'"' {
                self.at += if self.text[self.at] == b'\\' { 2 } else { 1 };
            }
            self.at += 1;
            return self.slice(start + 1, self.at - 1);
        }
        while self.text.get(self.at).is_some_and(|byte| {
            !matches!(byte, b',' | b']' | b'}' | b':') && !byte.is_ascii_whitespace()
        }) {
            self.at += 1;
        }
        self.slice(start, self.at)
    }

    fn slice(&self, start: usize, end: usize) -> &'t str {
        // The text is valid UTF-8 and the cut points are ASCII delimiters.
        core::str::from_utf8(&self.text[start..end]).expect("canonical jsonb text")
    }

    /// Steps over one whole value.
    fn skip_value(&mut self) {
        match self.kind() {
            Kind::Array | Kind::Object => {
                let mut depth = 0usize;
                loop {
                    match self.peek() {
                        b'[' | b'{' => {
                            depth += 1;
                            self.bump();
                        }
                        b']' | b'}' => {
                            depth -= 1;
                            self.bump();
                            if depth == 0 {
                                return;
                            }
                        }
                        b'"' => {
                            self.scalar();
                        }
                        _ => self.bump(),
                    }
                }
            }
            _ => {
                self.scalar();
            }
        }
    }

    /// The members of the container whose opening bracket is next: its
    /// element or pair count, the cursor left where it was.
    fn members(&mut self) -> usize {
        let mut probe = *self;
        let object = probe.peek() == b'{';
        probe.bump();
        if matches!(probe.peek(), b']' | b'}') {
            return 0;
        }
        let mut count = 0usize;
        loop {
            if object {
                probe.scalar();
                probe.skip_space();
                probe.bump();
            }
            probe.skip_value();
            count += 1;
            if probe.peek() == b',' {
                probe.bump();
            } else {
                return count;
            }
        }
    }
}

/// The next character of a JSON string's text, its escapes decoded.
fn next_char(chars: &mut core::str::Chars<'_>) -> Option<u32> {
    let next = chars.next()?;
    if next != '\\' {
        return Some(next as u32);
    }
    let hex = |chars: &mut core::str::Chars<'_>| {
        (0..4).fold(0u32, |value, _| {
            value * 16
                + chars
                    .next()
                    .and_then(|digit| digit.to_digit(16))
                    .unwrap_or(0)
        })
    };
    Some(match chars.next()? {
        'b' => 8,
        'f' => 12,
        'n' => 10,
        'r' => 13,
        't' => 9,
        'u' => {
            let unit = hex(chars);
            if (0xd800..0xdc00).contains(&unit) {
                // A surrogate pair: the escape that follows is the low half.
                chars.next();
                chars.next();
                0x10000 + ((unit - 0xd800) << 10) + (hex(chars) & 0x3ff)
            } else {
                unit
            }
        }
        other => other as u32,
    })
}

/// Orders two strings' characters, which is their UTF-8 bytes' order.
fn compare_strings(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars(), b.chars());
    loop {
        match (next_char(&mut a), next_char(&mut b)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x != y => return x.cmp(&y),
            _ => {}
        }
    }
}

/// `compareJsonbScalarValue` of two scalars of the same kind.
fn compare_scalars(kind: Kind, a: &str, b: &str) -> Ordering {
    match kind {
        Kind::Null => Ordering::Equal,
        Kind::String => compare_strings(a, b),
        Kind::Number => crate::sql::numeric::cmp_decimal_str(a, b).unwrap_or(Ordering::Equal),
        Kind::Bool => (a == "true").cmp(&(b == "true")),
        Kind::Array | Kind::Object => unreachable!("a scalar kind"),
    }
}

/// Compares the next value of `a` with the next of `b`, leaving both past
/// it when they are equal.
fn compare_values(a: &mut Scan<'_>, b: &mut Scan<'_>) -> Ordering {
    let (kind_a, kind_b) = (a.kind(), b.kind());
    if kind_a != kind_b {
        return kind_a.cmp(&kind_b);
    }
    match kind_a {
        Kind::Array | Kind::Object => {
            let (count_a, count_b) = (a.members(), b.members());
            if count_a != count_b {
                return count_a.cmp(&count_b);
            }
            let object = kind_a == Kind::Object;
            a.bump();
            b.bump();
            for _ in 0..count_a {
                if object {
                    let ordering = compare_strings(a.scalar(), b.scalar());
                    if ordering.is_ne() {
                        return ordering;
                    }
                    a.peek();
                    b.peek();
                    a.bump();
                    b.bump();
                }
                let ordering = compare_values(a, b);
                if ordering.is_ne() {
                    return ordering;
                }
                if a.peek() == b',' {
                    a.bump();
                }
                if b.peek() == b',' {
                    b.bump();
                }
            }
            a.peek();
            b.peek();
            a.bump();
            b.bump();
            Ordering::Equal
        }
        scalar => {
            let (x, y) = (a.scalar(), b.scalar());
            compare_scalars(scalar, x, y)
        }
    }
}

/// The order of two canonical `jsonb` documents.
pub(crate) fn compare_jsonb(a: &str, b: &str) -> Ordering {
    let mut a = Scan {
        text: a.as_bytes(),
        at: 0,
    };
    let mut b = Scan {
        text: b.as_bytes(),
        at: 0,
    };
    let (kind_a, kind_b) = (a.kind(), b.kind());
    let scalar = |kind: Kind| kind < Kind::Array;
    match (scalar(kind_a), scalar(kind_b)) {
        // Two scalar documents: raw-scalar arrays of one element each.
        (true, true) => compare_values(&mut a, &mut b),
        // A raw-scalar array against a real array: the sizes decide, else
        // the raw scalar sorts first; against an object, the array kind does.
        (true, false) if kind_b == Kind::Array => 1.cmp(&b.members()).then(Ordering::Less),
        (false, true) if kind_a == Kind::Array => a.members().cmp(&1).then(Ordering::Greater),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => compare_values(&mut a, &mut b),
    }
}

#[cfg(all(
    test,
    not(all(miri, feature = "miri-ownership")),
    not(feature = "sql-fixture-suite")
))]
mod tests {
    use super::*;

    // Orders from PostgreSQL 18.6's `jsonb` btree comparison.
    #[test]
    fn documents_order_as_jsonb_does() {
        let ascending = [
            "[]",
            "null",
            "\"a\"",
            "\"aA\"",
            "\"b\"",
            "\"z\"",
            "\"\u{e9}\"",
            "\"\u{1f600}\"",
            "-1",
            "1",
            "1.5",
            "22.0",
            "218.0",
            "false",
            "true",
            "[null]",
            "[\"a\"]",
            "[1]",
            "[2]",
            "[true]",
            "[[1]]",
            "[{}]",
            "[1, 2]",
            "{}",
            "{\"a\": null}",
            "{\"a\": 1}",
            "{\"a\": 2}",
            "{\"aa\": 1}",
            "{\"b\": 1}",
            "{\"b\": [1]}",
            "{\"b\": {\"c\": 1}}",
            "{\"a\": 1, \"b\": 1}",
            "{\"a\": 1, \"c\": 1}",
            "{\"a\": 1, \"n\": 22.0}",
            "{\"a\": 1, \"n\": 218.0}",
        ];
        for (low, first) in ascending.iter().enumerate() {
            for (high, second) in ascending.iter().enumerate() {
                assert_eq!(
                    compare_jsonb(first, second),
                    low.cmp(&high),
                    "{first} vs {second}"
                );
            }
        }
    }
}
