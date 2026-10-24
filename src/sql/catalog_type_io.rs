//! PostgreSQL type input/output function identities for catalog rows.
//! Source: https://github.com/postgres/postgres/blob/REL_18_6/src/include/catalog/pg_proc.dat

use super::{Datum, SqlError, sqlstate};
use crate::sql::catalog_metadata::{TYPE_IO, TypeIoRoutine};
use crate::sql_err;

const DOMAIN_INPUT: TypeIoRoutine = TypeIoRoutine {
    oid: 2597,
    name: "domain_in",
};

pub(super) fn functions<'a>(
    row: &'a [Datum<'a>],
    rows: &[&'a [Datum<'a>]],
    fields: [usize; 4],
) -> Result<[TypeIoRoutine; 2], SqlError> {
    let [oid, kind, element, base] = fields;
    let integer = |value| match value {
        Datum::Int4(value) => Ok(value),
        Datum::Oid(value) => Ok(value as i32),
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid pg_type source identity"
        )),
    };
    let mut current = row;
    let mut domain = false;
    // A domain inherits its final base type's output routine. This bounded
    // walk rejects a missing base or cycle instead of inventing an identity.
    for _ in 0..=rows.len() {
        let identity = integer(current[oid])?;
        let functions = if let Ok(position) = TYPE_IO.binary_search_by_key(&identity, |ty| ty.oid) {
            let ty = TYPE_IO[position];
            [ty.input, ty.output]
        } else {
            match current[kind] {
                Datum::Text("d") | Datum::Char(b'd') => {
                    let identity = integer(current[base])?;
                    if let Ok(position) = TYPE_IO.binary_search_by_key(&identity, |ty| ty.oid) {
                        return Ok([DOMAIN_INPUT, TYPE_IO[position].output]);
                    }
                    let mut found = None;
                    for row in rows {
                        if integer(row[oid])? == identity {
                            found = Some(*row);
                            break;
                        }
                    }
                    current = found.ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "pg_type domain references a missing base type"
                        )
                    })?;
                    domain = true;
                    continue;
                }
                // PostgreSQL 18.6 src/include/catalog/pg_proc.dat.
                Datum::Text("e") | Datum::Char(b'e') => [
                    TypeIoRoutine {
                        oid: 3506,
                        name: "enum_in",
                    },
                    TypeIoRoutine {
                        oid: 3507,
                        name: "enum_out",
                    },
                ],
                Datum::Text("c") | Datum::Char(b'c') => [
                    TypeIoRoutine {
                        oid: 2290,
                        name: "record_in",
                    },
                    TypeIoRoutine {
                        oid: 2291,
                        name: "record_out",
                    },
                ],
                Datum::Text("b") | Datum::Char(b'b') if integer(current[element])? != 0 => [
                    TypeIoRoutine {
                        oid: 750,
                        name: "array_in",
                    },
                    TypeIoRoutine {
                        oid: 751,
                        name: "array_out",
                    },
                ],
                _ => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "pg_type has no input/output function identities for type {}",
                        identity
                    ));
                }
            }
        };
        return Ok([
            if domain { DOMAIN_INPUT } else { functions[0] },
            functions[1],
        ]);
    }
    Err(sql_err!(
        sqlstate::INTERNAL_ERROR,
        "pg_type domain input/output dependency cycle"
    ))
}
