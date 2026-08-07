//! Bounded encoder for PostgreSQL's logical `pgoutput` framing.
//!
//! The wire carries an outer XLogData envelope followed by one plugin message.
//! Keeping this byte-level boundary separate from connection state makes every
//! emitted transaction testable without a socket or allocation.

use super::respond::Responder;
use super::wire::MsgOut;
use crate::sql::types::Datum;
use crate::storage::ColumnMeta;

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

/// pgoutput Relation declaration. Every field is binary and typed by the
/// exact PostgreSQL OID/typmod carried by the durable table definition.
pub fn relation(
    message: &mut MsgOut,
    relation_id: u32,
    schema: &str,
    name: &str,
    columns: &[ColumnMeta],
    type_oids: &[i32],
) {
    debug_assert_eq!(columns.len(), type_oids.len());
    message.u8(b'R');
    message.i32(relation_id as i32);
    message.cstr(schema);
    message.cstr(name);
    // Every row-change message below carries a complete old tuple.  Advertise
    // the matching replica identity instead of claiming DEFAULT while sending
    // a tuple shape a subscriber is not entitled to expect from DEFAULT.
    message.u8(b'f');
    message.i16(columns.len() as i16);
    for (column, type_oid) in columns.iter().zip(type_oids) {
        message.u8(u8::from(column.primary));
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

/// pgoutput Update. The previous tuple is emitted with the `O` tag, which is
/// valid for FULL replica identity and lets subscribers apply changes without
/// consulting the publisher's current heap.
pub fn update(
    message: &mut MsgOut,
    relation_id: u32,
    old_values: &[Datum],
    new_values: &[Datum],
    binary: bool,
) {
    message.u8(b'U');
    message.i32(relation_id as i32);
    message.u8(b'O');
    tuple(message, old_values, binary);
    tuple(message, new_values, binary);
}

/// pgoutput Delete with the removed tuple under FULL replica identity.
pub fn delete(message: &mut MsgOut, relation_id: u32, old_values: &[Datum], binary: bool) {
    message.u8(b'D');
    message.i32(relation_id as i32);
    message.u8(b'O');
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
    fn update_and_delete_carry_full_replica_identity_tuples() {
        let mut budget = Budget::new(1024);
        let mut buffer = FixedBuf::new(&mut budget, "pgoutput", 256).unwrap();
        let mut frame = MsgOut::begin(&mut buffer, b'd');
        update(&mut frame, 7, &[Datum::Int4(1)], &[Datum::Int4(2)], true);
        delete(&mut frame, 7, &[Datum::Int4(2)], true);
        frame.finish().unwrap();
        let bytes = buffer.readable();
        assert_eq!(bytes[5], b'U');
        assert_eq!(bytes[10], b'O');
        assert_eq!(bytes[35], b'D');
        assert_eq!(bytes[40], b'O');
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
