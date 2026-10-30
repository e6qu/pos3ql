//! A cursor owns its parsed query and copied parameters across client commands.

use super::*;
use crate::sql::ast::{Select, SetQuery, Stmt};

#[derive(Clone, Copy)]
pub(crate) enum CursorStatement<'a> {
    Select(&'a Select<'a>),
    SetQuery(&'a SetQuery<'a>),
}

pub(crate) struct CursorQuery {
    // Every reference in BoundQuery points into arena's allocation. Drop the
    // bound value before resetting or freeing that allocation.
    bound: Option<BoundQuery<'static>>,
    arena: Arena,
}

struct BoundQuery<'a> {
    statement: CursorStatement<'a>,
    params: &'a [Datum<'a>],
}

impl CursorQuery {
    pub(crate) fn new(budget: &mut Budget, bytes: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            bound: None,
            arena: Arena::new(budget, "cursor_query_bytes", bytes)?,
        })
    }

    pub(crate) fn clear(&mut self) {
        self.bound = None;
        self.arena.reset();
    }

    pub(crate) fn bind(&mut self, sql: &str, params: &[Datum]) -> Result<(), SqlError> {
        self.clear();
        let full = || {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cursor query and parameters exceed cursor_query_bytes"
            )
        };
        let source = self.arena.alloc_str(sql).map_err(|_| full())?;
        let statement = crate::sql::parser::parse_cursor_command(source, &self.arena)?;
        let statement = match statement {
            Stmt::Select(select) => CursorStatement::Select(select),
            Stmt::SetQuery(query) => CursorStatement::SetQuery(query),
            _ => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cursor queries other than SELECT are not supported"
                ));
            }
        };
        let encoded = crate::sql::exec::encode_projected_pub(params, &self.arena)?;
        let values = self
            .arena
            .alloc_slice_with(params.len(), |_| Datum::Null)
            .map_err(|_| full())?;
        crate::sql::exec::decode_projected_row_into(encoded, &self.arena, values)?;
        let bound = BoundQuery {
            statement,
            params: values,
        };
        // The parser only borrows the copied source and this private arena.
        // Parameter encoding and structural decoding copy every borrowed
        // value into the same allocation. No caller or Storage borrow remains.
        // with_query shortens these lifetimes; its result cannot borrow them.
        self.bound =
            Some(unsafe { core::mem::transmute::<BoundQuery<'_>, BoundQuery<'static>>(bound) });
        Ok(())
    }

    pub(crate) fn with_query<R>(
        &self,
        use_query: impl for<'a> FnOnce(CursorStatement<'a>, &'a [Datum<'a>], &'a Arena) -> R,
    ) -> Result<R, SqlError> {
        let bound = self.bound.as_ref().ok_or_else(|| {
            sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cursor query is not bound"
            )
        })?;
        Ok(use_query(bound.statement, bound.params, &self.arena))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_owned_query_keeps_source_parameters_and_nested_records_after_moves() {
        let mut budget = Budget::new(32 << 10);
        let mut query = CursorQuery::new(&mut budget, 16 << 10).unwrap();
        let mut source = String::from("SELECT $1, $2");
        let mut text = String::from("captured");
        let fields = [crate::sql::types::RecordField {
            name: "label",
            type_oid: crate::sql::types::oid::TEXT,
            value: Datum::Text(&text),
        }];
        crate::mem::guard::forbid_alloc(|| {
            query.bind(&source, &[Datum::Text(&text), Datum::Record(&fields)])
        })
        .unwrap();
        source.clear();
        text.clear();
        let mut moved = query;
        crate::mem::guard::forbid_alloc(|| {
            moved.with_query(|statement, params, _| {
                assert!(
                    matches!(statement, CursorStatement::Select(select) if select.items.len() == 2)
                );
                assert_eq!(params[0], Datum::Text("captured"));
                let Datum::Record(fields) = params[1] else {
                    panic!("typed record")
                };
                assert_eq!(fields[0].name, "label");
                assert_eq!(fields[0].value, Datum::Text("captured"));
            })
        })
        .unwrap();
        moved.clear();
        assert_eq!(
            moved.with_query(|_, _, _| ()).unwrap_err().sqlstate,
            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE
        );
        crate::mem::guard::forbid_alloc(|| moved.bind("SELECT 42", &[])).unwrap();
        moved
            .with_query(|_, params, _| assert!(params.is_empty()))
            .unwrap();
    }

    #[test]
    fn cursor_owned_query_rejects_extra_commands_and_recovers_after_capacity_errors() {
        let mut budget = Budget::new(4096);
        let mut query = CursorQuery::new(&mut budget, 4096).unwrap();
        assert_eq!(
            query.bind("SELECT 1; SELECT 2", &[]).unwrap_err().sqlstate,
            sqlstate::INVALID_CURSOR_DEFINITION
        );
        let wide = "x".repeat(8192);
        let error = crate::mem::guard::forbid_alloc(|| query.bind(&wide, &[])).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("cursor_query_bytes"));
        crate::mem::guard::forbid_alloc(|| query.bind("SELECT 1", &[])).unwrap();
    }
}
