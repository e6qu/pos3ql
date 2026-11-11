//! Volatile routine calls in an `INSERT ... SELECT` output list.
//!
//! PostgreSQL computes a query's output list as the insert pulls each row,
//! so a volatile function there sees the rows inserted before it. The source
//! here is materialized before the first row change; an output item that
//! calls a volatile user routine is therefore split: the source keeps the
//! parts without such calls as hidden columns, and the row phase evaluates
//! the calls over them as each row is inserted, as for `VALUES` rows.

use super::*;
use crate::sql::ast::SetTree;

/// Names a hidden source column inside an item's call skeleton.
const HIDDEN: &str = "\u{1}deferred";

#[derive(Clone, Copy)]
struct HiddenColumn<'a> {
    type_oid: i32,
    type_mod: i32,
    collation: Option<crate::sql::ast::Collation>,
    source: &'a Expr<'a>,
}

#[derive(Clone, Copy)]
pub(crate) struct DeferredSource<'a> {
    /// The source with each deferred item replaced and hidden columns appended.
    pub(super) select: &'a Select<'a>,
    /// The output width of the written source.
    pub(super) width: usize,
    items: &'a [DeferredItem<'a>],
    /// Where each leaf's hidden columns start among all hidden columns.
    offsets: &'a [usize],
    hidden: &'a [HiddenColumn<'a>],
}

#[derive(Clone, Copy)]
struct DeferredItem<'a> {
    leaf: usize,
    item: usize,
    calls: &'a Expr<'a>,
}

fn volatile_user_call(
    expression: &Expr<'_>,
    storage: &Storage,
    txid: u32,
) -> Result<bool, SqlError> {
    if let Expr::Call {
        kind: crate::sql::ast::CallKind::Routine,
        name,
        ..
    } = expression
    {
        let name = name.rsplit('.').next().unwrap_or(name);
        if storage.has_volatile_routine_named(name, txid) {
            return Ok(true);
        }
    }
    let mut found = false;
    crate::sql::query::walk_children(expression, &mut |child| {
        found |= volatile_user_call(child, storage, txid)?;
        Ok(())
    })?;
    Ok(found)
}

fn alloc<'a>(arena: &'a Arena, expression: Expr<'a>) -> Result<&'a Expr<'a>, SqlError> {
    arena
        .alloc(expression)
        .map(|expression| &*expression)
        .map_err(|_| crate::sql::eval::arena_full())
}

/// The call skeleton of `expression`, or `None` where its calls cannot be
/// separated from the rest. A part PostgreSQL may skip (a later `CASE`
/// branch, the right side of `AND`, a `COALESCE` argument) is `conditional`:
/// only its column references are read ahead, so nothing the row would not
/// evaluate can fail early.
fn skeleton<'a>(
    expression: &'a Expr<'a>,
    conditional: bool,
    hidden: &mut ArenaList<'a, &'a Expr<'a>>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    if matches!(
        expression,
        Expr::Null
            | Expr::Bool(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::NumericLit(_)
            | Expr::Str(_)
            | Expr::BitLit(_)
            | Expr::BoundLiteral { .. }
            | Expr::Param(_)
            | Expr::RoutineParam { .. }
    ) {
        // Literals keep their unknown type for routine selection.
        return Ok(Some(expression));
    }
    if !volatile_user_call(expression, storage, txid)?
        && (!conditional || matches!(expression, Expr::Column { .. }))
    {
        let index = hidden.len();
        hidden
            .push(expression)
            .map_err(|_| crate::sql::eval::arena_full())?;
        let name = arena
            .alloc_str(stack_format!(16, "{}", index).as_str())
            .map_err(|_| crate::sql::eval::arena_full())?;
        return alloc(
            arena,
            Expr::Column {
                qualifier: Some(HIDDEN),
                name,
            },
        )
        .map(Some);
    }
    let mut part =
        |operand, conditional| skeleton(operand, conditional, hidden, storage, txid, arena);
    let rebuilt = match *expression {
        Expr::Call {
            kind,
            name,
            bound_oid,
            bound_strict,
            source_position,
            args,
            argument_names,
            variadic,
            star: false,
            distinct: false,
            order_by: [],
            over: None,
            filter: None,
        } => {
            // COALESCE, NULLIF, GREATEST and LEAST stop early.
            let lazy = conditional || kind == crate::sql::ast::CallKind::SqlSyntax;
            let mut arguments = ArenaList::new(arena);
            for argument in args {
                let Some(argument) = part(argument, lazy)? else {
                    return Ok(None);
                };
                arguments
                    .push(argument)
                    .map_err(|_| crate::sql::eval::arena_full())?;
            }
            Expr::Call {
                kind,
                name,
                bound_oid,
                bound_strict,
                source_position,
                args: arguments.as_slice(),
                argument_names,
                variadic,
                star: false,
                distinct: false,
                order_by: &[],
                over: None,
                filter: None,
            }
        }
        Expr::Binary {
            source_position,
            operator,
            left,
            right,
        } => {
            // AND and OR stop early.
            let lazy = conditional
                || matches!(
                    operator,
                    crate::sql::ast::BinaryOp::And | crate::sql::ast::BinaryOp::Or
                );
            let (Some(left), Some(right)) = (part(left, conditional)?, part(right, lazy)?) else {
                return Ok(None);
            };
            Expr::Binary {
                source_position,
                operator,
                left,
                right,
            }
        }
        Expr::Unary {
            source_position,
            operator,
            operand,
        } => {
            let Some(operand) = part(operand, conditional)? else {
                return Ok(None);
            };
            Expr::Unary {
                source_position,
                operator,
                operand,
            }
        }
        Expr::Cast {
            source_position,
            operand,
            type_name,
            type_mod,
            binding,
        } => {
            let Some(operand) = part(operand, conditional)? else {
                return Ok(None);
            };
            Expr::Cast {
                source_position,
                operand,
                type_name,
                type_mod,
                binding,
            }
        }
        Expr::Collate { operand, collation } => {
            let Some(operand) = part(operand, conditional)? else {
                return Ok(None);
            };
            Expr::Collate { operand, collation }
        }
        Expr::IsNull { operand, negated } => {
            let Some(operand) = part(operand, conditional)? else {
                return Ok(None);
            };
            Expr::IsNull { operand, negated }
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            origin,
        } => {
            // The operand and the first condition always run.
            let operand = match operand {
                Some(operand) => match part(operand, conditional)? {
                    Some(operand) => Some(operand),
                    None => return Ok(None),
                },
                None => None,
            };
            let mut branches = ArenaList::new(arena);
            for (index, (condition, result)) in whens.iter().enumerate() {
                let (Some(condition), Some(result)) = (
                    part(condition, conditional || index > 0)?,
                    part(result, true)?,
                ) else {
                    return Ok(None);
                };
                branches
                    .push((condition, result))
                    .map_err(|_| crate::sql::eval::arena_full())?;
            }
            let otherwise = match otherwise {
                Some(otherwise) => match part(otherwise, true)? {
                    Some(otherwise) => Some(otherwise),
                    None => return Ok(None),
                },
                None => None,
            };
            Expr::Case {
                operand,
                whens: branches.as_slice(),
                otherwise,
                origin,
            }
        }
        _ => return Ok(None),
    };
    alloc(arena, rebuilt).map(Some)
}

/// Whether the source orders, groups or deduplicates by output `index`,
/// which PostgreSQL then computes before the rows are inserted.
fn output_used_by_source(select: &Select<'_>, index: usize, item: &SelectItem<'_>) -> bool {
    let SelectItem::Expr { expression, alias } = item else {
        return true;
    };
    let names_item = |key: &Expr<'_>| match key {
        Expr::Int(position) => *position == index as i64 + 1,
        Expr::Column {
            qualifier: None,
            name,
        } => alias.is_some_and(|alias| alias == *name),
        key => key == *expression,
    };
    select.order_by.iter().any(|key| names_item(key.expression))
        || select.group_by.iter().any(|key| names_item(key))
}

/// One `SELECT` of the source: its written items with the deferred ones
/// replaced, and the hidden expressions its skeletons read.
#[derive(Clone, Copy)]
struct Leaf<'a> {
    select: &'a Select<'a>,
    items: &'a [SelectItem<'a>],
    hidden: &'a [&'a Expr<'a>],
}

fn split_leaf<'a>(
    leaf: usize,
    select: &'a Select<'a>,
    deferred: &mut ArenaList<'a, DeferredItem<'a>>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<Leaf<'a>>, SqlError> {
    if select.set_body.is_some()
        || select.distinct
        || !select.distinct_on.is_empty()
        || !select.grouping_sets.is_empty()
        || select
            .items
            .iter()
            .any(|item| !matches!(item, SelectItem::Expr { .. }))
    {
        return Ok(None);
    }
    let mut items = ArenaList::new(arena);
    let mut hidden = ArenaList::new(arena);
    for (index, item) in select.items.iter().enumerate() {
        let SelectItem::Expr { expression, alias } = item else {
            unreachable!("checked above");
        };
        let mut written = *item;
        if volatile_user_call(expression, storage, txid)? {
            if expression.contains_subquery() || output_used_by_source(select, index, item) {
                return Ok(None);
            }
            let Some(calls) = skeleton(expression, false, &mut hidden, storage, txid, arena)?
            else {
                return Ok(None);
            };
            deferred
                .push(DeferredItem {
                    leaf,
                    item: index,
                    calls,
                })
                .map_err(|_| crate::sql::eval::arena_full())?;
            written = SelectItem::Expr {
                expression: &Expr::Null,
                alias: *alias,
            };
        }
        items
            .push(written)
            .map_err(|_| crate::sql::eval::arena_full())?;
    }
    Ok(Some(Leaf {
        select,
        items: items.as_slice(),
        hidden: hidden.as_slice(),
    }))
}

/// The `UNION ALL` leaves of `tree` in output order, or `false` when another
/// set operation computes its rows before they are inserted.
fn union_all_leaves<'a>(
    tree: &'a SetTree<'a>,
    leaves: &mut ArenaList<'a, &'a Select<'a>>,
) -> Result<bool, SqlError> {
    match tree {
        SetTree::Select(select) => {
            leaves
                .push(select)
                .map_err(|_| crate::sql::eval::arena_full())?;
            Ok(true)
        }
        SetTree::Op {
            operator: crate::sql::ast::SetOp::Union,
            all: true,
            left,
            right,
            ..
        } => Ok(union_all_leaves(left, leaves)? && union_all_leaves(right, leaves)?),
        SetTree::Op { .. } => Ok(false),
    }
}

/// Rebuilds `tree` with each leaf replaced, in the order `union_all_leaves`
/// listed them.
fn replace_leaves<'a>(
    tree: &'a SetTree<'a>,
    leaves: &[&'a Select<'a>],
    next: &mut usize,
    arena: &'a Arena,
) -> Result<&'a SetTree<'a>, SqlError> {
    let rebuilt = match *tree {
        SetTree::Select(_) => {
            *next += 1;
            SetTree::Select(leaves[*next - 1])
        }
        SetTree::Op {
            syntax,
            operator,
            all,
            left,
            right,
        } => SetTree::Op {
            syntax,
            operator,
            all,
            left: replace_leaves(left, leaves, next, arena)?,
            right: replace_leaves(right, leaves, next, arena)?,
        },
    };
    arena
        .alloc(rebuilt)
        .map(|tree| &*tree)
        .map_err(|_| crate::sql::eval::arena_full())
}

/// Splits the volatile user routine calls out of `select`'s output list, or
/// `None` when it has none that PostgreSQL computes row by row.
pub(super) fn defer_source_calls<'a>(
    select: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<Option<DeferredSource<'a>>, SqlError> {
    let mut sources = ArenaList::new(arena);
    match select.set_body {
        // A sorted set computes every row first.
        Some(tree) if select.order_by.is_empty() => {
            if !union_all_leaves(tree, &mut sources)? {
                return Ok(None);
            }
        }
        Some(_) => return Ok(None),
        None => sources
            .push(select)
            .map_err(|_| crate::sql::eval::arena_full())?,
    }
    let mut deferred = ArenaList::new(arena);
    let mut leaves = ArenaList::new(arena);
    for (index, leaf) in sources.as_slice().iter().enumerate() {
        let Some(split) = split_leaf(index, leaf, &mut deferred, storage, txid, arena)? else {
            return Ok(None);
        };
        leaves
            .push(split)
            .map_err(|_| crate::sql::eval::arena_full())?;
    }
    if deferred.is_empty() {
        return Ok(None);
    }
    for leaf in sources.as_slice() {
        crate::sql::query::require_source_routine_execute(leaf, storage, txid, arena, outer)?;
    }
    // Every leaf has the written items, its leaf number, then each leaf's
    // hidden columns: its own values and NULL in the other leaves' slots.
    let width = leaves.as_slice()[0].items.len();
    let mut offsets = ArenaList::new(arena);
    let mut hidden_sources = ArenaList::new(arena);
    for leaf in leaves.as_slice() {
        offsets
            .push(hidden_sources.len())
            .map_err(|_| crate::sql::eval::arena_full())?;
        for expression in leaf.hidden {
            hidden_sources
                .push(*expression)
                .map_err(|_| crate::sql::eval::arena_full())?;
        }
    }
    let mut rewritten_leaves = ArenaList::new(arena);
    for (number, leaf) in leaves.as_slice().iter().enumerate() {
        let mut items = ArenaList::new(arena);
        for item in leaf.items {
            items
                .push(*item)
                .map_err(|_| crate::sql::eval::arena_full())?;
        }
        items
            .push(SelectItem::Expr {
                expression: alloc(arena, Expr::Int(number as i64))?,
                alias: None,
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
        for (owner, other) in leaves.as_slice().iter().enumerate() {
            for expression in other.hidden {
                items
                    .push(SelectItem::Expr {
                        expression: if owner == number {
                            expression
                        } else {
                            &Expr::Null
                        },
                        alias: None,
                    })
                    .map_err(|_| crate::sql::eval::arena_full())?;
            }
        }
        rewritten_leaves
            .push(
                &*arena
                    .alloc(Select {
                        items: items.as_slice(),
                        ..*leaf.select
                    })
                    .map_err(|_| crate::sql::eval::arena_full())?,
            )
            .map_err(|_| crate::sql::eval::arena_full())?;
    }
    let rewritten = match select.set_body {
        Some(tree) => &*arena
            .alloc(Select {
                set_body: Some(replace_leaves(
                    tree,
                    rewritten_leaves.as_slice(),
                    &mut 0,
                    arena,
                )?),
                ..*select
            })
            .map_err(|_| crate::sql::eval::arena_full())?,
        None => rewritten_leaves.as_slice()[0],
    };
    let total = width + 1 + hidden_sources.len();
    let hidden_sources = hidden_sources.as_slice();
    // The visit rewinds the arena it plans in, so the types wait on the stack.
    let mut described = [(0, crate::sql::ast::Collation::None); MAX_PROJ];
    let mut column = 0usize;
    crate::sql::query::visit_projection_types(
        rewritten,
        storage,
        txid,
        arena,
        outer,
        &mut |type_oid, collation| {
            *described.get_mut(column).ok_or_else(|| {
                sql_err!(
                    sqlstate::TOO_MANY_COLUMNS,
                    "target lists can have at most 1664 entries"
                )
            })? = (type_oid, collation);
            column += 1;
            Ok(())
        },
    )?;
    if column != total {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "deferred INSERT source changed its width"
        ));
    }
    let mut types = ArenaList::new(arena);
    for (&(type_oid, collation), source) in described[width + 1..total].iter().zip(hidden_sources) {
        types
            .push(HiddenColumn {
                type_oid,
                type_mod: -1,
                collation: match collation {
                    crate::sql::ast::Collation::None => None,
                    collation => Some(collation),
                },
                source,
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
    }
    Ok(Some(DeferredSource {
        select: rewritten,
        width,
        items: deferred.as_slice(),
        offsets: offsets.as_slice(),
        hidden: types.as_slice(),
    }))
}

impl<'a> DeferredSource<'a> {
    /// Evaluates the deferred items of one materialized source row in place.
    pub(super) fn evaluate(
        &self,
        row: &mut [Datum<'a>],
        arena: &'a Arena,
        params: &[Datum<'a>],
        hooks: &EvalHooks<'_, 'a>,
    ) -> Result<(), SqlError> {
        let (written, rest) = row.split_at_mut(self.width);
        let Some((Datum::Int8(leaf), hidden)) = rest.split_first().map(|(leaf, hidden)| {
            (
                match *leaf {
                    Datum::Int4(leaf) => Datum::Int8(i64::from(leaf)),
                    leaf => leaf,
                },
                hidden,
            )
        }) else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "deferred source row has no leaf number"
            ));
        };
        let leaf = usize::try_from(leaf)
            .map_err(|_| sql_err!(sqlstate::INTERNAL_ERROR, "deferred source leaf is invalid"))?;
        let offset =
            self.offsets.get(leaf).copied().ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "deferred source leaf is invalid")
            })?;
        for deferred in self.items.iter().filter(|item| item.leaf == leaf) {
            let calls = self.bind(deferred.calls, offset, hidden, arena)?;
            written[deferred.item] =
                eval_full(calls, arena, params, &NO_TRIGGER_DML_COLUMNS, hooks)?;
        }
        Ok(())
    }

    /// The skeleton with this row's hidden values as typed literals.
    fn bind(
        &self,
        expression: &'a Expr<'a>,
        offset: usize,
        values: &[Datum<'a>],
        arena: &'a Arena,
    ) -> Result<&'a Expr<'a>, SqlError> {
        let rebuilt = match *expression {
            Expr::Column {
                qualifier: Some(HIDDEN),
                name,
            } => {
                let index = offset
                    + name.parse::<usize>().map_err(|_| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "deferred source column is invalid"
                        )
                    })?;
                let column = &self.hidden[index];
                Expr::BoundLiteral {
                    value: crate::sql::ast::BoundLiteralValue::CachedDefault(encode_projected_pub(
                        &[values[index]],
                        arena,
                    )?),
                    type_oid: column.type_oid,
                    type_mod: column.type_mod,
                    collation: column.collation,
                    source: column.source,
                }
            }
            Expr::Call {
                kind,
                name,
                bound_oid,
                bound_strict,
                source_position,
                args,
                argument_names,
                variadic,
                star,
                distinct,
                order_by,
                over,
                filter,
            } => {
                let mut arguments = ArenaList::new(arena);
                for argument in args {
                    arguments
                        .push(self.bind(argument, offset, values, arena)?)
                        .map_err(|_| crate::sql::eval::arena_full())?;
                }
                Expr::Call {
                    kind,
                    name,
                    bound_oid,
                    bound_strict,
                    source_position,
                    args: arguments.as_slice(),
                    argument_names,
                    variadic,
                    star,
                    distinct,
                    order_by,
                    over,
                    filter,
                }
            }
            Expr::Binary {
                source_position,
                operator,
                left,
                right,
            } => Expr::Binary {
                source_position,
                operator,
                left: self.bind(left, offset, values, arena)?,
                right: self.bind(right, offset, values, arena)?,
            },
            Expr::Unary {
                source_position,
                operator,
                operand,
            } => Expr::Unary {
                source_position,
                operator,
                operand: self.bind(operand, offset, values, arena)?,
            },
            Expr::Cast {
                source_position,
                operand,
                type_name,
                type_mod,
                binding,
            } => Expr::Cast {
                source_position,
                operand: self.bind(operand, offset, values, arena)?,
                type_name,
                type_mod,
                binding,
            },
            Expr::Collate { operand, collation } => Expr::Collate {
                operand: self.bind(operand, offset, values, arena)?,
                collation,
            },
            Expr::IsNull { operand, negated } => Expr::IsNull {
                operand: self.bind(operand, offset, values, arena)?,
                negated,
            },
            Expr::Case {
                operand,
                whens,
                otherwise,
                origin,
            } => {
                let mut branches = ArenaList::new(arena);
                for (condition, result) in whens {
                    branches
                        .push((
                            self.bind(condition, offset, values, arena)?,
                            self.bind(result, offset, values, arena)?,
                        ))
                        .map_err(|_| crate::sql::eval::arena_full())?;
                }
                Expr::Case {
                    operand: operand
                        .map(|operand| self.bind(operand, offset, values, arena))
                        .transpose()?,
                    whens: branches.as_slice(),
                    otherwise: otherwise
                        .map(|otherwise| self.bind(otherwise, offset, values, arena))
                        .transpose()?,
                    origin,
                }
            }
            _ => return Ok(expression),
        };
        alloc(arena, rebuilt)
    }
}
