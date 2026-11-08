//! Ordered AFTER work and its single write-completion response.

use super::*;

pub(super) enum WriteCompletion {
    Insert {
        rows: u64,
        conflict_update: Option<ColumnSet>,
    },
    Update {
        rows: u64,
        columns: ColumnSet,
    },
    Delete {
        rows: u64,
    },
    Merge {
        rows: u64,
        insert: bool,
        update: Option<ColumnSet>,
        delete: bool,
    },
}

pub(super) enum CompletionReply {
    Command,
    CountOnly,
}

impl WriteCompletion {
    fn events(&self) -> [Option<(u8, ColumnSet)>; 3] {
        match self {
            Self::Insert {
                conflict_update, ..
            } => [
                conflict_update.map(|columns| (TriggerEvents::UPDATE, columns)),
                Some((TriggerEvents::INSERT, ColumnSet::EMPTY)),
                None,
            ],
            Self::Update { columns, .. } => [Some((TriggerEvents::UPDATE, *columns)), None, None],
            Self::Delete { .. } => [Some((TriggerEvents::DELETE, ColumnSet::EMPTY)), None, None],
            Self::Merge {
                insert,
                update,
                delete,
                ..
            } => [
                delete.then_some((TriggerEvents::DELETE, ColumnSet::EMPTY)),
                update.map(|columns| (TriggerEvents::UPDATE, columns)),
                insert.then_some((TriggerEvents::INSERT, ColumnSet::EMPTY)),
            ],
        }
    }

    pub(super) fn finish<'values, Definition: crate::storage::DefinitionBacking>(
        self,
        mut context: TriggerExecContext<'_, 'values, '_>,
        target: crate::storage::TriggerTarget,
        definition: &crate::storage::TableDefinition<Definition>,
        transitions: TransitionRowSets<'values>,
        reply: CompletionReply,
    ) -> Outcome {
        let events = self.events();
        let depth = context.txn.trigger_depth();
        if events.iter().any(Option::is_some)
            && let Err(error) = fire_statement_row_trigger_events(&mut context, depth, transitions)
        {
            return sql_fail(error);
        }
        for (event, columns) in events.into_iter().flatten() {
            if let Err(error) = fire_statement_triggers_with_rows(
                context.for_trigger(),
                target,
                definition,
                event,
                false,
                columns,
                transitions.for_event(event),
            ) {
                return sql_fail(error);
            }
        }
        let (rows, tag) = match self {
            Self::Insert { rows, .. } => (rows, stack_format!(48, "INSERT 0 {}", rows)),
            Self::Update { rows, .. } => (rows, stack_format!(48, "UPDATE {}", rows)),
            Self::Delete { rows } => (rows, stack_format!(48, "DELETE {}", rows)),
            Self::Merge { rows, .. } => (rows, stack_format!(48, "MERGE {}", rows)),
        };
        match reply {
            CompletionReply::CountOnly => context.responder.set_affected_rows(rows),
            CompletionReply::Command => context
                .responder
                .command_complete_rows(tag.as_str(), rows)?,
        }
        sql_ok()
    }
}
