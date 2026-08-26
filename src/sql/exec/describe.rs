//! Static type analysis: what a query's result columns are, before a row of it
//! exists.
//!
//! The extended-query protocol makes a client ask for a statement's shape at
//! Describe time, so every expression's type and every column's name have to be
//! derivable from the statement and the catalog alone. That is what this does —
//! the same rules PostgreSQL's parse analysis applies, including which operand
//! combinations have no operator at all, so a query that cannot work is refused
//! here rather than part-way through a scan.

use crate::sql::ast::{Expr, SelectItem};
use crate::sql::eval::{ColumnLookup, SqlError, described_expression_collation, sqlstate};
use crate::sql::types::{ColDesc, ColType, CollationDerivation, Datum, oid};
use crate::sql_err;
use crate::storage::{ColumnMeta, MAX_ROUTINE_ARGUMENTS, RoutineArgumentDef, TableDef};
use core::cell::Cell;

/// Result-column names and types, statically inferred. Names borrow the
/// statement (aliases) or the catalog (wildcard columns); `'q` is whichever
/// is shorter at the call site.
/// The atttypmod RowDescription reports for an output expression: a bare table
/// column carries its declared modifier, a cast its target's, and every other
/// expression `-1` — matching what PostgreSQL sends (`upper(v)` has none even
/// when `v` does).
fn output_type_mod(expression: &Expr<'_>, column_mod: impl Fn(&str) -> i32) -> i32 {
    match expression {
        Expr::Column { name, .. } => column_mod(name),
        Expr::Cast { type_mod, .. } => *type_mod,
        Expr::Collate { operand, .. } => output_type_mod(operand, column_mod),
        _ => -1,
    }
}

pub fn describe_items<'q>(
    items: &[SelectItem<'q>],
    def: Option<&'q TableDef>,
    table_alias: Option<&str>,
    storage: Option<&'q crate::storage::Storage>,
    txid: u32,
    out: &mut [ColDesc<'q>],
) -> Result<usize, SqlError> {
    let mut n = 0;
    for item in items {
        let mut push = |desc: ColDesc<'q>| -> Result<(), SqlError> {
            if n == out.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "select list expands past {} columns",
                    out.len()
                ));
            }
            out[n] = desc;
            n += 1;
            Ok(())
        };
        match item {
            SelectItem::Wildcard => {
                let Some(def) = def else {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "SELECT * requires a FROM clause"
                    ));
                };
                for c in def.columns() {
                    push(
                        ColDesc::of_type(c.name.as_str(), c.ctype)
                            .with_type_mod(c.type_mod)
                            .with_collation(c.collation),
                    )?;
                }
            }
            SelectItem::TableWildcard(q) => {
                let matches = def
                    .is_some_and(|d| crate::sql::eval::qualifier_answers_target(d, table_alias, q));
                if !matches {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "missing FROM-clause entry for table \"{}\"",
                        q
                    ));
                }
                for c in def.expect("matched").columns() {
                    push(
                        ColDesc::of_type(c.name.as_str(), c.ctype)
                            .with_type_mod(c.type_mod)
                            .with_collation(c.collation),
                    )?;
                }
            }
            SelectItem::RecordStar(base) => {
                describe_record_star(base, def, table_alias, storage, txid, &mut push)?;
            }
            SelectItem::Expr { expression, alias } => {
                let catalog_resolver;
                let resolver: &dyn ColTypeResolver = match storage {
                    Some(storage) => {
                        catalog_resolver = CatalogCols {
                            definition: def,
                            alias: table_alias,
                            storage,
                            txid,
                        };
                        &catalog_resolver
                    }
                    None => match def {
                        Some(definition) => &AliasedDefCols {
                            definition,
                            alias: table_alias,
                        },
                        None => &NoCols,
                    },
                };
                let (mut type_oid, mut typlen) = infer_type_res(expression, resolver)?;
                // A bare unknown (string literal / param) resolves to text
                // for output, as PostgreSQL does.
                if type_oid == oid::UNKNOWN {
                    type_oid = oid::TEXT;
                    typlen = -1;
                }
                let name = alias.unwrap_or(derived_name(expression));
                let field_meta = match expression {
                    Expr::Field { base, field } => {
                        record_field_metadata(base, field, resolver).ok()
                    }
                    _ => None,
                };
                if let Some(meta) = field_meta {
                    type_oid = meta.ctype.oid();
                    typlen = meta.ctype.typlen();
                } else if let Some(meta) = routine_result_metadata(expression, resolver) {
                    type_oid = meta.ctype.oid();
                    typlen = meta.ctype.typlen();
                }
                let type_mod = field_meta.map_or_else(
                    || {
                        output_type_mod(expression, |column| {
                            def.and_then(|d| d.columns().iter().find(|c| c.name.as_str() == column))
                                .map_or(-1, |c| c.type_mod)
                        })
                    },
                    |meta| meta.type_mod,
                );
                let mut description = ColDesc::new(name, type_oid, typlen).with_type_mod(type_mod);
                if coltype_of_oid(type_oid).is_some_and(ColType::is_collatable) {
                    let metadata = match def {
                        Some(definition) => described_expression_collation(
                            expression,
                            &AliasedDefCols {
                                definition,
                                alias: table_alias,
                            },
                        )?,
                        None => described_expression_collation(expression, &NoColumnLookup)?,
                    };
                    (description.collation, description.collation_derivation) = metadata;
                } else if let Some(meta) = field_meta {
                    description.collation = meta.collation;
                }
                if coltype_of_oid(type_oid).is_some_and(ColType::is_collatable)
                    && description.collation_derivation == CollationDerivation::None
                {
                    description.collation = crate::sql::ast::Collation::Default;
                    description.collation_derivation = CollationDerivation::Implicit;
                }
                push(description)?;
            }
        }
    }
    Ok(n)
}

pub(crate) struct CatalogCols<'a> {
    pub(crate) definition: Option<&'a TableDef>,
    pub(crate) alias: Option<&'a str>,
    pub(crate) storage: &'a crate::storage::Storage,
    pub(crate) txid: u32,
}

pub(crate) fn static_meta_for_column(
    storage: &crate::storage::Storage,
    column: &ColumnMeta,
    txid: u32,
) -> Option<StaticTypeMeta> {
    Some(StaticTypeMeta {
        ctype: column.ctype,
        type_oid: storage.routine_type_oid(column.ctype, column.user_type, txid)?,
        type_mod: column.type_mod,
        collation: column.collation,
    })
}

impl ColTypeResolver for CatalogCols<'_> {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        match self.definition {
            Some(definition) => AliasedDefCols {
                definition,
                alias: self.alias,
            }
            .resolve(qualifier, name),
            None => NoCols.resolve(qualifier, name),
        }
    }

    fn column_meta(&self, qualifier: Option<&str>, name: &str) -> Option<StaticTypeMeta> {
        let definition = self.definition?;
        if let Some(qualifier) = qualifier
            && !crate::sql::eval::qualifier_answers_target(definition, self.alias, qualifier)
        {
            return None;
        }
        let column = definition.columns().get(definition.column_index(name)?)?;
        static_meta_for_column(self.storage, column, self.txid)
    }

    fn named_type_oid(&self, type_name: &str) -> Option<i32> {
        crate::sql::catalog::user_type_oid(self.storage, self.txid, type_name)
            .or_else(|| ColType::from_sql_name(type_name).map(ColType::oid))
    }

    fn routine_result(&self, name: &str, arguments: &[i32]) -> Option<StaticTypeMeta> {
        let result = self
            .storage
            .function_for_call_oids(name, arguments, self.txid)
            .and_then(|routine| match routine.kind {
                crate::storage::RoutineKind::Function { result }
                | crate::storage::RoutineKind::SetFunction { result } => Some(result),
                _ => None,
            })
            .or_else(|| {
                self.storage
                    .aggregate_for_call_oids(name, arguments, self.txid)
                    .map(|(_, _, aggregate)| aggregate.result_type)
            })?;
        let ctype = result.ctype;
        Some(StaticTypeMeta {
            type_oid: self
                .storage
                .routine_type_oid(result.ctype, result.user_type, self.txid)?,
            ctype,
            type_mod: -1,
            collation: if ctype.is_collatable() {
                crate::sql::ast::Collation::Default
            } else {
                crate::sql::ast::Collation::None
            },
        })
    }

    fn routine_record_field(
        &self,
        name: &str,
        arguments: &[i32],
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        let slot = self
            .storage
            .routine_slot_for_table_call_oids(name, arguments, self.txid)?;
        let routine = self.storage.routine_for(slot, self.txid);
        let column = routine.record_result_columns()?.get(index)?;
        Some((
            crate::util::StackStr::from_str(column.name.as_str()),
            StaticTypeMeta {
                ctype: column.ctype,
                type_oid: self.storage.routine_type_oid(
                    column.ctype,
                    column.user_type,
                    self.txid,
                )?,
                type_mod: -1,
                collation: if column.ctype.is_collatable() {
                    crate::sql::ast::Collation::Default
                } else {
                    crate::sql::ast::Collation::None
                },
            },
        ))
    }

    fn is_whole_row(&self, name: &str) -> bool {
        self.definition.is_some_and(|definition| {
            crate::sql::eval::qualifier_answers_target(definition, self.alias, name)
        })
    }

    fn table_columns(&self, name: &str) -> Option<&[ColumnMeta]> {
        let definition = self.definition?;
        crate::sql::eval::qualifier_answers_target(definition, self.alias, name)
            .then(|| definition.columns())
    }

    fn record_column_handle(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
        let definition = self.definition?;
        AliasedDefCols {
            definition,
            alias: self.alias,
        }
        .record_column_handle(qualifier, name)
    }

    fn named_composite_field(
        &self,
        type_name: &str,
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        let slot = self.storage.resolve_composite_slot(type_name, self.txid)?;
        let definition = self.storage.composite_for(slot, self.txid);
        let field = definition.active_field(index)?;
        Some((
            crate::util::StackStr::from_str(field.name.as_str()),
            StaticTypeMeta {
                ctype: field.ctype,
                type_oid: self
                    .storage
                    .routine_type_oid(field.ctype, field.user_type, self.txid)?,
                type_mod: field.type_mod,
                collation: field.collation,
            },
        ))
    }
}

/// Emits one `ColDesc` per field of a `(record).*` expansion, resolving field
/// names and types at the caller's `'q` lifetime (single-table describe path).
fn describe_record_star<'q>(
    base: &Expr<'q>,
    def: Option<&'q TableDef>,
    table_alias: Option<&str>,
    storage: Option<&'q crate::storage::Storage>,
    txid: u32,
    push: &mut impl FnMut(ColDesc<'q>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    match base {
        Expr::Call { name, .. } if name.eq_ignore_ascii_case("row") => {
            let catalog_resolver;
            let aliased_resolver;
            let resolver: &dyn ColTypeResolver = match (def, storage) {
                (definition, Some(storage)) => {
                    catalog_resolver = CatalogCols {
                        definition,
                        alias: table_alias,
                        storage,
                        txid,
                    };
                    &catalog_resolver
                }
                (Some(definition), None) => {
                    aliased_resolver = AliasedDefCols {
                        definition,
                        alias: table_alias,
                    };
                    &aliased_resolver
                }
                (None, None) => &NoCols,
            };
            check_row_field_types(base, resolver)?;
            let mut error = None;
            let mut index = 0usize;
            record_shape_metadata(base, resolver, |_, meta| {
                if error.is_none() {
                    error = push(
                        ColDesc::new(
                            RECORD_FIELD_NAMES[index],
                            meta.type_oid,
                            meta.ctype.typlen(),
                        )
                        .with_type_mod(meta.type_mod)
                        .with_collation(meta.collation),
                    )
                    .err();
                }
                index += 1;
            })
            .ok_or_else(|| could_not_identify("*"))?;
            if let Some(error) = error {
                return Err(error);
            }
            Ok(())
        }
        Expr::Call { name, .. } if builtin_record_srf_field(name, 0).is_some() => {
            let mut index = 0;
            while let Some((field, ctype)) = builtin_record_srf_field(name, index) {
                push(ColDesc::of_type(field, ctype))?;
                index += 1;
            }
            Ok(())
        }
        Expr::Call { name, args, .. } if storage.is_some() => {
            let storage = storage.expect("matched");
            let resolver = CatalogCols {
                definition: def,
                alias: table_alias,
                storage,
                txid,
            };
            let mut argument_oids = [oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
            if args.len() > argument_oids.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many function arguments"
                ));
            }
            for (index, argument) in args.iter().enumerate() {
                argument_oids[index] = infer_routine_argument_oid(argument, &resolver)?;
            }
            let Some(slot) =
                storage.routine_slot_for_table_call_oids(name, &argument_oids[..args.len()], txid)
            else {
                return Err(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "row expansion is not supported on this expression"
                ));
            };
            let routine = storage.routine(slot);
            let Some(columns) = routine.record_result_columns() else {
                return Err(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "row expansion is not supported on this expression"
                ));
            };
            for column in columns {
                let mut description = ColDesc::of_type(column.name.as_str(), column.ctype);
                if column.ctype.is_collatable() {
                    description = description.with_collation(crate::sql::ast::Collation::Default);
                }
                push(description)?;
            }
            Ok(())
        }
        Expr::WholeRow(table)
        | Expr::Column {
            qualifier: None,
            name: table,
        } if def
            .is_some_and(|d| crate::sql::eval::qualifier_answers_target(d, table_alias, table)) =>
        {
            for c in def.expect("matched").columns() {
                push(
                    ColDesc::of_type(c.name.as_str(), c.ctype)
                        .with_type_mod(c.type_mod)
                        .with_collation(c.collation),
                )?;
            }
            Ok(())
        }
        _ if storage.is_some() => {
            let resolver: &dyn ColTypeResolver = match def {
                Some(definition) => &AliasedDefCols {
                    definition,
                    alias: table_alias,
                },
                None => &NoCols,
            };
            let slot = match base {
                Expr::Column { qualifier, name } => match resolver.resolve(*qualifier, name)? {
                    ColType::Composite(slot) => Some(slot),
                    _ => None,
                },
                Expr::Field { base, field } => match record_field_type(base, field, resolver)? {
                    ColType::Composite(slot) => Some(slot),
                    _ => None,
                },
                Expr::Cast { type_name, .. } => storage
                    .expect("checked")
                    .resolve_composite_slot(type_name, txid)
                    .map(|slot| slot as u16),
                _ => None,
            };
            if let Some(slot) = slot {
                for field in storage
                    .expect("checked")
                    .composite(slot as usize)
                    .active_fields_for(txid)
                {
                    push(
                        ColDesc::of_type(field.name.as_str(), field.ctype)
                            .with_type_mod(field.type_mod)
                            .with_collation(field.collation),
                    )?;
                }
                return Ok(());
            }
            let Some(handle) = expr_record_handle(base, resolver) else {
                return Err(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "row expansion is not supported on this expression"
                ));
            };
            let mut push_err = None;
            visit_record_shape_metadata(handle, |field_name, meta| {
                if push_err.is_some() {
                    return;
                }
                let leased = RECORD_FIELD_NAMES
                    .iter()
                    .chain(["key", "value"].iter())
                    .find(|n| n.eq_ignore_ascii_case(field_name))
                    .copied();
                match leased {
                    Some(name) => {
                        if let Err(error) = push(
                            ColDesc::of_type(name, meta.ctype)
                                .with_type_mod(meta.type_mod)
                                .with_collation(meta.collation),
                        ) {
                            push_err = Some(error);
                        }
                    }
                    None => {
                        push_err = Some(sql_err!(
                            sqlstate::WRONG_OBJECT_TYPE,
                            "row expansion is not supported on this expression"
                        ))
                    }
                }
            });
            push_err.map_or(Ok(()), Err)
        }
        // A record-typed column (or nested record field) with a registered
        // shape expands to its fields. Names come from static leases (fN,
        // key/value); a shape whose names this cannot cover refuses loudly.
        _ => {
            let resolver: &dyn ColTypeResolver = match def {
                Some(definition) => &AliasedDefCols {
                    definition,
                    alias: table_alias,
                },
                None => &NoCols,
            };
            let Some(handle) = expr_record_handle(base, resolver) else {
                return Err(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "row expansion is not supported on this expression"
                ));
            };
            let mut push_err = None;
            visit_record_shape_metadata(handle, |field_name, meta| {
                if push_err.is_some() {
                    return;
                }
                let leased = RECORD_FIELD_NAMES
                    .iter()
                    .chain(["key", "value"].iter())
                    .find(|n| n.eq_ignore_ascii_case(field_name))
                    .copied();
                match leased {
                    Some(name) => {
                        if let Err(e) = push(
                            ColDesc::of_type(name, meta.ctype)
                                .with_type_mod(meta.type_mod)
                                .with_collation(meta.collation),
                        ) {
                            push_err = Some(e);
                        }
                    }
                    None => {
                        push_err = Some(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "row expansion of this record's field names is not supported here"
                        ))
                    }
                }
            });
            match push_err {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
    }
}

/// Maps a type OID back to its modeled column type. The canonical decoder owns
/// scalar, range, and array identities; this boundary adds only PostgreSQL's
/// internal `"char"` catalog type.
pub(crate) fn coltype_of_oid(o: i32) -> Option<ColType> {
    if o == 18 {
        Some(ColType::Text)
    } else {
        ColType::from_oid(o)
    }
}

/// Unifies two types by PostgreSQL's numeric preference (int4<int8<numeric<
/// float8); non-numeric or equal types keep the first.
/// The result type (oid, typlen) of an array function that promotes an array's
/// element type to also hold a new scalar element (`array_append`/`prepend`/
/// `replace`). An unknown element adopts the array argument's contextual type.
fn array_promoted(array_oid: Option<i32>, elem_oid: Option<i32>) -> (i32, i16) {
    let contextual = (array_oid.unwrap_or(oid::TEXT), -1i16);
    let (Some(ao), Some(eo)) = (array_oid, elem_oid) else {
        return contextual;
    };
    let (Some(ColType::Array(ae)), Some(et)) = (coltype_of_oid(ao), coltype_of_oid(eo)) else {
        return contextual;
    };
    let unified = unify_numeric_tower(ae.to_coltype(), et);
    match crate::sql::types::ArrElem::from_coltype(unified) {
        Some(e) => (ColType::Array(e).oid(), -1),
        None => contextual,
    }
}

pub(crate) fn unify_numeric_tower(a: ColType, b: ColType) -> ColType {
    use ColType::*;
    let rank = |t: ColType| match t {
        Int2 => 1,
        Int4 => 2,
        Int8 => 3,
        Numeric => 4,
        Float4 => 5,
        Float8 => 6,
        _ => 0,
    };
    let (ra, rb) = (rank(a), rank(b));
    if ra > 0 && rb > 0 {
        if ra >= rb { a } else { b }
    } else {
        a
    }
}

/// PostgreSQL's error when an aggregate has no signature for the argument
/// type (e.g. sum(text), max(boolean)).
fn agg_undefined(name: &str, arg_oid: i32) -> SqlError {
    let table_name = coltype_of_oid(arg_oid)
        .map(|t| t.name())
        .unwrap_or("unknown");
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "function {}({}) does not exist",
        name,
        table_name
    )
}

/// A specific output name for an expression, if it has one (parse_target.c
/// FigureColnameInternal): a column ref, a function call, a cast (the type
/// name), or a CASE whose ELSE yields a name. `None` for anything unnamed.
fn name_of<'a>(expression: &Expr<'a>) -> Option<&'a str> {
    match expression {
        Expr::Column { name, .. } | Expr::SchemaColumn { name, .. } => Some(name),
        // The desugarings of syntax-only constructs must not be labelled with
        // the internal name they carry: `SIMILAR TO` is an operator, so its
        // column is anonymous, while PostgreSQL does label OVERLAPS.
        Expr::Call {
            name: crate::sql::parser::SIMILAR_TO,
            ..
        } => None,
        Expr::Call {
            name: crate::sql::parser::OVERLAPS_PERIODS,
            ..
        } => Some("overlaps"),
        Expr::Call { name, .. } => Some(name),
        // A cast keeps its operand's name when the operand is a column or
        // function call (`count(*)::int` → `count`); otherwise it takes the
        // target type's name (`'x'::int` → `int4`), matching PostgreSQL.
        // A cast chain keeps a name originating in a column or function, but
        // a name manufactured by an inner cast does not propagate outward.
        Expr::Cast {
            operand, type_name, ..
        } => match operand {
            Expr::Column { .. } | Expr::Call { .. } | Expr::Array(_) | Expr::ArraySubquery(_) => {
                name_of(operand)
            }
            Expr::Cast { operand, .. } => {
                cast_source_name(operand).or_else(|| cast_target_name(type_name))
            }
            _ => cast_target_name(type_name),
        },
        Expr::Collate { operand, .. } => name_of(operand),
        // A desugared CASE (`IS TRUE`, `IS DISTINCT FROM`) is anonymous, as
        // PostgreSQL labels those `?column?`; a real CASE forwards to its ELSE.
        Expr::Case {
            synthetic: true, ..
        } => None,
        Expr::Case {
            otherwise: Some(e), ..
        } => name_of(e),
        Expr::Array(_) | Expr::ArraySubquery(_) => Some("array"),
        // An array subscript or slice keeps the base column's name (`m[1]` → `m`,
        // `m[1:2]` → `m`; a slice of an `ARRAY[...]` constructor is `array`).
        Expr::Subscript { base, .. } | Expr::Slice { base, .. } => name_of(base),
        // `(record).field` is named after the field.
        Expr::Field { field, .. } => Some(field),
        _ => None,
    }
}

fn cast_source_name<'a>(expression: &Expr<'a>) -> Option<&'a str> {
    match expression {
        Expr::Column { .. } | Expr::Call { .. } | Expr::Array(_) | Expr::ArraySubquery(_) => {
            name_of(expression)
        }
        Expr::Cast { operand, .. } | Expr::Collate { operand, .. } => cast_source_name(operand),
        _ => None,
    }
}

fn cast_target_name(type_name: &str) -> Option<&str> {
    if matches!(
        ColType::from_sql_name(type_name),
        Some(ColType::Regtype)
            | Some(ColType::Regproc)
            | Some(ColType::Regprocedure)
            | Some(ColType::Regoper)
            | Some(ColType::Regoperator)
            | Some(ColType::Regclass)
            | Some(ColType::Regnamespace)
            | Some(ColType::Regrole)
    ) {
        return ColType::from_sql_name(type_name).map(ColType::name);
    }
    if type_name.eq_ignore_ascii_case("oid") {
        return Some("oid");
    }
    if type_name.ends_with("[]") {
        return ColType::from_sql_name(type_name.trim_end_matches("[]"))
            .map(ColType::internal_name)
            .or_else(|| type_name.trim_end_matches("[]").rsplit('.').next());
    }
    ColType::from_sql_name(type_name)
        .map(ColType::internal_name)
        .or_else(|| type_name.rsplit('.').next())
}

/// PostgreSQL's output-column name for a SELECT-list expression: `name_of`
/// followed by the mandated per-node default ("case" for CASE, otherwise
/// "?column?").
pub fn derived_name<'a>(expression: &Expr<'a>) -> &'a str {
    if let Some(n) = name_of(expression) {
        return n;
    }
    match expression {
        Expr::Case {
            synthetic: false, ..
        } => "case",
        Expr::WholeRow(t) => t,
        Expr::Exists(_) => "exists",
        Expr::ArraySubquery(_) | Expr::Array(_) => "array",
        // A scalar subquery is named by its single output column.
        Expr::Subquery(s) => match s.items.first() {
            Some(SelectItem::Expr { alias: Some(a), .. }) => a,
            Some(SelectItem::Expr {
                expression,
                alias: None,
            }) => derived_name(expression),
            _ => "?column?",
        },
        _ => "?column?",
    }
}

#[derive(Clone, Copy)]
pub struct StaticTypeMeta {
    pub ctype: ColType,
    pub type_oid: i32,
    pub type_mod: i32,
    pub collation: crate::sql::ast::Collation,
}

impl StaticTypeMeta {
    fn of(ctype: ColType) -> Self {
        Self {
            ctype,
            type_oid: ctype.oid(),
            type_mod: -1,
            collation: crate::sql::ast::Collation::None,
        }
    }
}

/// Resolves a column reference's type during static analysis. Returns an
/// error for an unknown column (or absent FROM clause).
pub trait ColTypeResolver {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError>;

    fn column_meta(&self, qualifier: Option<&str>, name: &str) -> Option<StaticTypeMeta> {
        self.resolve(qualifier, name).ok().map(StaticTypeMeta::of)
    }

    fn named_type_oid(&self, type_name: &str) -> Option<i32> {
        ColType::from_sql_name(type_name).map(ColType::oid)
    }

    /// SQL-routine result type resolved from already-inferred argument type
    /// identities. OIDs retain a domain identity that its runtime value does
    /// not carry.
    /// Plain column resolvers have no catalog and therefore expose none.
    fn routine_result(&self, _name: &str, _arguments: &[i32]) -> Option<StaticTypeMeta> {
        None
    }

    fn routine_record_field(
        &self,
        _name: &str,
        _arguments: &[i32],
        _index: usize,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        None
    }

    /// Whether an unqualified `name` names a FROM item (so a bare reference to
    /// it is a whole-row/record value). Defaults to false.
    fn is_whole_row(&self, _name: &str) -> bool {
        false
    }

    /// If a whole-row reference to `name` is actually a scalar (a
    /// set-returning-function scan's single output column), that column's type.
    /// Defaults to None, meaning the whole-row reference is an anonymous record.
    fn whole_row_scalar_type(&self, _name: &str) -> Option<ColType> {
        None
    }

    /// The columns of the FROM item exposed as `name`, for resolving a
    /// whole-row record's field shape (`(t).c`, `(t).*`). Defaults to None.
    fn table_columns(&self, _name: &str) -> Option<&[ColumnMeta]> {
        None
    }

    /// One field of a whole-row qualifier. Unlike `table_columns`, this also
    /// represents synthetic row qualifiers such as `USING (...) AS alias`.
    fn whole_row_field(
        &self,
        name: &str,
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        let column = self.table_columns(name)?.get(index)?;
        Some((
            crate::util::StackStr::from_str(column.name.as_str()),
            self.column_meta(Some(name), column.name.as_str())?,
        ))
    }

    /// The registered shape handle of a record-typed *column* (a derived
    /// table's `type_mod` carries it), or None when the column is not a
    /// record or has no shape.
    fn record_column_handle(&self, _qualifier: Option<&str>, _name: &str) -> Option<i32> {
        None
    }

    /// One durable named-composite field, copied out of the bounded catalog.
    fn named_composite_field(
        &self,
        _type_name: &str,
        _index: usize,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        None
    }
}

/// Declared formal-parameter types for the SQL routine currently undergoing
/// static analysis.  Routine bodies re-enter normal query planning for CTEs,
/// derived tables, and set-operation leaves; keeping the declaration at this
/// shared inference boundary prevents any one of those paths from silently
/// degrading `$n` to `unknown`.
#[derive(Clone, Copy)]
struct RoutineParameterTypes {
    types: [Option<ColType>; MAX_ROUTINE_ARGUMENTS],
}

impl RoutineParameterTypes {
    const EMPTY: Self = Self {
        types: [None; MAX_ROUTINE_ARGUMENTS],
    };
}

std::thread_local! {
    static ROUTINE_PARAMETER_TYPES: Cell<RoutineParameterTypes> =
        const { Cell::new(RoutineParameterTypes::EMPTY) };
    static BOUND_PARAMETER_TYPES: Cell<BoundParameterTypes> =
        const { Cell::new(BoundParameterTypes::EMPTY) };
}

#[derive(Clone, Copy)]
struct BoundParameterTypes {
    oids: [Option<i32>; MAX_ROUTINE_ARGUMENTS],
}

impl BoundParameterTypes {
    const EMPTY: Self = Self {
        oids: [None; MAX_ROUTINE_ARGUMENTS],
    };
}

pub(crate) struct BoundParameterScope(BoundParameterTypes);

pub(crate) fn enter_bound_parameter_types(oids: &[i32]) -> BoundParameterScope {
    let mut current = BoundParameterTypes::EMPTY;
    for (slot, oid) in oids.iter().copied().enumerate().take(MAX_ROUTINE_ARGUMENTS) {
        current.oids[slot] = (oid != 0).then_some(oid);
    }
    BoundParameterScope(BOUND_PARAMETER_TYPES.with(|slot| slot.replace(current)))
}

impl Drop for BoundParameterScope {
    fn drop(&mut self) {
        BOUND_PARAMETER_TYPES.with(|slot| slot.set(self.0));
    }
}

pub(crate) fn bound_parameter_type_oid(index: u32) -> Option<i32> {
    index.checked_sub(1).and_then(|index| {
        BOUND_PARAMETER_TYPES.with(|types| types.get().oids.get(index as usize).copied().flatten())
    })
}

/// Restores the enclosing routine's declaration when nested SQL routines
/// return.  A parameter type is execution context, not a process-wide default.
pub(crate) struct RoutineParameterScope(RoutineParameterTypes);

pub(crate) fn enter_routine_parameter_types(
    arguments: &[RoutineArgumentDef],
) -> RoutineParameterScope {
    let mut current = RoutineParameterTypes::EMPTY;
    for (slot, argument) in arguments.iter().enumerate() {
        current.types[slot] = Some(argument.ctype);
    }
    let prior = ROUTINE_PARAMETER_TYPES.with(|slot| slot.replace(current));
    RoutineParameterScope(prior)
}

impl Drop for RoutineParameterScope {
    fn drop(&mut self) {
        ROUTINE_PARAMETER_TYPES.with(|slot| slot.set(self.0));
    }
}

fn routine_parameter_type(index: u32) -> Option<ColType> {
    index.checked_sub(1).and_then(|index| {
        ROUTINE_PARAMETER_TYPES
            .with(|types| types.get().types.get(index as usize).copied().flatten())
    })
}

/// One field of a registered record shape (see [`register_record_shape`]).
#[derive(Clone, Copy)]
pub(crate) struct RecordShapeField {
    pub name: crate::util::StackStr<64>,
    pub ctype: ColType,
    pub type_oid: i32,
    pub type_mod: i32,
    pub collation: crate::sql::ast::Collation,
    /// Registry handle of a record-typed field's own shape, or -1.
    pub nested: i32,
}

impl RecordShapeField {
    fn metadata(self) -> StaticTypeMeta {
        StaticTypeMeta {
            ctype: self.ctype,
            type_oid: self.type_oid,
            type_mod: self.type_mod,
            collation: self.collation,
        }
    }
}

const MAX_SHAPE_FIELDS: usize = 16;
const MAX_SHAPES: usize = 32;

struct ShapePool {
    fields: [[RecordShapeField; MAX_SHAPE_FIELDS]; MAX_SHAPES],
    lens: [u8; MAX_SHAPES],
    n: usize,
    named: [[RecordShapeField; MAX_SHAPE_FIELDS]; MAX_SHAPES],
    named_names: [crate::util::StackStr<64>; MAX_SHAPES],
    named_lens: [u8; MAX_SHAPES],
    named_slots: [u16; MAX_SHAPES],
    named_n: usize,
}

std::thread_local! {
    /// Statement-scoped registry of record shapes: a derived table's
    /// record-typed column stores a handle here (in its `type_mod`, which
    /// records otherwise never use) so field access can be typed statically —
    /// PostgreSQL knows the row type; this is our transient stand-in. Reset
    /// at each statement start. Boxed, not inline: glibc places static TLS
    /// inside each thread's stack allocation (the PR-114 stack overflow), so
    /// TLS slots stay pointer-sized; the box is allocated by
    /// [`init_record_shapes`] before the allocator freezes.
    static RECORD_SHAPES: core::cell::RefCell<Option<Box<ShapePool>>> =
        const { core::cell::RefCell::new(None) };
}

fn empty_shape_pool() -> Box<ShapePool> {
    Box::new(ShapePool {
        fields: [[RecordShapeField {
            name: crate::util::StackStr::new(),
            ctype: ColType::Record,
            type_oid: oid::RECORD,
            type_mod: -1,
            collation: crate::sql::ast::Collation::None,
            nested: -1,
        }; MAX_SHAPE_FIELDS]; MAX_SHAPES],
        lens: [0; MAX_SHAPES],
        n: 0,
        named: [[RecordShapeField {
            name: crate::util::StackStr::new(),
            ctype: ColType::Record,
            type_oid: oid::RECORD,
            type_mod: -1,
            collation: crate::sql::ast::Collation::None,
            nested: -1,
        }; MAX_SHAPE_FIELDS]; MAX_SHAPES],
        named_names: [crate::util::StackStr::new(); MAX_SHAPES],
        named_lens: [0; MAX_SHAPES],
        named_slots: [u16::MAX; MAX_SHAPES],
        named_n: 0,
    })
}

/// Allocates the shape pool; the server calls this at startup, before the
/// allocator freezes. (Tests allocate lazily on first registration instead —
/// their allocator never freezes.)
pub fn init_record_shapes() {
    RECORD_SHAPES.with(|p| {
        let mut p = p.borrow_mut();
        if p.is_none() {
            *p = Some(empty_shape_pool());
        }
    });
}

/// Clears the shape registry; the engine calls this per statement.
pub fn reset_record_shapes() {
    RECORD_SHAPES.with(|p| {
        if let Some(pool) = p.borrow_mut().as_mut() {
            pool.n = 0;
            pool.named_n = 0;
        }
    });
}

/// Publishes the durable fields of one named composite for this statement.
/// Both the name and fields are copied into fixed storage, making planner
/// access independent of runtime values and without post-startup allocation.
pub fn register_named_composite_shape(
    slot: u16,
    name: &str,
    fields: &[crate::storage::CompositeFieldDef],
    storage: &crate::storage::Storage,
    txid: u32,
) -> Result<(), SqlError> {
    RECORD_SHAPES.with(|p| -> Result<(), SqlError> {
        let mut p = p.borrow_mut();
        let pool = p.get_or_insert_with(empty_shape_pool);
        if pool.named_n == MAX_SHAPES || fields.len() > MAX_SHAPE_FIELDS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "record shape capacity exceeded"
            ));
        }
        let at = pool.named_n;
        pool.named_names[at] = crate::util::StackStr::from_str(name);
        pool.named_slots[at] = slot;
        pool.named_lens[at] = fields.len() as u8;
        for (out, field) in pool.named[at].iter_mut().zip(fields) {
            out.name = crate::util::StackStr::from_str(field.name.as_str());
            out.ctype = field.ctype;
            out.type_oid = storage
                .routine_type_oid(field.ctype, field.user_type, txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "composite field type is absent from the catalog"
                    )
                })?;
            out.type_mod = field.type_mod;
            out.collation = field.collation;
            out.nested = -1;
        }
        pool.named_n += 1;
        Ok(())
    })
}

fn visit_composite_slot_shape_metadata(
    slot: u16,
    mut visit: impl FnMut(&str, StaticTypeMeta),
) -> Option<usize> {
    RECORD_SHAPES.with(|p| {
        let p = p.borrow();
        let pool = p.as_ref()?;
        let at = pool.named_slots[..pool.named_n]
            .iter()
            .position(|candidate| *candidate == slot)?;
        let len = pool.named_lens[at] as usize;
        for field in &pool.named[at][..len] {
            visit(field.name.as_str(), field.metadata());
        }
        Some(len)
    })
}

fn composite_slot_field_metadata(slot: u16, field: &str) -> Option<StaticTypeMeta> {
    RECORD_SHAPES.with(|p| {
        let p = p.borrow();
        let pool = p.as_ref()?;
        let at = pool.named_slots[..pool.named_n]
            .iter()
            .position(|candidate| *candidate == slot)?;
        pool.named[at][..pool.named_lens[at] as usize]
            .iter()
            .find(|candidate| candidate.name.as_str().eq_ignore_ascii_case(field))
            .map(|candidate| candidate.metadata())
    })
}

/// Registers a record shape, returning its handle, or None when the
/// statement's pool is exhausted (the caller then leaves the column without
/// a shape and field access fails loudly, never wrongly).
pub(crate) fn register_record_shape(fields: &[RecordShapeField]) -> Option<i32> {
    RECORD_SHAPES.with(|p| {
        let mut p = p.borrow_mut();
        let pool = p.get_or_insert_with(empty_shape_pool);
        if pool.n == MAX_SHAPES || fields.len() > MAX_SHAPE_FIELDS {
            return None;
        }
        let at = pool.n;
        pool.fields[at][..fields.len()].copy_from_slice(fields);
        pool.lens[at] = fields.len() as u8;
        pool.n += 1;
        Some(at as i32)
    })
}

/// Looks up one field of a registered shape by (case-insensitive) name.
pub(crate) fn record_shape_field(handle: i32, field: &str) -> Option<(ColType, i32)> {
    record_shape_field_metadata(handle, field).map(|(meta, nested)| (meta.ctype, nested))
}

pub(crate) fn record_shape_field_metadata(
    handle: i32,
    field: &str,
) -> Option<(StaticTypeMeta, i32)> {
    RECORD_SHAPES.with(|p| {
        let p = p.borrow();
        let pool = p.as_ref()?;
        let at = usize::try_from(handle).ok()?;
        if at >= pool.n {
            return None;
        }
        pool.fields[at][..pool.lens[at] as usize]
            .iter()
            .find(|f| f.name.as_str().eq_ignore_ascii_case(field))
            .map(|f| (f.metadata(), f.nested))
    })
}

/// Visits every (name, type) of a registered shape.
pub fn visit_record_shape(handle: i32, mut visit: impl FnMut(&str, ColType)) -> Option<usize> {
    visit_record_shape_metadata(handle, |name, meta| visit(name, meta.ctype))
}

pub fn visit_record_shape_metadata(
    handle: i32,
    mut visit: impl FnMut(&str, StaticTypeMeta),
) -> Option<usize> {
    RECORD_SHAPES.with(|p| {
        let p = p.borrow();
        let pool = p.as_ref()?;
        let at = usize::try_from(handle).ok()?;
        if at >= pool.n {
            return None;
        }
        for f in &pool.fields[at][..pool.lens[at] as usize] {
            visit(f.name.as_str(), f.metadata());
        }
        Some(pool.lens[at] as usize)
    })
}

/// The shape handle of a record-valued expression, when one is registered: a
/// record-typed column carries it in its type modifier, and a field of such a
/// column may itself be a nested record.
pub fn expr_record_handle(base: &Expr, columns: &dyn ColTypeResolver) -> Option<i32> {
    match base {
        Expr::Column { qualifier, name } => {
            if columns.is_whole_row(name) {
                return None;
            }
            let handle = columns.record_column_handle(*qualifier, name)?;
            (handle >= 0).then_some(handle)
        }
        Expr::Field { base: inner, field } => {
            if let Some(parent) = expr_record_handle(inner, columns) {
                let (ctype, nested) = record_shape_field(parent, field)?;
                return (ctype == ColType::Record && nested >= 0).then_some(nested);
            }
            // A record-typed column reached through its table's whole row
            // (`(v).r` where `r` is a record column of `v`).
            let table = match inner {
                Expr::WholeRow(table) => table,
                Expr::Column {
                    qualifier: None,
                    name,
                } if columns.is_whole_row(name) => name,
                _ => return None,
            };
            let column = columns
                .table_columns(table)?
                .iter()
                .find(|c| c.name.as_str().eq_ignore_ascii_case(field))?;
            (column.ctype == ColType::Record && column.type_mod >= 0).then_some(column.type_mod)
        }
        _ => None,
    }
}

fn expression_static_metadata(
    expression: &Expr,
    columns: &dyn ColTypeResolver,
) -> Option<StaticTypeMeta> {
    match expression {
        Expr::Column { qualifier, name } if !columns.is_whole_row(name) => {
            columns.column_meta(*qualifier, name)
        }
        Expr::Field { base, field } => record_field_metadata(base, field, columns).ok(),
        Expr::Cast {
            operand, type_mod, ..
        } => {
            let (type_oid, _) = infer_type_res(expression, columns).ok()?;
            let ctype = coltype_of_oid(type_oid)?;
            let collation = if ctype.is_collatable() {
                expression_static_metadata(operand, columns)
                    .map(|meta| meta.collation)
                    .filter(|collation| *collation != crate::sql::ast::Collation::None)
                    .unwrap_or(crate::sql::ast::Collation::Default)
            } else {
                crate::sql::ast::Collation::None
            };
            Some(StaticTypeMeta {
                ctype,
                type_oid,
                type_mod: *type_mod,
                collation,
            })
        }
        Expr::Collate { operand, collation } => {
            let mut meta = expression_static_metadata(operand, columns)?;
            meta.collation = *collation;
            Some(meta)
        }
        _ => {
            let (type_oid, _) = infer_type_res(expression, columns).ok()?;
            let ctype = if type_oid == oid::UNKNOWN {
                ColType::Text
            } else {
                coltype_of_oid(type_oid)?
            };
            Some(StaticTypeMeta {
                ctype,
                type_oid,
                type_mod: -1,
                collation: if ctype.is_collatable() {
                    crate::sql::ast::Collation::Default
                } else {
                    crate::sql::ast::Collation::None
                },
            })
        }
    }
}

/// Registers the shape of a record-valued select item, so a derived table's
/// record column can carry it (in `type_mod`) for later field access. A
/// record column propagates its existing handle; a `ROW(...)` derives one
/// from its arguments (nested rows recursively); a whole-row reference takes
/// its table's columns; the `json_each` family its declared pair. None when
/// no static shape exists (field access then fails loudly, never wrongly).
pub fn register_shape_for(expr: &Expr, columns: &dyn ColTypeResolver) -> Option<i32> {
    if let Some(handle) = expr_record_handle(expr, columns) {
        return Some(handle);
    }
    let mut fields = [RecordShapeField {
        name: crate::util::StackStr::new(),
        ctype: ColType::Record,
        type_oid: oid::RECORD,
        type_mod: -1,
        collation: crate::sql::ast::Collation::None,
        nested: -1,
    }; MAX_SHAPE_FIELDS];
    let mut n = 0usize;
    match expr {
        Expr::Call { name, .. } if name.eq_ignore_ascii_case("row") => {
            record_shape_metadata(expr, columns, |name, meta| {
                if n < MAX_SHAPE_FIELDS {
                    fields[n] = RecordShapeField {
                        name: crate::util::StackStr::from_str(name),
                        ctype: meta.ctype,
                        type_oid: meta.type_oid,
                        type_mod: meta.type_mod,
                        collation: meta.collation,
                        nested: -1,
                    };
                    n += 1;
                }
            })?;
        }
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("_pg_expandarray") => {
            let element = expand_array_element_metadata(args, columns)?;
            for (index, (name, meta)) in [("x", element), ("n", StaticTypeMeta::of(ColType::Int4))]
                .into_iter()
                .enumerate()
            {
                let mut field_name = crate::util::StackStr::new();
                let _ = core::fmt::Write::write_str(&mut field_name, name);
                fields[index] = RecordShapeField {
                    name: field_name,
                    ctype: meta.ctype,
                    type_oid: meta.type_oid,
                    type_mod: meta.type_mod,
                    collation: meta.collation,
                    nested: -1,
                };
                n += 1;
            }
        }
        Expr::Call { name, .. } if builtin_record_srf_field(name, 0).is_some() => {
            while let Some((field, ctype)) = builtin_record_srf_field(name, n) {
                let mut field_name = crate::util::StackStr::new();
                let _ = core::fmt::Write::write_str(&mut field_name, field);
                fields[n] = RecordShapeField {
                    name: field_name,
                    ctype,
                    type_oid: ctype.oid(),
                    type_mod: -1,
                    collation: if ctype.is_collatable() {
                        crate::sql::ast::Collation::Default
                    } else {
                        crate::sql::ast::Collation::None
                    },
                    nested: -1,
                };
                n += 1;
            }
        }
        Expr::WholeRow(table) => {
            while let Some((name, meta)) = columns.whole_row_field(table, n) {
                if n == MAX_SHAPE_FIELDS {
                    return None;
                }
                fields[n] = RecordShapeField {
                    name,
                    ctype: meta.ctype,
                    type_oid: meta.type_oid,
                    type_mod: meta.type_mod,
                    collation: meta.collation,
                    nested: -1,
                };
                n += 1;
            }
            if n == 0 {
                return None;
            }
        }
        Expr::Column {
            qualifier: None,
            name,
        } if columns.is_whole_row(name) => {
            while let Some((field_name, meta)) = columns.whole_row_field(name, n) {
                if n == MAX_SHAPE_FIELDS {
                    return None;
                }
                fields[n] = RecordShapeField {
                    name: field_name,
                    ctype: meta.ctype,
                    type_oid: meta.type_oid,
                    type_mod: meta.type_mod,
                    collation: meta.collation,
                    nested: -1,
                };
                n += 1;
            }
            if n == 0 {
                return None;
            }
        }
        _ => return None,
    }
    register_record_shape(&fields[..n])
}

/// Static field names PostgreSQL assigns an anonymous record (`ROW(...)`):
/// `f1`, `f2`, … Indexed 1-based by the caller.
pub const RECORD_FIELD_NAMES: [&str; 64] = [
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14", "f15",
    "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24", "f25", "f26", "f27", "f28",
    "f29", "f30", "f31", "f32", "f33", "f34", "f35", "f36", "f37", "f38", "f39", "f40", "f41",
    "f42", "f43", "f44", "f45", "f46", "f47", "f48", "f49", "f50", "f51", "f52", "f53", "f54",
    "f55", "f56", "f57", "f58", "f59", "f60", "f61", "f62", "f63", "f64",
];

/// The value type of `json_each`-family output's `value` column.
fn json_each_value_type(name: &str) -> Option<ColType> {
    if name.eq_ignore_ascii_case("json_each") {
        Some(ColType::Json)
    } else if name.eq_ignore_ascii_case("jsonb_each") {
        Some(ColType::Jsonb)
    } else if name.eq_ignore_ascii_case("json_each_text")
        || name.eq_ignore_ascii_case("jsonb_each_text")
    {
        Some(ColType::Text)
    } else {
        None
    }
}

fn builtin_record_srf_field(name: &str, index: usize) -> Option<(&'static str, ColType)> {
    if let Some(value_type) = json_each_value_type(name) {
        return match index {
            0 => Some(("key", ColType::Text)),
            1 => Some(("value", value_type)),
            _ => None,
        };
    }
    if name.eq_ignore_ascii_case("pg_options_to_table") {
        return match index {
            0 => Some(("option_name", ColType::Text)),
            1 => Some(("option_value", ColType::Text)),
            _ => None,
        };
    }
    if name.eq_ignore_ascii_case("pg_get_sequence_data") {
        return match index {
            0 => Some(("last_value", ColType::Int8)),
            1 => Some(("is_called", ColType::Bool)),
            _ => None,
        };
    }
    None
}

fn expand_array_element_metadata(
    args: &[&Expr<'_>],
    columns: &dyn ColTypeResolver,
) -> Option<StaticTypeMeta> {
    let argument = *args.first()?;
    let array = expression_static_metadata(argument, columns)?;
    let (ctype, type_oid) = match array.ctype {
        ColType::Array(element) => (element.to_coltype(), element.element_oid()),
        ColType::Int2Vector => (ColType::Int2, oid::INT2),
        ColType::OidVector => (ColType::Oid, oid::OID),
        _ => return None,
    };
    Some(StaticTypeMeta {
        ctype,
        type_oid,
        type_mod: array.type_mod,
        collation: if ctype.is_collatable() {
            array.collation
        } else {
            crate::sql::ast::Collation::None
        },
    })
}

pub(crate) fn builtin_record_srf_field_pub(
    name: &str,
    index: usize,
) -> Option<(&'static str, ColType)> {
    builtin_record_srf_field(name, index)
}

/// Visits each `(field_name, type)` of a record-valued expression's shape,
/// returning the field count, or None when `base` is not a record whose shape
/// is statically known. Handles `ROW(...)`, a whole-row reference to a FROM
/// table, and the `json_each` family. The visited names borrow only for the
/// call, so callers copy them (into the arena, or into a `ColDesc`).
pub fn record_shape(
    base: &Expr,
    columns: &dyn ColTypeResolver,
    mut visit: impl FnMut(&str, ColType),
) -> Option<usize> {
    record_shape_metadata(base, columns, |name, meta| visit(name, meta.ctype))
}

fn record_shape_metadata(
    base: &Expr,
    columns: &dyn ColTypeResolver,
    mut visit: impl FnMut(&str, StaticTypeMeta),
) -> Option<usize> {
    record_shape_metadata_dyn(base, columns, &mut visit)
}

fn record_shape_metadata_dyn(
    base: &Expr,
    columns: &dyn ColTypeResolver,
    visit: &mut dyn FnMut(&str, StaticTypeMeta),
) -> Option<usize> {
    match base {
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => {
            let mut count = 0usize;
            for arg in *args {
                let expansion = match arg {
                    Expr::WholeRow(_) => Some(*arg),
                    Expr::Field { base, field: "*" } => Some(*base),
                    _ => None,
                };
                if let Some(base) = expansion {
                    let mut append = |_: &str, meta| {
                        if count < RECORD_FIELD_NAMES.len() {
                            visit(RECORD_FIELD_NAMES[count], meta);
                            count += 1;
                        }
                    };
                    record_shape_metadata_dyn(base, columns, &mut append)?;
                } else {
                    if count == RECORD_FIELD_NAMES.len() {
                        return None;
                    }
                    visit(
                        RECORD_FIELD_NAMES[count],
                        expression_static_metadata(arg, columns)?,
                    );
                    count += 1;
                }
            }
            Some(count)
        }
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("_pg_expandarray") => {
            visit("x", expand_array_element_metadata(args, columns)?);
            visit("n", StaticTypeMeta::of(ColType::Int4));
            Some(2)
        }
        Expr::Call { name, .. } if builtin_record_srf_field(name, 0).is_some() => {
            let mut count = 0;
            while let Some((field, ctype)) = builtin_record_srf_field(name, count) {
                visit(field, StaticTypeMeta::of(ctype));
                count += 1;
            }
            Some(count)
        }
        Expr::Call { name, args, .. } => {
            let mut argument_oids = [oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
            if args.len() > argument_oids.len() {
                return None;
            }
            for (index, argument) in args.iter().enumerate() {
                argument_oids[index] = infer_routine_argument_oid(argument, columns).ok()?;
            }
            let mut count = 0usize;
            while let Some((field_name, meta)) =
                columns.routine_record_field(name, &argument_oids[..args.len()], count)
            {
                visit(field_name.as_str(), meta);
                count += 1;
            }
            (count != 0).then_some(count)
        }
        Expr::WholeRow(table) => shape_from_columns_metadata(table, columns, visit),
        Expr::Cast { type_name, .. } => {
            let mut n = 0;
            while let Some((name, meta)) = columns.named_composite_field(type_name, n) {
                visit(name.as_str(), meta);
                n += 1;
            }
            (n != 0).then_some(n)
        }
        Expr::Column { qualifier, name }
            if matches!(columns.resolve(*qualifier, name), Ok(ColType::Composite(_))) =>
        {
            let ColType::Composite(slot) = columns.resolve(*qualifier, name).ok()? else {
                unreachable!()
            };
            visit_composite_slot_shape_metadata(slot, visit)
        }
        Expr::Field { base, field }
            if matches!(
                record_field_type(base, field, columns),
                Ok(ColType::Composite(_))
            ) =>
        {
            let Ok(ColType::Composite(slot)) = record_field_type(base, field, columns) else {
                unreachable!()
            };
            visit_composite_slot_shape_metadata(slot, visit)
        }
        Expr::Subscript { base: array, .. } => {
            let element = match &**array {
                Expr::Column { qualifier, name } => columns
                    .resolve(*qualifier, name)
                    .ok()
                    .and_then(|ctype| match ctype {
                        ColType::Array(element) => Some(element.to_coltype()),
                        _ => None,
                    }),
                Expr::Field { base, field } => record_field_type(base, field, columns)
                    .ok()
                    .and_then(|ctype| match ctype {
                        ColType::Array(element) => Some(element.to_coltype()),
                        _ => None,
                    }),
                _ => infer_type_res(array, columns)
                    .ok()
                    .and_then(|(oid, _)| coltype_of_oid(oid))
                    .and_then(|ctype| match ctype {
                        ColType::Array(element) => Some(element.to_coltype()),
                        _ => None,
                    }),
            };
            let Some(ColType::Composite(slot)) = element else {
                return None;
            };
            visit_composite_slot_shape_metadata(slot, visit)
        }
        // A record-typed column (or a record field of one) with a registered
        // shape exposes its fields for selection and star expansion.
        Expr::Column { .. } | Expr::Field { .. } if expr_record_handle(base, columns).is_some() => {
            visit_record_shape_metadata(expr_record_handle(base, columns)?, visit)
        }
        Expr::Column {
            qualifier: None,
            name,
        } if columns.is_whole_row(name) => shape_from_columns_metadata(name, columns, visit),
        _ => None,
    }
}

fn shape_from_columns_metadata(
    table: &str,
    columns: &dyn ColTypeResolver,
    mut visit: impl FnMut(&str, StaticTypeMeta),
) -> Option<usize> {
    let mut count = 0usize;
    while let Some((name, meta)) = columns.whole_row_field(table, count) {
        visit(name.as_str(), meta);
        count += 1;
    }
    (count != 0).then_some(count)
}

/// PostgreSQL cannot form the composite type of a `ROW(...)` that contains a
/// bare unknown literal, so selecting a field of (or expanding) such a record
/// fails — even for a well-typed sibling field. Mirror that so `(ROW(1,'x')).f1`
/// errors exactly as PostgreSQL does.
pub fn check_row_field_types(base: &Expr, columns: &dyn ColTypeResolver) -> Result<(), SqlError> {
    if let Expr::Call { name, args, .. } = base
        && name.eq_ignore_ascii_case("row")
    {
        for arg in *args {
            if infer_type_res(arg, columns)?.0 == oid::UNKNOWN && !matches!(arg, Expr::Cast { .. })
            {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "failed to find conversion function from unknown to text"
                ));
            }
        }
    }
    Ok(())
}

/// The type of a record's field `field` (for `(base).field`), or an error if
/// `base` is not a record whose shape is known or the field does not exist.
pub fn record_field_metadata(
    base: &Expr,
    field: &str,
    columns: &dyn ColTypeResolver,
) -> Result<StaticTypeMeta, SqlError> {
    if field == "*" {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "row expansion via \"*\" is not supported here"
        ));
    }
    // A bare unknown literal cannot be coerced out of a ROW(...) — but only
    // selecting *that* field (star expansion checks every field elsewhere)
    // hits the failure; a typed sibling selects fine.
    if let Expr::Call { name, args, .. } = base
        && name.eq_ignore_ascii_case("row")
        && let Some(position) = RECORD_FIELD_NAMES
            .iter()
            .position(|n| n.eq_ignore_ascii_case(field))
        && let Some(arg) = args.get(position)
        && infer_type_res(arg, columns)?.0 == oid::UNKNOWN
        && !matches!(arg, Expr::Cast { .. })
    {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "failed to find conversion function from unknown to text"
        ));
    }
    let precise = match base {
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => RECORD_FIELD_NAMES
            .iter()
            .position(|name| name.eq_ignore_ascii_case(field))
            .and_then(|position| args.get(position))
            .and_then(|argument| expression_static_metadata(argument, columns)),
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("_pg_expandarray") => {
            if field.eq_ignore_ascii_case("x") {
                expand_array_element_metadata(args, columns)
            } else if field.eq_ignore_ascii_case("n") {
                Some(StaticTypeMeta::of(ColType::Int4))
            } else {
                None
            }
        }
        Expr::Call { name, .. } if builtin_record_srf_field(name, 0).is_some() => {
            let mut index = 0usize;
            let mut found = None;
            while let Some((name, ctype)) = builtin_record_srf_field(name, index) {
                if name.eq_ignore_ascii_case(field) {
                    found = Some(StaticTypeMeta::of(ctype));
                    break;
                }
                index += 1;
            }
            found
        }
        Expr::Call { name, args, .. } => {
            let mut argument_oids = [oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
            if args.len() > argument_oids.len() {
                None
            } else {
                for (index, argument) in args.iter().enumerate() {
                    argument_oids[index] = infer_routine_argument_oid(argument, columns)?;
                }
                let mut index = 0usize;
                let mut found = None;
                while let Some((name, meta)) =
                    columns.routine_record_field(name, &argument_oids[..args.len()], index)
                {
                    if name.as_str().eq_ignore_ascii_case(field) {
                        found = Some(meta);
                        break;
                    }
                    index += 1;
                }
                found
            }
        }
        Expr::WholeRow(table) => columns.column_meta(Some(table), field),
        Expr::Column {
            qualifier: None,
            name,
        } if columns.is_whole_row(name) => columns.column_meta(Some(name), field),
        Expr::Column { qualifier, name } => columns
            .record_column_handle(*qualifier, name)
            .and_then(|handle| record_shape_field_metadata(handle, field).map(|value| value.0))
            .or_else(|| match columns.resolve(*qualifier, name).ok()? {
                ColType::Composite(slot) => composite_slot_field_metadata(slot, field),
                _ => None,
            }),
        Expr::Field {
            base: inner,
            field: inner_field,
        } => expr_record_handle(base, columns)
            .and_then(|handle| record_shape_field_metadata(handle, field).map(|value| value.0))
            .or_else(
                || match record_field_type(inner, inner_field, columns).ok()? {
                    ColType::Composite(slot) => composite_slot_field_metadata(slot, field),
                    _ => None,
                },
            ),
        Expr::Cast { type_name, .. } => {
            let mut index = 0usize;
            let mut found = None;
            while let Some((name, meta)) = columns.named_composite_field(type_name, index) {
                if name.as_str().eq_ignore_ascii_case(field) {
                    found = Some(meta);
                    break;
                }
                index += 1;
            }
            found
        }
        _ => None,
    };
    if let Some(meta) = precise {
        return Ok(meta);
    }
    let mut found = None;
    let shape = record_shape_metadata(base, columns, |name, meta| {
        if found.is_none() && name.eq_ignore_ascii_case(field) {
            found = Some(meta);
        }
    });
    if shape.is_none() {
        // Not a record at all: PostgreSQL names the non-composite type.
        let type_name = infer_type_res(base, columns)
            .ok()
            .and_then(|(oid, _)| coltype_of_oid(oid))
            .map(|t| t.name())
            .unwrap_or("record");
        if type_name == "record" {
            return Err(could_not_identify(field));
        }
        return Err(not_composite(field, type_name));
    }
    found.ok_or_else(|| match base {
        // A whole-row reference names the missing column with its table; a
        // record-typed *column* keeps the anonymous-record wording.
        Expr::WholeRow(table) => sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column {}.{} does not exist",
            table,
            field
        ),
        Expr::Column {
            qualifier: None,
            name: table,
        } if columns.is_whole_row(table) => {
            sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column {}.{} does not exist",
                table,
                field
            )
        }
        _ => could_not_identify(field),
    })
}

pub fn record_field_type(
    base: &Expr,
    field: &str,
    columns: &dyn ColTypeResolver,
) -> Result<ColType, SqlError> {
    record_field_metadata(base, field, columns).map(|meta| meta.ctype)
}

/// PostgreSQL's 42703 for a field of an anonymous record.
pub fn could_not_identify(field: &str) -> SqlError {
    sql_err!(
        sqlstate::UNDEFINED_COLUMN,
        "could not identify column \"{}\" in record data type",
        field
    )
}

/// PostgreSQL's 42809 for column notation on a non-composite value.
pub fn not_composite(field: &str, type_name: &str) -> SqlError {
    sql_err!(
        sqlstate::WRONG_OBJECT_TYPE,
        "column notation .{} applied to type {}, which is not a composite type",
        field,
        type_name
    )
}

/// No FROM clause: any column reference is an error.
pub struct NoCols;
impl ColTypeResolver for NoCols {
    fn resolve(&self, _q: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        Err(sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" does not exist",
            name
        ))
    }
}

struct NoColumnLookup;

impl<'a> ColumnLookup<'a> for NoColumnLookup {
    fn lookup(&self, _qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        Err(sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" does not exist",
            name
        ))
    }
}

/// A single table's columns.
pub struct DefCols<'d>(pub &'d TableDef);
impl ColTypeResolver for DefCols<'_> {
    fn resolve(&self, q: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        if let Some(q) = q
            && !crate::sql::eval::qualifier_answers_single(self.0, q)
        {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "missing FROM-clause entry for table \"{}\"",
                q
            ));
        }
        match self.0.column_index(name) {
            Some(i) => Ok(self.0.columns()[i].ctype),
            None => Err(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" does not exist",
                name
            )),
        }
    }

    fn record_column_handle(&self, _qualifier: Option<&str>, name: &str) -> Option<i32> {
        let i = self.0.column_index(name)?;
        let column = &self.0.columns()[i];
        (column.ctype == ColType::Record).then_some(column.type_mod)
    }

    fn column_meta(&self, qualifier: Option<&str>, name: &str) -> Option<StaticTypeMeta> {
        self.resolve(qualifier, name).ok()?;
        let column = self.0.columns().get(self.0.column_index(name)?)?;
        Some(StaticTypeMeta {
            ctype: column.ctype,
            type_oid: column.ctype.oid(),
            type_mod: column.type_mod,
            collation: column.collation,
        })
    }

    fn is_whole_row(&self, name: &str) -> bool {
        name == self.0.name.as_str()
    }

    fn table_columns(&self, name: &str) -> Option<&[ColumnMeta]> {
        (name == self.0.name.as_str()).then(|| self.0.columns())
    }
}

impl<'a> ColumnLookup<'a> for DefCols<'_> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        self.resolve(qualifier, name)?;
        Ok(Datum::Null)
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        self.resolve(qualifier, name).ok()
    }

    fn collation(&self, qualifier: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        self.resolve(qualifier, name)
            .ok()
            .and_then(|_| self.0.column_index(name))
            .map(|index| self.0.columns()[index].collation)
            .unwrap_or(crate::sql::ast::Collation::None)
    }

    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> crate::sql::ast::Collation {
        record_field_metadata(base, field, self)
            .map_or(crate::sql::ast::Collation::None, |meta| meta.collation)
    }
}

/// A single DML target's columns, with its optional PostgreSQL correlation name.
pub(crate) struct AliasedDefCols<'d, 'a> {
    pub definition: &'d TableDef,
    pub alias: Option<&'a str>,
}
impl ColTypeResolver for AliasedDefCols<'_, '_> {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        if let Some(qualifier) = qualifier
            && !crate::sql::eval::qualifier_answers_target(self.definition, self.alias, qualifier)
        {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "missing FROM-clause entry for table \"{}\"",
                qualifier
            ));
        }
        self.definition
            .column_index(name)
            .map(|index| self.definition.columns()[index].ctype)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                )
            })
    }
    fn record_column_handle(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
        self.resolve(qualifier, name).ok()?;
        let index = self.definition.column_index(name)?;
        let column = &self.definition.columns()[index];
        (column.ctype == ColType::Record).then_some(column.type_mod)
    }
    fn column_meta(&self, qualifier: Option<&str>, name: &str) -> Option<StaticTypeMeta> {
        self.resolve(qualifier, name).ok()?;
        let column = self
            .definition
            .columns()
            .get(self.definition.column_index(name)?)?;
        Some(StaticTypeMeta {
            ctype: column.ctype,
            type_oid: column.ctype.oid(),
            type_mod: column.type_mod,
            collation: column.collation,
        })
    }
    fn is_whole_row(&self, name: &str) -> bool {
        crate::sql::eval::qualifier_answers_target(self.definition, self.alias, name)
    }
    fn table_columns(&self, name: &str) -> Option<&[ColumnMeta]> {
        crate::sql::eval::qualifier_answers_target(self.definition, self.alias, name)
            .then(|| self.definition.columns())
    }
}

impl<'a> ColumnLookup<'a> for AliasedDefCols<'_, '_> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        self.resolve(qualifier, name)?;
        Ok(Datum::Null)
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        self.resolve(qualifier, name).ok()
    }

    fn collation(&self, qualifier: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        self.resolve(qualifier, name)
            .ok()
            .and_then(|_| self.definition.column_index(name))
            .map(|index| self.definition.columns()[index].collation)
            .unwrap_or(crate::sql::ast::Collation::None)
    }

    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> crate::sql::ast::Collation {
        record_field_metadata(base, field, self)
            .map_or(crate::sql::ast::Collation::None, |meta| meta.collation)
    }
}

/// Adapts a runtime row (`ColumnLookup`) to the static `ColTypeResolver` that
/// `infer_type_res` needs, so an expression's declared type can be recovered
/// during evaluation even when its value is NULL.
struct RowCols<'r, 'a> {
    row: &'r dyn crate::sql::eval::ColumnLookup<'a>,
    catalog: Option<&'r dyn crate::sql::eval::CatalogAccess>,
}
impl<'a> ColTypeResolver for RowCols<'_, 'a> {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        self.row.col_type(qualifier, name).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" does not exist",
                name
            )
        })
    }

    fn named_type_oid(&self, type_name: &str) -> Option<i32> {
        self.catalog
            .and_then(|catalog| catalog.user_type_oid(type_name))
            .or_else(|| ColType::from_sql_name(type_name).map(ColType::oid))
    }
}

/// The PostgreSQL type name `pg_typeof` reports for `expression` evaluated
/// against `row`, resolved statically (so a NULL value still names its declared
/// type, matching PostgreSQL). `None` when the static type can't be pinned down
/// (the caller then falls back to the runtime datum's type).
/// The static [`ColType`] behind [`typeof_static`], for callers that must
/// check the resolution against a runtime value before trusting it.
pub fn typeof_static_coltype<'a>(
    expression: &Expr,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
) -> Option<ColType> {
    let (type_oid, _) = infer_type_res(expression, &RowCols { row, catalog }).ok()?;
    coltype_of_oid(type_oid)
}

pub fn typeof_static<'a>(
    expression: &Expr,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
) -> Option<&'static str> {
    let (type_oid, _) = infer_type_res(expression, &RowCols { row, catalog }).ok()?;
    Some(match coltype_of_oid(type_oid)? {
        ColType::Array(elem) => elem.typeof_name(),
        other => other.name(),
    })
}

/// The exact static OID reported by `pg_typeof`, including pseudo and catalog
/// types which do not have a standalone [`ColType`] representation.
pub fn typeof_static_oid<'a>(
    expression: &Expr,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
) -> Option<i32> {
    infer_type_res(expression, &RowCols { row, catalog })
        .ok()
        .map(|(type_oid, _)| type_oid)
}

/// Whether two concrete types have a comparison operator, per PostgreSQL:
/// same type, both numeric-tower, or both in the date/time family.
/// Whether an OID names a range type (so range operators apply).
fn is_range_oid(oid: i32) -> bool {
    matches!(coltype_of_oid(oid), Some(ColType::Range(_)))
}

fn is_multirange_oid(oid: i32) -> bool {
    matches!(coltype_of_oid(oid), Some(ColType::Multirange(_)))
}

fn is_network_oid(oid: i32) -> bool {
    matches!(
        oid,
        crate::sql::types::oid::INET | crate::sql::types::oid::CIDR
    )
}

fn comparable(a: ColType, b: ColType) -> bool {
    use ColType::*;
    // `json` has no equality operator in PostgreSQL — two documents that differ
    // only in whitespace or key order are the same value but not the same text,
    // so it declines to say. `jsonb`, which is canonicalized, does compare.
    if matches!(a, Json) || matches!(b, Json) {
        return false;
    }
    if a == b {
        return true;
    }
    if matches!(a, ColType::Record | ColType::Composite(_))
        && matches!(b, ColType::Record | ColType::Composite(_))
    {
        return true;
    }
    let numeric = |t: ColType| matches!(t, Int2 | Int4 | Oid | Int8 | Numeric | Float8 | Float4);
    let datetime = |t: ColType| matches!(t, Date | Timestamp | Timestamptz);
    let timeofday = |t: ColType| matches!(t, Time | Timetz);
    let bit = |t: ColType| matches!(t, Bit { .. });
    let stringy = |t: ColType| matches!(t, Text | Varchar | Bpchar | Name);
    let catalog_object = |t: ColType| t.is_reg_object();
    let oid_integer = |t: ColType| matches!(t, Int2 | Int4 | Oid | Int8);
    (numeric(a) && numeric(b))
        || (catalog_object(a) && oid_integer(b))
        || (oid_integer(a) && catalog_object(b))
        || (datetime(a) && datetime(b))
        || (timeofday(a) && timeofday(b))
        || (bit(a) && bit(b))
        || (stringy(a) && stringy(b))
}

fn operator_undefined(l: ColType, operator: &str, r: ColType) -> SqlError {
    use core::fmt::Write;
    let mut left = crate::util::StackStr::<64>::new();
    let mut right = crate::util::StackStr::<64>::new();
    for (ctype, output) in [(l, &mut left), (r, &mut right)] {
        match ctype {
            ColType::Array(element) => {
                let _ = write!(output, "{}[]", element.to_coltype().name());
            }
            _ => {
                let _ = output.write_str(ctype.name());
            }
        }
    }
    sql_err!(
        sqlstate::UNDEFINED_FUNCTION,
        "operator does not exist: {} {} {}",
        left.as_str(),
        operator,
        right.as_str()
    )
}

pub fn infer_type_pub(expression: &Expr, def: Option<&TableDef>) -> Result<(i32, i16), SqlError> {
    match def {
        Some(d) => infer_type_res(expression, &DefCols(d)),
        None => infer_type_res(expression, &NoCols),
    }
}

pub(crate) fn infer_type_catalog(
    expression: &Expr,
    definition: Option<&TableDef>,
    storage: &crate::storage::Storage,
    txid: u32,
) -> Result<(i32, i16), SqlError> {
    infer_type_res(
        expression,
        &CatalogCols {
            definition,
            alias: None,
            storage,
            txid,
        },
    )
}

/// Static type inference with operator/aggregate validation, matching
/// PostgreSQL's plan-time analysis: comparisons and arithmetic over
/// incompatible types raise 42883 here, before any row is scanned. String
/// literals and parameters are UNKNOWN and coerce to the other operand.
pub(crate) fn infer_routine_argument_oid(
    expression: &Expr,
    columns: &dyn ColTypeResolver,
) -> Result<i32, SqlError> {
    match expression {
        Expr::Collate { operand, .. } => infer_routine_argument_oid(operand, columns),
        Expr::Column { qualifier, name }
        | Expr::RoutineParam {
            qualifier, name, ..
        } => match columns.column_meta(*qualifier, name) {
            Some(meta) => Ok(meta.type_oid),
            None => Ok(infer_type_res(expression, columns)?.0),
        },
        Expr::SchemaColumn {
            schema,
            table,
            name,
        } => {
            let mut composed = crate::util::StackStr::<130>::new();
            let _ = core::fmt::Write::write_fmt(&mut composed, format_args!("{schema}.{table}"));
            match columns.column_meta(Some(composed.as_str()), name) {
                Some(meta) => Ok(meta.type_oid),
                None => Ok(infer_type_res(expression, columns)?.0),
            }
        }
        Expr::Cast { type_name, .. } => match columns.named_type_oid(type_name) {
            Some(type_oid) => Ok(type_oid),
            None => Ok(infer_type_res(expression, columns)?.0),
        },
        _ => Ok(infer_type_res(expression, columns)?.0),
    }
}

pub(crate) fn routine_result_metadata(
    expression: &Expr,
    columns: &dyn ColTypeResolver,
) -> Option<StaticTypeMeta> {
    let Expr::Call {
        name,
        args,
        order_by,
        ..
    } = expression
    else {
        return None;
    };
    let mut argument_type_oids = [oid::UNKNOWN; MAX_ROUTINE_ARGUMENTS];
    if args.len() > argument_type_oids.len() {
        return None;
    }
    for (index, argument) in args.iter().enumerate() {
        argument_type_oids[index] = infer_routine_argument_oid(argument, columns).ok()?;
    }
    if let Some(result) = columns.routine_result(name, &argument_type_oids[..args.len()]) {
        return Some(result);
    }
    if args.len() + order_by.len() > argument_type_oids.len() {
        return None;
    }
    for (index, ordering) in order_by.iter().enumerate() {
        argument_type_oids[args.len() + index] =
            infer_routine_argument_oid(ordering.expression, columns).ok()?;
    }
    columns.routine_result(name, &argument_type_oids[..args.len() + order_by.len()])
}

pub fn infer_type_res(
    expression: &Expr,
    columns: &dyn ColTypeResolver,
) -> Result<(i32, i16), SqlError> {
    let of = |t: ColType| (t.oid(), t.typlen());
    if let Some(result) = routine_result_metadata(expression, columns) {
        return Ok((result.type_oid, result.ctype.typlen()));
    }
    Ok(match expression {
        Expr::RecursiveState { ctype, .. } => of(*ctype),
        Expr::Null | Expr::Str(_) => (oid::UNKNOWN, -2),
        Expr::Param(index) => bound_parameter_type_oid(*index)
            .map(|oid| (oid, coltype_of_oid(oid).map_or(-1, ColType::typlen)))
            .or_else(|| routine_parameter_type(*index).map(of))
            .unwrap_or((oid::UNKNOWN, -2)),
        Expr::RoutineParam {
            qualifier,
            name,
            index,
        } => match columns.resolve(*qualifier, name) {
            Ok(column) => of(column),
            Err(error)
                if error.sqlstate == sqlstate::UNDEFINED_COLUMN
                    || error.sqlstate == sqlstate::UNDEFINED_TABLE =>
            {
                routine_parameter_type(*index)
                    .map(of)
                    .unwrap_or((oid::UNKNOWN, -2))
            }
            Err(error) => return Err(error),
        },
        // A whole-row reference is an anonymous record — unless it is a function
        // scan's whole row, which is its single scalar column.
        Expr::WholeRow(t) => match columns.whole_row_scalar_type(t) {
            Some(ty) => of(ty),
            None => (oid::RECORD, -1),
        },
        Expr::SchemaColumn {
            schema,
            table,
            name,
        } => {
            // Composed-qualifier resolution, as the evaluator binds it: only
            // an unaliased base table of that schema answers.
            let mut composed = crate::util::StackStr::<130>::new();
            let _ = core::fmt::Write::write_fmt(&mut composed, format_args!("{schema}.{table}"));
            of(columns.resolve(Some(composed.as_str()), name)?)
        }
        Expr::BitLit(_) => (oid::BIT, -1),
        Expr::Bool(_) => of(ColType::Bool),
        Expr::Int(v) => {
            if i32::try_from(*v).is_ok() {
                of(ColType::Int4)
            } else {
                of(ColType::Int8)
            }
        }
        Expr::Float(_) => of(ColType::Float8),
        Expr::NumericLit(_) => of(ColType::Numeric),
        Expr::Column { qualifier, name } => match columns.resolve(*qualifier, name) {
            Ok(t) => of(t),
            // A bare name that is not a column but names a FROM item is a
            // whole-row/record value — except a function scan's whole row,
            // which is its single scalar column.
            Err(e) if qualifier.is_none() && columns.is_whole_row(name) => {
                let _ = e;
                match columns.whole_row_scalar_type(name) {
                    Some(t) => of(t),
                    None => (oid::RECORD, -1),
                }
            }
            Err(e) => return Err(e),
        },
        Expr::Unary { operator, operand } => match operator {
            crate::sql::ast::UnaryOp::Not => of(ColType::Bool),
            crate::sql::ast::UnaryOp::Neg | crate::sql::ast::UnaryOp::BitNot => {
                infer_type_res(operand, columns)?
            }
            crate::sql::ast::UnaryOp::SquareRoot | crate::sql::ast::UnaryOp::CubeRoot => {
                of(ColType::Float8)
            }
            crate::sql::ast::UnaryOp::AbsoluteValue => infer_type_res(operand, columns)?,
        },
        Expr::Binary {
            operator,
            left,
            right,
        } => {
            use crate::sql::ast::BinaryOp::*;
            let lo = infer_type_res(left, columns)?.0;
            let ro = infer_type_res(right, columns)?.0;
            let is_bit = |o: i32| matches!(o, oid::BIT | oid::VARBIT);
            match operator {
                Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                    // Unknown coerces; two concrete types must be comparable.
                    if lo != oid::UNKNOWN
                        && ro != oid::UNKNOWN
                        && let (Some(a), Some(b)) = (coltype_of_oid(lo), coltype_of_oid(ro))
                        && !comparable(a, b)
                    {
                        let sym = match operator {
                            Eq => "=",
                            NotEq => "<>",
                            Lt => "<",
                            LtEq => "<=",
                            Gt => ">",
                            _ => ">=",
                        };
                        return Err(operator_undefined(a, sym, b));
                    }
                    of(ColType::Bool)
                }
                And | Or | Like | ILike => of(ColType::Bool),
                Contains | ContainedBy | Overlaps | NotRightOf | NotLeftOf | Adjacent => {
                    of(ColType::Bool)
                }
                // Network containment predicates.
                NetContainedEq | NetContainsEq => of(ColType::Bool),
                Shl | Shr if is_network_oid(lo) || is_network_oid(ro) => of(ColType::Bool),
                // `inet & inet` / `inet | inet` return inet.
                BitAnd | BitOr if is_network_oid(lo) || is_network_oid(ro) => (oid::INET, -1),
                // `inet - inet` is int8; `inet ± integer` is inet.
                Sub if is_network_oid(lo) && is_network_oid(ro) => of(ColType::Int8),
                Add | Sub if is_network_oid(lo) || is_network_oid(ro) => (oid::INET, -1),
                // Multirange set operators (`+`/`-`/`*`) return a multirange of
                // the same subtype.
                Add | Sub | Mul if is_multirange_oid(lo) || is_multirange_oid(ro) => {
                    (if is_multirange_oid(lo) { lo } else { ro }, -1)
                }
                // Range set operators (`+`/`-`/`*` on ranges) return a range of
                // the same type; shifts on ranges (`<<`/`>>`) return boolean.
                Add | Sub | Mul if is_range_oid(lo) || is_range_oid(ro) => {
                    (if is_range_oid(lo) { lo } else { ro }, -1)
                }
                Shl | Shr if is_range_oid(lo) || is_range_oid(ro) => of(ColType::Bool),
                // `jsonb - key/keys/index` deletes and returns jsonb.
                Sub if lo == oid::JSONB => (oid::JSONB, -1),
                // `||` concatenates arrays when either side is an array (the
                // array type is preserved), otherwise it is text concatenation.
                Concat if coltype_of_oid(lo).is_some_and(|t| matches!(t, ColType::Array(_))) => {
                    (lo, -1)
                }
                Concat if coltype_of_oid(ro).is_some_and(|t| matches!(t, ColType::Array(_))) => {
                    (ro, -1)
                }
                // `^` stays numeric when an operand is numeric (and none is a
                // float); otherwise it is double precision.
                Pow => {
                    if (lo == oid::NUMERIC || ro == oid::NUMERIC)
                        && lo != oid::FLOAT8
                        && ro != oid::FLOAT8
                        && lo != oid::FLOAT4
                        && ro != oid::FLOAT4
                    {
                        of(ColType::Numeric)
                    } else {
                        of(ColType::Float8)
                    }
                }
                // Bit-string concatenation yields varbit; otherwise text.
                Concat => {
                    if lo == oid::JSONB || ro == oid::JSONB {
                        (oid::JSONB, -1)
                    } else if is_bit(lo) || is_bit(ro) {
                        (oid::VARBIT, -1)
                    } else {
                        (oid::TEXT, -1)
                    }
                }
                // `json -> k` keeps the json/jsonb type; `->>` yields text.
                JsonGet | JsonPath => (
                    if lo == oid::JSONB {
                        oid::JSONB
                    } else {
                        oid::JSON
                    },
                    -1,
                ),
                JsonGetText | JsonPathText => (oid::TEXT, -1),
                JsonDeletePath => (oid::JSONB, -1),
                JsonExists | JsonExistsAny | JsonExistsAll => of(ColType::Bool),
                // On bit strings the bitwise/shift operators return a bit
                // string; on integers they keep the wider integer width.
                BitAnd | BitOr | BitXor | Shl | Shr => {
                    if is_bit(lo) || is_bit(ro) {
                        (
                            if lo == oid::VARBIT || ro == oid::VARBIT {
                                oid::VARBIT
                            } else {
                                oid::BIT
                            },
                            -1,
                        )
                    } else if matches!(operator, Shl | Shr) {
                        // A shift keeps its left operand's type.
                        match lo {
                            oid::INT2 => of(ColType::Int2),
                            oid::INT8 => of(ColType::Int8),
                            _ => of(ColType::Int4),
                        }
                    } else if lo == oid::INT8 || ro == oid::INT8 {
                        of(ColType::Int8)
                    } else if lo == oid::INT2 && ro == oid::INT2 {
                        of(ColType::Int2)
                    } else {
                        of(ColType::Int4)
                    }
                }
                Add | Sub | Mul | Div | Mod => {
                    let numeric = |o: i32| {
                        matches!(
                            o,
                            oid::INT2
                                | oid::INT4
                                | oid::INT8
                                | oid::NUMERIC
                                | oid::FLOAT4
                                | oid::FLOAT8
                        )
                    };
                    let int_like =
                        |o: i32| matches!(o, oid::INT2 | oid::INT4 | oid::INT8 | oid::UNKNOWN);
                    // Date arithmetic: date - date -> int4; date +/- int -> date;
                    // int + date -> date.
                    if lo == oid::DATE && ro == oid::DATE && matches!(operator, Sub) {
                        return Ok(of(ColType::Int4));
                    }
                    // timestamp - timestamp -> interval.
                    if matches!(operator, Sub)
                        && (lo == oid::TIMESTAMP && ro == oid::TIMESTAMP
                            || lo == oid::TIMESTAMPTZ && ro == oid::TIMESTAMPTZ)
                    {
                        return Ok(of(ColType::Interval));
                    }
                    if lo == oid::DATE && matches!(operator, Add | Sub) && int_like(ro) {
                        return Ok(of(ColType::Date));
                    }
                    if ro == oid::DATE && matches!(operator, Add) && int_like(lo) {
                        return Ok(of(ColType::Date));
                    }
                    // Interval arithmetic: date/timestamp ± interval -> the
                    // timestamp type; interval ± interval -> interval.
                    let is_dt = |o: i32| matches!(o, oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ);
                    if matches!(operator, Add | Sub) {
                        if lo == oid::INTERVAL && ro == oid::INTERVAL {
                            return Ok(of(ColType::Interval));
                        }
                        if is_dt(lo) && ro == oid::INTERVAL {
                            return Ok(of(if lo == oid::TIMESTAMPTZ {
                                ColType::Timestamptz
                            } else {
                                ColType::Timestamp
                            }));
                        }
                        if matches!(operator, Add) && lo == oid::INTERVAL && is_dt(ro) {
                            return Ok(of(if ro == oid::TIMESTAMPTZ {
                                ColType::Timestamptz
                            } else {
                                ColType::Timestamp
                            }));
                        }
                        // A time of day keeps its own type, and its zone; the
                        // result wraps within the day.
                        let time_of_day = |o: i32| matches!(o, oid::TIME | oid::TIMETZ);
                        if time_of_day(lo) && ro == oid::INTERVAL {
                            return Ok(of(if lo == oid::TIMETZ {
                                ColType::Timetz
                            } else {
                                ColType::Time
                            }));
                        }
                        if matches!(operator, Add) && lo == oid::INTERVAL && time_of_day(ro) {
                            return Ok(of(if ro == oid::TIMETZ {
                                ColType::Timetz
                            } else {
                                ColType::Time
                            }));
                        }
                    }
                    // interval * number / number * interval / interval / number.
                    if (matches!(operator, Mul) && lo == oid::INTERVAL && numeric(ro))
                        || (matches!(operator, Mul) && numeric(lo) && ro == oid::INTERVAL)
                        || (matches!(operator, Div) && lo == oid::INTERVAL && numeric(ro))
                    {
                        return Ok(of(ColType::Interval));
                    }
                    let l_ok = lo == oid::UNKNOWN || numeric(lo);
                    let r_ok = ro == oid::UNKNOWN || numeric(ro);
                    // PostgreSQL defines no modulo for the floating types, so
                    // `real % …` and `float8 % …` are undefined even between two
                    // otherwise-numeric operands.
                    let is_float = |o: i32| matches!(o, oid::FLOAT4 | oid::FLOAT8);
                    let mod_on_float = matches!(operator, Mod) && (is_float(lo) || is_float(ro));
                    if (!l_ok || !r_ok || mod_on_float)
                        && let (Some(a), Some(b)) = (coltype_of_oid(lo), coltype_of_oid(ro))
                    {
                        let sym = match operator {
                            Add => "+",
                            Sub => "-",
                            Mul => "*",
                            Div => "/",
                            _ => "%",
                        };
                        return Err(operator_undefined(a, sym, b));
                    }
                    // Promotion: float8 > real > numeric > int8 > int4; unknown
                    // is absorbed by the concrete side. real op real stays real;
                    // real mixed with int/numeric widens to double precision.
                    if lo == oid::FLOAT8 || ro == oid::FLOAT8 {
                        of(ColType::Float8)
                    } else if lo == oid::FLOAT4 && ro == oid::FLOAT4 {
                        of(ColType::Float4)
                    } else if lo == oid::FLOAT4 || ro == oid::FLOAT4 {
                        of(ColType::Float8)
                    } else if lo == oid::NUMERIC || ro == oid::NUMERIC {
                        of(ColType::Numeric)
                    } else if lo == oid::INT8 || ro == oid::INT8 {
                        of(ColType::Int8)
                    } else if lo == oid::UNKNOWN && ro == oid::UNKNOWN {
                        of(ColType::Numeric)
                    } else if lo == oid::UNKNOWN {
                        (ro, coltype_of_oid(ro).map(|t| t.typlen()).unwrap_or(-1))
                    } else if ro == oid::UNKNOWN {
                        (lo, coltype_of_oid(lo).map(|t| t.typlen()).unwrap_or(-1))
                    } else if lo == oid::INT2 && ro == oid::INT2 {
                        // smallint op smallint stays smallint.
                        of(ColType::Int2)
                    } else {
                        of(ColType::Int4)
                    }
                }
            }
        }
        Expr::Collate { operand, .. } => infer_type_res(operand, columns)?,
        Expr::Cast {
            operand: _,
            type_name,
            ..
        } => {
            if let Some(type_oid) = columns.named_type_oid(type_name) {
                return Ok((
                    type_oid,
                    coltype_of_oid(type_oid).map_or(-1, ColType::typlen),
                ));
            }
            match ColType::from_sql_name(type_name) {
                Some(t) => of(t),
                // A resolver without a catalog cannot identify a user type.
                None => (oid::UNKNOWN, -2),
            }
        }
        Expr::IsNull { .. } => of(ColType::Bool),
        Expr::InList { .. } | Expr::Between { .. } | Expr::Like { .. } | Expr::Match { .. } => {
            of(ColType::Bool)
        }
        Expr::Case {
            whens, otherwise, ..
        } => {
            let mut acc: Option<ColType> = None;
            let mut consider = |e: &Expr| -> Result<(), SqlError> {
                let (o, _) = infer_type_res(e, columns)?;
                if let Some(t) = coltype_of_oid(o) {
                    acc = Some(match acc {
                        None => t,
                        Some(prev) => unify_numeric_tower(prev, t),
                    });
                }
                Ok(())
            };
            for (_, result) in whens.iter() {
                consider(result)?;
            }
            if let Some(e) = otherwise {
                consider(e)?;
            }
            match acc {
                Some(t) => of(t),
                None => (oid::UNKNOWN, -2),
            }
        }
        Expr::DefaultMarker => (oid::UNKNOWN, -2),
        // A scalar subquery's type is not known at static-inference time (its
        // body is resolved against storage only at execution); an array-from-
        // subquery is likewise unknown here. Both carry their real type in the
        // pre-evaluated datum.
        Expr::Subquery(_) | Expr::ArraySubquery(_) => (oid::UNKNOWN, -2),
        // `x IN (subquery)` and EXISTS are predicates: their result is boolean.
        Expr::InSubquery { .. } | Expr::QuantifiedSubquery { .. } | Expr::Exists(_) => {
            of(ColType::Bool)
        }
        Expr::AnyAll { .. } => of(ColType::Bool),
        Expr::Array(items) => {
            // An unknown-typed element (a bare string literal) makes the array
            // text[], as PostgreSQL coerces it; only a concrete element type
            // narrows it further.
            let element = items
                .first()
                .and_then(|e| infer_type_res(e, columns).ok())
                .and_then(|(o, _)| coltype_of_oid(o))
                .and_then(|ctype| match ctype {
                    ColType::Array(element) => Some(element),
                    scalar => crate::sql::types::ArrElem::from_coltype(scalar),
                })
                .unwrap_or(crate::sql::types::ArrElem::Text);
            of(ColType::Array(element))
        }
        Expr::Subscript { base, .. } => {
            // A catalog-backed column retains its array element identity here:
            // a domain element has a structural base `ColType`, but its Result
            // OID and `pg_typeof` identity are the domain itself.
            let direct_element = match &**base {
                Expr::Column { qualifier, name } => columns
                    .resolve(*qualifier, name)
                    .ok()
                    .and_then(|ctype| match ctype {
                        ColType::Array(element) => Some(element),
                        _ => None,
                    }),
                Expr::Field { base, field } => record_field_type(base, field, columns)
                    .ok()
                    .and_then(|ctype| match ctype {
                        ColType::Array(element) => Some(element),
                        _ => None,
                    }),
                _ => None,
            };
            if let Some(element) = direct_element {
                let ctype = element.to_coltype();
                (element.element_oid(), ctype.typlen())
            } else {
                match coltype_of_oid(infer_type_res(base, columns)?.0) {
                    Some(ColType::Array(e)) => of(e.to_coltype()),
                    Some(ColType::Name) => of(ColType::Bpchar),
                    Some(ctype) if matches!(base, Expr::Subscript { .. }) => of(ctype),
                    _ => (oid::UNKNOWN, -2),
                }
            }
        }
        // An array slice keeps the array type (unlike a subscript, which yields
        // the element type).
        Expr::Slice { base, .. } => infer_type_res(base, columns)?,
        // `(record).field`: the field's type from the record's shape.
        Expr::Field { base, field } => match record_field_metadata(base, field, columns) {
            Ok(meta) => (meta.type_oid, meta.ctype.typlen()),
            // The subquery executor preserves the element's named-composite
            // identity, but this catalog-free inference boundary cannot
            // resolve an inner FROM item. Query description refines this
            // explicit unresolved type before exposing it to a client.
            Err(e)
                if e.sqlstate == "42703"
                    && matches!(
                        &**base,
                        Expr::Subscript {
                            base: array,
                            ..
                        } if matches!(&**array, Expr::ArraySubquery(_))
                    ) =>
            {
                (oid::UNKNOWN, -2)
            }
            Err(e) => return Err(e),
        },
        Expr::Call {
            name,
            args,
            order_by,
            ..
        } => match *name {
            // Catalog-introspection helpers (for psql \d).
            "pg_get_userbyid"
            | "format_type"
            | "pg_get_expr"
            | "pg_get_indexdef"
            | "pg_get_constraintdef"
            | "pg_get_partkeydef"
            | "pg_get_functiondef"
            | "pg_get_triggerdef"
            | "pg_get_function_arguments"
            | "pg_get_function_identity_arguments"
            | "pg_get_function_result"
            | "pg_get_function_sqlbody"
            | "pg_get_viewdef"
            | "col_description"
            | "obj_description"
            | "shobj_description"
            | "pg_encoding_to_char"
            | "array_to_string"
            | "pg_get_statisticsobjdef"
            | "pg_get_statisticsobjdef_columns" => (oid::TEXT, -1),
            "pg_get_statisticsobjdef_expressions" => {
                (crate::sql::types::ArrElem::Text.array_oid(), -1)
            }
            "pg_typeof" => (oid::REGTYPE, 4),
            "pg_extension_config_dump" => (oid::VOID, 4),
            "version" | "getdatabaseencoding" | "pg_tablespace_location" => of(ColType::Text),
            "pg_table_is_visible"
            | "pg_type_is_visible"
            | "pg_function_is_visible"
            | "has_table_privilege"
            | "has_column_privilege"
            | "has_sequence_privilege"
            | "has_schema_privilege"
            | "has_type_privilege"
            | "has_database_privilege"
            | "pg_relation_is_publishable" => of(ColType::Bool),
            "array_length" | "cardinality" | "array_upper" | "array_lower" | "array_ndims" => {
                of(ColType::Int4)
            }
            // Network address functions.
            "family" | "masklen" => of(ColType::Int4),
            "host" | "abbrev" => of(ColType::Text),
            "broadcast" | "netmask" | "hostmask" | "set_masklen" => of(ColType::Inet),
            "network" | "inet_merge" => of(ColType::Cidr),
            "inet_same_family" => of(ColType::Bool),
            "macaddr8_set7bit" => of(ColType::Macaddr8),
            "array_dims" => of(ColType::Text),
            "current_schemas" => of(ColType::Array(crate::sql::types::ArrElem::Text)),
            "array_to_json" => of(ColType::Json),
            "jsonb_set" | "jsonb_set_lax" | "jsonb_insert" | "jsonb_strip_nulls" => {
                of(ColType::Jsonb)
            }
            "json_strip_nulls" => of(ColType::Json),
            "jsonb_pretty" => of(ColType::Text),
            "pg_char_to_encoding" => of(ColType::Int4),
            "pg_table_size" | "pg_database_size" | "pg_tablespace_size" => of(ColType::Int8),
            // Array-manipulation functions keep the array argument's type, but
            // promote its element type to hold a wider new/replacement element
            // (PostgreSQL's polymorphic anyarray/anyelement resolution).
            "array_append" => {
                let array_oid = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                let elem_oid = args
                    .get(1)
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                array_promoted(array_oid, elem_oid)
            }
            "array_prepend" => {
                let elem_oid = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                let array_oid = args
                    .get(1)
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                array_promoted(array_oid, elem_oid)
            }
            "array_replace" => {
                let array_oid = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                let to_oid = args
                    .get(2)
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                array_promoted(array_oid, to_oid)
            }
            "array_cat" => {
                let a_oid = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                let b_oid = args
                    .get(1)
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                // Element-type promotion across the two arrays.
                match (
                    a_oid.and_then(coltype_of_oid),
                    b_oid.and_then(coltype_of_oid),
                ) {
                    (Some(ColType::Array(ae)), Some(ColType::Array(be))) => {
                        let e = unify_numeric_tower(ae.to_coltype(), be.to_coltype());
                        of(ColType::Array(
                            crate::sql::types::ArrElem::from_coltype(e).unwrap_or(ae),
                        ))
                    }
                    _ => (a_oid.unwrap_or(oid::TEXT), -1),
                }
            }
            "array_remove" | "trim_array" => args
                .first()
                .map(|a| infer_type_res(a, columns))
                .transpose()?
                .unwrap_or((oid::TEXT, -1)),
            "pg_partition_ancestors" | "pg_partition_root" | "pg_partition_tree" => args
                .first()
                .map(|a| infer_type_res(a, columns))
                .transpose()?
                .unwrap_or((oid::INT4, 4)),
            // Window-only functions.
            "row_number" | "rank" | "dense_rank" | "ntile" => of(ColType::Int8),
            "percent_rank" | "cume_dist" => of(ColType::Float8),
            "lag" | "lead" | "first_value" | "last_value" | "nth_value" => args
                .first()
                .map(|a| infer_type_res(a, columns))
                .transpose()?
                .unwrap_or_else(|| of(ColType::Int8)),
            "count" => of(ColType::Int8),
            "row_to_json" | "to_json" | "json_build_object" | "json_build_array" => {
                of(ColType::Json)
            }
            "to_jsonb" | "jsonb_build_object" | "jsonb_build_array" => of(ColType::Jsonb),
            "row" => (oid::RECORD, -1),
            "sum" | "avg" => {
                let a = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                match a {
                    Some(oid::INT2 | oid::INT4) if *name == "sum" => of(ColType::Int8),
                    Some(oid::INT2 | oid::INT4 | oid::INT8 | oid::NUMERIC) => of(ColType::Numeric),
                    // sum(real) stays real; avg(real) widens to double precision.
                    Some(oid::FLOAT4) if *name == "sum" => of(ColType::Float4),
                    Some(oid::FLOAT4) => of(ColType::Float8),
                    Some(oid::FLOAT8) => of(ColType::Float8),
                    Some(oid::UNKNOWN) | None => of(ColType::Numeric),
                    Some(other) => return Err(agg_undefined(name, other)),
                }
            }
            "min" | "max" => {
                // PostgreSQL defines min/max only where a total order is part
                // of the type's contract: the numeric tower, strings, the
                // temporal types, bytea and arrays. It has none for boolean,
                // uuid, json or jsonb, bit strings, ranges or multiranges —
                // this engine can order most of those internally, but ordering
                // them is not the same as PostgreSQL offering the aggregate.
                let t = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?;
                if let Some((o, _)) = t {
                    let unordered = o == oid::BOOL
                        || o == oid::UUID
                        || matches!(
                            coltype_of_oid(o),
                            Some(
                                ColType::Json
                                    | ColType::Jsonb
                                    | ColType::Bit { .. }
                                    | ColType::Range(_)
                                    | ColType::Multirange(_)
                            )
                        );
                    if unordered {
                        return Err(agg_undefined(name, o));
                    }
                }
                t.unwrap_or_else(|| of(ColType::Int8))
            }
            // Functions returning the common type of their arguments (numeric
            // tower: float8 > numeric > int8 > int4), so a NULL of a wider type
            // still widens the result — matching PostgreSQL and the runtime
            // promotion in `greatest`/`least`.
            "greatest" | "least" => {
                let rank = |o: i32| {
                    if o == oid::FLOAT8 {
                        5
                    } else if o == oid::FLOAT4 {
                        4
                    } else if o == oid::NUMERIC {
                        3
                    } else if o == oid::INT8 {
                        2
                    } else if o == oid::INT2 || o == oid::INT4 {
                        1
                    } else {
                        0
                    }
                };
                let mut best: Option<(i32, i16)> = None;
                for a in args.iter() {
                    let t = infer_type_res(a, columns)?;
                    best = Some(match best {
                        None => t,
                        Some(p) => {
                            if rank(t.0) > rank(p.0) {
                                t
                            } else {
                                p
                            }
                        }
                    });
                }
                best.unwrap_or(of(ColType::Int8))
            }
            // `abs`/`nullif` take their first argument's type. `coalesce`
            // unifies across all of them, so an untyped NULL in front must not
            // decide the result: `coalesce(NULL, 1)` is integer, not text.
            "coalesce" | "abs" | "nullif" => {
                let mut chosen = None;
                for a in args.iter() {
                    let t = infer_type_res(a, columns)?;
                    if t.0 != oid::UNKNOWN {
                        chosen = Some(t);
                        break;
                    }
                    if !name.eq_ignore_ascii_case("coalesce") {
                        break;
                    }
                }
                match chosen {
                    Some(t) => t,
                    None if args.is_empty() => of(ColType::Int8),
                    // All arguments untyped: PostgreSQL resolves the unknown
                    // to text, exactly as it does for a bare literal.
                    None if name.eq_ignore_ascii_case("coalesce") => of(ColType::Text),
                    None => infer_type_res(args[0], columns)?,
                }
            }
            "length" | "char_length" | "character_length" | "octet_length" | "strpos"
            | "position" | "ascii" => of(ColType::Int4),
            // Math: sqrt/exp/ln/power stay numeric for a numeric argument (and
            // no float argument outranking it), else double; floor/ceil/trunc/
            // round/sign are numeric for a numeric argument and double
            // otherwise; mod returns the integer type of its arguments.
            "sqrt" | "exp" | "ln" | "power" | "pow" | "log" | "log10" => {
                let mut numeric = false;
                let mut float = false;
                for a in args.iter() {
                    match infer_type_res(a, columns)?.0 {
                        oid::NUMERIC => numeric = true,
                        oid::FLOAT8 | oid::FLOAT4 => float = true,
                        _ => {}
                    }
                }
                if numeric && !float {
                    of(ColType::Numeric)
                } else {
                    of(ColType::Float8)
                }
            }
            "div" | "trim_scale" | "to_number" => of(ColType::Numeric),
            "scale" | "min_scale" | "width_bucket" | "regexp_count" | "regexp_instr"
            | "array_position" | "jsonb_array_length" | "json_array_length" | "num_nonnulls"
            | "num_nulls" => of(ColType::Int4),
            "array_positions" => of(ColType::Array(crate::sql::types::ArrElem::Int4)),
            // array_fill returns an array of its value argument's element type.
            "array_fill" => {
                let elem = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .and_then(|(oid, _)| coltype_of_oid(oid))
                    .and_then(crate::sql::types::ArrElem::from_coltype)
                    .unwrap_or(crate::sql::types::ArrElem::Int4);
                of(ColType::Array(elem))
            }
            "jsonb_typeof"
            | "json_typeof"
            | "json_extract_path_text"
            | "jsonb_extract_path_text" => of(ColType::Text),
            "json_extract_path" => of(ColType::Json),
            "jsonb_extract_path" => of(ColType::Jsonb),
            "regexp_substr" => of(ColType::Text),
            "regexp_like" => of(ColType::Bool),
            "regexp_split_to_array" | "string_to_array" => {
                of(ColType::Array(crate::sql::types::ArrElem::Text))
            }
            "format" | "overlay" | "regexp_replace" => of(ColType::Text),
            "floor" | "ceil" | "ceiling" | "sign" => {
                let a = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                if a == Some(oid::NUMERIC) {
                    of(ColType::Numeric)
                } else {
                    of(ColType::Float8)
                }
            }
            "round" | "trunc" => {
                if args.len() == 2 {
                    of(ColType::Numeric)
                } else {
                    let a = args
                        .first()
                        .map(|a| infer_type_res(a, columns))
                        .transpose()?
                        .map(|t| t.0);
                    match a {
                        // trunc(macaddr)/trunc(macaddr8) keep their type.
                        Some(oid::MACADDR) if *name == "trunc" => of(ColType::Macaddr),
                        Some(oid::MACADDR8) if *name == "trunc" => of(ColType::Macaddr8),
                        Some(oid::NUMERIC) => of(ColType::Numeric),
                        _ => of(ColType::Float8),
                    }
                }
            }
            "mod" | "gcd" | "lcm" => {
                let a = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                let b = args
                    .get(1)
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                // `mod` keeps a numeric operand's type; gcd/lcm are integer-only.
                if *name == "mod" && (a == Some(oid::NUMERIC) || b == Some(oid::NUMERIC)) {
                    of(ColType::Numeric)
                } else if a == Some(oid::INT8) || b == Some(oid::INT8) {
                    of(ColType::Int8)
                } else {
                    of(ColType::Int4)
                }
            }
            "to_hex" | "md5" | "to_char" | "pg_size_pretty" => of(ColType::Text),
            "factorial" => of(ColType::Numeric),
            "bit_length" => of(ColType::Int4),
            "starts_with" => of(ColType::Bool),
            "cbrt" | "sin" | "cos" | "tan" | "cot" | "asin" | "acos" | "atan" | "atan2"
            | "sinh" | "cosh" | "tanh" | "asinh" | "acosh" | "atanh" | "degrees" | "radians"
            | "pi" => of(ColType::Float8),
            "bool_and" | "bool_or" | "every" => of(ColType::Bool),
            // Bitwise aggregates preserve the argument's (integer or bit) type.
            "bit_and" | "bit_or" | "bit_xor" => args
                .first()
                .map(|a| infer_type_res(a, columns))
                .transpose()?
                .unwrap_or(of(ColType::Int4)),
            // Single-argument variance/stddev mirror the input class: numeric for
            // integer/numeric inputs, double precision for float8 (PostgreSQL's
            // aggregate signatures).
            "var_pop" | "var_samp" | "variance" | "stddev_pop" | "stddev_samp" | "stddev" => {
                let a = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                match a {
                    Some(oid::FLOAT8) | Some(oid::FLOAT4) => of(ColType::Float8),
                    _ => of(ColType::Numeric),
                }
            }
            // The two-argument regression/covariance/correlation aggregates take
            // and return double precision; regr_count returns bigint.
            "corr" | "covar_pop" | "covar_samp" | "regr_slope" | "regr_intercept" | "regr_r2"
            | "regr_avgx" | "regr_avgy" | "regr_sxx" | "regr_syy" | "regr_sxy" => {
                of(ColType::Float8)
            }
            "regr_count" => of(ColType::Int8),
            "string_agg" => of(ColType::Text),
            "array_agg" => {
                // Element type from the argument; the result is elem[].
                let elem = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .and_then(|(oid, _)| coltype_of_oid(oid))
                    .and_then(crate::sql::types::ArrElem::from_coltype)
                    .unwrap_or(crate::sql::types::ArrElem::Int4);
                of(ColType::Array(elem))
            }
            // Ordered-set aggregates: percentile_cont yields double precision
            // (numeric for a numeric input); percentile_disc/mode yield the
            // WITHIN GROUP input type.
            "percentile_cont" | "percentile_disc" | "mode" => {
                let input = order_by
                    .first()
                    .map(|o| infer_type_res(o.expression, columns))
                    .transpose()?
                    .map(|t| t.0);
                match *name {
                    "percentile_cont" if input == Some(oid::NUMERIC) => of(ColType::Numeric),
                    "percentile_cont" => of(ColType::Float8),
                    _ => match input.and_then(coltype_of_oid) {
                        Some(t) => of(t),
                        None => (oid::UNKNOWN, -2),
                    },
                }
            }
            "extract" => of(ColType::Numeric),
            "date_part" => of(ColType::Float8),
            // Paren-less temporal functions carry a proper type so date/time
            // arithmetic (e.g. `current_date - 1`) type-checks correctly.
            "to_date" => of(ColType::Date),
            "to_timestamp" => of(ColType::Timestamptz),
            "generate_series" => {
                let start = args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .and_then(|(type_oid, _)| coltype_of_oid(type_oid));
                let has_numeric = args.iter().any(|argument| {
                    infer_type_res(argument, columns)
                        .ok()
                        .and_then(|(type_oid, _)| coltype_of_oid(type_oid))
                        == Some(ColType::Numeric)
                });
                let has_int8 = args.iter().any(|argument| {
                    infer_type_res(argument, columns)
                        .ok()
                        .and_then(|(type_oid, _)| coltype_of_oid(type_oid))
                        == Some(ColType::Int8)
                });
                of(crate::sql::eval::generate_series_result_type(
                    start,
                    has_numeric,
                    has_int8,
                ))
            }
            "unnest" => {
                // The element type of the array argument.
                match args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0)
                {
                    Some(o) => match coltype_of_oid(o) {
                        Some(ColType::Array(element)) => of(element.to_coltype()),
                        _ => of(ColType::Text),
                    },
                    None => of(ColType::Text),
                }
            }
            // regexp_matches returns each match's capture groups as text[].
            "regexp_matches" => of(ColType::Array(crate::sql::types::ArrElem::Text)),
            "regexp_split_to_table" | "string_to_table" => of(ColType::Text),
            "generate_subscripts" => of(ColType::Int4),
            "jsonb_object_keys"
            | "json_object_keys"
            | "jsonb_array_elements_text"
            | "json_array_elements_text" => of(ColType::Text),
            "jsonb_array_elements" => of(ColType::Jsonb),
            "json_array_elements" => of(ColType::Json),
            // The `each` family yields a `(key, value)` composite per member.
            "json_each"
            | "jsonb_each"
            | "json_each_text"
            | "jsonb_each_text"
            | "pg_options_to_table"
            | "pg_get_sequence_data"
            | "_pg_expandarray" => (oid::RECORD, -1),
            "grouping" => of(ColType::Int4),
            "make_date" => of(ColType::Date),
            "make_time" => of(ColType::Time),
            "make_timestamp" => of(ColType::Timestamp),
            "make_timestamptz" => of(ColType::Timestamptz),
            "isfinite" => of(ColType::Bool),
            // Encoding / hashing / bytea manipulation.
            "sha224" | "sha256" | "sha384" | "sha512" | "decode" | "set_byte" | "set_bit"
            | "convert_to" => of(ColType::Bytea),
            "encode" | "convert_from" | "quote_ident" | "quote_literal" | "quote_nullable" => {
                of(ColType::Text)
            }
            "get_byte" | "get_bit" => of(ColType::Int4),
            crate::sql::parser::OVERLAPS_PERIODS => of(ColType::Bool),
            "bit_count" => of(ColType::Int8),
            "parse_ident" => of(ColType::Array(crate::sql::types::ArrElem::Text)),
            // date_bin returns the type of its source timestamp (arg 1).
            "date_bin" => {
                let src = args
                    .get(1)
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                if src == Some(oid::TIMESTAMPTZ) {
                    of(ColType::Timestamptz)
                } else {
                    of(ColType::Timestamp)
                }
            }
            "age" | "justify_hours" | "justify_days" | "justify_interval" | "make_interval" => {
                of(ColType::Interval)
            }
            // timezone(zone, ts) == ts AT TIME ZONE zone: timestamptz <-> timestamp.
            "timezone" => {
                let arg = args
                    .get(1)
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                match arg {
                    Some(oid::TIMESTAMPTZ) => of(ColType::Timestamp),
                    _ => of(ColType::Timestamptz),
                }
            }
            "int4range" | "int8range" | "numrange" | "daterange" | "tsrange" | "tstzrange" => of(
                ColType::Range(crate::sql::types::RangeKind::from_name(name).expect("range name")),
            ),
            "int4multirange" | "int8multirange" | "nummultirange" | "datemultirange"
            | "tsmultirange" | "tstzmultirange" => of(ColType::Multirange(
                crate::sql::types::RangeKind::from_multirange_name(name).expect("multirange name"),
            )),
            "similar_to" | "isempty" | "lower_inc" | "upper_inc" | "lower_inf" | "upper_inf" => {
                of(ColType::Bool)
            }
            "range_merge" => {
                // Same range type as its arguments.
                match args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0)
                {
                    Some(o) if is_range_oid(o) => (o, -1),
                    _ => (oid::TEXT, -1),
                }
            }
            "lower" | "upper" => {
                // A range argument yields its element type; otherwise text.
                match args
                    .first()
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0)
                {
                    Some(o) => match coltype_of_oid(o) {
                        Some(ColType::Range(kind)) | Some(ColType::Multirange(kind)) => {
                            of(kind.elem_type())
                        }
                        _ => (oid::TEXT, -1),
                    },
                    None => (oid::TEXT, -1),
                }
            }
            "current_date" => of(ColType::Date),
            "pg_is_in_recovery" => of(ColType::Bool),
            // The identifier-returning functions are `name`-typed in PostgreSQL.
            "current_user" | "session_user" | "user" | "current_role" | "current_schema"
            | "current_database" | "current_catalog" => of(ColType::Name),
            // current_setting(name [, missing_ok]) returns the value as text.
            "current_setting" | "set_config" | "acldefault" => of(ColType::Text),
            "current_time" => of(ColType::Timetz),
            "localtime" => of(ColType::Time),
            "localtimestamp" => of(ColType::Timestamp),
            "now"
            | "current_timestamp"
            | "transaction_timestamp"
            | "statement_timestamp"
            | "clock_timestamp" => of(ColType::Timestamptz),
            // Sequence functions return bigint.
            "nextval" | "currval" | "lastval" | "setval" => of(ColType::Int8),
            "date_trunc" => {
                // Returns the timestamp type of its second argument.
                let a = args
                    .get(1)
                    .map(|a| infer_type_res(a, columns))
                    .transpose()?
                    .map(|t| t.0);
                if a == Some(oid::TIMESTAMPTZ) {
                    of(ColType::Timestamptz)
                } else {
                    of(ColType::Timestamp)
                }
            }
            // The remaining implemented functions (trim family, substr, replace,
            // repeat, reverse, left, right, concat[_ws], initcap, chr, ...) and
            // any not-yet-modeled function default to text.
            _ => (oid::TEXT, -1),
        },
    })
}
