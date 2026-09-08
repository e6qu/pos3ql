//! PostgreSQL transaction-snapshot values.
//!
//! Values keep PostgreSQL's binary payload (`nxip`, `xmin`, `xmax`, `xip`) as
//! their canonical representation.  That makes validation one parse boundary
//! shared by SQL input, arrays, rows, COPY, and the extended protocol.

use core::fmt;

use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, arena_full, sqlstate};
use crate::sql_err;

const HEADER_BYTES: usize = 4 + 8 + 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot<'a> {
    raw: &'a [u8],
}

pub const WITNESS: Snapshot<'static> = Snapshot {
    raw: &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1],
};

impl<'a> Snapshot<'a> {
    pub fn from_binary(raw: &[u8], arena: &'a Arena) -> Result<Self, SqlError> {
        validate_binary(raw)?;
        Ok(Self {
            raw: arena.alloc_slice_copy(raw).map_err(|_| arena_full())?,
        })
    }

    pub fn from_text(input: &str, arena: &'a Arena) -> Result<Self, SqlError> {
        let mut fields = input.splitn(3, ':');
        let xmin_text = fields.next().ok_or_else(|| invalid_text(input))?;
        let xmax_text = fields.next().ok_or_else(|| invalid_text(input))?;
        let xip_text = fields.next().ok_or_else(|| invalid_text(input))?;
        let xmin = parse_u64(xmin_text, input)?;
        let xmax = parse_u64(xmax_text, input)?;
        if xmin == 0 || xmax == 0 || xmax < xmin || xip_text.contains(':') {
            return Err(invalid_text(input));
        }

        let mut count = 0usize;
        let mut previous = None;
        if !xip_text.is_empty() {
            for item in xip_text.split(',') {
                let value = parse_u64(item, input)?;
                if value < xmin || value >= xmax || previous.is_some_and(|prior| value < prior) {
                    return Err(invalid_text(input));
                }
                if previous != Some(value) {
                    count = count.checked_add(1).ok_or_else(|| invalid_text(input))?;
                }
                previous = Some(value);
            }
        }
        let byte_len = HEADER_BYTES
            .checked_add(count.checked_mul(8).ok_or_else(|| invalid_text(input))?)
            .ok_or_else(|| invalid_text(input))?;
        let raw = arena
            .alloc_slice_with(byte_len, |_| 0u8)
            .map_err(|_| arena_full())?;
        raw[..4].copy_from_slice(&(count as u32).to_be_bytes());
        raw[4..12].copy_from_slice(&xmin.to_be_bytes());
        raw[12..20].copy_from_slice(&xmax.to_be_bytes());
        let mut at = HEADER_BYTES;
        previous = None;
        if !xip_text.is_empty() {
            for item in xip_text.split(',') {
                let value = parse_u64(item, input)?;
                if previous != Some(value) {
                    raw[at..at + 8].copy_from_slice(&value.to_be_bytes());
                    at += 8;
                }
                previous = Some(value);
            }
        }
        Ok(Self { raw })
    }

    pub fn from_parts(
        xmin: u64,
        xmax: u64,
        count: usize,
        xip: impl Iterator<Item = u64>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let byte_len = HEADER_BYTES
            .checked_add(count.checked_mul(8).ok_or_else(arena_full)?)
            .ok_or_else(arena_full)?;
        let raw = arena
            .alloc_slice_with(byte_len, |_| 0u8)
            .map_err(|_| arena_full())?;
        raw[..4].copy_from_slice(&(count as u32).to_be_bytes());
        raw[4..12].copy_from_slice(&xmin.to_be_bytes());
        raw[12..20].copy_from_slice(&xmax.to_be_bytes());
        let mut actual_count = 0usize;
        for (index, value) in xip.enumerate() {
            if index == count {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "transaction snapshot changed while being materialized"
                ));
            }
            let at = HEADER_BYTES + index * 8;
            raw[at..at + 8].copy_from_slice(&value.to_be_bytes());
            actual_count += 1;
        }
        if actual_count != count {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "transaction snapshot changed while being materialized"
            ));
        }
        raw[HEADER_BYTES..]
            .as_chunks_mut::<8>()
            .0
            .sort_unstable_by_key(|bytes| u64::from_be_bytes(*bytes));
        validate_binary(raw)?;
        Ok(Self { raw })
    }

    pub const fn raw(self) -> &'a [u8] {
        self.raw
    }

    pub(crate) fn restore(raw: &'a [u8]) -> Result<Self, SqlError> {
        validate_binary(raw)?;
        Ok(Self { raw })
    }

    pub fn xmin(self) -> u64 {
        u64::from_be_bytes(
            self.raw[4..12]
                .try_into()
                .expect("validated snapshot header"),
        )
    }

    pub fn xmax(self) -> u64 {
        u64::from_be_bytes(
            self.raw[12..20]
                .try_into()
                .expect("validated snapshot header"),
        )
    }

    pub fn xip(self) -> impl ExactSizeIterator<Item = u64> + 'a {
        self.raw[HEADER_BYTES..]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|bytes| u64::from_be_bytes(*bytes))
    }

    pub fn visible(self, xid: u64) -> bool {
        xid < self.xmin() || (xid < self.xmax() && !self.xip().any(|active| active == xid))
    }
}

impl fmt::Display for Snapshot<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:", self.xmin(), self.xmax())?;
        for (index, xid) in self.xip().enumerate() {
            if index != 0 {
                f.write_str(",")?;
            }
            write!(f, "{xid}")?;
        }
        Ok(())
    }
}

fn parse_u64(value: &str, whole: &str) -> Result<u64, SqlError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_text(whole));
    }
    value.parse().map_err(|_| invalid_text(whole))
}

fn invalid_text(value: &str) -> SqlError {
    sql_err!(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type pg_snapshot: \"{}\"",
        value
    )
}

fn validate_binary(raw: &[u8]) -> Result<(), SqlError> {
    let invalid = || {
        sql_err!(
            sqlstate::INVALID_BINARY_REPRESENTATION,
            "invalid external pg_snapshot data"
        )
    };
    if raw.len() < HEADER_BYTES {
        return Err(invalid());
    }
    let count = u32::from_be_bytes(raw[..4].try_into().map_err(|_| invalid())?) as usize;
    if raw.len() != HEADER_BYTES.saturating_add(count.saturating_mul(8)) {
        return Err(invalid());
    }
    let xmin = u64::from_be_bytes(raw[4..12].try_into().map_err(|_| invalid())?);
    let xmax = u64::from_be_bytes(raw[12..20].try_into().map_err(|_| invalid())?);
    if xmin == 0 || xmax == 0 || xmax < xmin {
        return Err(invalid());
    }
    let mut previous = None;
    for bytes in raw[HEADER_BYTES..].as_chunks::<8>().0 {
        let value = u64::from_be_bytes(*bytes);
        if value < xmin || value >= xmax || previous.is_some_and(|prior| value <= prior) {
            return Err(invalid());
        }
        previous = Some(value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    #[test]
    fn snapshot_text_is_validated_and_canonicalized() {
        let mut budget = Budget::new(4096);
        let arena = Arena::new(&mut budget, "snapshot test", 4096).unwrap();
        let snapshot = Snapshot::from_text("10:20:10,14,14,15", &arena).unwrap();
        assert_eq!(snapshot.to_string(), "10:20:10,14,15");
        assert!(!snapshot.visible(14));
        assert!(snapshot.visible(13));
        assert!(Snapshot::from_text("10:20:14,13", &arena).is_err());
        assert!(Snapshot::from_text("10:20:20", &arena).is_err());
    }
}
