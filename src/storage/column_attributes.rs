//! PostgreSQL column numbers survive compaction of the engine's live row schema.

use super::{ColumnMeta, MAX_COLUMNS, MAX_RELATION_COLUMNS, SqlName, TableDef};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::type_layouts::PhysicalLayout;
use crate::sql_err;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttributeSlot {
    Empty,
    Live(u16),
    Dropped { layout: PhysicalLayout, compression: u8 },
}

impl AttributeSlot {
    pub(crate) fn encode(self) -> [u8; 8] {
        match self {
            Self::Empty => [0; 8],
            Self::Live(column) => {
                let column = column.to_le_bytes();
                [1, column[0], column[1], 0, 0, 0, 0, 0]
            }
            Self::Dropped { layout, compression } => {
                let length = layout.length.to_le_bytes();
                [2, length[0], length[1], u8::from(layout.by_value), layout.alignment, layout.storage, compression, 0]
            }
        }
    }

    pub(crate) fn decode(bytes: [u8; 8]) -> Option<Self> {
        match bytes[0] {
            1 if bytes[3..] == [0; 5] => Some(Self::Live(u16::from_le_bytes([bytes[1], bytes[2]]))),
            2 if bytes[3] <= 1 && matches!(bytes[4], b'c' | b's' | b'i' | b'd')
                && crate::sql::ast::ColumnStorage::from_code(bytes[5]).is_some()
                && crate::sql::ast::ColumnCompression::from_code(bytes[6]).is_some() && bytes[7] == 0 => {
                let length = i16::from_le_bytes([bytes[1], bytes[2]]);
                if length < -2 || length == 0 { return None; }
                Some(Self::Dropped { layout: PhysicalLayout { length, by_value: bytes[3] != 0,
                    alignment: bytes[4], storage: bytes[5] }, compression: bytes[6] })
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColumnAttributes {
    pub(crate) count: u16,
    pub(crate) slots: [AttributeSlot; MAX_RELATION_COLUMNS],
}

impl ColumnAttributes {
    pub(crate) const EMPTY: Self = Self { count: 0, slots: [AttributeSlot::Empty; MAX_RELATION_COLUMNS] };

    pub(crate) fn encoded_len(&self) -> usize { 2 + usize::from(self.count) * 8 }

    pub(crate) fn append(&self, output: &mut crate::mem::buffer::FixedBuf) -> bool {
        output.append(&self.count.to_le_bytes())
            && self.slots[..usize::from(self.count)].iter().all(|slot| output.append(&slot.encode()))
    }

    pub(crate) fn decode(bytes: &[u8], at: &mut usize) -> Option<Self> {
        let count = u16::from_le_bytes(bytes.get(*at..*at + 2)?.try_into().ok()?);
        *at += 2;
        if usize::from(count) > MAX_RELATION_COLUMNS { return None; }
        let mut result = Self { count, ..Self::EMPTY };
        for slot in &mut result.slots[..usize::from(count)] {
            *slot = AttributeSlot::decode(bytes.get(*at..*at + 8)?.try_into().ok()?)?;
            *at += 8;
        }
        Some(result)
    }

    pub(crate) fn initialize(&mut self, columns: usize) -> Result<(), SqlError> {
        if self.count != 0 { return Ok(()); }
        if columns > MAX_RELATION_COLUMNS { return Err(exhausted()); }
        self.count = columns as u16;
        for (column, slot) in self.slots[..columns].iter_mut().enumerate() {
            *slot = AttributeSlot::Live(column as u16);
        }
        Ok(())
    }

    pub(crate) fn validate(&self, columns: usize) -> bool {
        let count = usize::from(self.count);
        if count > MAX_RELATION_COLUMNS || columns > MAX_RELATION_COLUMNS { return false; }
        let mut seen = [false; MAX_RELATION_COLUMNS];
        let mut live = 0;
        for slot in &self.slots[..count] {
            match *slot {
                AttributeSlot::Empty => return false,
                AttributeSlot::Live(column) => {
                    let column = usize::from(column);
                    if column >= columns || seen[column] { return false; }
                    seen[column] = true;
                    live += 1;
                }
                AttributeSlot::Dropped { .. } => {}
            }
        }
        live == columns && self.slots[count..].iter().all(|slot| *slot == AttributeSlot::Empty)
    }

    pub(crate) fn number(&self, column: usize) -> Option<u16> {
        self.slots[..usize::from(self.count)].iter().position(|slot| *slot == AttributeSlot::Live(column as u16))
            .map(|position| position as u16 + 1)
    }

    pub(crate) fn column(&self, number: u16) -> Option<usize> {
        match *self.slots.get(usize::from(number.checked_sub(1)?))? {
            AttributeSlot::Live(column) => Some(usize::from(column)),
            _ => None,
        }
    }

    pub(crate) fn rewrite(previous: &TableDef, columns: &[ColumnMeta], mapping: &[Option<SqlName>; MAX_COLUMNS])
        -> Result<Self, SqlError> {
        if columns.len() > MAX_RELATION_COLUMNS { return Err(exhausted()); }
        if !previous.attributes.validate(previous.n_columns) {
            return Err(sql_err!(sqlstate::DATA_EXCEPTION, "invalid previous column attribute layout"));
        }
        let mut result = previous.attributes;
        let mut seen = [false; MAX_RELATION_COLUMNS];
        for slot in &mut result.slots[..usize::from(result.count)] {
            let AttributeSlot::Live(old) = *slot else { continue; };
            let old = usize::from(old);
            if let Some(name) = mapping[old] {
                let target = columns.iter().position(|column| column.name == name).ok_or_else(||
                    sql_err!(sqlstate::DATA_EXCEPTION, "column mapping has no destination"))?;
                if seen[target] { return Err(sql_err!(sqlstate::DATA_EXCEPTION, "column mapping has duplicate destinations")); }
                seen[target] = true;
                *slot = AttributeSlot::Live(target as u16);
            } else {
                let column = previous.columns[old];
                let mut layout = column.ctype.physical_layout();
                layout.storage = column.storage.code();
                *slot = AttributeSlot::Dropped { layout, compression: column.compression.code() };
            }
        }
        for (column, retained) in seen[..columns.len()].iter().enumerate() {
            if !retained {
                let count = usize::from(result.count);
                if count == MAX_RELATION_COLUMNS { return Err(exhausted()); }
                result.slots[count] = AttributeSlot::Live(column as u16);
                result.count += 1;
            }
        }
        Ok(result)
    }
}

fn exhausted() -> SqlError {
    sql_err!(sqlstate::TOO_MANY_COLUMNS, "tables can have at most {} columns", MAX_RELATION_COLUMNS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::types::ColType;

    #[test]
    fn catalog_snapshot_column_numbers_survive_drop_rename_and_same_name_addition() {
        let mut table = TableDef::empty();
        table.n_columns = 2;
        table.columns[0] = ColumnMeta { name: SqlName::parse("a").unwrap(), ctype: ColType::Int8, ..ColumnMeta::EMPTY };
        table.columns[1] = ColumnMeta { name: SqlName::parse("b").unwrap(), ctype: ColType::Int2, ..ColumnMeta::EMPTY };
        table.attributes.initialize(2).unwrap();
        let columns = [ColumnMeta { name: SqlName::parse("renamed_b").unwrap(), ..table.columns[1] }, table.columns[0]];
        let mut mapping = [None; MAX_COLUMNS];
        mapping[1] = Some(columns[0].name);
        crate::mem::guard::forbid_alloc(|| {
            let rewritten = ColumnAttributes::rewrite(&table, &columns, &mapping).unwrap();
            assert!(rewritten.validate(2));
            assert_eq!(rewritten.number(0), Some(2));
            assert_eq!(rewritten.number(1), Some(3));
            assert_eq!(rewritten.column(1), None);
            assert_eq!(rewritten.column(2), Some(0));
            assert_eq!(rewritten.column(3), Some(1));
            let AttributeSlot::Dropped { layout, .. } = rewritten.slots[0] else { panic!("dropped column retained"); };
            assert_eq!(layout.length, 8);
            assert_eq!(layout.alignment, b'd');
            assert!(layout.by_value);
            assert_eq!(table.attributes.number(0), Some(1));
            for slot in rewritten.slots[..usize::from(rewritten.count)].iter().copied() {
                assert_eq!(AttributeSlot::decode(slot.encode()), Some(slot));
            }
            let mut invalid = rewritten;
            invalid.slots[0] = AttributeSlot::Live(0);
            assert!(!invalid.validate(2));
            invalid.slots[0] = AttributeSlot::Live(2);
            assert!(!invalid.validate(2));
            invalid.slots[0] = AttributeSlot::Empty;
            assert!(!invalid.validate(2));
            assert!(AttributeSlot::decode([0; 8]).is_none());
            assert!(AttributeSlot::decode([1, 0, 0, 0, 0, 0, 0, 1]).is_none());
            assert!(AttributeSlot::decode([2, 8, 0, 1, b'z', b'p', 0, 0]).is_none());
        });
    }
}
