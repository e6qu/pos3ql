//! Bounded encoder for PostgreSQL's logical `pgoutput` framing.
//!
//! The wire carries an outer XLogData envelope followed by one plugin message.
//! Keeping this byte-level boundary separate from connection state makes every
//! emitted transaction testable without a socket or allocation.

use super::respond::Responder;
use super::wire::MsgOut;
use crate::sql::types::Datum;
use crate::storage::ColumnMeta;

/// PostgreSQL's relation-level logical replica identity code. A parser-free
/// closed type keeps relation metadata and old-tuple tags coherent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaIdentity {
    Default,
    Nothing,
    Full,
    Index,
}

impl ReplicaIdentity {
    pub const fn code(self) -> u8 {
        match self {
            Self::Default => b'd',
            Self::Nothing => b'n',
            Self::Full => b'f',
            Self::Index => b'i',
        }
    }

    pub const fn old_tuple_tag(self) -> Option<u8> {
        match self {
            Self::Default | Self::Index => Some(b'K'),
            Self::Full => Some(b'O'),
            Self::Nothing => None,
        }
    }
}

/// A pgoutput protocol version accepted by PostgreSQL 18.
///
/// The value crosses the wire only after this parser boundary, so downstream
/// encoders cannot accidentally receive an unsupported version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProtocolVersion(u8);

impl ProtocolVersion {
    pub const V1: Self = Self(1);
    pub const V2: Self = Self(2);
    pub const V3: Self = Self(3);
    pub const V4: Self = Self(4);

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "1" => Some(Self::V1),
            "2" => Some(Self::V2),
            "3" => Some(Self::V3),
            "4" => Some(Self::V4),
            _ => None,
        }
    }
}

/// Emits one replication `XLogData` envelope around a pgoutput message.
pub fn xlog_data(
    message: &mut MsgOut,
    start_lsn: u64,
    end_lsn: u64,
    write_plugin: impl FnOnce(&mut MsgOut),
) {
    message.u8(b'w');
    message.i64(start_lsn as i64);
    message.i64(end_lsn as i64);
    // PostgreSQL epoch microseconds. WAL currently records no wall-clock
    // timestamp, so the transaction encoder supplies zero rather than
    // inventing an observation time.
    message.i64(0);
    write_plugin(message);
}

/// pgoutput Begin: final LSN, commit timestamp, and durable transaction id.
pub fn begin(message: &mut MsgOut, final_lsn: u64, transaction_id: u32) {
    message.u8(b'B');
    message.i64(final_lsn as i64);
    message.i64(0);
    message.i32(transaction_id as i32);
}

/// pgoutput Commit with the transaction's durable commit boundary.
pub fn commit(message: &mut MsgOut, commit_lsn: u64) {
    message.u8(b'C');
    message.u8(0);
    message.i64(commit_lsn as i64);
    message.i64(commit_lsn as i64);
    message.i64(0);
}

/// Complete pgoutput Relation metadata, assembled before encoding so the
/// parallel column fields cannot diverge at the wire boundary.
pub struct Relation<'a> {
    pub relation_id: u32,
    pub schema: &'a str,
    pub name: &'a str,
    pub columns: &'a [ColumnMeta],
    pub type_oids: &'a [i32],
    pub replica_identity: ReplicaIdentity,
    pub key_columns: &'a [bool],
}

/// pgoutput Relation declaration. Every field is binary and typed by the
/// exact PostgreSQL OID/typmod carried by the durable table definition.
pub fn relation(message: &mut MsgOut, relation: Relation<'_>) {
    debug_assert_eq!(relation.columns.len(), relation.type_oids.len());
    debug_assert_eq!(relation.columns.len(), relation.key_columns.len());
    message.u8(b'R');
    message.i32(relation.relation_id as i32);
    message.cstr(relation.schema);
    message.cstr(relation.name);
    message.u8(relation.replica_identity.code());
    message.i16(relation.columns.len() as i16);
    for ((column, type_oid), key) in relation
        .columns
        .iter()
        .zip(relation.type_oids)
        .zip(relation.key_columns)
    {
        message.u8(u8::from(*key));
        message.cstr(column.name.as_str());
        message.i32(*type_oid);
        message.i32(column.type_mod);
    }
}

/// pgoutput Type declaration. Subscribers must receive this before a Relation
/// that refers to a non-built-in type OID.
pub fn type_message(message: &mut MsgOut, type_oid: i32, schema: &str, name: &str) {
    message.u8(b'Y');
    message.i32(type_oid);
    message.cstr(schema);
    message.cstr(name);
}

/// pgoutput Insert with a negotiated text or binary new-tuple payload.
pub fn insert(message: &mut MsgOut, relation_id: u32, values: &[Datum], binary: bool) {
    message.u8(b'I');
    message.i32(relation_id as i32);
    tuple(message, values, binary);
}

/// pgoutput Update carries the exact old-tuple tag required by the relation's
/// typed replica-identity mode.
pub fn update(
    message: &mut MsgOut,
    relation_id: u32,
    old_values: &[Datum],
    new_values: &[Datum],
    binary: bool,
    replica_identity: ReplicaIdentity,
) {
    let old_tag = replica_identity
        .old_tuple_tag()
        .expect("UPDATE requires a usable replica identity");
    message.u8(b'U');
    message.i32(relation_id as i32);
    message.u8(old_tag);
    tuple(message, old_values, binary);
    tuple(message, new_values, binary);
}

/// pgoutput Delete carries the exact old-tuple tag required by the relation's
/// typed replica-identity mode.
pub fn delete(
    message: &mut MsgOut,
    relation_id: u32,
    old_values: &[Datum],
    binary: bool,
    replica_identity: ReplicaIdentity,
) {
    let old_tag = replica_identity
        .old_tuple_tag()
        .expect("DELETE requires a usable replica identity");
    message.u8(b'D');
    message.i32(relation_id as i32);
    message.u8(old_tag);
    tuple(message, old_values, binary);
}

/// pgoutput Truncate, available from protocol version 2. The option byte is
/// bit 0 for CASCADE and bit 1 for RESTART IDENTITY.
pub fn truncate(message: &mut MsgOut, relation_ids: &[u32], cascade: bool, restart_identity: bool) {
    message.u8(b'T');
    message.i32(relation_ids.len() as i32);
    message.u8(u8::from(cascade) | (u8::from(restart_identity) << 1));
    for relation_id in relation_ids {
        message.i32(*relation_id as i32);
    }
}

fn tuple(message: &mut MsgOut, values: &[Datum], binary: bool) {
    message.u8(b'N');
    message.i16(values.len() as i16);
    for value in values {
        if matches!(value, Datum::Null) {
            message.u8(b'n');
        } else if binary {
            message.u8(b'b');
            Responder::encode_value_binary(message, value);
        } else {
            message.u8(b't');
            Responder::encode_value_text(message, value, crate::sql::guc::RenderContext::default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::mem::buffer::FixedBuf;

    #[test]
    fn xlog_data_wraps_a_pgoutput_transaction_boundary() {
        let mut budget = Budget::new(1024);
        let mut buffer = FixedBuf::new(&mut budget, "pgoutput", 256).unwrap();
        let mut frame = MsgOut::begin(&mut buffer, b'd');
        xlog_data(&mut frame, 7, 9, |message| begin(message, 9, 42));
        frame.finish().unwrap();
        let bytes = buffer.readable();
        assert_eq!(bytes[0], b'd');
        assert_eq!(&bytes[5..6], b"w");
        assert_eq!(&bytes[30..31], b"B");
        assert_eq!(i64::from_be_bytes(bytes[6..14].try_into().unwrap()), 7);
        assert_eq!(i64::from_be_bytes(bytes[14..22].try_into().unwrap()), 9);
    }

    #[test]
    fn update_and_delete_carry_the_declared_replica_identity_tuple_kind() {
        let mut budget = Budget::new(1024);
        let mut buffer = FixedBuf::new(&mut budget, "pgoutput", 256).unwrap();
        let mut frame = MsgOut::begin(&mut buffer, b'd');
        update(
            &mut frame,
            7,
            &[Datum::Int4(1)],
            &[Datum::Int4(2)],
            true,
            ReplicaIdentity::Index,
        );
        delete(
            &mut frame,
            7,
            &[Datum::Int4(2)],
            true,
            ReplicaIdentity::Full,
        );
        frame.finish().unwrap();
        let bytes = buffer.readable();
        assert_eq!(bytes[5], b'U');
        assert_eq!(bytes[10], b'K');
        assert_eq!(bytes[35], b'D');
        assert_eq!(bytes[40], b'O');
    }

    #[test]
    fn relation_declares_index_identity_and_only_its_key_columns() {
        let mut budget = Budget::new(1024);
        let mut buffer = FixedBuf::new(&mut budget, "pgoutput", 256).unwrap();
        let mut frame = MsgOut::begin(&mut buffer, b'd');
        let mut columns = [ColumnMeta::EMPTY; 2];
        columns[0].name = crate::storage::SqlName::parse("alternate").unwrap();
        columns[1].name = crate::storage::SqlName::parse("payload").unwrap();
        relation(
            &mut frame,
            Relation {
                relation_id: 7,
                schema: "public",
                name: "replica_rows",
                columns: &columns,
                type_oids: &[23, 25],
                replica_identity: ReplicaIdentity::Index,
                key_columns: &[true, false],
            },
        );
        frame.finish().unwrap();
        let bytes = buffer.readable();
        let marker = bytes
            .windows(b"replica_rows\0".len())
            .position(|window| window == b"replica_rows\0")
            .unwrap()
            + b"replica_rows\0".len();
        assert_eq!(bytes[marker], b'i');
        assert_eq!(bytes[marker + 3], 1);
        assert_eq!(bytes[marker + 3 + 1 + b"alternate\0".len() + 8], 0);
    }

    #[test]
    fn default_pgoutput_tuples_are_text_and_binary_is_negotiated() {
        let mut budget = Budget::new(1024);
        let mut buffer = FixedBuf::new(&mut budget, "pgoutput", 256).unwrap();
        let mut frame = MsgOut::begin(&mut buffer, b'd');
        insert(&mut frame, 7, &[Datum::Int4(42)], false);
        insert(&mut frame, 7, &[Datum::Int4(42)], true);
        frame.finish().unwrap();
        let bytes = buffer.readable();
        assert_eq!(bytes[13], b't');
        assert_eq!(&bytes[14..18], &2i32.to_be_bytes());
        assert_eq!(&bytes[18..20], b"42");
        assert_eq!(bytes[28], b'b');
        assert_eq!(&bytes[29..33], &4i32.to_be_bytes());
    }

    #[test]
    fn truncate_carries_all_relations_and_options() {
        let mut budget = Budget::new(1024);
        let mut buffer = FixedBuf::new(&mut budget, "pgoutput", 256).unwrap();
        let mut frame = MsgOut::begin(&mut buffer, b'd');
        truncate(&mut frame, &[7, 11], true, true);
        frame.finish().unwrap();
        let bytes = buffer.readable();
        assert_eq!(bytes[5], b'T');
        assert_eq!(&bytes[6..10], &2i32.to_be_bytes());
        assert_eq!(bytes[10], 3);
        assert_eq!(&bytes[11..15], &7i32.to_be_bytes());
        assert_eq!(&bytes[15..19], &11i32.to_be_bytes());
    }
}
