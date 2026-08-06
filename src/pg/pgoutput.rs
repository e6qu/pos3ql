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
) {
    message.u8(b'R');
    message.i32(relation_id as i32);
    message.cstr(schema);
    message.cstr(name);
    message.u8(b'd');
    message.i16(columns.len() as i16);
    for column in columns {
        message.u8(u8::from(column.primary));
        message.cstr(column.name.as_str());
        message.i32(column.ctype.oid());
        message.i32(column.type_mod);
    }
}

/// pgoutput Insert with a binary new-tuple payload.
pub fn insert(message: &mut MsgOut, relation_id: u32, values: &[Datum]) {
    message.u8(b'I');
    message.i32(relation_id as i32);
    tuple(message, values);
}

fn tuple(message: &mut MsgOut, values: &[Datum]) {
    message.u8(b'N');
    message.i16(values.len() as i16);
    for value in values {
        if matches!(value, Datum::Null) {
            message.u8(b'n');
        } else {
            message.u8(b'b');
            Responder::encode_value_binary(message, value);
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
}
