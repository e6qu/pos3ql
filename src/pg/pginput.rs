//! Bounded decoder for PostgreSQL logical-replication `pgoutput` frames.
//!
//! The decoder is deliberately separate from transport and table mutation:
//! decoding a wire frame creates a complete typed message, or no message at
//! all.  An apply worker therefore cannot advance a durable subscription from
//! a truncated tuple, an unknown relation shape, or a partly parsed frame.

use crate::storage::MAX_COLUMNS;

/// PostgreSQL permits an arbitrary list here.  The apply worker is
/// startup-bounded, so its protocol boundary names the matching limit.
pub const MAX_TRUNCATE_RELATIONS: usize = 255;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    Invalid,
    Limit,
    Utf8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TupleColumn<'a> {
    Null,
    UnchangedToast,
    Text(&'a [u8]),
    Binary(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuple<'a> {
    columns: [TupleColumn<'a>; MAX_COLUMNS],
    count: usize,
}

/// Which publisher row image accompanies an UPDATE or DELETE.  `Key` carries
/// exactly the relation columns marked as replica-identity keys; `Old` carries
/// every relation column.  Keeping this distinction at the parse boundary
/// prevents an apply caller from guessing how a short tuple is mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaIdentity {
    Key,
    Old,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OldTuple<'a> {
    pub identity: ReplicaIdentity,
    pub tuple: Tuple<'a>,
}

impl<'a> Tuple<'a> {
    pub fn columns(&self) -> &[TupleColumn<'a>] {
        &self.columns[..self.count]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelationColumn<'a> {
    pub key: bool,
    pub name: &'a str,
    pub type_oid: u32,
    pub type_modifier: i32,
}

const EMPTY_RELATION_COLUMN: RelationColumn<'static> = RelationColumn {
    key: false,
    name: "",
    type_oid: 0,
    type_modifier: 0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Relation<'a> {
    pub id: u32,
    pub namespace: &'a str,
    pub name: &'a str,
    pub replica_identity: u8,
    columns: [RelationColumn<'a>; MAX_COLUMNS],
    count: usize,
}

impl<'a> Relation<'a> {
    pub fn columns(&self) -> &[RelationColumn<'a>] {
        &self.columns[..self.count]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Truncate {
    relation_ids: [u32; MAX_TRUNCATE_RELATIONS],
    count: usize,
    pub cascade: bool,
    pub restart_identity: bool,
}

impl Truncate {
    pub fn relation_ids(&self) -> &[u32] {
        &self.relation_ids[..self.count]
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "relation, tuple, and truncation frames stay inline so decoding never allocates"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    Begin {
        final_lsn: u64,
        xid: u32,
    },
    Commit {
        commit_lsn: u64,
        end_lsn: u64,
    },
    Relation(Relation<'a>),
    Type {
        oid: u32,
        namespace: &'a str,
        name: &'a str,
    },
    Insert {
        relation_id: u32,
        new: Tuple<'a>,
    },
    Update {
        relation_id: u32,
        old: Option<OldTuple<'a>>,
        new: Tuple<'a>,
    },
    Delete {
        relation_id: u32,
        old: OldTuple<'a>,
    },
    Truncate(Truncate),
}

#[allow(
    clippy::large_enum_variant,
    reason = "a CopyData frame owns its decoded fixed message rather than a heap indirection"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyData<'a> {
    XLogData {
        start_lsn: u64,
        end_lsn: u64,
        message: Message<'a>,
    },
    PrimaryKeepalive {
        end_lsn: u64,
        reply_requested: bool,
    },
}

struct Input<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Input<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        let value = *self.bytes.get(self.at).ok_or(DecodeError::Truncated)?;
        self.at += 1;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.take::<2>()?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.take::<4>()?))
    }

    fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_be_bytes(self.take::<4>()?))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.take::<8>()?))
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let bytes = self
            .bytes
            .get(self.at..self.at + N)
            .ok_or(DecodeError::Truncated)?;
        self.at += N;
        bytes.try_into().map_err(|_| DecodeError::Truncated)
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let bytes = self
            .bytes
            .get(self.at..self.at.checked_add(len).ok_or(DecodeError::Invalid)?)
            .ok_or(DecodeError::Truncated)?;
        self.at += len;
        Ok(bytes)
    }

    fn cstr(&mut self) -> Result<&'a str, DecodeError> {
        let rest = self.bytes.get(self.at..).ok_or(DecodeError::Truncated)?;
        let length = rest
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(DecodeError::Truncated)?;
        let raw = self.bytes(length)?;
        self.u8()?;
        core::str::from_utf8(raw).map_err(|_| DecodeError::Utf8)
    }
}

fn tuple<'a>(input: &mut Input<'a>) -> Result<Tuple<'a>, DecodeError> {
    if input.u8()? != b'N' {
        return Err(DecodeError::Invalid);
    }
    let count = usize::from(input.u16()?);
    if count > MAX_COLUMNS {
        return Err(DecodeError::Limit);
    }
    let mut columns = [TupleColumn::Null; MAX_COLUMNS];
    for column in &mut columns[..count] {
        *column = match input.u8()? {
            b'n' => TupleColumn::Null,
            b'u' => TupleColumn::UnchangedToast,
            b't' => {
                let length = input.i32()?.try_into().map_err(|_| DecodeError::Invalid)?;
                TupleColumn::Text(input.bytes(length)?)
            }
            b'b' => {
                let length = input.i32()?.try_into().map_err(|_| DecodeError::Invalid)?;
                TupleColumn::Binary(input.bytes(length)?)
            }
            _ => return Err(DecodeError::Invalid),
        };
    }
    Ok(Tuple { columns, count })
}

fn message(bytes: &[u8]) -> Result<Message<'_>, DecodeError> {
    let mut input = Input::new(bytes);
    let message = match input.u8()? {
        b'B' => {
            let final_lsn = input.u64()?;
            let _commit_time = input.u64()?;
            let xid = input.u32()?;
            Message::Begin { final_lsn, xid }
        }
        b'C' => {
            if input.u8()? != 0 {
                return Err(DecodeError::Invalid);
            }
            let commit_lsn = input.u64()?;
            let end_lsn = input.u64()?;
            let _commit_time = input.u64()?;
            Message::Commit {
                commit_lsn,
                end_lsn,
            }
        }
        b'R' => {
            let id = input.u32()?;
            let namespace = input.cstr()?;
            let name = input.cstr()?;
            let replica_identity = input.u8()?;
            let count = usize::from(input.u16()?);
            if count > MAX_COLUMNS {
                return Err(DecodeError::Limit);
            }
            let mut columns = [EMPTY_RELATION_COLUMN; MAX_COLUMNS];
            for column in &mut columns[..count] {
                let key = match input.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(DecodeError::Invalid),
                };
                *column = RelationColumn {
                    key,
                    name: input.cstr()?,
                    type_oid: input.u32()?,
                    type_modifier: input.i32()?,
                };
            }
            Message::Relation(Relation {
                id,
                namespace,
                name,
                replica_identity,
                columns,
                count,
            })
        }
        b'Y' => Message::Type {
            oid: input.u32()?,
            namespace: input.cstr()?,
            name: input.cstr()?,
        },
        b'I' => {
            let relation_id = input.u32()?;
            Message::Insert {
                relation_id,
                new: tuple(&mut input)?,
            }
        }
        b'U' => {
            let relation_id = input.u32()?;
            let old = match input.bytes.get(input.at).copied() {
                Some(tag @ (b'K' | b'O')) => {
                    input.at += 1;
                    Some(OldTuple {
                        identity: if tag == b'K' {
                            ReplicaIdentity::Key
                        } else {
                            ReplicaIdentity::Old
                        },
                        tuple: tuple(&mut input)?,
                    })
                }
                _ => None,
            };
            Message::Update {
                relation_id,
                old,
                new: tuple(&mut input)?,
            }
        }
        b'D' => {
            let relation_id = input.u32()?;
            let tag = input.u8()?;
            let identity = match tag {
                b'K' => ReplicaIdentity::Key,
                b'O' => ReplicaIdentity::Old,
                _ => return Err(DecodeError::Invalid),
            };
            Message::Delete {
                relation_id,
                old: OldTuple {
                    identity,
                    tuple: tuple(&mut input)?,
                },
            }
        }
        b'T' => {
            let count: usize = input.u32()?.try_into().map_err(|_| DecodeError::Limit)?;
            if count > MAX_TRUNCATE_RELATIONS {
                return Err(DecodeError::Limit);
            }
            let flags = input.u8()?;
            if flags & !3 != 0 {
                return Err(DecodeError::Invalid);
            }
            let mut relation_ids = [0; MAX_TRUNCATE_RELATIONS];
            for relation_id in &mut relation_ids[..count] {
                *relation_id = input.u32()?;
            }
            Message::Truncate(Truncate {
                relation_ids,
                count,
                cascade: flags & 1 != 0,
                restart_identity: flags & 2 != 0,
            })
        }
        _ => return Err(DecodeError::Invalid),
    };
    input.done().then_some(message).ok_or(DecodeError::Invalid)
}

/// Decodes one complete `CopyData` payload received during `START_REPLICATION`.
pub fn copy_data(bytes: &[u8]) -> Result<CopyData<'_>, DecodeError> {
    let mut input = Input::new(bytes);
    match input.u8()? {
        b'w' => {
            let start_lsn = input.u64()?;
            let end_lsn = input.u64()?;
            let _send_time = input.u64()?;
            let message = message(input.bytes(input.bytes.len() - input.at)?)?;
            Ok(CopyData::XLogData {
                start_lsn,
                end_lsn,
                message,
            })
        }
        b'k' => {
            let end_lsn = input.u64()?;
            let _send_time = input.u64()?;
            let reply_requested = match input.u8()? {
                0 => false,
                1 => true,
                _ => return Err(DecodeError::Invalid),
            };
            input
                .done()
                .then_some(CopyData::PrimaryKeepalive {
                    end_lsn,
                    reply_requested,
                })
                .ok_or(DecodeError::Invalid)
        }
        _ => Err(DecodeError::Invalid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::guard;

    fn xlog(plugin: &[u8]) -> [u8; 256] {
        let mut bytes = [0u8; 256];
        bytes[0] = b'w';
        bytes[1..9].copy_from_slice(&7u64.to_be_bytes());
        bytes[9..17].copy_from_slice(&9u64.to_be_bytes());
        bytes[25..25 + plugin.len()].copy_from_slice(plugin);
        bytes
    }

    #[test]
    fn decodes_bounded_insert_text_and_binary_tuples_without_allocation() {
        let plugin = [
            b'I', 0, 0, 0, 7, b'N', 0, 3, b't', 0, 0, 0, 2, b'4', b'2', b'n', b'b', 0, 0, 0, 1, 9,
        ];
        let bytes = xlog(&plugin);
        guard::forbid_alloc(|| {
            let CopyData::XLogData {
                start_lsn,
                end_lsn,
                message: Message::Insert { relation_id, new },
            } = copy_data(&bytes[..25 + plugin.len()]).unwrap()
            else {
                panic!("wrong frame")
            };
            assert_eq!((start_lsn, end_lsn, relation_id), (7, 9, 7));
            assert_eq!(
                new.columns(),
                [
                    TupleColumn::Text(b"42"),
                    TupleColumn::Null,
                    TupleColumn::Binary(&[9]),
                ]
            );
        });
    }

    #[test]
    fn relation_and_truncation_require_complete_valid_frames() {
        let relation_frame = [
            b'R', 0, 0, 0, 5, b'p', b'u', b'b', 0, b't', 0, b'f', 0, 1, 1, b'i', b'd', 0, 0, 0, 0,
            23, 255, 255, 255, 255,
        ];
        let bytes = xlog(&relation_frame);
        let CopyData::XLogData {
            message: Message::Relation(relation),
            ..
        } = copy_data(&bytes[..25 + relation_frame.len()]).unwrap()
        else {
            panic!("wrong frame")
        };
        assert_eq!(relation.id, 5);
        assert_eq!(relation.namespace, "pub");
        assert_eq!(relation.columns()[0].name, "id");
        assert_eq!(
            copy_data(&bytes[..25 + relation_frame.len() - 1]),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn old_tuple_kind_preserves_key_vs_full_row_mapping() {
        let plugin = [
            b'U', 0, 0, 0, 7, b'K', b'N', 0, 1, b't', 0, 0, 0, 1, b'1', b'N', 0, 2, b't', 0, 0, 0,
            1, b'1', b'u',
        ];
        let bytes = xlog(&plugin);
        let CopyData::XLogData {
            message:
                Message::Update {
                    relation_id,
                    old: Some(old),
                    new,
                },
            ..
        } = copy_data(&bytes[..25 + plugin.len()]).unwrap()
        else {
            panic!("wrong frame")
        };
        assert_eq!(relation_id, 7);
        assert_eq!(old.identity, ReplicaIdentity::Key);
        assert_eq!(old.tuple.columns(), [TupleColumn::Text(b"1")]);
        assert_eq!(
            new.columns(),
            [TupleColumn::Text(b"1"), TupleColumn::UnchangedToast]
        );
    }

    #[test]
    fn keepalive_and_unknown_messages_are_not_reinterpreted() {
        let keepalive = [b'k', 0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        assert_eq!(
            copy_data(&keepalive),
            Ok(CopyData::PrimaryKeepalive {
                end_lsn: 9,
                reply_requested: true,
            })
        );
        assert_eq!(copy_data(b"w\0\0\0\0"), Err(DecodeError::Truncated));
    }
}
