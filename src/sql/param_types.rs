//! Shared SQL parameter inference for clients and durable typed constants.

use super::*;

type CapturedTypeResolver<'a> = &'a dyn Fn(&str) -> Option<i32>;

pub(crate) fn infer<'a>(
    storage: &'a Storage,
    statement: &'a Stmt<'a>,
    txid: u32,
    arena: &'a Arena,
    oids: &mut [i32],
) {
    infer_declared(storage, statement, txid, oids, None, arena);
    infer_resolved(storage, statement, txid, arena, oids);
}

pub(crate) fn infer_declared(
    storage: &Storage,
    statement: &Stmt<'_>,
    txid: u32,
    oids: &mut [i32],
    captured_types: Option<CapturedTypeResolver<'_>>,
    arena: &Arena,
) {
    ParameterInference {
        storage,
        captured_types,
        arena,
    }
    .infer_stmt_params(statement, txid, oids);
}

pub(crate) fn infer_resolved<'a>(
    storage: &'a Storage,
    statement: &'a Stmt<'a>,
    txid: u32,
    arena: &'a Arena,
    oids: &mut [i32],
) {
    ParameterInference {
        storage,
        captured_types: None,
        arena,
    }
    .infer_resolved_stmt_params(statement, txid, arena, oids);
}

struct ParameterInference<'a> {
    storage: &'a Storage,
    captured_types: Option<CapturedTypeResolver<'a>>,
    arena: &'a Arena,
}

impl ParameterInference<'_> {
    fn infer_resolved_stmt_params<'a>(
        &'a self,
        statement: &'a Stmt<'a>,
        txid: u32,
        arena: &'a Arena,
        oids: &mut [i32],
    ) {
        match statement {
            Stmt::Select(select) => self.infer_resolved_select_params(select, txid, arena, oids),
            Stmt::SetQuery(query) => {
                self.infer_resolved_set_tree_params(query.body, txid, arena, oids);
                for cte in query.with {
                    match cte.dml {
                        Some(dml) => self.infer_resolved_stmt_params(dml, txid, arena, oids),
                        None => self.infer_resolved_select_params(cte.query, txid, arena, oids),
                    }
                }
            }
            Stmt::With { ctes, statement } => {
                for cte in *ctes {
                    match cte.dml {
                        Some(dml) => self.infer_resolved_stmt_params(dml, txid, arena, oids),
                        None => self.infer_resolved_select_params(cte.query, txid, arena, oids),
                    }
                }
                self.infer_resolved_stmt_params(statement, txid, arena, oids);
            }
            Stmt::Insert(insert) => {
                if let Some(select) = insert.select {
                    self.infer_resolved_select_params(select, txid, arena, oids);
                }
                self.infer_dml_scope_params(statement, txid, arena, oids);
            }
            Stmt::Update(_) | Stmt::Delete(_) => {
                self.infer_dml_scope_params(statement, txid, arena, oids);
            }
            Stmt::Call {
                name,
                arguments,
                argument_names,
                variadic,
                ..
            } => {
                let mut actual = [types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
                if arguments.len() > actual.len() {
                    return;
                }
                for (index, argument) in arguments.iter().copied().enumerate() {
                    actual[index] =
                        exec::infer_type_catalog(argument, None, self.storage, txid, self.arena)
                            .map_or(types::oid::UNKNOWN, |inferred| inferred.0);
                }
                let qualified = match name.schema {
                    Some(schema) => stack_format!(260, "{}.{}", schema, name.name),
                    None => stack_format!(260, "{}", name.name),
                };
                if let Some(expected) = self.storage.procedure_call_parameter_oids(
                    qualified.as_str(),
                    argument_names,
                    *variadic,
                    &actual[..arguments.len()],
                    txid,
                ) {
                    for (argument, expected_oid) in arguments.iter().copied().zip(expected) {
                        if let Expr::Param(index) = argument
                            && expected_oid != types::oid::UNKNOWN
                            && *index >= 1
                            && (*index as usize) <= oids.len()
                        {
                            oids[*index as usize - 1] = expected_oid;
                        }
                    }
                }
            }
            Stmt::Explain { statement, .. } => {
                self.infer_resolved_stmt_params(statement, txid, arena, oids);
            }
            _ => {}
        }
    }

    fn infer_resolved_set_tree_params<'a>(
        &'a self,
        tree: &'a ast::SetTree<'a>,
        txid: u32,
        arena: &'a Arena,
        oids: &mut [i32],
    ) {
        match tree {
            ast::SetTree::Select(select) => {
                self.infer_resolved_select_params(select, txid, arena, oids)
            }
            ast::SetTree::Op { left, right, .. } => {
                self.infer_resolved_set_tree_params(left, txid, arena, oids);
                self.infer_resolved_set_tree_params(right, txid, arena, oids);
            }
        }
    }

    /// Parameters in a data-modifying statement's own expressions, typed
    /// against the target's columns as a query over the same scope would.
    fn infer_dml_scope_params<'a>(
        &'a self,
        statement: &'a Stmt<'a>,
        txid: u32,
        arena: &'a Arena,
        oids: &mut [i32],
    ) {
        if let Ok(Some(select)) = query::dml_parameter_scope_query(statement, arena) {
            self.infer_resolved_select_params(select, txid, arena, oids);
        }
    }

    fn infer_resolved_select_params<'a>(
        &'a self,
        select: &'a ast::Select<'a>,
        txid: u32,
        arena: &'a Arena,
        oids: &mut [i32],
    ) {
        if let Some(tree) = select.set_body {
            self.infer_resolved_set_tree_params(tree, txid, arena, oids);
        }
        for cte in select.with {
            match cte.dml {
                Some(dml) => self.infer_resolved_stmt_params(dml, txid, arena, oids),
                None => self.infer_resolved_select_params(cte.query, txid, arena, oids),
            }
        }

        let scope = match select.from.as_ref() {
            Some(from) => {
                match query::QueryScope::resolve_schema(self.storage, from, txid, arena, None) {
                    Ok(scope) => Some(scope),
                    Err(_) => return,
                }
            }
            None => None,
        };
        let resolver = scope.as_ref().map(|scope| query::CatalogScopeCols {
            scope,
            outer: None,
            storage: self.storage,
            txid,
            arena: self.arena,
        });
        let infer = |expression: &Expr<'a>| {
            let inferred = match &resolver {
                Some(resolver) => exec::infer_type_res(expression, resolver),
                None => exec::infer_type_catalog(expression, None, self.storage, txid, self.arena),
            }
            .ok()?
            .0;
            (inferred != types::oid::UNKNOWN).then_some(inferred)
        };

        let mut visit =
            |expression| self.infer_expression_params(expression, txid, arena, oids, &infer);
        for item in select.items {
            match item {
                ast::SelectItem::Expr { expression, .. }
                | ast::SelectItem::RecordStar(expression) => visit(expression),
                ast::SelectItem::Wildcard(_) | ast::SelectItem::TableWildcard(_) => {}
            }
        }
        if let Some(from) = select.from {
            for join in from.joins {
                if let Some(on) = join.on {
                    visit(on);
                }
            }
        }
        for expression in select
            .where_clause
            .into_iter()
            .chain(select.group_by.iter().copied())
            .chain(select.having)
            .chain(select.order_by.iter().map(|order| order.expression))
            .chain(select.limit)
            .chain(select.offset)
        {
            visit(expression);
        }
    }

    fn infer_expression_params<'a>(
        &'a self,
        expression: &'a Expr<'a>,
        txid: u32,
        arena: &'a Arena,
        oids: &mut [i32],
        infer: &impl Fn(&Expr<'a>) -> Option<i32>,
    ) {
        // `"any"` marks a parameter first reached, still untyped, by a
        // position imposing no type. PostgreSQL then rejects it even when a
        // later use would type it (`pg_typeof($1), $1 + 1`).
        let assign = |expression: &Expr, type_oid: i32, oids: &mut [i32]| {
            if let Expr::Param(index) = expression
                && *index >= 1
                && (*index as usize) <= oids.len()
            {
                // The first use decides: a parameter that first reaches a
                // pseudo-type position stays undetermined, as PostgreSQL's
                // left-to-right analysis leaves it.
                let slot = &mut oids[*index as usize - 1];
                if !accepts_any_type(*slot)
                    && (!accepts_any_type(type_oid) || *slot == types::oid::UNKNOWN)
                {
                    *slot = type_oid;
                }
            }
        };
        match expression {
            Expr::Call {
                name,
                args,
                argument_names,
                variadic,
                ..
            } => {
                let mut actual = [types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
                if args.len() <= actual.len() {
                    for (index, argument) in args.iter().copied().enumerate() {
                        actual[index] = infer(argument).unwrap_or(types::oid::UNKNOWN);
                    }
                    if let Some((schema, operator)) = crate::sql::ast::catalog_operator_call(name) {
                        if args.len() == 1 {
                            if let Some(expected) = self
                                .storage
                                .operator_prefix_parameter_oid(schema, operator, actual[0], txid)
                            {
                                assign(args[0], expected, oids);
                            }
                        } else if args.len() == 2
                            && let Some(expected) = self.storage.operator_call_parameter_oids(
                                schema,
                                operator,
                                [actual[0], actual[1]],
                                txid,
                            )
                        {
                            for (argument, expected_oid) in args.iter().copied().zip(expected) {
                                if expected_oid != types::oid::UNKNOWN {
                                    assign(argument, expected_oid, oids);
                                }
                            }
                        }
                    } else if let Some(expected) =
                        intrinsic_uuid_parameter_oids(name, argument_names, *variadic, args.len())
                            .or_else(|| {
                                self.storage.function_call_parameter_oids(
                                    name,
                                    argument_names,
                                    *variadic,
                                    &actual[..args.len()],
                                    txid,
                                )
                            })
                    {
                        for (argument, expected_oid) in args.iter().copied().zip(expected) {
                            if expected_oid != types::oid::UNKNOWN {
                                assign(argument, expected_oid, oids);
                            }
                        }
                    }
                }
            }
            Expr::Cast {
                operand, type_name, ..
            } => {
                let type_oid = self.cast_type_oid(type_name, txid);
                if let Some(type_oid) = type_oid {
                    assign(operand, type_oid, oids);
                }
            }
            Expr::BoundDefault {
                expression,
                type_oid,
            } => assign(expression, *type_oid, oids),
            Expr::Binary { left, right, .. } => {
                if let Some(type_oid) = infer(right) {
                    assign(left, type_oid, oids);
                }
                if let Some(type_oid) = infer(left) {
                    assign(right, type_oid, oids);
                }
            }
            Expr::InList { operand, list, .. } => {
                if let Some(type_oid) = infer(operand) {
                    for item in *list {
                        assign(item, type_oid, oids);
                    }
                } else if let Some(type_oid) = list.iter().find_map(|item| infer(item)) {
                    assign(operand, type_oid, oids);
                }
            }
            Expr::IsNull { operand, .. } => assign(operand, types::oid::ANY, oids),
            Expr::Like {
                operand, pattern, ..
            }
            | Expr::Match {
                operand, pattern, ..
            } => {
                assign(operand, types::oid::TEXT, oids);
                assign(pattern, types::oid::TEXT, oids);
                if let Expr::Like { escape, .. } = expression
                    && let Some(escape) = escape
                {
                    assign(escape, types::oid::TEXT, oids);
                }
            }
            Expr::Unary {
                operator: ast::UnaryOp::Not,
                operand,
                ..
            } => assign(operand, types::oid::BOOL, oids),
            Expr::Case { whens, .. } => {
                for (condition, _) in *whens {
                    assign(condition, types::oid::BOOL, oids);
                }
            }
            Expr::Subquery(select) | Expr::Exists(select) | Expr::ArraySubquery(select) => {
                self.infer_resolved_select_params(select, txid, arena, oids);
            }
            Expr::InSubquery { select, .. } | Expr::QuantifiedSubquery { select, .. } => {
                self.infer_resolved_select_params(select, txid, arena, oids);
            }
            _ => {}
        }
        query::walk_children(expression, &mut |child| {
            self.infer_expression_params(child, txid, arena, oids, infer);
            Ok(())
        })
        .expect("parameter expression walk cannot fail");
    }

    /// The OID of a named column of a visible table, if resolvable.
    fn parameter_type_oid(&self, table: &ast::QualName, col: &str, txid: u32) -> Option<i32> {
        let slot = match self
            .storage
            .resolve_relation(table.schema, table.name, txid)
        {
            Some(crate::storage::ResolvedRelation::Table(slot)) => slot,
            _ => return None,
        };
        let def = self.storage.table_def(slot, txid);
        let index = def.column_index(col)?;
        Some(
            self.storage
                .declared_column_type(&def.columns()[index], txid)
                .expect("table column declared type is catalog-validated")
                .parameter_oid()
                .raw(),
        )
    }

    fn infer_stmt_params(&self, statement: &Stmt, txid: u32, oids: &mut [i32]) {
        let set = |oids: &mut [i32], e: &Expr, ty: i32| {
            if let Expr::Param(n) = e
                && *n >= 1
                && (*n as usize) <= oids.len()
            {
                oids[*n as usize - 1] = ty;
            }
        };
        match statement {
            Stmt::Explain { statement, .. } => {
                self.infer_stmt_params(statement, txid, oids);
            }
            Stmt::With { ctes, statement } => {
                for cte in *ctes {
                    match cte.dml {
                        Some(dml) => self.infer_stmt_params(dml, txid, oids),
                        None => self.infer_stmt_params(&Stmt::Select(*cte.query), txid, oids),
                    }
                }
                self.infer_stmt_params(statement, txid, oids);
            }
            Stmt::Insert(ins) => {
                let slot =
                    match self
                        .storage
                        .resolve_relation(ins.table.schema, ins.table.name, txid)
                    {
                        Some(crate::storage::ResolvedRelation::Table(slot)) => Some(slot),
                        _ => None,
                    };
                let def = slot.map(|s| self.storage.table_def(s, txid));
                for row in ins.rows {
                    for (i, value) in row.iter().enumerate() {
                        let ty = def.as_ref().and_then(|d| {
                            let ci = if ins.columns.is_empty() {
                                (i < d.n_columns).then_some(i)
                            } else {
                                ins.columns.get(i).and_then(|c| d.column_index(c))
                            };
                            ci.map(|ci| {
                                self.storage
                                    .declared_column_type(&d.columns()[ci], txid)
                                    .expect("table column declared type is catalog-validated")
                                    .parameter_oid()
                                    .raw()
                            })
                        });
                        if let Some(ty) = ty {
                            set(oids, value, ty);
                        }
                    }
                }
                if let Some(select) = ins.select {
                    self.infer_select_source_params(select, txid, oids);
                }
                for (column, value) in ins
                    .on_conflict
                    .and_then(|conflict| conflict.update)
                    .unwrap_or(&[])
                {
                    if let Some(ty) = self.parameter_type_oid(&ins.table, column, txid) {
                        set(oids, value, ty);
                    }
                }
            }
            Stmt::Update(u) => {
                for (col, value) in u.assignments {
                    if let Some(ty) = self.parameter_type_oid(&u.table, col, txid) {
                        set(oids, value, ty);
                    }
                }
                if let Some(w) = u.where_clause {
                    self.infer_where_params(&u.table, w, txid, oids);
                }
                if let Some(from) = u.from {
                    self.infer_from_source_params(from, txid, oids);
                }
            }
            Stmt::Delete(d) => {
                if let Some(w) = d.where_clause {
                    self.infer_where_params(&d.table, w, txid, oids);
                }
                if let Some(using) = d.using {
                    self.infer_from_source_params(using, txid, oids);
                }
            }
            Stmt::Merge(merge) => {
                Self::infer_table_sample_params(&merge.source, oids);
            }
            Stmt::Select(s) => {
                self.infer_select_source_params(s, txid, oids);
                if self.captured_types.is_none()
                    && let (Some(from), Some(w)) = (&s.from, s.where_clause)
                    && from.joins.is_empty()
                    && from.base.subquery.is_none()
                {
                    let table = ast::QualName {
                        schema: from.base.schema,
                        name: from.base.table,
                    };
                    self.infer_where_params(&table, w, txid, oids);
                }
            }
            Stmt::SetQuery(query) => {
                self.infer_set_tree_source_params(query.body, txid, oids);
                for cte in query.with {
                    match cte.dml {
                        Some(dml) => self.infer_stmt_params(dml, txid, oids),
                        None => self.infer_stmt_params(&Stmt::Select(*cte.query), txid, oids),
                    }
                }
            }
            _ => {}
        }
    }

    fn infer_select_source_params(&self, select: &ast::Select<'_>, txid: u32, oids: &mut [i32]) {
        // Explicit casts type their own input independently of FROM schemas.
        for item in select.items {
            if let ast::SelectItem::Expr { expression, .. }
            | ast::SelectItem::RecordStar(expression) = item
            {
                self.infer_explicit_cast_params(expression, txid, oids);
            }
        }
        for expression in select
            .where_clause
            .into_iter()
            .chain(select.group_by.iter().copied())
            .chain(select.having)
            .chain(select.order_by.iter().map(|order| order.expression))
            .chain(select.limit)
            .chain(select.offset)
        {
            self.infer_explicit_cast_params(expression, txid, oids);
        }
        if let Some(from) = select.from {
            self.infer_from_source_params(&from, txid, oids);
        }
        if let Some(tree) = select.set_body {
            self.infer_set_tree_source_params(tree, txid, oids);
        }
        for cte in select.with {
            match cte.dml {
                Some(dml) => self.infer_stmt_params(dml, txid, oids),
                None => self.infer_stmt_params(&Stmt::Select(*cte.query), txid, oids),
            }
        }
    }

    fn infer_from_source_params(&self, from: &ast::FromClause<'_>, txid: u32, oids: &mut [i32]) {
        for table in core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table)) {
            Self::infer_table_sample_params(table, oids);
            if let Some(select) = table.subquery {
                self.infer_stmt_params(&Stmt::Select(*select), txid, oids);
            }
            for expression in table.func_args.unwrap_or(&[]) {
                self.infer_explicit_cast_params(expression, txid, oids);
            }
        }
        for expression in from.joins.iter().filter_map(|join| join.on) {
            self.infer_explicit_cast_params(expression, txid, oids);
        }
    }

    fn infer_table_sample_params(table: &ast::TableRef<'_>, oids: &mut [i32]) {
        let Some(sample) = table.sample else { return };
        let assign = |expression: &Expr, oid: i32, oids: &mut [i32]| {
            if let Expr::Param(parameter) = expression
                && *parameter >= 1
                && (*parameter as usize) <= oids.len()
            {
                oids[*parameter as usize - 1] = oid;
            }
        };
        assign(sample.percentage, types::ColType::Float4.oid(), oids);
        if let Some(repeatable) = sample.repeatable {
            assign(repeatable, types::ColType::Float8.oid(), oids);
        }
    }

    fn infer_set_tree_source_params(&self, tree: &ast::SetTree<'_>, txid: u32, oids: &mut [i32]) {
        match tree {
            ast::SetTree::Select(select) => self.infer_select_source_params(select, txid, oids),
            ast::SetTree::Op { left, right, .. } => {
                self.infer_set_tree_source_params(left, txid, oids);
                self.infer_set_tree_source_params(right, txid, oids);
            }
        }
    }

    fn cast_type_oid(&self, name: &str, txid: u32) -> Option<i32> {
        self.captured_types
            .and_then(|resolve| resolve(name))
            .or_else(|| types::ColType::from_sql_name(name).map(types::ColType::oid))
            .or_else(|| catalog::user_type_oid(self.storage, txid, name))
    }

    /// The innermost explicit cast selects a parameter's input type, including
    /// user-defined types whose declarations are still uncommitted.
    fn infer_explicit_cast_params(&self, expression: &Expr, txid: u32, oids: &mut [i32]) {
        if let Expr::Cast {
            operand, type_name, ..
        } = expression
            && let Expr::Param(index) = operand
            && *index >= 1
            && (*index as usize) <= oids.len()
            && let Some(type_oid) = self.cast_type_oid(type_name, txid)
        {
            oids[*index as usize - 1] = type_oid;
        }
        query::walk_children(expression, &mut |child| {
            self.infer_explicit_cast_params(child, txid, oids);
            Ok(())
        })
        .expect("explicit cast parameter walk cannot fail");
        if let Expr::Subquery(select)
        | Expr::Exists(select)
        | Expr::ArraySubquery(select)
        | Expr::InSubquery { select, .. }
        | Expr::QuantifiedSubquery { select, .. } = expression
        {
            self.infer_stmt_params(&Stmt::Select(**select), txid, oids);
        }
    }

    /// Walks a single-table predicate, typing a `Column OP $n` (or the mirror)
    /// parameter from the column's type.
    fn infer_where_params(
        &self,
        table: &ast::QualName,
        expression: &Expr,
        txid: u32,
        oids: &mut [i32],
    ) {
        use ast::BinaryOp::*;
        if let Expr::Binary {
            operator,
            left,
            right,
            ..
        } = expression
        {
            match operator {
                And | Or => {
                    self.infer_where_params(table, left, txid, oids);
                    self.infer_where_params(table, right, txid, oids);
                }
                Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                    let mut pair = |c: &Expr, p: &Expr| {
                        if let (Expr::Column { name, .. }, Expr::Param(n)) = (c, p)
                            && *n >= 1
                            && (*n as usize) <= oids.len()
                            && let Some(ty) = self.parameter_type_oid(table, name, txid)
                        {
                            oids[*n as usize - 1] = ty;
                        }
                    };
                    pair(left, right);
                    pair(right, left);
                }
                _ => {}
            }
        }
    }
}

/// Finishes inferred parameter types as PostgreSQL's parse analysis does: a
/// referenced parameter still `unknown` resolves to `text`, while one never
/// referenced, or only passed where any type is accepted (`pg_typeof($1)`),
/// has no type and fails with `42P18`.
/// https://github.com/postgres/postgres/blob/REL_18_6/src/backend/parser/parse_param.c#L270
pub(crate) fn finish_unresolved(
    text: &str,
    arena: &Arena,
    oids: &mut [i32],
) -> Result<(), SqlError> {
    let undetermined = |index: usize| {
        sql_err!(
            sqlstate::INDETERMINATE_DATATYPE,
            "could not determine data type of parameter ${}",
            index + 1
        )
    };
    for (index, oid) in oids.iter_mut().enumerate() {
        if accepts_any_type(*oid) {
            return Err(undetermined(index));
        }
        if *oid == types::oid::UNKNOWN {
            if !references_parameter(text, arena, index + 1) {
                return Err(undetermined(index));
            }
            *oid = types::oid::TEXT;
        }
    }
    Ok(())
}

/// Whether `oid` is a pseudo-type an argument position accepts without
/// determining the argument's own type.
fn accepts_any_type(oid: i32) -> bool {
    matches!(
        oid,
        types::oid::ANY
            | types::oid::ANYELEMENT
            | types::oid::ANYNONARRAY
            | types::oid::ANYENUM
            | types::oid::ANYRANGE
            | types::oid::ANYMULTIRANGE
            | types::oid::ANYARRAY
            | types::oid::ANYCOMPATIBLE
            | types::oid::ANYCOMPATIBLEARRAY
            | types::oid::ANYCOMPATIBLENONARRAY
            | types::oid::ANYCOMPATIBLERANGE
            | types::oid::ANYCOMPATIBLEMULTIRANGE
    )
}

fn references_parameter(text: &str, arena: &Arena, parameter: usize) -> bool {
    let mut lexer = crate::sql::lexer::Lexer::new(text, arena);
    loop {
        match lexer.next_token() {
            Ok(crate::sql::lexer::Tok::Param(found)) if found as usize == parameter => {
                return true;
            }
            Ok(crate::sql::lexer::Tok::Eof) | Err(_) => return false,
            Ok(_) => {}
        }
    }
}
