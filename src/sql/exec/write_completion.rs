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

/// The next event belongs to this statement's trigger depth. Its completion
/// marker is installed by the firing, before recursive trigger SQL can run.
pub(super) struct QueuedFiring {
    pub(super) index: usize,
    pub(super) trigger_slot: u16,
    pub(super) table: u16,
    pub(super) event: u8,
}

pub(super) struct AfterRowCursor {
    depth: u16,
    next: usize,
}

impl AfterRowCursor {
    pub(super) fn new(depth: u16) -> Self {
        Self { depth, next: 0 }
    }

    pub(super) fn next(
        &mut self,
        context: &mut TriggerExecContext<'_, '_, '_>,
    ) -> Result<Option<QueuedFiring>, SqlError> {
        while self.next < context.txn.deferred_trigger_events().len() {
            let index = self.next;
            self.next += 1;
            if context.txn.deferred_trigger_is_complete(index) {
                continue;
            }
            let event = context.txn.deferred_trigger_events()[index];
            let selected = match event.kind {
                crate::sql::txn::DeferredTriggerKind::AfterRow {
                    trigger_slot,
                    effective_table,
                    trigger_depth,
                } if trigger_depth == self.depth => Some((trigger_slot, effective_table)),
                crate::sql::txn::DeferredTriggerKind::Constraint {
                    identity,
                    effective_table,
                    trigger_depth,
                } if trigger_depth == self.depth => {
                    let Some(timing) = constraints::constraint_timing(
                        context.storage(),
                        identity,
                        context.txn.txid,
                    ) else {
                        context.txn.complete_deferred_trigger(index)?;
                        continue;
                    };
                    if context.txn.constraint_mode(identity, timing)
                        != crate::sql::ast::ConstraintMode::Immediate
                    {
                        None
                    } else if let crate::sql::txn::ConstraintIdentity::Trigger { slot, .. } =
                        identity
                    {
                        Some((slot, effective_table))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some((trigger_slot, table)) = selected {
                return Ok(Some(QueuedFiring {
                    index,
                    trigger_slot,
                    table,
                    event: event.event,
                }));
            }
        }
        Ok(None)
    }
}

pub(super) struct StatementTriggerCursor {
    last_name: Option<SqlName>,
}

impl StatementTriggerCursor {
    pub(super) fn new() -> Self {
        Self { last_name: None }
    }

    pub(super) fn next(
        &mut self,
        context: &TriggerExecContext<'_, '_, '_>,
        target: crate::storage::TriggerTarget,
        event: u8,
        before: bool,
        columns: ColumnSet,
    ) -> Option<crate::storage::TriggerDef> {
        while let Some(trigger) = context
            .storage()
            .triggers_for_target(target, context.txn.txid)
            .filter(|(_, trigger)| {
                self.last_name
                    .is_none_or(|last| trigger.name_to(context.txn.txid).as_str() > last.as_str())
            })
            .min_by(|(_, left), (_, right)| {
                left.name_to(context.txn.txid)
                    .as_str()
                    .cmp(right.name_to(context.txn.txid).as_str())
            })
            .map(|(_, trigger)| trigger)
        {
            self.last_name = Some(trigger.name_to(context.txn.txid));
            if !(if context.txn.replication_apply {
                trigger.enabled_to(context.txn.txid).fires_for_replication()
            } else {
                trigger.enabled_to(context.txn.txid).fires_for_origin()
            }) || !matches!(trigger.level, TriggerLevel::Statement)
                || trigger.timing
                    != if before {
                        TriggerTiming::Before
                    } else {
                        TriggerTiming::After
                    }
                || !trigger.events.contains(event)
                || (event == TriggerEvents::UPDATE
                    && !trigger.update_columns.is_empty()
                    && !trigger.update_columns.intersects(columns))
            {
                continue;
            }
            return Some(trigger);
        }
        None
    }
}

enum AfterPhase {
    Rows(AfterRowCursor),
    Statements {
        event: usize,
        trigger: StatementTriggerCursor,
    },
    Reply,
    Replied,
}

#[expect(
    clippy::large_enum_variant,
    reason = "selected trigger definitions stay inline without heap allocation"
)]
enum AfterFiring {
    Row(QueuedFiring),
    Statement {
        trigger: crate::storage::TriggerDef,
        event: u8,
    },
}

struct CompletionCursor {
    completion: WriteCompletion,
    reply: CompletionReply,
    phase: AfterPhase,
}

impl CompletionCursor {
    fn new(completion: WriteCompletion, reply: CompletionReply, depth: u16) -> Self {
        Self {
            completion,
            reply,
            phase: AfterPhase::Rows(AfterRowCursor::new(depth)),
        }
    }

    fn next(
        &mut self,
        context: &mut TriggerExecContext<'_, '_, '_>,
        target: crate::storage::TriggerTarget,
    ) -> Result<Option<AfterFiring>, SqlError> {
        let events = self.completion.events();
        loop {
            match &mut self.phase {
                AfterPhase::Rows(cursor) => {
                    if events.iter().any(Option::is_some)
                        && let Some(firing) = cursor.next(context)?
                    {
                        return Ok(Some(AfterFiring::Row(firing)));
                    }
                    self.phase = AfterPhase::Statements {
                        event: 0,
                        trigger: StatementTriggerCursor::new(),
                    };
                }
                AfterPhase::Statements { event, trigger } => {
                    if *event == events.len() {
                        self.phase = AfterPhase::Reply;
                        continue;
                    }
                    if let Some((operation, columns)) = events[*event]
                        && let Some(selected) =
                            trigger.next(context, target, operation, false, columns)
                    {
                        return Ok(Some(AfterFiring::Statement {
                            trigger: selected,
                            event: operation,
                        }));
                    }
                    *event += 1;
                    *trigger = StatementTriggerCursor::new();
                }
                AfterPhase::Reply | AfterPhase::Replied => return Ok(None),
            }
        }
    }

    fn complete(&mut self, responder: &mut Responder<'_>) -> Outcome {
        if !matches!(self.phase, AfterPhase::Reply) {
            return sql_fail(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "write completion is not awaiting its response"
            ));
        }
        let (rows, tag) = match &self.completion {
            WriteCompletion::Insert { rows, .. } => (*rows, stack_format!(48, "INSERT 0 {}", rows)),
            WriteCompletion::Update { rows, .. } => (*rows, stack_format!(48, "UPDATE {}", rows)),
            WriteCompletion::Delete { rows } => (*rows, stack_format!(48, "DELETE {}", rows)),
            WriteCompletion::Merge { rows, .. } => (*rows, stack_format!(48, "MERGE {}", rows)),
        };
        match self.reply {
            CompletionReply::CountOnly => responder.set_affected_rows(rows),
            CompletionReply::Command => responder.command_complete_rows(tag.as_str(), rows)?,
        }
        self.phase = AfterPhase::Replied;
        sql_ok()
    }
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
        let depth = context.txn.trigger_depth();
        let mut cursor = CompletionCursor::new(self, reply, depth);
        loop {
            let firing = match cursor.next(&mut context, target) {
                Ok(Some(firing)) => firing,
                Ok(None) => break,
                Err(error) => return sql_fail(error),
            };
            let result = match firing {
                AfterFiring::Row(firing) => execute_deferred_trigger_event(
                    &mut context,
                    firing.index,
                    firing.trigger_slot,
                    firing.table,
                    transitions.for_event(firing.event),
                ),
                AfterFiring::Statement { trigger, event } => execute_statement_trigger(
                    &mut context,
                    &trigger,
                    target,
                    definition,
                    event,
                    false,
                    transitions.for_event(event),
                ),
            };
            if let Err(error) = result {
                return sql_fail(error);
            }
        }
        cursor.complete(context.responder)
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn completion_response_retains_count_across_full_buffer_and_move() {
        let mut budget = crate::mem::Budget::new(64);
        let mut buffer = crate::mem::FixedBuf::new(&mut budget, "completion response", 64).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut cursor = CompletionCursor::new(
                WriteCompletion::Insert {
                    rows: u64::MAX,
                    conflict_update: None,
                },
                CompletionReply::Command,
                0,
            );
            let mut responder = Responder::new(&mut buffer);
            assert!(responder.buffer.append(&[0xa5; 50]));
            responder.buffer.consume(20);
            assert!(
                responder
                    .data_row(&[Datum::Text("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")])
                    .is_err()
            );
            assert_eq!(responder.buffer.readable(), &[0xa5; 30]);
            responder.buffer.clear();
            responder.set_affected_rows(7);
            assert_eq!(
                cursor
                    .complete(&mut responder)
                    .unwrap()
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            assert!(responder.buffer.is_empty());
            cursor.phase = AfterPhase::Reply;
            assert!(responder.buffer.append(&[0xa5; 50]));
            assert!(cursor.complete(&mut responder).is_err());
            assert_eq!(responder.buffer.readable(), &[0xa5; 50]);
            assert_eq!(responder.take_affected_rows(), Some(7));
            assert!(matches!(cursor.phase, AfterPhase::Reply));
            let mut moved = cursor;
            responder.buffer.clear();
            moved.complete(&mut responder).unwrap().unwrap();
            let tag = b"INSERT 0 18446744073709551615\0";
            let bytes = responder.buffer.readable();
            assert_eq!(bytes[0], b'C');
            assert_eq!(&bytes[1..5], &((tag.len() + 4) as u32).to_be_bytes());
            assert_eq!(&bytes[5..], tag);
            assert_eq!(responder.take_affected_rows(), Some(u64::MAX));
            let completed_bytes = responder.buffer.len();
            assert_eq!(
                moved
                    .complete(&mut responder)
                    .unwrap()
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            assert_eq!(responder.buffer.len(), completed_bytes);
            assert_eq!(responder.take_affected_rows(), None);
            let mut nested = CompletionCursor {
                completion: WriteCompletion::Update {
                    rows: 13,
                    columns: ColumnSet::EMPTY,
                },
                reply: CompletionReply::CountOnly,
                phase: AfterPhase::Reply,
            };
            nested.complete(&mut responder).unwrap().unwrap();
            assert_eq!(responder.buffer.len(), completed_bytes);
            assert_eq!(responder.take_affected_rows(), Some(13));
            assert_eq!(
                nested
                    .complete(&mut responder)
                    .unwrap()
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_ERROR
            );
            assert_eq!(responder.buffer.len(), completed_bytes);
        });
    }
}
