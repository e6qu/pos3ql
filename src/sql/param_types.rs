//! Shared SQL parameter inference for clients and durable typed constants.

use super::*;

pub(crate) fn infer<'a>(
    storage: &'a Storage,
    statement: &'a Stmt<'a>,
    txid: u32,
    arena: &'a Arena,
    oids: &mut [i32],
) {
    let inference = ParameterInference { storage };
    inference.infer_stmt_params(statement, txid, oids);
    inference.infer_resolved_stmt_params(statement, txid, arena, oids);
}

struct ParameterInference<'a> {
    storage: &'a Storage,
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
            }
            Stmt::Call {
                name,
                arguments,
                argument_names,
                variadic,
            } => {
                let mut actual = [types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
                if arguments.len() > actual.len() {
                    return;
                }
                for (index, argument) in arguments.iter().copied().enumerate() {
                    actual[index] = exec::infer_type_catalog(argument, None, self.storage, txid)
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
                match query::QueryScope::resolve_schema(self.storage, from, txid, arena) {
                    Ok(scope) => Some(scope),
                    Err(_) => return,
                }
            }
            None => None,
        };
        let resolver = scope.as_ref().map(|scope| query::CatalogScopeCols {
            scope,
            outer_scope: None,
            storage: self.storage,
            txid,
        });
        let infer = |expression: &Expr<'a>| {
            let inferred = match &resolver {
                Some(resolver) => exec::infer_type_res(expression, resolver),
                None => exec::infer_type_catalog(expression, None, self.storage, txid),
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
                ast::SelectItem::Wildcard | ast::SelectItem::TableWildcard(_) => {}
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
        let assign = |expression: &Expr, type_oid: i32, oids: &mut [i32]| {
            if let Expr::Param(index) = expression
                && *index >= 1
                && (*index as usize) <= oids.len()
            {
                oids[*index as usize - 1] = type_oid;
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
                let type_oid = types::ColType::from_sql_name(type_name)
                    .map(types::ColType::oid)
                    .or_else(|| catalog::user_type_oid(self.storage, txid, type_name));
                if let Some(type_oid) = type_oid {
                    assign(operand, type_oid, oids);
                }
            }
            Expr::Binary { left, right, .. } => {
                if let Some(type_oid) = infer(right) {
                    assign(left, type_oid, oids);
                }
                if let Some(type_oid) = infer(left) {
                    assign(right, type_oid, oids);
                }
            }
            Expr::Between {
                operand, low, high, ..
            } => {
                if let Some(type_oid) = infer(operand) {
                    assign(low, type_oid, oids);
                    assign(high, type_oid, oids);
                } else if let Some(type_oid) = infer(low).or_else(|| infer(high)) {
                    assign(operand, type_oid, oids);
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
                    Self::infer_from_source_params(from, oids);
                }
            }
            Stmt::Delete(d) => {
                if let Some(w) = d.where_clause {
                    self.infer_where_params(&d.table, w, txid, oids);
                }
                if let Some(using) = d.using {
                    Self::infer_from_source_params(using, oids);
                }
            }
            Stmt::Merge(merge) => {
                Self::infer_table_sample_params(&merge.source, oids);
            }
            Stmt::Select(s) => {
                self.infer_select_source_params(s, txid, oids);
                if let (Some(from), Some(w)) = (&s.from, s.where_clause)
                    && from.joins.is_empty()
                    && from.base.subquery.is_none()
                {
                    let table = ast::QualName {
                        schema: from.base.schema,
                        name: from.base.table,
                    };
                    self.infer_where_params(&table, w, txid, oids);
                }
                // A parameter explicitly cast in the select list — `$n::type`
                // — takes that type, as PostgreSQL resolves an otherwise-unknown
                // parameter from the cast wrapping it.
                for item in s.items {
                    if let ast::SelectItem::Expr { expression, .. } = item {
                        Self::infer_cast_param(expression, oids);
                    }
                }
            }
            Stmt::SetQuery(query) => {
                self.infer_set_tree_source_params(query.body, txid, oids);
                for cte in query.with {
                    match cte.dml {
                        Some(dml) => self.infer_stmt_params(dml, txid, oids),
                        None => self.infer_select_source_params(cte.query, txid, oids),
                    }
                }
            }
            _ => {}
        }
    }

    fn infer_select_source_params(&self, select: &ast::Select<'_>, txid: u32, oids: &mut [i32]) {
        if let Some(from) = select.from {
            Self::infer_from_source_params(&from, oids);
        }
        if let Some(tree) = select.set_body {
            self.infer_set_tree_source_params(tree, txid, oids);
        }
        for cte in select.with {
            match cte.dml {
                Some(dml) => self.infer_stmt_params(dml, txid, oids),
                None => self.infer_select_source_params(cte.query, txid, oids),
            }
        }
    }

    fn infer_from_source_params(from: &ast::FromClause<'_>, oids: &mut [i32]) {
        Self::infer_table_sample_params(&from.base, oids);
        for join in from.joins {
            Self::infer_table_sample_params(&join.table, oids);
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

    /// Types a parameter written as `$n::type` (possibly through further casts)
    /// by the innermost cast wrapping it, as PostgreSQL resolves an otherwise-
    /// unknown parameter from the cast.
    fn infer_cast_param(expr: &Expr, oids: &mut [i32]) {
        if let Expr::Cast {
            operand, type_name, ..
        } = expr
        {
            if let Expr::Param(n) = operand {
                if *n >= 1
                    && (*n as usize) <= oids.len()
                    && let Some(ct) = types::ColType::from_sql_name(type_name)
                {
                    oids[*n as usize - 1] = ct.oid();
                }
            } else {
                Self::infer_cast_param(operand, oids);
            }
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
