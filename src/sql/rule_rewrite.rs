//! The shape of PostgreSQL's rewrite of a data-changing statement
//! (`RewriteQuery`/`fireRules`): which queries rules and auto-updatable
//! views turn it into, in the order PostgreSQL runs them.
//!
//! The rewriter applies rules to every rule action again, so a statement
//! can become a tree of queries. PostgreSQL builds that whole list before
//! anything runs: infinite recursion, a view that cannot be updated and the
//! shapes a data-modifying `WITH` query rejects fail before the first row
//! changes. Execution and EXPLAIN walk this shape first for that reason.

use crate::mem::arena::Arena;
use crate::sql::ast::{QualName, Stmt, TriggerEvents, TriggerLevel, TriggerTiming};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::guc::SessionReplicationRole;
use crate::sql::txn::MAX_RULE_NESTING;
use crate::sql_err;
use crate::storage::{ResolvedRelation, RewriteEvent, RewriteMode, RuleTarget, Storage};

/// Where a rewritten query came from (PostgreSQL's `QuerySource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuerySource {
    Original,
    InsteadRule,
    QualifiedInsteadRule,
    NonInsteadRule,
}

/// The command of one rewritten query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueryCommand {
    Select,
    Insert,
    Update,
    Delete,
    Merge,
    Notify,
}

/// One query of a rewritten statement, as the walk reports it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RewrittenQuery {
    pub command: QueryCommand,
    pub source: QuerySource,
}

/// A data-changing statement's target and rewrite event.
pub(crate) fn statement_target<'a>(statement: &Stmt<'a>) -> Option<(QualName<'a>, RewriteEvent)> {
    match statement {
        Stmt::Insert(insert) => Some((insert.table, RewriteEvent::Insert)),
        Stmt::Update(update) => Some((update.table, RewriteEvent::Update)),
        Stmt::Delete(delete) => Some((delete.table, RewriteEvent::Delete)),
        _ => None,
    }
}

fn command_of(statement: &Stmt<'_>) -> QueryCommand {
    match statement {
        Stmt::Insert(_) => QueryCommand::Insert,
        Stmt::Update(_) => QueryCommand::Update,
        Stmt::Delete(_) => QueryCommand::Delete,
        Stmt::Merge(_) => QueryCommand::Merge,
        Stmt::Notify { .. } => QueryCommand::Notify,
        Stmt::With { statement, .. } => command_of(statement),
        _ => QueryCommand::Select,
    }
}

const fn command_of_event(event: RewriteEvent) -> QueryCommand {
    match event {
        RewriteEvent::Insert => QueryCommand::Insert,
        RewriteEvent::Update => QueryCommand::Update,
        RewriteEvent::Delete => QueryCommand::Delete,
        RewriteEvent::Select => QueryCommand::Select,
    }
}

/// A stored rule action's target as execution binds it: through the
/// rule's recorded dependencies, else under the current search path.
pub(crate) fn rule_action_target(
    storage: &Storage,
    txid: u32,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    name: QualName<'_>,
) -> Result<Option<ResolvedRelation>, SqlError> {
    let recorded = dependencies.entries().iter().find(|dependency| {
        matches!(
            dependency.class,
            crate::storage::DependencyClass::Table | crate::storage::DependencyClass::View
        ) && dependency.referenced_schema.as_str() == name.schema.unwrap_or("")
            && dependency.referenced_name.as_str() == name.name
    });
    match recorded {
        Some(dependency) => storage
            .resolve_stored_query_relation(dependency, txid)
            .map(Some),
        None => Ok(storage.resolve_relation(name.schema, name.name, txid)),
    }
}

/// Firing rules of `target` for `event`, in rule-name order (the order
/// PostgreSQL reads them from `pg_rewrite`).
pub(crate) fn rules_in_order(
    storage: &Storage,
    target: RuleTarget,
    event: RewriteEvent,
    role: SessionReplicationRole,
    txid: u32,
    mut visit: impl FnMut(usize, crate::storage::RuleDefinition) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let mut last: Option<crate::storage::SqlName> = None;
    loop {
        let next = storage
            .firing_rules_for(target, event, role, txid)
            .map(|(slot, rule)| (slot, rule.definition_for(txid)))
            .filter(|(_, definition)| {
                last.is_none_or(|last| definition.name.as_str() > last.as_str())
            })
            .min_by(|(_, left), (_, right)| left.name.as_str().cmp(right.name.as_str()));
        let Some((slot, definition)) = next else {
            return Ok(());
        };
        last = Some(definition.name);
        visit(slot, definition)?;
    }
}

/// Whether `view` has a row-level `INSTEAD OF` trigger for `event`.
pub(crate) fn view_has_instead_trigger(
    storage: &Storage,
    view: usize,
    event: RewriteEvent,
    txid: u32,
) -> bool {
    let events = match event {
        RewriteEvent::Insert => TriggerEvents::INSERT,
        RewriteEvent::Update => TriggerEvents::UPDATE,
        RewriteEvent::Delete => TriggerEvents::DELETE,
        RewriteEvent::Select => return false,
    };
    storage.triggers_for_view(view, txid).any(|(_, trigger)| {
        matches!(trigger.level, TriggerLevel::Row)
            && matches!(trigger.timing, TriggerTiming::InsteadOf)
            && trigger.events.contains(events)
    })
}

/// What a write of `event` is, for the view errors PostgreSQL reports.
pub(crate) const fn view_write(event: RewriteEvent) -> crate::sql::query::ViewWrite {
    match event {
        RewriteEvent::Insert => crate::sql::query::ViewWrite::Insert,
        RewriteEvent::Update => crate::sql::query::ViewWrite::Update,
        _ => crate::sql::query::ViewWrite::Delete,
    }
}

/// The relations and events the walk is inside of, as PostgreSQL's
/// `rewrite_events` list.
struct Events {
    entries: [(RuleTarget, RewriteEvent); MAX_RULE_NESTING],
    depth: usize,
}

struct Walk<'s, 'v> {
    storage: &'s Storage,
    txid: u32,
    role: SessionReplicationRole,
    arena: &'s Arena,
    events: Events,
    visit: &'v mut dyn FnMut(RewrittenQuery) -> Result<(), SqlError>,
}

/// Visits every query PostgreSQL's rewriter turns `statement` into, in the
/// order they run, or fails as the rewriter does before anything runs.
pub(crate) fn walk_rewritten(
    storage: &Storage,
    txid: u32,
    role: SessionReplicationRole,
    statement: &Stmt<'_>,
    arena: &Arena,
    visit: &mut dyn FnMut(RewrittenQuery) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let mark = arena.mark();
    let mut walk = Walk {
        storage,
        txid,
        role,
        arena,
        events: Events {
            entries: [(RuleTarget::Table(0), RewriteEvent::Insert); MAX_RULE_NESTING],
            depth: 0,
        },
        visit,
    };
    let returning = crate::sql::query::statement_returns_rows(statement);
    let result = match statement_target(statement) {
        Some((name, event)) => match storage.resolve_relation(name.schema, name.name, txid) {
            Some(relation) => {
                walk.query(relation, name.name, event, QuerySource::Original, returning)
            }
            None => (walk.visit)(RewrittenQuery {
                command: command_of(statement),
                source: QuerySource::Original,
            }),
        },
        None => (walk.visit)(RewrittenQuery {
            command: command_of(statement),
            source: QuerySource::Original,
        }),
    };
    // Only the verdict leaves; parsed actions are discarded.
    unsafe { arena.rewind_to(mark) };
    result
}

/// Whether rewriting `statement` changes it: its target has rules for its
/// event, or is a view PostgreSQL updates through its base relation.
pub(crate) fn statement_is_rewritten(
    storage: &Storage,
    txid: u32,
    role: SessionReplicationRole,
    statement: &Stmt<'_>,
) -> bool {
    let Some((name, event)) = statement_target(statement) else {
        return false;
    };
    match storage.resolve_relation(name.schema, name.name, txid) {
        Some(ResolvedRelation::Table(slot)) => storage
            .firing_rules_for(RuleTarget::Table(slot as u16), event, role, txid)
            .next()
            .is_some(),
        Some(ResolvedRelation::View(_)) => true,
        _ => false,
    }
}

impl Walk<'_, '_> {
    /// Rewrites one query; `returning` is whether it has a RETURNING list.
    fn query(
        &mut self,
        relation: ResolvedRelation,
        name: &str,
        event: RewriteEvent,
        source: QuerySource,
        returning: bool,
    ) -> Result<(), SqlError> {
        let target = match relation {
            ResolvedRelation::Table(slot) => RuleTarget::Table(slot as u16),
            ResolvedRelation::View(slot) => RuleTarget::View(slot as u16),
            ResolvedRelation::Catalog => {
                return (self.visit)(RewrittenQuery {
                    command: command_of_event(event),
                    source,
                });
            }
        };
        let storage = self.storage;
        let txid = self.txid;
        let mut instead = false;
        let mut qualified = false;
        let mut returning_rules = 0usize;
        rules_in_order(storage, target, event, self.role, txid, |_, definition| {
            if definition.mode == RewriteMode::Instead {
                if definition.condition.is_some() {
                    qualified = true;
                } else {
                    instead = true;
                }
            }
            returning_rules += usize::from(definition.returning_action.is_some());
            Ok(())
        })?;
        // PostgreSQL rejects several RETURNING rules while it builds the
        // actions, before rewriting them; the missing-RETURNING check
        // follows the recursion.
        if returning && returning_rules > 1 {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "cannot have RETURNING lists in multiple rules"
            ));
        }
        let instead_rule = instead || qualified;
        // An auto-updatable view becomes a query on its base relation, run
        // with the rule actions and rewritten again.
        let base = match relation {
            ResolvedRelation::View(view)
                if !instead && !view_has_instead_trigger(storage, view, event, txid) =>
            {
                if qualified {
                    return Err(crate::sql::query::conditional_instead_view_error(
                        name,
                        view_write(event),
                    ));
                }
                let view_name = storage.view(view).name_for(txid);
                let view_schema = storage.view(view).schema_for(txid);
                let resolved = crate::sql::query::resolve_view_for_dml(
                    storage,
                    QualName {
                        schema: Some(view_schema.as_str()),
                        name: view_name.as_str(),
                    },
                    txid,
                    self.arena,
                    view_write(event),
                )?
                .expect("a view resolves as a view");
                let base = storage
                    .resolve_relation(resolved.base.schema, resolved.base.name, txid)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_TABLE,
                            "relation \"{}\" does not exist",
                            resolved.base.name
                        )
                    })?;
                instead = true;
                Some((base, resolved.base.name))
            }
            _ => None,
        };
        let original = RewrittenQuery {
            command: command_of_event(event),
            source,
        };
        if !instead && event == RewriteEvent::Insert {
            (self.visit)(original)?;
        }
        let has_actions = base.is_some() || {
            let mut any = false;
            rules_in_order(storage, target, event, self.role, txid, |_, definition| {
                any |= definition.action_sql().next().is_some();
                Ok(())
            })?;
            any
        };
        if has_actions {
            if self.events.entries[..self.events.depth]
                .iter()
                .any(|&(seen, seen_event)| seen == target && seen_event == event)
            {
                return Err(sql_err!(
                    sqlstate::INVALID_OBJECT_DEFINITION,
                    "infinite recursion detected in rules for relation \"{}\"",
                    name
                ));
            }
            if self.events.depth == MAX_RULE_NESTING {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "rewrite rules nest more than {} levels deep",
                    MAX_RULE_NESTING
                ));
            }
            self.events.entries[self.events.depth] = (target, event);
            self.events.depth += 1;
            let result = self.products(target, event, base, returning);
            self.events.depth -= 1;
            result?;
        }
        if returning && instead_rule && returning_rules == 0 {
            let command = match event {
                RewriteEvent::Insert => "INSERT",
                RewriteEvent::Update => "UPDATE",
                RewriteEvent::Delete => "DELETE",
                RewriteEvent::Select => "SELECT",
            };
            return Err(crate::sql::error_position::with_computed_hint(
                sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot perform {} RETURNING on relation \"{}\"",
                    command,
                    name
                ),
                format_args!(
                    "You need an unconditional ON {} DO INSTEAD rule with a RETURNING clause.",
                    command
                ),
            ));
        }
        if !instead && event != RewriteEvent::Insert {
            (self.visit)(original)?;
        }
        Ok(())
    }

    /// The rule actions, and the base relation's query of an auto-updatable
    /// view (first for INSERT, last otherwise), each rewritten again. The
    /// view's query keeps the RETURNING list; a rule action keeps its own
    /// only when the query it replaces has one, as `rewriteRuleAction`
    /// drops it otherwise.
    fn products(
        &mut self,
        target: RuleTarget,
        event: RewriteEvent,
        base: Option<(ResolvedRelation, &str)>,
        returning: bool,
    ) -> Result<(), SqlError> {
        if let Some((relation, name)) = base
            && event == RewriteEvent::Insert
        {
            self.query(relation, name, event, QuerySource::Original, returning)?;
        }
        let storage = self.storage;
        let txid = self.txid;
        let arena = self.arena;
        let role = self.role;
        rules_in_order(storage, target, event, role, txid, |slot, definition| {
            let source = match (definition.mode, definition.condition.is_some()) {
                (RewriteMode::Instead, true) => QuerySource::QualifiedInsteadRule,
                (RewriteMode::Instead, false) => QuerySource::InsteadRule,
                (RewriteMode::Also, _) => QuerySource::NonInsteadRule,
            };
            let dependencies = storage.snapshot_rule_dependencies(slot, txid, arena)?;
            for (index, sql) in definition.action_sql().enumerate() {
                let action_returning = returning
                    && definition
                        .returning_action
                        .is_some_and(|action| usize::from(action) == index);
                let action = crate::sql::query::parse_stored_rule_statement(
                    &definition,
                    sql,
                    storage,
                    txid,
                    dependencies.view(),
                    arena,
                )?;
                let relation = match statement_target(action) {
                    Some((name, action_event)) => {
                        rule_action_target(storage, txid, dependencies.view(), name)?
                            .map(|relation| (relation, name.name, action_event))
                    }
                    None => None,
                };
                match relation {
                    Some((relation, name, action_event)) => {
                        self.query(relation, name, action_event, source, action_returning)?;
                    }
                    None => (self.visit)(RewrittenQuery {
                        command: command_of(action),
                        source,
                    })?,
                }
            }
            Ok(())
        })?;
        if let Some((relation, name)) = base
            && event != RewriteEvent::Insert
        {
            self.query(relation, name, event, QuerySource::Original, returning)?;
        }
        Ok(())
    }
}

/// PostgreSQL's check of a data-modifying `WITH` query: its rewrite must
/// leave exactly one `INSERT`, `UPDATE`, `DELETE` or `MERGE`.
pub(crate) fn check_with_query(
    storage: &Storage,
    txid: u32,
    role: SessionReplicationRole,
    statement: &Stmt<'_>,
    arena: &Arena,
) -> Result<(), SqlError> {
    let mut count = 0usize;
    let mut first = None;
    let mut conditional = false;
    let mut also = false;
    walk_rewritten(storage, txid, role, statement, arena, &mut |query| {
        count += 1;
        first.get_or_insert(query.command);
        // PostgreSQL names the first query that came from a qualified
        // INSTEAD or an ALSO rule.
        if !conditional && !also {
            match query.source {
                QuerySource::QualifiedInsteadRule => conditional = true,
                QuerySource::NonInsteadRule => also = true,
                _ => {}
            }
        }
        Ok(())
    })?;
    let unsupported = |what: &str| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "{} rules are not supported for data-modifying statements in WITH",
            what
        )
    };
    match count {
        0 => Err(unsupported("DO INSTEAD NOTHING")),
        1 if first == Some(QueryCommand::Notify) => Err(unsupported("DO INSTEAD NOTIFY")),
        1 => Ok(()),
        _ if conditional => Err(unsupported("conditional DO INSTEAD")),
        _ if also => Err(unsupported("DO ALSO")),
        _ => Err(unsupported("multi-statement DO INSTEAD")),
    }
}
