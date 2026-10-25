//! Admission of durable, flat catalog rows before trusted projected decoding.

use super::types::{ArrElem, ColType, Datum};

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let (value, rest) = self.0.split_at_checked(length)?;
        self.0 = rest;
        Some(value)
    }

    fn byte(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }

    fn word(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn payload(&mut self) -> Option<&'a [u8]> {
        let length = usize::try_from(self.word()?).ok()?;
        self.take(length)
    }

    fn text(&mut self) -> Option<&'a str> {
        let value = core::str::from_utf8(self.payload()?).ok()?;
        (!value.contains('\0')).then_some(value)
    }

    fn field(&mut self, ctype: ColType) -> Option<()> {
        let tag = self.byte()?;
        if tag == 0 { return Some(()); }
        let fixed = match (ctype, tag) {
            (ColType::Bool, 1) => { return (self.byte()? <= 1).then_some(()); }
            (ColType::Char, 40) => 1,
            (ColType::Int2, 22) => 2,
            (ColType::Int4, 2) | (ColType::Oid | ColType::Xid, 35) | (ColType::Float4, 23) => 4,
            (ColType::Int8, 3) | (ColType::Float8, 4) | (ColType::Timestamptz, 8) | (ColType::PgLsn, 41) => 8,
            (ColType::Interval, 13) => 16,
            (ColType::Text | ColType::Name | ColType::Varchar | ColType::PgNdistinct
                | ColType::PgDependencies | ColType::PgMcvList | ColType::PgStatisticArray, 5)
                | (ColType::Bpchar, 21) => { self.text()?; return Some(()); }
            (ColType::Bytea, 10) => { self.payload()?; return Some(()); }
            (ColType::PgNodeTree, 50) => {
                super::catalog_node::CatalogNode::from_encoded(self.text()?)?;
                return Some(());
            }
            (ColType::Regtype, 30) => {
                self.take(4)?;
                self.text()?;
                return Some(());
            }
            (ColType::Regproc, 31) => {
                if self.word()? != super::types::oid::REGPROC as u32 { return None; }
                self.take(4)?;
                self.text()?;
                return Some(());
            }
            (ColType::Int2Vector, 29) | (ColType::OidVector, 34) => {
                let length = usize::try_from(self.word()?).ok()?;
                let lower = i32::from_le_bytes(self.take(4)?.try_into().ok()?);
                let width = if tag == 29 { 2 } else { 4 };
                if !length.is_multiple_of(width) { return None; }
                lower.checked_add(i32::try_from(length / width).ok()?)?;
                self.take(length)?;
                return Some(());
            }
            (ColType::Array(expected), 15) => {
                let element = ArrElem::from_code(self.byte()?)?;
                if element != expected || !ArrElem::BUILTIN.contains(&element) || element == ArrElem::Record {
                    return None;
                }
                let slot = u16::from_le_bytes(self.take(2)?.try_into().ok()?);
                let base = self.byte()?;
                let base_slot = u16::from_le_bytes(self.take(2)?.try_into().ok()?);
                if slot != ColType::ENUM_SLOT_UNRESOLVED || base != 0 || base_slot != ColType::ENUM_SLOT_UNRESOLVED {
                    return None;
                }
                let raw = self.payload()?;
                super::array::shape(raw)?;
                for encoded in super::array::element_payloads(raw) {
                    let mut value = [Datum::Null];
                    crate::storage::rowenc::decode(encoded, &[element.to_coltype()], &mut value).ok()?;
                }
                return Some(());
            }
            _ => return None,
        };
        self.take(fixed)?;
        Some(())
    }
}

pub(crate) fn validate(
    bytes: &[u8],
    columns: impl Iterator<Item = ColType>,
    table_oid: Option<u32>,
) -> Option<()> {
    let mut reader = Reader(bytes);
    let count = usize::from(u16::from_le_bytes(reader.take(2)?.try_into().ok()?));
    if count > crate::storage::MAX_COLUMNS { return None; }
    let mut seen = 0;
    for ctype in columns {
        if seen == count { return None; }
        reader.field(ctype)?;
        seen += 1;
    }
    if let Some(oid) = table_oid {
        if reader.byte()? != 35 || reader.word()? != oid { return None; }
        seen += 1;
    }
    (seen == count && reader.0.is_empty()).then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_snapshot_durable_nodes_reject_corruption_before_trusted_decoding() {
        let node = super::super::catalog_node::CatalogNode::NULL.as_str();
        let mut bytes = [0u8; 128];
        bytes[..2].copy_from_slice(&2u16.to_le_bytes());
        bytes[2] = 50;
        bytes[3..7].copy_from_slice(&(node.len() as u32).to_le_bytes());
        bytes[7..7 + node.len()].copy_from_slice(node.as_bytes());
        let offset = 7 + node.len();
        bytes[offset] = 35;
        bytes[offset + 1..offset + 5].copy_from_slice(&2606u32.to_le_bytes());
        let length = offset + 5;
        crate::mem::guard::forbid_alloc(|| {
            assert!(validate(&bytes[..length], [ColType::PgNodeTree].into_iter(), Some(2606)).is_some());
            for end in 0..length {
                assert!(validate(&bytes[..end], [ColType::PgNodeTree].into_iter(), Some(2606)).is_none());
            }
            assert!(validate(&bytes[..length], [ColType::PgNodeTree].into_iter(), Some(1259)).is_none());
            assert!(validate(&bytes[..length], [ColType::Text].into_iter(), Some(2606)).is_none());
            bytes[2] = 5;
            assert!(validate(&bytes[..length], [ColType::PgNodeTree].into_iter(), Some(2606)).is_none());
            bytes[2] = 50;
            bytes[7] = b'x';
            assert!(validate(&bytes[..length], [ColType::PgNodeTree].into_iter(), Some(2606)).is_none());
        });
    }
}
