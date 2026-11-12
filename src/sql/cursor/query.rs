//! A cursor owns its parsed query and copied parameters across client commands.

use super::*;
use crate::sql::ast::{Select, SetQuery, Stmt};

#[derive(Clone, Copy)]
pub(crate) struct CursorLocal<'a> {
    pub name: SqlName,
    pub type_oid: i32,
    pub value: Datum<'a>,
}

#[derive(Clone, Copy)]
pub(crate) enum CursorStatement<'a> {
    Select(&'a Select<'a>),
    SetQuery(&'a SetQuery<'a>),
}

pub(crate) struct CursorQuery {
    // Every reference in BoundQuery points into arena's allocation. Drop the
    // bound value before resetting or freeing that allocation.
    bound: Option<BoundQuery<'static>>,
    pub(crate) retained: Option<crate::sql::query::RetainedSelect<'static>>,
    /// Where the retained source's bytes start in `arena`.
    retained_mark: Option<crate::mem::arena::ArenaMark>,
    result_type_oids: Option<crate::mem::arena::FrozenArenaSliceHandle<i32>>,
    arena: Arena,
    owner: crate::sql::type_input_plans::Owner,
}

struct BoundQuery<'a> {
    statement: CursorStatement<'a>,
    params: &'a [Datum<'a>],
    parameter_types: &'a [i32],
}

fn query_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "cursor query and parameters exceed cursor_query_bytes"
    )
}

fn copy_values<'a>(arena: &'a Arena, values: &[Datum]) -> Result<&'a [Datum<'a>], SqlError> {
    let length = crate::sql::exec::projected_row_len(values)?;
    let encoded = arena
        .alloc_slice_with(length, |_| 0u8)
        .map_err(|_| query_full())?;
    crate::sql::exec::encode_projected_into(values, encoded)?;
    let copied = arena
        .alloc_slice_with(values.len(), |_| Datum::Null)
        .map_err(|_| query_full())?;
    // Width and tags were written above; the remaining decode failure is
    // insufficient arena space for a nested record's field descriptors.
    crate::sql::exec::decode_projected_row_into(encoded, arena, copied)
        .map_err(|_| query_full())?;
    Ok(copied)
}

impl CursorQuery {
    pub(crate) fn new(budget: &mut Budget, bytes: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            bound: None,
            retained: None,
            retained_mark: None,
            result_type_oids: None,
            arena: Arena::new(budget, "cursor_query_bytes", bytes)?,
            owner: crate::sql::type_input_plans::Owner::Unlocated,
        })
    }

    pub(crate) fn clear(&mut self) {
        self.retained = None;
        self.retained_mark = None;
        self.result_type_oids = None;
        self.bound = None;
        self.arena.reset();
        self.owner = crate::sql::type_input_plans::Owner::Unlocated;
    }

    pub(crate) fn owner(&self) -> crate::sql::type_input_plans::Owner {
        self.owner
    }

    pub(crate) fn is_bound(&self) -> bool {
        self.bound.is_some()
    }

    pub(crate) fn result_type_oids(&self) -> Result<Option<&[i32]>, SqlError> {
        self.result_type_oids
            .map(|handle| {
                self.arena.retained_slice(handle).map_err(|_| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "cursor SQL result types are inactive"
                    )
                })
            })
            .transpose()
    }

    pub(crate) fn finish_source(&mut self) {
        self.retained = None;
        self.retained_mark = None;
        self.bound = None;
        // Held rows keep their declared types after the query and its readers
        // end. Their checked tail storage survives front reclamation.
        self.arena.recycle_front();
        self.owner = crate::sql::type_input_plans::Owner::Unlocated;
    }

    pub(crate) fn bind(
        &mut self,
        sql: &str,
        params: &[Datum],
        locals: &[CursorLocal],
        inputs: &[crate::sql::type_inputs::Binding<'_>],
        owner: crate::sql::type_input_plans::Owner,
        validate: impl FnOnce(&crate::sql::parser::Parser<'_>) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        self.clear();
        self.owner = owner;
        let full = query_full;
        let source = self.arena.alloc_str(sql).map_err(|_| full())?;
        let values = copy_values(&self.arena, params)?;
        let field_count = |value| match value {
            Datum::Record(fields) | Datum::Composite { fields, .. } => fields.len(),
            _ => 0,
        };
        let count = locals.iter().try_fold(params.len(), |count, local| {
            count
                .checked_add(1 + field_count(local.value))
                .ok_or_else(full)
        })?;
        if count > u16::MAX as usize {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cursor captured parameters exceed 65535 values"
            ));
        }
        let parameters = self
            .arena
            .alloc_slice_with(count, |index| {
                values.get(index).copied().unwrap_or(Datum::Null)
            })
            .map_err(|_| full())?;
        let parameter_types = self
            .arena
            .alloc_slice_with(count, |index| {
                if index < params.len() {
                    crate::sql::exec::bound_parameter_type_oid((index + 1) as u32).unwrap_or(0)
                } else {
                    0
                }
            })
            .map_err(|_| full())?;
        let bindings = self
            .arena
            .alloc_slice_with(count - params.len(), |_| {
                crate::sql::parser::NamedParameter {
                    qualifier: None,
                    name: "",
                    index: 0,
                }
            })
            .map_err(|_| full())?;
        let mut at = params.len();
        for local in locals {
            let name = self
                .arena
                .alloc_str(local.name.as_str())
                .map_err(|_| full())?;
            let copied = copy_values(&self.arena, &[local.value])?[0];
            parameters[at] = copied;
            parameter_types[at] = local.type_oid;
            bindings[at - params.len()] = crate::sql::parser::NamedParameter {
                qualifier: None,
                name,
                index: at as u32 + 1,
            };
            at += 1;
            if let Datum::Record(fields) | Datum::Composite { fields, .. } = copied {
                for field in fields {
                    parameters[at] = field.value;
                    parameter_types[at] = field.type_oid;
                    bindings[at - params.len()] = crate::sql::parser::NamedParameter {
                        qualifier: Some(name),
                        name: field.name,
                        index: at as u32 + 1,
                    };
                    at += 1;
                }
            }
        }
        let bytes = self
            .arena
            .alloc_slice_with(crate::sql::type_inputs::encoded_len(inputs)?, |_| 0u8)
            .map_err(|_| full())?;
        crate::sql::type_inputs::encode_into(inputs, bytes)?;
        let inputs = crate::sql::type_inputs::decode(bytes, &self.arena)?;
        let statement = crate::sql::parser::parse_cursor_command(
            source,
            &self.arena,
            bindings,
            inputs,
            validate,
        )?;
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
        let bound = BoundQuery {
            statement,
            params: parameters,
            parameter_types,
        };
        // The parser only borrows the copied source and this private arena.
        // Parameter encoding and structural decoding copy every borrowed
        // value into the same allocation. No caller or Storage borrow remains.
        // with_query shortens these lifetimes; its result cannot borrow them.
        self.bound =
            Some(unsafe { core::mem::transmute::<BoundQuery<'_>, BoundQuery<'static>>(bound) });
        Ok(())
    }

    pub(crate) fn prepare_source(
        &mut self,
        storage: &crate::storage::Storage,
        txid: u32,
        responder: &mut crate::pg::respond::Responder<'_>,
        reader_use: crate::sql::query::RetainedUse,
    ) -> Result<bool, SqlError> {
        let bound = self.bound.as_ref().expect("bound cursor query");
        if self.result_type_oids.is_none() {
            let mut types = [0; crate::sql::MAX_PROJ];
            let count = self
                .arena
                .with_remaining_region(|scratch| {
                    crate::sql::type_input_plans::with_owner(self.owner, || {
                        crate::sql::exec::with_bound_parameter_types(bound.parameter_types, || {
                            let mut columns =
                                [crate::sql::types::ColDesc::new("", 0, 0); crate::sql::MAX_PROJ];
                            let count = match bound.statement {
                                CursorStatement::Select(select) => {
                                    let select = crate::sql::query::expand_ctes(
                                        select, storage, txid, scratch,
                                    )?;
                                    crate::sql::query::describe_select(
                                        select,
                                        storage,
                                        txid,
                                        scratch,
                                        &mut columns,
                                        crate::sql::exec::DescriptionTarget::Sql,
                                        None,
                                    )?
                                }
                                CursorStatement::SetQuery(query) => {
                                    crate::sql::query::describe_set_query(
                                        storage,
                                        txid,
                                        query,
                                        &mut columns,
                                        scratch,
                                        crate::sql::exec::DescriptionTarget::Sql,
                                    )?
                                }
                            };
                            for (target, column) in types.iter_mut().zip(&columns[..count]) {
                                *target = column.type_oid;
                            }
                            Ok::<_, SqlError>(count)
                        })
                    })
                })
                .map_err(|_| query_full())??;
            let types = self
                .arena
                .retain_slice_with(count, |index| types[index])
                .map_err(|_| query_full())?;
            self.result_type_oids =
                Some(self.arena.freeze_retained_slice(types).map_err(|_| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "cursor SQL result types are inactive"
                    )
                })?);
        }
        let mark = self.arena.mark();
        // A stream appends the queries of a UNION ALL itself.
        let statement = match (bound.statement, reader_use) {
            (CursorStatement::Select(statement), crate::sql::query::RetainedUse::Stream) => {
                crate::sql::query::stream_query(statement, storage, txid, &self.arena)
            }
            (CursorStatement::Select(statement), crate::sql::query::RetainedUse::Cursor) => {
                Ok(statement)
            }
            (CursorStatement::SetQuery(query), crate::sql::query::RetainedUse::Stream) => {
                crate::sql::query::set_query_select(query, &self.arena).and_then(|select| {
                    crate::sql::query::stream_query(select, storage, txid, &self.arena)
                })
            }
            (CursorStatement::SetQuery(_), crate::sql::query::RetainedUse::Cursor) => {
                return Ok(false);
            }
        };
        let statement = match statement {
            Ok(statement) => statement,
            Err(error) => {
                // SAFETY: nothing borrows the rejected query's bytes.
                unsafe { self.arena.rewind_to(mark) };
                return Err(error);
            }
        };
        let source = crate::sql::type_input_plans::with_owner(self.owner, || {
            crate::sql::exec::with_bound_parameter_types(bound.parameter_types, || {
                crate::sql::query::RetainedSelect::open(
                    storage,
                    txid,
                    statement,
                    bound.params,
                    &self.arena,
                    responder,
                    reader_use,
                )
            })
        });
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                // No retained source escaped; pending invocation arguments are
                // copied into the caller's invocation frame before yielding.
                unsafe { self.arena.rewind_to(mark) };
                return Err(error);
            }
        };
        match source {
            Some(source) => {
                // Scope names and arrays borrow this allocation; definitions,
                // heap bytes and saved files have owned readers. No Storage or
                // Arena object reference remains in the retained source.
                self.retained = Some(unsafe {
                    core::mem::transmute::<
                        crate::sql::query::RetainedSelect<'_>,
                        crate::sql::query::RetainedSelect<'static>,
                    >(source)
                });
                self.retained_mark = Some(mark);
                Ok(true)
            }
            None => {
                // No source or borrowed description escaped the rejected plan.
                unsafe { self.arena.rewind_to(mark) };
                Ok(false)
            }
        }
    }

    pub(crate) fn prepare_row(
        &mut self,
        storage: &crate::storage::Storage,
        txid: u32,
        observed_at: i64,
        work: &Arena,
        buffers: &mut crate::storage::RowCursorBuffers,
    ) -> Result<bool, SqlError> {
        let bound = self.bound.as_ref().expect("bound cursor query");
        let source = self.retained.as_mut().expect("retained cursor input");
        // Retention stores bytes in this allocation, never the movable Arena object.
        let arena: &'static Arena = unsafe { &*(&self.arena as *const Arena) };
        crate::sql::type_input_plans::with_owner(self.owner, || {
            crate::sql::exec::with_bound_parameter_types(bound.parameter_types, || {
                source.prepare_cursor_row(
                    storage,
                    txid,
                    observed_at,
                    arena,
                    buffers,
                    work,
                    bound.params,
                )
            })
        })
    }

    /// Builds the retained source's hash tables; see
    /// [`crate::sql::query::RetainedSelect::build_tables`].
    pub(crate) fn build_tables(
        &mut self,
        storage: &crate::storage::Storage,
        txid: u32,
        observed_at: i64,
        work: &Arena,
        buffers: &mut crate::storage::RowCursorBuffers,
    ) -> Result<(), SqlError> {
        let source = self.retained.as_mut().expect("retained cursor input");
        // Retention stores bytes in this allocation, never the movable Arena object.
        let arena: &'static Arena = unsafe { &*(&self.arena as *const Arena) };
        source.build_tables(storage, txid, observed_at, arena, buffers, work)
    }

    /// Drops a retained source whose builds did not fit, with its leases,
    /// so the cursor saves its rows instead.
    pub(crate) fn discard_source(&mut self) {
        self.retained = None;
        if let Some(mark) = self.retained_mark.take() {
            // SAFETY: the dropped source was the last user of its bytes.
            unsafe { self.arena.rewind_to(mark) };
        }
    }

    /// Returns the retained source to its first row; see
    /// [`crate::sql::query::RetainedSelect::rewind`].
    pub(crate) fn rewind_source(&mut self) -> Result<(), SqlError> {
        let source = self.retained.as_mut().expect("retained cursor input");
        // Retention stores bytes in this allocation, never the movable Arena object.
        let arena: &'static Arena = unsafe { &*(&self.arena as *const Arena) };
        source.rewind(arena)
    }

    pub(crate) fn source(&self) -> Option<&crate::sql::query::RetainedSelect<'static>> {
        self.retained.as_ref()
    }

    pub(crate) fn lock_pending(
        &self,
        storage: &crate::storage::Storage,
        txid: u32,
    ) -> Result<bool, SqlError> {
        self.retained
            .as_ref()
            .expect("retained cursor input")
            .lock_pending(storage, txid)
    }

    pub(crate) fn recheck_pending(
        &mut self,
        storage: &crate::storage::Storage,
        txid: u32,
    ) -> Result<crate::sql::query::LockedRow, SqlError> {
        // Retention stores bytes in this allocation, never the movable Arena object.
        let arena: &'static Arena = unsafe { &*(&self.arena as *const Arena) };
        self.retained
            .as_mut()
            .expect("retained cursor input")
            .recheck_pending(storage, txid, arena)
    }

    pub(crate) fn take_limits(&mut self, work: &Arena) -> Result<Option<(u64, u64)>, SqlError> {
        let bound = self.bound.as_ref().expect("bound cursor query");
        let source = self.retained.as_mut().expect("retained cursor input");
        crate::sql::type_input_plans::with_owner(self.owner, || {
            crate::sql::exec::with_bound_parameter_types(bound.parameter_types, || {
                source.take_cursor_limits(work, bound.params)
            })
        })
    }

    /// Makes saved input `index` the source's pending row; a wait may have
    /// left it pending already.
    pub(crate) fn prepare_saved_input(
        &mut self,
        index: usize,
        bytes: &[u8],
    ) -> Result<(), SqlError> {
        let source = self.retained.as_mut().expect("retained cursor input");
        // Retention stores bytes in this allocation, never the movable Arena object.
        let arena: &'static Arena = unsafe { &*(&self.arena as *const Arena) };
        source.prepare_saved_row(index, bytes, arena)
    }

    pub(crate) fn evaluate_predicate(
        &self,
        storage: &crate::storage::Storage,
        txid: u32,
        work: &Arena,
        sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
        invocations: &crate::sql::query::RoutineInvocationState,
        frame: &Arena,
    ) -> Result<bool, SqlError> {
        let bound = self.bound.as_ref().expect("bound cursor query");
        crate::sql::type_input_plans::with_owner(self.owner, || {
            crate::sql::exec::with_bound_parameter_types(bound.parameter_types, || {
                self.retained
                    .as_ref()
                    .expect("retained cursor input")
                    .evaluate_statement_predicate(
                        storage,
                        txid,
                        work,
                        bound.params,
                        sequences,
                        Some(invocations),
                        frame,
                    )
            })
        })
    }

    pub(crate) fn items_len(&self) -> usize {
        self.retained
            .as_ref()
            .expect("retained cursor input")
            .statement_items_len()
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "cursor row expressions retain typed values in their external invocation frame"
    )]
    pub(crate) fn evaluate_item<'frame>(
        &self,
        index: usize,
        storage: &crate::storage::Storage,
        txid: u32,
        work: &Arena,
        sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
        invocations: &crate::sql::query::RoutineInvocationState,
        frame: &'frame Arena,
    ) -> Result<&'frame [Datum<'frame>], SqlError> {
        let bound = self.bound.as_ref().expect("bound cursor query");
        crate::sql::type_input_plans::with_owner(self.owner, || {
            crate::sql::exec::with_bound_parameter_types(bound.parameter_types, || {
                self.retained
                    .as_ref()
                    .expect("retained cursor input")
                    .evaluate_statement_item(
                        index,
                        storage,
                        txid,
                        work,
                        bound.params,
                        sequences,
                        Some(invocations),
                        frame,
                    )
            })
        })
    }

    pub(crate) fn skip_row(&mut self) -> bool {
        self.retained
            .as_mut()
            .expect("retained cursor input")
            .skip_statement_row()
    }

    pub(crate) fn finish_row(&mut self, emitted: bool) {
        self.retained
            .as_mut()
            .expect("retained cursor input")
            .finish_statement_row(&self.arena, emitted);
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
        Ok(crate::sql::type_input_plans::with_owner(self.owner, || {
            crate::sql::exec::with_bound_parameter_types(bound.parameter_types, || {
                use_query(bound.statement, bound.params, &self.arena)
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_owned_query_releases_held_source_without_losing_declared_result_types() {
        let mut budget = Budget::new(8192);
        let mut query = CursorQuery::new(&mut budget, 8192).unwrap();
        let (query, saved) = crate::mem::guard::forbid_alloc(|| {
            query
                .bind(
                    "SELECT 1",
                    &[],
                    &[],
                    &[],
                    crate::sql::type_input_plans::Owner::Unlocated,
                    |_| Ok(()),
                )
                .unwrap();
            query.arena.alloc_slice_with(512, |_| 0xccu8).unwrap();
            let types = query
                .arena
                .retain_slice_with(2, |index| [23, 123456][index])
                .unwrap();
            let types = query.arena.freeze_retained_slice(types).unwrap();
            query.result_type_oids = Some(types);
            let before = query.arena.used();
            query.finish_source();
            assert!(!query.is_bound());
            assert!(query.retained.is_none());
            assert_eq!(query.result_type_oids().unwrap(), Some(&[23, 123456][..]));
            assert!(query.arena.used() < before);
            assert!(query.arena.used() <= Arena::retained_slice_capacity::<i32>(2).unwrap());
            (query, types)
        });
        let mut moved = query;
        crate::mem::guard::forbid_alloc(|| {
            moved.arena.alloc_slice_with(512, |_| 0xddu8).unwrap();
            assert_eq!(moved.result_type_oids().unwrap(), Some(&[23, 123456][..]));
            moved.clear();
            assert!(moved.result_type_oids().unwrap().is_none());
            assert!(moved.arena.retained_slice(saved).is_err());
            assert_eq!(moved.arena.used(), 0);
        });
    }

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
            query.bind(
                &source,
                &[Datum::Text(&text), Datum::Record(&fields)],
                &[],
                &[],
                crate::sql::type_input_plans::Owner::Unlocated,
                |_| Ok(()),
            )
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
        crate::mem::guard::forbid_alloc(|| {
            moved.bind(
                "SELECT 42",
                &[],
                &[],
                &[],
                crate::sql::type_input_plans::Owner::Unlocated,
                |_| Ok(()),
            )
        })
        .unwrap();
        moved
            .with_query(|_, params, _| assert!(params.is_empty()))
            .unwrap();
    }

    #[test]
    fn cursor_owned_query_preserves_null_types_beyond_routine_argument_limit() {
        use crate::sql::exec::{bound_parameter_type_oid, with_bound_parameter_types};
        use crate::sql::types::oid;

        let mut budget = Budget::new(64 << 10);
        let mut query = CursorQuery::new(&mut budget, 64 << 10).unwrap();
        let types = [oid::DATE; 101];
        let params = [Datum::Null; 101];
        crate::mem::guard::forbid_alloc(|| {
            with_bound_parameter_types(&types, || {
                query.bind(
                    "SELECT $101",
                    &params,
                    &[],
                    &[],
                    crate::sql::type_input_plans::Owner::Unlocated,
                    |_| Ok(()),
                )
            })
        })
        .unwrap();
        assert_eq!(bound_parameter_type_oid(101), None);
        let other_types = [oid::INT4; 101];
        crate::mem::guard::forbid_alloc(|| {
            with_bound_parameter_types(&other_types, || {
                query
                    .with_query(|_, params, _| {
                        assert_eq!(params[100], Datum::Null);
                        assert_eq!(bound_parameter_type_oid(101), Some(oid::DATE));
                        assert_eq!(bound_parameter_type_oid(0), None);
                        assert_eq!(bound_parameter_type_oid(102), None);
                    })
                    .unwrap();
                assert_eq!(bound_parameter_type_oid(101), Some(oid::INT4));
            });
        });
        assert_eq!(bound_parameter_type_oid(101), None);
    }

    #[test]
    fn cursor_owned_query_parameter_types_restore_after_unwind() {
        use crate::sql::exec::{bound_parameter_type_oid, with_bound_parameter_types};
        use crate::sql::types::oid;

        with_bound_parameter_types(&[oid::DATE, 0], || {
            assert_eq!(bound_parameter_type_oid(2), None);
            let result = std::panic::catch_unwind(|| {
                with_bound_parameter_types(&[oid::INT4], || {
                    assert_eq!(bound_parameter_type_oid(1), Some(oid::INT4));
                    panic!("unwind the nested parameter context");
                });
            });
            assert!(result.is_err());
            assert_eq!(bound_parameter_type_oid(1), Some(oid::DATE));
        });
        assert_eq!(bound_parameter_type_oid(1), None);
    }

    #[test]
    fn cursor_owned_query_copies_routine_locals_and_record_type_metadata() {
        use crate::sql::types::{RecordField, oid};

        let mut budget = Budget::new(64 << 10);
        let mut query = CursorQuery::new(&mut budget, 64 << 10).unwrap();
        let mut text = String::from("opening value");
        let fields = [RecordField {
            name: "label",
            type_oid: oid::TEXT,
            value: Datum::Text(&text),
        }];
        let locals = [
            CursorLocal {
                name: SqlName::parse("r").unwrap(),
                type_oid: oid::RECORD,
                value: Datum::Record(&fields),
            },
            CursorLocal {
                name: SqlName::parse("d").unwrap(),
                type_oid: oid::DATE,
                value: Datum::Null,
            },
            CursorLocal {
                name: SqlName::parse("found").unwrap(),
                type_oid: oid::BOOL,
                value: Datum::Bool(true),
            },
        ];
        crate::mem::guard::forbid_alloc(|| {
            query.bind(
                "SELECT r.label, d, found",
                &[],
                &locals,
                &[],
                crate::sql::type_input_plans::Owner::Unlocated,
                |_| Ok(()),
            )
        })
        .unwrap();
        text.clear();
        crate::mem::guard::forbid_alloc(|| {
            query.with_query(|statement, params, arena| {
                let CursorStatement::Select(select) = statement else {
                    panic!("SELECT")
                };
                let crate::sql::ast::SelectItem::Expr { expression, .. } = select.items[0] else {
                    panic!("expression")
                };
                let value = crate::sql::eval::eval_full(
                    expression,
                    arena,
                    params,
                    &crate::sql::eval::NoColumns,
                    &crate::sql::eval::NO_HOOKS,
                )
                .unwrap();
                assert_eq!(value, Datum::Text("opening value"));
                assert_eq!(params[2], Datum::Null);
                assert_eq!(
                    crate::sql::exec::bound_parameter_type_oid(3),
                    Some(oid::DATE)
                );
                assert_eq!(params[3], Datum::Bool(true));
            })
        })
        .unwrap();
    }

    #[test]
    fn cursor_owned_query_retains_wide_records_after_input_memory_is_freed() {
        use crate::sql::types::{RecordField, oid};

        let mut budget = Budget::new(256 << 10);
        let mut query = CursorQuery::new(&mut budget, 256 << 10).unwrap();
        let names: Vec<_> = (0..256).map(|index| format!("field{index}")).collect();
        let mut text = String::from("opening value");
        let fields: Vec<_> = names
            .iter()
            .map(|name| RecordField {
                name,
                type_oid: oid::TEXT,
                value: Datum::Text(&text),
            })
            .collect();
        let local = CursorLocal {
            name: SqlName::parse("r").unwrap(),
            type_oid: oid::RECORD,
            value: Datum::Record(&fields),
        };
        crate::mem::guard::forbid_alloc(|| {
            query.bind(
                "SELECT $1, r.field255",
                &[Datum::Record(&fields)],
                &[local],
                &[],
                crate::sql::type_input_plans::Owner::Unlocated,
                |_| Ok(()),
            )
        })
        .unwrap();
        drop(fields);
        drop(names);
        text.clear();
        crate::mem::guard::forbid_alloc(|| {
            query.with_query(|_, params, arena| {
                let detached = crate::sql::exec::detach_routine_datum(params[0], arena).unwrap();
                let Datum::Record(fields) = detached else {
                    panic!("record parameter")
                };
                assert_eq!(fields.len(), 256);
                assert_eq!(fields[255].name, "field255");
                assert_eq!(fields[255].value, Datum::Text("opening value"));
                assert_eq!(params[257], Datum::Text("opening value"));
            })
        })
        .unwrap();
    }

    #[test]
    fn cursor_owned_query_keeps_captured_types_separate_from_sql_routine_formals() {
        use crate::sql::ast::SelectItem;
        use crate::sql::types::{ColType, oid};

        let mut budget = Budget::new(32 << 10);
        let mut query = CursorQuery::new(&mut budget, 16 << 10).unwrap();
        let arena = Arena::new(&mut budget, "routine parameter parsing", 4096).unwrap();
        let formal = [crate::storage::RoutineArgumentDef {
            name: SqlName::parse("p").unwrap(),
            ctype: ColType::Int4,
            user_type: None,
        }];
        crate::mem::guard::forbid_alloc(|| {
            let _formal_scope = crate::sql::exec::enter_routine_parameter_types(
                &formal,
                None,
                crate::sql::ast::Collation::None,
            )
            .unwrap();
            assert_eq!(
                crate::sql::exec::bound_parameter_type_oid(1),
                Some(oid::INT4)
            );
            crate::sql::exec::with_bound_parameter_types(&[oid::DATE], || {
                let mut parser = crate::sql::parser::Parser::new("SELECT p", &arena)
                    .unwrap()
                    .with_routine_parameters("f", &formal)
                    .unwrap();
                let Stmt::Select(select) = parser.next_stmt().unwrap().unwrap() else {
                    panic!("SELECT")
                };
                let SelectItem::Expr { expression, .. } = select.items[0] else {
                    panic!("expression")
                };
                assert_eq!(
                    crate::sql::exec::infer_type_pub(expression, None)
                        .unwrap()
                        .0,
                    oid::INT4
                );
            });
            query
                .bind(
                    "SELECT p",
                    &[],
                    &[CursorLocal {
                        name: SqlName::parse("p").unwrap(),
                        type_oid: oid::DATE,
                        value: Datum::Null,
                    }],
                    &[],
                    crate::sql::type_input_plans::Owner::Unlocated,
                    |_| Ok(()),
                )
                .unwrap();
            query
                .with_query(|statement, _, _| {
                    let CursorStatement::Select(select) = statement else {
                        panic!("SELECT")
                    };
                    let SelectItem::Expr { expression, .. } = select.items[0] else {
                        panic!("expression")
                    };
                    assert_eq!(
                        crate::sql::exec::infer_type_pub(expression, None)
                            .unwrap()
                            .0,
                        oid::DATE
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn cursor_owned_query_nested_parameter_scopes_restore_the_innermost_declaration() {
        use crate::sql::exec::{
            bound_parameter_type_oid, enter_routine_parameter_types, with_bound_parameter_types,
        };
        use crate::sql::types::{ColType, oid};
        let formal = [crate::storage::RoutineArgumentDef {
            name: SqlName::parse("p").unwrap(),
            ctype: ColType::Int4,
            user_type: None,
        }];
        with_bound_parameter_types(&[oid::DATE], || {
            {
                let _formal_scope =
                    enter_routine_parameter_types(&formal, None, crate::sql::ast::Collation::None)
                        .unwrap();
                assert_eq!(bound_parameter_type_oid(1), Some(oid::INT4));
                with_bound_parameter_types(&[oid::TEXT], || {
                    assert_eq!(bound_parameter_type_oid(1), Some(oid::TEXT))
                });
                assert_eq!(bound_parameter_type_oid(1), Some(oid::INT4));
            }
            assert_eq!(bound_parameter_type_oid(1), Some(oid::DATE));
        });
        let source = vec![oid::DATE];
        let escaped = with_bound_parameter_types(&source, || {
            enter_routine_parameter_types(&formal, None, crate::sql::ast::Collation::None).unwrap()
        });
        drop(source);
        drop(escaped);
        assert_eq!(bound_parameter_type_oid(1), None);
    }

    #[test]
    fn cursor_owned_query_rejects_extra_commands_and_recovers_after_capacity_errors() {
        let mut budget = Budget::new(4096);
        let mut query = CursorQuery::new(&mut budget, 4096).unwrap();
        assert_eq!(
            query
                .bind(
                    "SELECT 1; SELECT 2",
                    &[],
                    &[],
                    &[],
                    crate::sql::type_input_plans::Owner::Unlocated,
                    |_| Ok(())
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::INVALID_CURSOR_DEFINITION
        );
        let wide = "x".repeat(8192);
        let error = crate::mem::guard::forbid_alloc(|| {
            query.bind(
                &wide,
                &[],
                &[],
                &[],
                crate::sql::type_input_plans::Owner::Unlocated,
                |_| Ok(()),
            )
        })
        .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("cursor_query_bytes"));
        let error = crate::mem::guard::forbid_alloc(|| {
            query.bind(
                "SELECT $1",
                &[Datum::Text(&wide)],
                &[],
                &[],
                crate::sql::type_input_plans::Owner::Unlocated,
                |_| Ok(()),
            )
        })
        .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("cursor_query_bytes"));
        crate::mem::guard::forbid_alloc(|| {
            query.bind(
                "SELECT 1",
                &[],
                &[],
                &[],
                crate::sql::type_input_plans::Owner::Unlocated,
                |_| Ok(()),
            )
        })
        .unwrap();
    }
    #[test]
    fn cursor_owned_query_retains_domain_parameter_identity_without_borrowing_catalog() {
        use crate::sql::exec::{
            bound_parameter_type_oid, enter_routine_parameter_types, with_bound_parameter_types,
        };
        use crate::sql::types::{ColType, oid};
        struct DomainCatalog;
        impl crate::sql::eval::CatalogAccess for DomainCatalog {
            fn relation_is_visible(&self, _: i32) -> Option<bool> {
                unreachable!()
            }

            fn type_is_visible(&self, _: i32) -> Option<bool> {
                unreachable!()
            }

            fn function_is_visible(&self, _: i32) -> Option<bool> {
                unreachable!()
            }

            fn collation_is_visible(&self, _: i32) -> Option<bool> {
                unreachable!()
            }

            fn relation_is_publishable(&self, _: i32) -> Option<bool> {
                unreachable!()
            }

            fn index_def<'a>(
                &self,
                _: i32,
                _: i32,
                _: bool,
                _: &'a Arena,
            ) -> Result<Option<&'a str>, SqlError> {
                unreachable!()
            }

            fn constraint_def<'a>(
                &self,
                _: i32,
                _: &'a Arena,
            ) -> Result<Option<&'a str>, SqlError> {
                unreachable!()
            }

            fn partition_key_def<'a>(
                &self,
                _: i32,
                _: &'a Arena,
            ) -> Result<Option<&'a str>, SqlError> {
                unreachable!()
            }

            fn relname<'a>(&self, _: i32, _: &'a Arena) -> Result<Option<&'a str>, SqlError> {
                unreachable!()
            }

            fn reloid(&self, _: &str) -> Result<Option<i32>, SqlError> {
                unreachable!()
            }

            fn comment<'a>(
                &self,
                _: crate::sql::eval::DescriptionTarget<'_>,
                _: u32,
                _: &'a Arena,
            ) -> Result<Option<&'a str>, SqlError> {
                unreachable!()
            }

            fn declared_user_type_oid(
                &self,
                ctype: ColType,
                identity: crate::storage::UserTypeName,
            ) -> Option<i32> {
                assert_eq!(ctype, ColType::Date);
                assert_eq!(identity.name.as_str(), "cursor_day");
                Some(123456)
            }
        }
        let formal = [crate::storage::RoutineArgumentDef {
            name: SqlName::parse("p").unwrap(),
            ctype: ColType::Date,
            user_type: Some(crate::storage::UserTypeName::declaration(
                SqlName::parse("public").unwrap(),
                SqlName::parse("cursor_day").unwrap(),
            )),
        }];
        let mut budget = Budget::new(16 << 10);
        let mut query = CursorQuery::new(&mut budget, 16 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            with_bound_parameter_types(&[oid::INT4], || {
                let scope = {
                    let catalog = DomainCatalog;
                    enter_routine_parameter_types(
                        &formal,
                        Some(&catalog),
                        crate::sql::ast::Collation::None,
                    )
                    .unwrap()
                };
                assert_eq!(bound_parameter_type_oid(1), Some(123456));
                query
                    .bind(
                        "SELECT $1",
                        &[Datum::Null],
                        &[],
                        &[],
                        crate::sql::type_input_plans::Owner::Unlocated,
                        |_| Ok(()),
                    )
                    .unwrap();
                drop(scope);
                assert_eq!(bound_parameter_type_oid(1), Some(oid::INT4));
                let error =
                    enter_routine_parameter_types(&formal, None, crate::sql::ast::Collation::None)
                        .err()
                        .expect("missing declared type");
                assert_eq!(error.sqlstate, sqlstate::UNDEFINED_OBJECT);
                assert_eq!(bound_parameter_type_oid(1), Some(oid::INT4));
                let too_many = [formal[0]; 101];
                let error = enter_routine_parameter_types(
                    &too_many,
                    Some(&DomainCatalog),
                    crate::sql::ast::Collation::None,
                )
                .err()
                .expect("argument limit");
                assert_eq!(error.sqlstate, sqlstate::TOO_MANY_ARGUMENTS);
                assert_eq!(bound_parameter_type_oid(1), Some(oid::INT4));
            });
            query
                .with_query(|_, params, _| {
                    assert_eq!(params[0], Datum::Null);
                    assert_eq!(bound_parameter_type_oid(1), Some(123456));
                })
                .unwrap();
        });
        assert_eq!(bound_parameter_type_oid(1), None);
    }
    #[test]
    fn cursor_owned_query_record_expansion_checks_the_complete_field_count() {
        let mut budget = Budget::new(2 << 20);
        let mut query = CursorQuery::new(&mut budget, 2 << 20).unwrap();
        for fields in [crate::sql::MAX_PROJ - 1, crate::sql::MAX_PROJ] {
            let values = vec!["1"; fields].join(",");
            let sql = format!("SELECT ROW((ROW({values})).*, 1)");
            crate::mem::guard::forbid_alloc(|| {
                query
                    .bind(
                        &sql,
                        &[],
                        &[],
                        &[],
                        crate::sql::type_input_plans::Owner::Unlocated,
                        |_| Ok(()),
                    )
                    .unwrap();
                query
                    .with_query(|statement, _, _| {
                        let CursorStatement::Select(select) = statement else {
                            panic!("SELECT")
                        };
                        let crate::sql::ast::SelectItem::Expr { expression, .. } = select.items[0]
                        else {
                            panic!("expression")
                        };
                        let mut visited = 0;
                        let result = crate::sql::exec::record_shape_metadata(
                            expression,
                            &crate::sql::exec::NoCols,
                            |_, _| {
                                visited += 1;
                            },
                        );
                        assert_eq!(visited, crate::sql::MAX_PROJ);
                        if fields == crate::sql::MAX_PROJ {
                            let error = result.unwrap_err();
                            assert_eq!(error.sqlstate, sqlstate::TOO_MANY_COLUMNS);
                            let error = crate::sql::exec::infer_type_res(
                                expression,
                                &crate::sql::exec::NoCols,
                            )
                            .unwrap_err();
                            assert_eq!(error.sqlstate, sqlstate::TOO_MANY_COLUMNS);
                            assert_eq!(
                                error.message.as_str(),
                                "ROW expressions can have at most 1664 entries"
                            );
                        } else {
                            assert_eq!(result.unwrap(), Some(crate::sql::MAX_PROJ));
                        }
                    })
                    .unwrap();
            });
        }
    }
}
