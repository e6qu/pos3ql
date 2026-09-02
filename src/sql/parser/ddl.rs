//! Parsing the data-definition statements.
//!
//! `CREATE TABLE` and its column definitions, constraints and `LIKE` clauses;
//! `CREATE INDEX`; `CREATE VIEW`; and the `DROP` family. Split from the rest of
//! the parser as a second `impl Parser` block: these share the cursor and the
//! token helpers with every other statement, but nothing else refers to them.

use super::{
    ColumnDef, CreateTable, DropTable, FkAction, LikeClause, MAX_LIST, ParseError, Parser,
    QualName, Stmt, TableConstraint, Tok,
};
use crate::sql::ast::{
    AggregateArgument, AggregateArguments, AggregateDefinition, AggregateFinal,
    AggregateFinalModify, AggregateIdentity, AggregateMoving, AggregatePartial, AlterDomainAction,
    AlterEventTriggerAction, AlterExtensionAction, AlterForeignDataWrapperAction,
    AlterForeignServerAction, AlterIndexAction, AlterOperatorAction, AlterOperatorClassAction,
    AlterOperatorFamilyAction, AlterPublicationAction, AlterRoutineAction, AlterStatisticsAction,
    AlterTablespaceAction, AlterTriggerAction, AlterTypeAction, BtreeStrategy, CastContext,
    CastMethod, ConstraintMode, ConstraintTiming, ConstraintValidation, CreateCast, CreateDomain,
    CreateEventTrigger, CreateForeignDataWrapper, CreateForeignServer, CreateForeignTable,
    CreateOperator, CreateOperatorClass, CreateRoutine, CreateRule, CreateSchemaElement,
    CreateStatistics, CreateTextSearchConfiguration, CreateTextSearchDictionary,
    CreateTextSearchParser, CreateTextSearchTemplate, CreateTrigger, DomainCheck,
    EventTriggerEvent, ExclusionOperator, Expr, ExtensionMemberIdentity, ExtensionRelationKind,
    ForeignDataHandler, ForeignDataValidator, ForeignOption, ForeignOptionAction,
    ForeignSchemaSelection, ForeignUser, ImportForeignSchema, IndexAccessMethod, IndexBuildMode,
    IndexStorageOptionNames, IndexStorageOptions, IndexTargetScope, OperatorClassMember,
    OperatorFamilyMember, OperatorFamilyMemberIdentity, OperatorIdentity, OperatorOperands,
    PartitionBound, PartitionClause, PartitionStrategy, PolicyCommand, PolicyExpression,
    PolicyIdentity, PolicyPermissiveness, PolicyRole, PublicationOperations, PublicationTarget,
    RelationPersistence, RelationStorageOptionNames, RelationStorageOptions, RoleOptions,
    RoutineArgument, RoutineArgumentMode, RoutineCreateKind, RoutineIdentity, RoutineParallel,
    RoutineResultColumn, RoutineTargetKind, RuleAction, RuleEvent, RuleMode, StatisticsExpression,
    StatisticsKey, StatisticsKeys, StatisticsKinds, StatisticsName, StatisticsTarget,
    SubscriptionBehavior, SubscriptionConnect, SubscriptionOptions, SubscriptionOrigin,
    SubscriptionSlotName, SubscriptionSlotPlan, SubscriptionStreaming,
    SubscriptionSynchronousCommit, TableAccessMethod, TableMembership, TablespaceOptionNames,
    TablespaceOptions, TextSearchConfigurationSource, TextSearchObjectKind, TextSearchOption,
    TriggerEvent, TriggerIdentity, TriggerKind, TriggerTiming, TriggerTransitionTables,
    ViewSecurity,
};
use crate::sql::eval::sqlstate;

fn rule_action_statement(statement: &Stmt<'_>) -> bool {
    match statement {
        Stmt::Select(_)
        | Stmt::SetQuery(_)
        | Stmt::Insert(_)
        | Stmt::Update(_)
        | Stmt::Delete(_)
        | Stmt::Notify { .. } => true,
        Stmt::With { statement, .. } => rule_action_statement(statement),
        _ => false,
    }
}

fn rule_action_name(statement: &Stmt<'_>) -> &'static str {
    match statement {
        Stmt::Select(_) | Stmt::SetQuery(_) => "SELECT",
        Stmt::Insert(_) => "INSERT",
        Stmt::Update(_) => "UPDATE",
        Stmt::Delete(_) => "DELETE",
        Stmt::Notify { .. } => "NOTIFY",
        Stmt::With { statement, .. } => rule_action_name(statement),
        _ => "utility",
    }
}
use crate::stack_format;
use crate::storage::MAX_INDEX_COLS;

fn index_expression_source<'a>(expression: &Expr<'a>, text: &'a str) -> &'a str {
    // PostgreSQL's index definition printer discards redundant grouping around
    // a function call, while arithmetic grouping remains semantically useful.
    if !matches!(expression, Expr::Call { .. }) {
        return text;
    }
    let mut text = text.trim();
    while text.starts_with('(') && text.ends_with(')') {
        text = text[1..text.len() - 1].trim();
    }
    text
}

#[derive(Clone, Copy)]
struct AggregateOptions<'a> {
    transition: Option<QualName<'a>>,
    state_type: Option<&'a str>,
    state_space: Option<u32>,
    final_function: Option<QualName<'a>>,
    final_extra: Option<bool>,
    final_modify: Option<AggregateFinalModify>,
    combine: Option<QualName<'a>>,
    serial: Option<QualName<'a>>,
    deserial: Option<QualName<'a>>,
    initial_condition: Option<&'a str>,
    moving_transition: Option<QualName<'a>>,
    moving_inverse: Option<QualName<'a>>,
    moving_state_type: Option<&'a str>,
    moving_state_space: Option<u32>,
    moving_final: Option<QualName<'a>>,
    moving_final_extra: Option<bool>,
    moving_final_modify: Option<AggregateFinalModify>,
    moving_initial_condition: Option<&'a str>,
    sort_operator: Option<&'a str>,
    parallel: Option<RoutineParallel>,
    hypothetical: Option<bool>,
}

impl AggregateOptions<'_> {
    const EMPTY: Self = Self {
        transition: None,
        state_type: None,
        state_space: None,
        final_function: None,
        final_extra: None,
        final_modify: None,
        combine: None,
        serial: None,
        deserial: None,
        initial_condition: None,
        moving_transition: None,
        moving_inverse: None,
        moving_state_type: None,
        moving_state_space: None,
        moving_final: None,
        moving_final_extra: None,
        moving_final_modify: None,
        moving_initial_condition: None,
        sort_operator: None,
        parallel: None,
        hypothetical: None,
    };
}

impl<'a> Parser<'a> {
    fn event_trigger_event(&mut self) -> Result<EventTriggerEvent, ParseError> {
        let event = self.any_ident("event trigger event")?;
        if event.eq_ignore_ascii_case("login") {
            Ok(EventTriggerEvent::Login)
        } else if event.eq_ignore_ascii_case("ddl_command_start") {
            Ok(EventTriggerEvent::DdlCommandStart)
        } else if event.eq_ignore_ascii_case("ddl_command_end") {
            Ok(EventTriggerEvent::DdlCommandEnd)
        } else if event.eq_ignore_ascii_case("sql_drop") {
            Ok(EventTriggerEvent::SqlDrop)
        } else if event.eq_ignore_ascii_case("table_rewrite") {
            Ok(EventTriggerEvent::TableRewrite)
        } else {
            Err(self.err_here("unrecognized event name"))
        }
    }

    fn create_event_trigger(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("event trigger name")?;
        self.expect_ident("on")?;
        let event = self.event_trigger_event()?;
        let tags = if self.eat_ident("when")? {
            if !event.supports_tag_filter() {
                return Err(self.err_here("WHEN clause is not supported for this event"));
            }
            self.expect_ident("tag")?;
            self.expect_ident("in")?;
            self.expect_op("(")?;
            let mut tags = [""; MAX_LIST];
            let mut count = 0usize;
            loop {
                if count == tags.len() {
                    return Err(self.limit("event trigger tags", tags.len()));
                }
                tags[count] = self.str_literal("command tag")?;
                count += 1;
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
            if self.eat_ident("and")? {
                return Err(self.err_here("filter variable TAG specified more than once"));
            }
            self.arena_slice(&tags[..count])?
        } else {
            &[]
        };
        self.expect_ident("execute")?;
        if !self.eat_ident("function")? {
            self.expect_ident("procedure")?;
        }
        let function = self.qual_name("event trigger function")?;
        self.expect_op("(")?;
        self.expect_op(")")?;
        Ok(Stmt::CreateEventTrigger(CreateEventTrigger {
            name,
            event,
            tags,
            function,
        }))
    }

    pub(super) fn alter_event_trigger(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("event trigger name")?;
        let action = if self.eat_ident("disable")? {
            AlterEventTriggerAction::SetEnabled(crate::sql::ast::TriggerEnableMode::Disabled)
        } else if self.eat_ident("enable")? {
            AlterEventTriggerAction::SetEnabled(if self.eat_ident("replica")? {
                crate::sql::ast::TriggerEnableMode::Replica
            } else if self.eat_ident("always")? {
                crate::sql::ast::TriggerEnableMode::Always
            } else {
                crate::sql::ast::TriggerEnableMode::Origin
            })
        } else if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterEventTriggerAction::SetOwner(self.any_ident("role name")?)
        } else if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterEventTriggerAction::Rename(self.col_ident("new event trigger name")?)
        } else {
            return Err(self.err_here("expected an ALTER EVENT TRIGGER action"));
        };
        Ok(Stmt::AlterEventTrigger { name, action })
    }

    pub(super) fn drop_event_trigger(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.col_ident("event trigger name")?;
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        Ok(Stmt::DropEventTrigger {
            name,
            if_exists,
            cascade,
        })
    }

    fn aggregate_argument(&mut self) -> Result<AggregateArgument<'a>, ParseError> {
        let _ = self.eat_ident("in")?;
        let variadic = self.eat_ident("variadic")?;
        let first = self.any_ident("aggregate argument")?;
        let (name, type_name) = if self.peeked == Tok::Op("[") {
            self.advance()?;
            self.expect_op("]")?;
            while self.peeked == Tok::Op("[") {
                self.advance()?;
                self.expect_op("]")?;
            }
            (
                "",
                self.arena_str(stack_format!(132, "{}[]", first).as_str())?,
            )
        } else if matches!(self.peeked, Tok::Op(",") | Tok::Op(")"))
            || matches!(self.peeked, Tok::Ident("order"))
        {
            ("", first)
        } else {
            (first, self.type_name()?)
        };
        Ok(AggregateArgument {
            name,
            type_name,
            variadic,
        })
    }

    fn aggregate_arguments(&mut self) -> Result<AggregateArguments<'a>, ParseError> {
        if self.eat_op("*")? {
            self.expect_op(")")?;
            return Ok(AggregateArguments::Normal(&[]));
        }
        let mut direct = [AggregateArgument {
            name: "",
            type_name: "",
            variadic: false,
        }; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut aggregated = direct;
        let mut direct_count = 0usize;
        let mut aggregated_count = 0usize;
        let mut ordered_set = false;
        if !self.eat_op(")")? {
            loop {
                if self.eat_ident("order")? {
                    self.expect_ident("by")?;
                    ordered_set = true;
                    if self.eat_op(")")? {
                        break;
                    }
                    continue;
                }
                let target = if ordered_set {
                    &mut aggregated
                } else {
                    &mut direct
                };
                let count = if ordered_set {
                    &mut aggregated_count
                } else {
                    &mut direct_count
                };
                if *count == target.len() {
                    return Err(self.limit("aggregate arguments", target.len()));
                }
                target[*count] = self.aggregate_argument()?;
                *count += 1;
                if self.eat_op(")")? {
                    break;
                }
                if !ordered_set && self.eat_ident("order")? {
                    self.expect_ident("by")?;
                    ordered_set = true;
                    if self.eat_op(")")? {
                        break;
                    }
                    continue;
                }
                self.expect_op(",")?;
            }
        }
        if ordered_set {
            Ok(AggregateArguments::OrderedSet {
                direct: self.arena_slice(&direct[..direct_count])?,
                aggregated: self.arena_slice(&aggregated[..aggregated_count])?,
                hypothetical: false,
            })
        } else {
            Ok(AggregateArguments::Normal(
                self.arena_slice(&direct[..direct_count])?,
            ))
        }
    }

    fn aggregate_option_equals(&mut self) -> Result<(), ParseError> {
        let _ = self.eat_op("=")?;
        Ok(())
    }

    fn aggregate_space(&mut self) -> Result<u32, ParseError> {
        self.aggregate_option_equals()?;
        u32::try_from(self.seq_int()?)
            .map_err(|_| self.err_here("aggregate state space is out of range"))
    }

    fn aggregate_modify(&mut self) -> Result<AggregateFinalModify, ParseError> {
        self.aggregate_option_equals()?;
        if self.eat_ident("read_only")? {
            Ok(AggregateFinalModify::ReadOnly)
        } else if self.eat_ident("shareable")? {
            Ok(AggregateFinalModify::Shareable)
        } else if self.eat_ident("read_write")? {
            Ok(AggregateFinalModify::ReadWrite)
        } else {
            Err(self.unexpected("expected READ_ONLY, SHAREABLE, or READ_WRITE"))
        }
    }

    fn duplicate_aggregate_option(&self, name: &str) -> ParseError {
        let _ = name;
        self.err_here("aggregate option specified more than once")
    }

    fn create_aggregate(&mut self, or_replace: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("aggregate name")?;
        self.expect_op("(")?;
        let starts_old_options = matches!(
            self.peeked,
            Tok::Ident(
                "sfunc"
                    | "stype"
                    | "sspace"
                    | "finalfunc"
                    | "finalfunc_extra"
                    | "finalfunc_modify"
                    | "combinefunc"
                    | "serialfunc"
                    | "deserialfunc"
                    | "initcond"
                    | "msfunc"
                    | "minvfunc"
                    | "mstype"
                    | "msspace"
                    | "mfinalfunc"
                    | "mfinalfunc_extra"
                    | "mfinalfunc_modify"
                    | "minitcond"
                    | "sortop"
                    | "parallel"
                    | "hypothetical"
            )
        );
        let (mut arguments, old_syntax) = if self.eat_ident("basetype")? {
            self.aggregate_option_equals()?;
            let arguments = if self.eat_ident("any")? {
                AggregateArguments::Normal(&[])
            } else {
                let type_name = self.type_name()?;
                let argument = [AggregateArgument {
                    name: "",
                    type_name,
                    variadic: false,
                }];
                AggregateArguments::Normal(self.arena_slice(&argument)?)
            };
            self.expect_op(",")?;
            (arguments, true)
        } else if starts_old_options {
            let mut error = self.err_here("aggregate input type must be specified");
            error.sqlstate = sqlstate::INVALID_FUNCTION_DEFINITION;
            return Err(error);
        } else {
            let arguments = self.aggregate_arguments()?;
            self.expect_op("(")?;
            (arguments, false)
        };
        let mut options = AggregateOptions::EMPTY;
        loop {
            if self.eat_ident("sfunc")? {
                if options.transition.is_some() {
                    return Err(self.duplicate_aggregate_option("SFUNC"));
                }
                self.aggregate_option_equals()?;
                options.transition = Some(self.qual_name("transition function")?);
            } else if self.eat_ident("stype")? {
                if options.state_type.is_some() {
                    return Err(self.duplicate_aggregate_option("STYPE"));
                }
                self.aggregate_option_equals()?;
                options.state_type = Some(self.type_name()?);
            } else if self.eat_ident("sspace")? {
                if options.state_space.is_some() {
                    return Err(self.duplicate_aggregate_option("SSPACE"));
                }
                options.state_space = Some(self.aggregate_space()?);
            } else if self.eat_ident("finalfunc")? {
                if options.final_function.is_some() {
                    return Err(self.duplicate_aggregate_option("FINALFUNC"));
                }
                self.aggregate_option_equals()?;
                options.final_function = Some(self.qual_name("final function")?);
            } else if self.eat_ident("finalfunc_extra")? {
                if options.final_extra.replace(true).is_some() {
                    return Err(self.duplicate_aggregate_option("FINALFUNC_EXTRA"));
                }
            } else if self.eat_ident("finalfunc_modify")? {
                if options.final_modify.is_some() {
                    return Err(self.duplicate_aggregate_option("FINALFUNC_MODIFY"));
                }
                options.final_modify = Some(self.aggregate_modify()?);
            } else if self.eat_ident("combinefunc")? {
                if options.combine.is_some() {
                    return Err(self.duplicate_aggregate_option("COMBINEFUNC"));
                }
                self.aggregate_option_equals()?;
                options.combine = Some(self.qual_name("combine function")?);
            } else if self.eat_ident("serialfunc")? {
                if options.serial.is_some() {
                    return Err(self.duplicate_aggregate_option("SERIALFUNC"));
                }
                self.aggregate_option_equals()?;
                options.serial = Some(self.qual_name("serialization function")?);
            } else if self.eat_ident("deserialfunc")? {
                if options.deserial.is_some() {
                    return Err(self.duplicate_aggregate_option("DESERIALFUNC"));
                }
                self.aggregate_option_equals()?;
                options.deserial = Some(self.qual_name("deserialization function")?);
            } else if self.eat_ident("initcond")? {
                if options.initial_condition.is_some() {
                    return Err(self.duplicate_aggregate_option("INITCOND"));
                }
                self.aggregate_option_equals()?;
                options.initial_condition = Some(self.str_literal("initial condition")?);
            } else if self.eat_ident("msfunc")? {
                if options.moving_transition.is_some() {
                    return Err(self.duplicate_aggregate_option("MSFUNC"));
                }
                self.aggregate_option_equals()?;
                options.moving_transition = Some(self.qual_name("moving transition function")?);
            } else if self.eat_ident("minvfunc")? {
                if options.moving_inverse.is_some() {
                    return Err(self.duplicate_aggregate_option("MINVFUNC"));
                }
                self.aggregate_option_equals()?;
                options.moving_inverse = Some(self.qual_name("moving inverse function")?);
            } else if self.eat_ident("mstype")? {
                if options.moving_state_type.is_some() {
                    return Err(self.duplicate_aggregate_option("MSTYPE"));
                }
                self.aggregate_option_equals()?;
                options.moving_state_type = Some(self.type_name()?);
            } else if self.eat_ident("msspace")? {
                if options.moving_state_space.is_some() {
                    return Err(self.duplicate_aggregate_option("MSSPACE"));
                }
                options.moving_state_space = Some(self.aggregate_space()?);
            } else if self.eat_ident("mfinalfunc")? {
                if options.moving_final.is_some() {
                    return Err(self.duplicate_aggregate_option("MFINALFUNC"));
                }
                self.aggregate_option_equals()?;
                options.moving_final = Some(self.qual_name("moving final function")?);
            } else if self.eat_ident("mfinalfunc_extra")? {
                if options.moving_final_extra.replace(true).is_some() {
                    return Err(self.duplicate_aggregate_option("MFINALFUNC_EXTRA"));
                }
            } else if self.eat_ident("mfinalfunc_modify")? {
                if options.moving_final_modify.is_some() {
                    return Err(self.duplicate_aggregate_option("MFINALFUNC_MODIFY"));
                }
                options.moving_final_modify = Some(self.aggregate_modify()?);
            } else if self.eat_ident("minitcond")? {
                if options.moving_initial_condition.is_some() {
                    return Err(self.duplicate_aggregate_option("MINITCOND"));
                }
                self.aggregate_option_equals()?;
                options.moving_initial_condition =
                    Some(self.str_literal("moving initial condition")?);
            } else if self.eat_ident("sortop")? {
                if options.sort_operator.is_some() {
                    return Err(self.duplicate_aggregate_option("SORTOP"));
                }
                self.aggregate_option_equals()?;
                let operator = match self.peeked {
                    Tok::Ident(value) | Tok::Op(value) => value,
                    _ => return Err(self.unexpected("expected an operator")),
                };
                self.advance()?;
                options.sort_operator = Some(operator);
            } else if self.eat_ident("parallel")? {
                if options.parallel.is_some() {
                    return Err(self.duplicate_aggregate_option("PARALLEL"));
                }
                self.aggregate_option_equals()?;
                options.parallel = Some(if self.eat_ident("safe")? {
                    RoutineParallel::Safe
                } else if self.eat_ident("restricted")? {
                    RoutineParallel::Restricted
                } else if self.eat_ident("unsafe")? {
                    RoutineParallel::Unsafe
                } else {
                    return Err(self.unexpected("expected SAFE, RESTRICTED, or UNSAFE"));
                });
            } else if self.eat_ident("hypothetical")? {
                if options.hypothetical.replace(true).is_some() {
                    return Err(self.duplicate_aggregate_option("HYPOTHETICAL"));
                }
            } else {
                return Err(self.unexpected("expected an aggregate option"));
            }
            if self.eat_op(")")? {
                break;
            }
            self.expect_op(",")?;
        }
        let transition = options
            .transition
            .ok_or_else(|| self.err_here("aggregate SFUNC is required"))?;
        let state_type = options
            .state_type
            .ok_or_else(|| self.err_here("aggregate STYPE is required"))?;
        let ordered_set = matches!(arguments, AggregateArguments::OrderedSet { .. });
        let final_function = options.final_function.map(|function| AggregateFinal {
            function,
            extra: options.final_extra.unwrap_or(false),
            modify: options.final_modify.unwrap_or(if ordered_set {
                AggregateFinalModify::ReadWrite
            } else {
                AggregateFinalModify::ReadOnly
            }),
        });
        if final_function.is_none()
            && (options.final_extra.is_some() || options.final_modify.is_some())
        {
            return Err(self.err_here("FINALFUNC_EXTRA and FINALFUNC_MODIFY require FINALFUNC"));
        }
        if options.serial.is_some() != options.deserial.is_some() {
            return Err(self.err_here("SERIALFUNC and DESERIALFUNC must be specified together"));
        }
        if (options.serial.is_some() || options.deserial.is_some()) && options.combine.is_none() {
            return Err(self.err_here("SERIALFUNC and DESERIALFUNC require COMBINEFUNC"));
        }
        let partial = options.combine.map(|combine| AggregatePartial {
            combine,
            serial: options.serial,
            deserial: options.deserial,
        });
        let any_moving = options.moving_transition.is_some()
            || options.moving_inverse.is_some()
            || options.moving_state_type.is_some()
            || options.moving_state_space.is_some()
            || options.moving_final.is_some()
            || options.moving_final_extra.is_some()
            || options.moving_final_modify.is_some()
            || options.moving_initial_condition.is_some();
        let moving = if any_moving {
            let transition = options
                .moving_transition
                .ok_or_else(|| self.err_here("MSFUNC is required for moving aggregation"))?;
            let inverse = options
                .moving_inverse
                .ok_or_else(|| self.err_here("MINVFUNC is required for moving aggregation"))?;
            let state_type = options
                .moving_state_type
                .ok_or_else(|| self.err_here("MSTYPE is required for moving aggregation"))?;
            let final_function = options.moving_final.map(|function| AggregateFinal {
                function,
                extra: options.moving_final_extra.unwrap_or(false),
                modify: options
                    .moving_final_modify
                    .unwrap_or(AggregateFinalModify::ReadOnly),
            });
            if final_function.is_none()
                && (options.moving_final_extra.is_some() || options.moving_final_modify.is_some())
            {
                return Err(
                    self.err_here("MFINALFUNC_EXTRA and MFINALFUNC_MODIFY require MFINALFUNC")
                );
            }
            Some(AggregateMoving {
                transition,
                inverse,
                state_type,
                state_space: options.moving_state_space,
                final_function,
                initial_condition: options.moving_initial_condition,
            })
        } else {
            None
        };
        if options.hypothetical.unwrap_or(false) {
            arguments = match arguments {
                AggregateArguments::OrderedSet {
                    direct, aggregated, ..
                } => AggregateArguments::OrderedSet {
                    direct,
                    aggregated,
                    hypothetical: true,
                },
                AggregateArguments::Normal(_) => {
                    return Err(self.err_here("HYPOTHETICAL requires ordered-set arguments"));
                }
            };
        }
        if old_syntax && matches!(arguments, AggregateArguments::OrderedSet { .. }) {
            return Err(
                self.err_here("old aggregate syntax cannot define an ordered-set aggregate")
            );
        }
        Ok(Stmt::CreateAggregate(crate::sql::ast::CreateAggregate {
            name,
            or_replace,
            arguments,
            definition: AggregateDefinition {
                transition,
                state_type,
                state_space: options.state_space,
                final_function,
                partial,
                moving,
                initial_condition: options.initial_condition,
                sort_operator: options.sort_operator,
                parallel: options.parallel.unwrap_or(RoutineParallel::Unsafe),
            },
        }))
    }

    pub(super) fn alter_trigger(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("trigger name")?;
        self.expect_ident("on")?;
        let table = self.qual_name("trigger table")?;
        let action = if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterTriggerAction::Rename(self.col_ident("new trigger name")?)
        } else {
            let enabled = !self.eat_ident("no")?;
            self.expect_ident("depends")?;
            self.expect_ident("on")?;
            self.expect_ident("extension")?;
            AlterTriggerAction::DependsOnExtension {
                extension: self.col_ident("extension name")?,
                enabled,
            }
        };
        Ok(Stmt::AlterTrigger {
            trigger: TriggerIdentity { name, table },
            action,
        })
    }

    pub(super) fn alter_index(&mut self) -> Result<Stmt<'a>, ParseError> {
        if self.eat_ident("all")? {
            self.expect_ident("in")?;
            self.expect_ident("tablespace")?;
            let source = self.any_ident("tablespace name")?;
            let owners = if self.eat_ident("owned")? {
                self.expect_ident("by")?;
                self.role_name_list("role name")?
            } else {
                &[]
            };
            self.expect_ident("set")?;
            self.expect_ident("tablespace")?;
            let target = self.any_ident("tablespace name")?;
            let nowait = self.eat_ident("nowait")?;
            return Ok(Stmt::AlterIndexesTablespace {
                source,
                owners,
                target,
                nowait,
            });
        }
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.qual_name("index name")?;
        if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            return Ok(Stmt::AlterIndex {
                name,
                if_exists,
                action: AlterIndexAction::Rename(self.col_ident("new index name")?),
            });
        }
        let action = if self.eat_ident("set")? {
            if self.eat_ident("tablespace")? {
                AlterIndexAction::SetTablespace(self.any_ident("tablespace name")?)
            } else {
                self.expect_op("(")?;
                let options = self.index_storage_options()?;
                self.expect_op(")")?;
                AlterIndexAction::SetOptions(options)
            }
        } else if self.eat_ident("reset")? {
            self.expect_op("(")?;
            let options = self.index_storage_option_names()?;
            self.expect_op(")")?;
            AlterIndexAction::ResetOptions(options)
        } else if self.eat_ident("alter")? {
            let _ = self.eat_ident("column")?;
            let column = self.seq_int()?;
            let column = u16::try_from(column)
                .map_err(|_| self.err_here("index column number is out of range"))?;
            self.expect_ident("set")?;
            self.expect_ident("statistics")?;
            let target = self.seq_int()?;
            let target = i16::try_from(target)
                .map_err(|_| self.err_here("statistics target is out of range"))?;
            AlterIndexAction::SetStatistics { column, target }
        } else if self.eat_ident("attach")? {
            self.expect_ident("partition")?;
            AlterIndexAction::AttachPartition(self.qual_name("partition index name")?)
        } else if self.peeked == Tok::Ident("depends") || self.peeked == Tok::Ident("no") {
            if if_exists {
                return Err(self.err_here("IF EXISTS is not allowed with DEPENDS ON EXTENSION"));
            }
            let enabled = !self.eat_ident("no")?;
            self.expect_ident("depends")?;
            self.expect_ident("on")?;
            self.expect_ident("extension")?;
            AlterIndexAction::ExtensionDependency {
                extension: self.col_ident("extension name")?,
                enabled,
            }
        } else {
            return Err(self.err_here("expected an ALTER INDEX action"));
        };
        Ok(Stmt::AlterIndex {
            name,
            if_exists,
            action,
        })
    }

    fn create_collation(&mut self) -> Result<Stmt<'a>, ParseError> {
        use crate::sql::ast::{CreateCollation, CreateCollationDefinition};
        let if_not_exists = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.qual_name("collation name")?;
        let definition = if self.eat_ident("from")? {
            CreateCollationDefinition::From(self.qual_name("source collation")?)
        } else {
            self.expect_op("(")?;
            let mut locale = None;
            let mut lc_collate = None;
            let mut lc_ctype = None;
            let mut provider = None;
            let mut deterministic = None;
            let mut rules = None;
            let mut version = None;
            if !self.eat_op(")")? {
                loop {
                    let option = self.any_ident("collation option")?;
                    self.expect_op("=")?;
                    let value = match self.peeked {
                        Tok::Ident(value) | Tok::Str(value) => value,
                        _ => return Err(self.err_here("collation option value is required")),
                    };
                    self.advance()?;
                    let duplicate = if option.eq_ignore_ascii_case("locale") {
                        locale.replace(value).is_some()
                    } else if option.eq_ignore_ascii_case("lc_collate") {
                        lc_collate.replace(value).is_some()
                    } else if option.eq_ignore_ascii_case("lc_ctype") {
                        lc_ctype.replace(value).is_some()
                    } else if option.eq_ignore_ascii_case("provider") {
                        let value = if value.eq_ignore_ascii_case("builtin") {
                            crate::sql::ast::ParsedCollationProvider::Builtin
                        } else if value.eq_ignore_ascii_case("libc") {
                            crate::sql::ast::ParsedCollationProvider::Libc
                        } else if value.eq_ignore_ascii_case("icu") {
                            crate::sql::ast::ParsedCollationProvider::Icu
                        } else {
                            return Err(self.err_here("unrecognized collation provider"));
                        };
                        provider.replace(value).is_some()
                    } else if option.eq_ignore_ascii_case("deterministic") {
                        let parsed = if value == "1"
                            || value.eq_ignore_ascii_case("true")
                            || value.eq_ignore_ascii_case("on")
                        {
                            true
                        } else if value == "0"
                            || value.eq_ignore_ascii_case("false")
                            || value.eq_ignore_ascii_case("off")
                        {
                            false
                        } else {
                            return Err(self.err_here("DETERMINISTIC requires a boolean value"));
                        };
                        deterministic.replace(parsed).is_some()
                    } else if option.eq_ignore_ascii_case("rules") {
                        rules.replace(value).is_some()
                    } else if option.eq_ignore_ascii_case("version") {
                        version.replace(value).is_some()
                    } else {
                        return Err(self.err_here("unrecognized collation option"));
                    };
                    if duplicate {
                        return Err(self.err_here("conflicting or redundant collation options"));
                    }
                    if self.eat_op(")")? {
                        break;
                    }
                    self.expect_op(",")?;
                }
            }
            CreateCollationDefinition::Options {
                locale,
                lc_collate,
                lc_ctype,
                provider,
                deterministic,
                rules,
                version,
            }
        };
        Ok(Stmt::CreateCollation(CreateCollation {
            name,
            if_not_exists,
            definition,
        }))
    }

    fn create_conversion(&mut self, default: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("conversion name")?;
        self.expect_ident("for")?;
        let source_encoding = self.str_literal("source encoding")?;
        let source_encoding = crate::storage::PgEncoding::parse(source_encoding)
            .ok_or_else(|| self.err_here("invalid source encoding"))?;
        self.expect_ident("to")?;
        let destination_encoding = self.str_literal("destination encoding")?;
        let destination_encoding = crate::storage::PgEncoding::parse(destination_encoding)
            .ok_or_else(|| self.err_here("invalid destination encoding"))?;
        self.expect_ident("from")?;
        let function = self.qual_name("conversion function")?;
        Ok(Stmt::CreateConversion(crate::sql::ast::CreateConversion {
            default,
            name,
            source_encoding,
            destination_encoding,
            function,
        }))
    }

    pub(super) fn text_search_options(&mut self) -> Result<&'a [TextSearchOption<'a>], ParseError> {
        let mut options = [TextSearchOption {
            name: "",
            value: "",
        }; 32];
        let mut count = 0usize;
        loop {
            if count == options.len() {
                return Err(self.err_here("too many text search options"));
            }
            let name = self.col_ident("text search option name")?;
            self.expect_op("=")?;
            let value = self.database_option_value("text search option value is required")?;
            if options[..count]
                .iter()
                .any(|option| option.name.eq_ignore_ascii_case(name))
            {
                return Err(self.err_here("duplicate text search option"));
            }
            options[count] = TextSearchOption { name, value };
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.arena_slice(&options[..count])
    }

    fn create_text_search(&mut self) -> Result<Stmt<'a>, ParseError> {
        if self.eat_ident("parser")? {
            let name = self.any_qual_name("text search parser name")?;
            self.expect_op("(")?;
            let mut start = None;
            let mut gettoken = None;
            let mut end = None;
            let mut headline = None;
            let mut lextypes = None;
            loop {
                let parameter = self.any_ident("text search parser parameter")?;
                self.expect_op("=")?;
                let routine = self.any_qual_name("text search parser routine")?;
                let destination = if parameter.eq_ignore_ascii_case("start") {
                    &mut start
                } else if parameter.eq_ignore_ascii_case("gettoken") {
                    &mut gettoken
                } else if parameter.eq_ignore_ascii_case("end") {
                    &mut end
                } else if parameter.eq_ignore_ascii_case("headline") {
                    &mut headline
                } else if parameter.eq_ignore_ascii_case("lextypes") {
                    &mut lextypes
                } else {
                    return Err(self.err_here("unrecognized text search parser parameter"));
                };
                if destination.replace(routine).is_some() {
                    return Err(self.err_here("duplicate text search parser parameter"));
                }
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
            return Ok(Stmt::CreateTextSearchParser(CreateTextSearchParser {
                name,
                start: start.ok_or_else(|| self.err_here("START is required"))?,
                gettoken: gettoken.ok_or_else(|| self.err_here("GETTOKEN is required"))?,
                end: end.ok_or_else(|| self.err_here("END is required"))?,
                headline,
                lextypes: lextypes.ok_or_else(|| self.err_here("LEXTYPES is required"))?,
            }));
        }
        if self.eat_ident("template")? {
            let name = self.any_qual_name("text search template name")?;
            self.expect_op("(")?;
            let mut init = None;
            let mut lexize = None;
            loop {
                let parameter = self.any_ident("text search template parameter")?;
                self.expect_op("=")?;
                let routine = self.any_qual_name("text search template routine")?;
                if parameter.eq_ignore_ascii_case("init") {
                    if init.replace(routine).is_some() {
                        return Err(self.err_here("duplicate INIT parameter"));
                    }
                } else if parameter.eq_ignore_ascii_case("lexize") {
                    if lexize.replace(routine).is_some() {
                        return Err(self.err_here("duplicate LEXIZE parameter"));
                    }
                } else {
                    return Err(self.err_here("unrecognized text search template parameter"));
                }
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
            return Ok(Stmt::CreateTextSearchTemplate(CreateTextSearchTemplate {
                name,
                init,
                lexize: lexize.ok_or_else(|| self.err_here("LEXIZE is required"))?,
            }));
        }
        if self.eat_ident("dictionary")? {
            let name = self.any_qual_name("text search dictionary name")?;
            self.expect_op("(")?;
            self.expect_ident("template")?;
            self.expect_op("=")?;
            let template = self.any_qual_name("text search template name")?;
            let options = if self.eat_op(",")? {
                self.text_search_options()?
            } else {
                &[]
            };
            self.expect_op(")")?;
            return Ok(Stmt::CreateTextSearchDictionary(
                CreateTextSearchDictionary {
                    name,
                    template,
                    options,
                },
            ));
        }
        if self.eat_ident("configuration")? {
            let name = self.any_qual_name("text search configuration name")?;
            self.expect_op("(")?;
            let source = if self.eat_ident("parser")? {
                self.expect_op("=")?;
                TextSearchConfigurationSource::Parser(
                    self.any_qual_name("text search parser name")?,
                )
            } else if self.eat_ident("copy")? {
                self.expect_op("=")?;
                TextSearchConfigurationSource::Copy(
                    self.any_qual_name("text search configuration name")?,
                )
            } else {
                return Err(self.err_here("expected PARSER or COPY"));
            };
            self.expect_op(")")?;
            return Ok(Stmt::CreateTextSearchConfiguration(
                CreateTextSearchConfiguration { name, source },
            ));
        }
        Err(self.err_here("expected PARSER, TEMPLATE, DICTIONARY, or CONFIGURATION"))
    }

    pub(super) fn foreign_options(&mut self) -> Result<&'a [ForeignOption<'a>], ParseError> {
        self.expect_op("(")?;
        let mut options = [ForeignOption {
            name: "",
            value: "",
        }; MAX_LIST];
        let mut count = 0usize;
        if self.eat_op(")")? {
            return Ok(&[]);
        }
        loop {
            if count == options.len() {
                return Err(self.limit("foreign options", options.len()));
            }
            let name = self.any_ident("foreign option name")?;
            if options[..count].iter().any(|option| option.name == name) {
                return Err(self.err_here("foreign option specified more than once"));
            }
            options[count] = ForeignOption {
                name,
                value: self.str_literal("foreign option value")?,
            };
            count += 1;
            if self.eat_op(")")? {
                break;
            }
            self.expect_op(",")?;
        }
        self.arena_slice(&options[..count])
    }

    pub(super) fn alter_foreign_options(
        &mut self,
    ) -> Result<&'a [ForeignOptionAction<'a>], ParseError> {
        self.expect_op("(")?;
        let mut options = [ForeignOptionAction::Drop(""); MAX_LIST];
        let mut names = [""; MAX_LIST];
        let mut count = 0usize;
        if self.eat_op(")")? {
            return Ok(&[]);
        }
        loop {
            if count == options.len() {
                return Err(self.limit("foreign option alterations", options.len()));
            }
            let operation = if self.eat_ident("set")? {
                1u8
            } else if self.eat_ident("drop")? {
                2u8
            } else {
                let _ = self.eat_ident("add")?;
                0u8
            };
            let name = self.any_ident("foreign option name")?;
            if names[..count].contains(&name) {
                return Err(self.err_here("foreign option specified more than once"));
            }
            names[count] = name;
            options[count] = if operation == 2 {
                ForeignOptionAction::Drop(name)
            } else {
                let option = ForeignOption {
                    name,
                    value: self.str_literal("foreign option value")?,
                };
                if operation == 1 {
                    ForeignOptionAction::Set(option)
                } else {
                    ForeignOptionAction::Add(option)
                }
            };
            count += 1;
            if self.eat_op(")")? {
                break;
            }
            self.expect_op(",")?;
        }
        self.arena_slice(&options[..count])
    }

    fn foreign_user(&mut self) -> Result<ForeignUser<'a>, ParseError> {
        if self.eat_ident("current_role")? {
            Ok(ForeignUser::CurrentRole)
        } else if self.eat_ident("current_user")? {
            Ok(ForeignUser::CurrentUser)
        } else if self.eat_ident("user")? {
            Ok(ForeignUser::User)
        } else if self.eat_ident("public")? {
            Ok(ForeignUser::Public)
        } else {
            Ok(ForeignUser::Named(self.col_ident("user mapping role")?))
        }
    }

    fn create_foreign_data_wrapper(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("foreign-data wrapper name")?;
        let handler = if self.eat_ident("handler")? {
            ForeignDataHandler::Function(self.qual_name("foreign-data wrapper handler")?)
        } else if self.eat_ident("no")? {
            self.expect_ident("handler")?;
            ForeignDataHandler::None
        } else {
            ForeignDataHandler::None
        };
        let validator = if self.eat_ident("validator")? {
            ForeignDataValidator::Function(self.qual_name("foreign-data wrapper validator")?)
        } else if self.eat_ident("no")? {
            self.expect_ident("validator")?;
            ForeignDataValidator::None
        } else {
            ForeignDataValidator::None
        };
        let options = if self.eat_ident("options")? {
            self.foreign_options()?
        } else {
            &[]
        };
        Ok(Stmt::CreateForeignDataWrapper(CreateForeignDataWrapper {
            name,
            handler,
            validator,
            options,
        }))
    }

    fn create_foreign_server(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_not_exists = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.col_ident("foreign server name")?;
        let server_type = if self.eat_ident("type")? {
            Some(self.str_literal("foreign server type")?)
        } else {
            None
        };
        let version = if self.eat_ident("version")? {
            Some(self.str_literal("foreign server version")?)
        } else {
            None
        };
        self.expect_ident("foreign")?;
        self.expect_ident("data")?;
        self.expect_ident("wrapper")?;
        let wrapper = self.col_ident("foreign-data wrapper name")?;
        let options = if self.eat_ident("options")? {
            self.foreign_options()?
        } else {
            &[]
        };
        Ok(Stmt::CreateForeignServer(CreateForeignServer {
            name,
            if_not_exists,
            server_type,
            version,
            wrapper,
            options,
        }))
    }

    fn create_user_mapping(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_not_exists = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        self.expect_ident("for")?;
        let user = self.foreign_user()?;
        self.expect_ident("server")?;
        let server = self.col_ident("foreign server name")?;
        let options = if self.eat_ident("options")? {
            self.foreign_options()?
        } else {
            &[]
        };
        Ok(Stmt::CreateUserMapping(
            crate::sql::ast::CreateUserMapping {
                user,
                server,
                if_not_exists,
                options,
            },
        ))
    }

    pub(super) fn alter_foreign_data_wrapper(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("foreign-data wrapper name")?;
        let action = if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterForeignDataWrapperAction::Owner(self.any_ident("role name")?)
        } else if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterForeignDataWrapperAction::Rename(self.col_ident("new foreign-data wrapper name")?)
        } else {
            let handler = if self.eat_ident("handler")? {
                Some(ForeignDataHandler::Function(
                    self.qual_name("foreign-data wrapper handler")?,
                ))
            } else if self.eat_ident("no")? {
                self.expect_ident("handler")?;
                Some(ForeignDataHandler::None)
            } else {
                None
            };
            let validator = if self.eat_ident("validator")? {
                Some(ForeignDataValidator::Function(
                    self.qual_name("foreign-data wrapper validator")?,
                ))
            } else if self.eat_ident("no")? {
                self.expect_ident("validator")?;
                Some(ForeignDataValidator::None)
            } else {
                None
            };
            let options = if self.eat_ident("options")? {
                self.alter_foreign_options()?
            } else {
                &[]
            };
            if handler.is_none() && validator.is_none() && options.is_empty() {
                return Err(self.err_here("expected a foreign-data wrapper alteration"));
            }
            AlterForeignDataWrapperAction::Definition {
                handler,
                validator,
                options,
            }
        };
        Ok(Stmt::AlterForeignDataWrapper { name, action })
    }

    pub(super) fn alter_foreign_server(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("foreign server name")?;
        let action = if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterForeignServerAction::Owner(self.any_ident("role name")?)
        } else if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterForeignServerAction::Rename(self.col_ident("new foreign server name")?)
        } else {
            let version = if self.eat_ident("version")? {
                if self.eat_ident("null")? {
                    Some(None)
                } else {
                    Some(Some(self.str_literal("foreign server version")?))
                }
            } else {
                None
            };
            let options = if self.eat_ident("options")? {
                self.alter_foreign_options()?
            } else {
                &[]
            };
            if version.is_none() && options.is_empty() {
                return Err(self.err_here("expected a foreign server alteration"));
            }
            AlterForeignServerAction::Definition { version, options }
        };
        Ok(Stmt::AlterForeignServer { name, action })
    }

    pub(super) fn alter_user_mapping(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("for")?;
        let user = self.foreign_user()?;
        self.expect_ident("server")?;
        let server = self.col_ident("foreign server name")?;
        self.expect_ident("options")?;
        Ok(Stmt::AlterUserMapping(crate::sql::ast::AlterUserMapping {
            user,
            server,
            options: self.alter_foreign_options()?,
        }))
    }

    pub(super) fn import_foreign_schema(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("import")?;
        self.expect_ident("foreign")?;
        self.expect_ident("schema")?;
        let remote_schema = self.col_ident("remote schema name")?;
        let selection = if self.eat_ident("limit")? {
            self.expect_ident("to")?;
            ForeignSchemaSelection::LimitTo(self.column_name_list()?)
        } else if self.eat_ident("except")? {
            ForeignSchemaSelection::Except(self.column_name_list()?)
        } else {
            ForeignSchemaSelection::All
        };
        self.expect_ident("from")?;
        self.expect_ident("server")?;
        let server = self.col_ident("foreign server name")?;
        self.expect_ident("into")?;
        let local_schema = self.col_ident("local schema name")?;
        let options = if self.eat_ident("options")? {
            self.foreign_options()?
        } else {
            &[]
        };
        Ok(Stmt::ImportForeignSchema(ImportForeignSchema {
            remote_schema,
            selection,
            server,
            local_schema,
            options,
        }))
    }

    /// Dispatches CREATE: `[OR REPLACE] VIEW`, `TABLE`, `INDEX` or `SCHEMA`
    /// ("create" consumed here).
    pub(super) fn create(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("create")?;
        let or_replace = if self.eat_ident("or")? {
            self.expect_ident("replace")?;
            true
        } else {
            false
        };
        if or_replace {
            if self.eat_ident("rule")? {
                return self.create_rule(true);
            }
            if self.eat_ident("view")? {
                return self.create_view(true);
            }
            if self.eat_ident("function")? {
                return self.create_routine(true, true);
            }
            if self.eat_ident("procedure")? {
                return self.create_routine(true, false);
            }
            if self.eat_ident("aggregate")? {
                return self.create_aggregate(true);
            }
            if self.eat_ident("trigger")? {
                return self.create_trigger(true, false);
            }
            let trusted = self.eat_ident("trusted")?;
            let _procedural = self.eat_ident("procedural")?;
            if trusted || _procedural || self.eat_ident("language")? {
                if trusted || _procedural {
                    self.expect_ident("language")?;
                }
                return self.create_language(true, trusted);
            }
            return Err(self.unexpected(
                "expected RULE, VIEW, FUNCTION, PROCEDURE, AGGREGATE, or TRIGGER after CREATE OR REPLACE",
            ));
        }
        if self.eat_ident("unique")? {
            self.expect_ident("index")?;
            return self.create_index(true);
        }
        if self.eat_ident("default")? {
            self.expect_ident("conversion")?;
            return self.create_conversion(true);
        }
        let trusted = self.eat_ident("trusted")?;
        let procedural = self.eat_ident("procedural")?;
        if trusted || procedural || self.eat_ident("language")? {
            if trusted || procedural {
                self.expect_ident("language")?;
            }
            return self.create_language(false, trusted);
        }
        if self.eat_ident("view")? {
            return self.create_view(false);
        }
        if self.eat_ident("rule")? {
            return self.create_rule(false);
        }
        if self.eat_ident("collation")? {
            return self.create_collation();
        }
        if self.eat_ident("text")? {
            self.expect_ident("search")?;
            return self.create_text_search();
        }
        if self.eat_ident("conversion")? {
            return self.create_conversion(false);
        }
        if self.eat_ident("foreign")? {
            if self.eat_ident("data")? {
                self.expect_ident("wrapper")?;
                return self.create_foreign_data_wrapper();
            }
            return self.create_table(true, RelationPersistence::Permanent);
        }
        if self.eat_ident("server")? {
            return self.create_foreign_server();
        }
        if self.eat_ident("user")? {
            if self.eat_ident("mapping")? {
                return self.create_user_mapping();
            }
            return self.create_role(true);
        }
        if self.eat_ident("function")? {
            return self.create_routine(false, true);
        }
        if self.eat_ident("procedure")? {
            return self.create_routine(false, false);
        }
        if self.eat_ident("aggregate")? {
            return self.create_aggregate(false);
        }
        if self.eat_ident("cast")? {
            return self.create_cast();
        }
        if self.eat_ident("operator")? {
            if self.eat_ident("family")? {
                return self.create_operator_family();
            }
            if self.eat_ident("class")? {
                return self.create_operator_class();
            }
            return self.create_operator();
        }
        if self.eat_ident("extension")? {
            return self.create_extension();
        }
        if self.eat_ident("publication")? {
            return self.create_publication();
        }
        if self.eat_ident("subscription")? {
            return self.create_subscription();
        }
        if self.eat_ident("event")? {
            self.expect_ident("trigger")?;
            return self.create_event_trigger();
        }
        if self.eat_ident("trigger")? {
            return self.create_trigger(false, false);
        }
        if self.eat_ident("constraint")? {
            self.expect_ident("trigger")?;
            return self.create_trigger(false, true);
        }
        if self.eat_ident("policy")? {
            return self.create_policy();
        }
        if self.eat_ident("statistics")? {
            return self.create_statistics();
        }
        if self.eat_ident("materialized")? {
            self.expect_ident("view")?;
            return self.create_materialized_view();
        }
        if self.eat_ident("index")? {
            return self.create_index(false);
        }
        if self.eat_ident("tablespace")? {
            return self.create_tablespace();
        }
        if self.eat_ident("database")? {
            return self.create_database();
        }
        if self.eat_ident("schema")? {
            return self.create_schema();
        }
        if self.eat_ident("sequence")? {
            return self.create_sequence();
        }
        if self.eat_ident("domain")? {
            return self.create_domain();
        }
        if self.eat_ident("type")? {
            return self.create_type();
        }
        if self.eat_ident("role")? {
            return self.create_role(false);
        }
        if self.eat_ident("group")? {
            return self.create_role(false);
        }
        let persistence = if self.eat_ident("unlogged")? {
            RelationPersistence::Unlogged
        } else if self.eat_ident("temporary")? || self.eat_ident("temp")? {
            RelationPersistence::Temporary
        } else {
            RelationPersistence::Permanent
        };
        self.create_table(false, persistence)
    }

    pub(super) fn access_method(&mut self) -> Result<IndexAccessMethod, ParseError> {
        let method = self.col_ident("index access method")?;
        if method.eq_ignore_ascii_case("btree") {
            Ok(IndexAccessMethod::Btree)
        } else {
            Err(ParseError {
                at: self.peek_at,
                message: stack_format!(
                    96,
                    "access method \"{}\" is not supported; pos3ql models btree",
                    method
                ),
                sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
            })
        }
    }

    pub(super) fn unmodified_type_name(&mut self) -> Result<&'a str, ParseError> {
        let (name, modifier) = self.type_name_mod()?;
        if modifier != -1 {
            return Err(self.err_here("type modifiers are not allowed in this definition"));
        }
        Ok(name)
    }

    fn optional_type_signature(&mut self) -> Result<&'a [&'a str], ParseError> {
        if !self.eat_op("(")? {
            return Ok(&[]);
        }
        let mut types = [""; MAX_LIST];
        let mut count = 0usize;
        if !self.eat_op(")")? {
            loop {
                if count == types.len() {
                    return Err(self.limit("type signature", types.len()));
                }
                types[count] = self.unmodified_type_name()?;
                count += 1;
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        self.arena_slice(&types[..count])
    }

    fn operator_qual_name(&mut self, what: &'static str) -> Result<QualName<'a>, ParseError> {
        let wrapped = self.eat_ident("operator")?;
        if wrapped {
            self.expect_op("(")?;
        }
        let first = self.any_op_token()?;
        let qualified = if self.eat_op(".")? {
            let name = self.any_op_token()?;
            QualName {
                schema: Some(first),
                name,
            }
        } else {
            if first
                .chars()
                .any(|character| character.is_alphanumeric() || character == '_')
            {
                return Err(self.err_here(what));
            }
            QualName::bare(first)
        };
        if wrapped {
            self.expect_op(")")?;
        }
        Ok(qualified)
    }

    fn operator_operand_type(&mut self) -> Result<Option<&'a str>, ParseError> {
        if self.eat_ident("none")? {
            Ok(None)
        } else {
            Ok(Some(self.unmodified_type_name()?))
        }
    }

    pub(super) fn operator_identity(&mut self) -> Result<OperatorIdentity<'a>, ParseError> {
        let name = self.operator_qual_name("operator name is invalid")?;
        self.expect_op("(")?;
        let left_type = self.operator_operand_type()?;
        self.expect_op(",")?;
        let right_type = self.operator_operand_type()?;
        self.expect_op(")")?;
        let operands = match (left_type, right_type) {
            (None, Some(right)) => OperatorOperands::Prefix(right),
            (Some(left), Some(right)) => OperatorOperands::Binary { left, right },
            (Some(_), None) => return Err(self.err_here("postfix operators are not supported")),
            (None, None) => return Err(self.err_here("operator must have at least one argument")),
        };
        Ok(OperatorIdentity { name, operands })
    }

    fn btree_strategy(&mut self) -> Result<BtreeStrategy, ParseError> {
        let Tok::Num(number) = self.peeked else {
            return Err(self.err_here("btree strategy number is required"));
        };
        self.advance()?;
        let number = number
            .parse::<u32>()
            .ok()
            .and_then(BtreeStrategy::from_number)
            .ok_or_else(|| self.err_here("btree strategy number must be between 1 and 5"))?;
        Ok(number)
    }

    fn create_cast(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_op("(")?;
        let source_type = self.unmodified_type_name()?;
        self.expect_ident("as")?;
        let target_type = self.unmodified_type_name()?;
        self.expect_op(")")?;
        let method = if self.eat_ident("with")? {
            if self.eat_ident("inout")? {
                CastMethod::InOut
            } else {
                self.expect_ident("function")?;
                CastMethod::Function {
                    name: self.qual_name("cast function")?,
                    argument_types: self.optional_type_signature()?,
                }
            }
        } else {
            self.expect_ident("without")?;
            self.expect_ident("function")?;
            CastMethod::Binary
        };
        let context = if self.eat_ident("as")? {
            if self.eat_ident("assignment")? {
                CastContext::Assignment
            } else {
                self.expect_ident("implicit")?;
                CastContext::Implicit
            }
        } else {
            CastContext::Explicit
        };
        Ok(Stmt::CreateCast(CreateCast {
            source_type,
            target_type,
            method,
            context,
        }))
    }

    fn create_operator(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.operator_qual_name("operator name is invalid")?;
        self.expect_op("(")?;
        let mut function = None;
        let mut left_type = None;
        let mut right_type = None;
        let mut commutator = None;
        let mut negator = None;
        let mut hashes = false;
        let mut merges = false;
        loop {
            if self.eat_ident("function")? || self.eat_ident("procedure")? {
                self.expect_op("=")?;
                if function
                    .replace(self.qual_name("operator function")?)
                    .is_some()
                {
                    return Err(self.err_here("operator FUNCTION specified more than once"));
                }
            } else if self.eat_ident("leftarg")? {
                self.expect_op("=")?;
                if left_type.replace(self.unmodified_type_name()?).is_some() {
                    return Err(self.err_here("operator LEFTARG specified more than once"));
                }
            } else if self.eat_ident("rightarg")? {
                self.expect_op("=")?;
                if right_type.replace(self.unmodified_type_name()?).is_some() {
                    return Err(self.err_here("operator RIGHTARG specified more than once"));
                }
            } else if self.eat_ident("commutator")? {
                self.expect_op("=")?;
                if commutator
                    .replace(self.operator_qual_name("commutator name is invalid")?)
                    .is_some()
                {
                    return Err(self.err_here("operator COMMUTATOR specified more than once"));
                }
            } else if self.eat_ident("negator")? {
                self.expect_op("=")?;
                if negator
                    .replace(self.operator_qual_name("negator name is invalid")?)
                    .is_some()
                {
                    return Err(self.err_here("operator NEGATOR specified more than once"));
                }
            } else if self.eat_ident("hashes")? {
                if hashes {
                    return Err(self.err_here("operator HASHES specified more than once"));
                }
                hashes = true;
            } else if self.eat_ident("merges")? {
                if merges {
                    return Err(self.err_here("operator MERGES specified more than once"));
                }
                merges = true;
            } else if self.eat_ident("restrict")? || self.eat_ident("join")? {
                self.expect_op("=")?;
                let _ = self.qual_name("selectivity function")?;
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(
                        96,
                        "custom selectivity functions are not supported by the bounded planner"
                    ),
                    sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                });
            } else {
                return Err(self.err_here("invalid CREATE OPERATOR option"));
            }
            if self.eat_op(")")? {
                break;
            }
            self.expect_op(",")?;
        }
        let function = function.ok_or_else(|| self.err_here("operator FUNCTION is required"))?;
        let operands = match (left_type, right_type) {
            (None, Some(right)) => OperatorOperands::Prefix(right),
            (Some(left), Some(right)) => OperatorOperands::Binary { left, right },
            (Some(_), None) => return Err(self.err_here("postfix operators are not supported")),
            (None, None) => return Err(self.err_here("operator must have at least one argument")),
        };
        Ok(Stmt::CreateOperator(CreateOperator {
            name,
            function,
            operands,
            commutator,
            negator,
            hashes,
            merges,
        }))
    }

    fn create_operator_family(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("operator family name")?;
        self.expect_ident("using")?;
        Ok(Stmt::CreateOperatorFamily {
            name,
            method: self.access_method()?,
        })
    }

    fn create_operator_class(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("operator class name")?;
        let default = self.eat_ident("default")?;
        self.expect_ident("for")?;
        self.expect_ident("type")?;
        let input_type = self.unmodified_type_name()?;
        self.expect_ident("using")?;
        let method = self.access_method()?;
        let family = if self.eat_ident("family")? {
            Some(self.qual_name("operator family name")?)
        } else {
            None
        };
        self.expect_ident("as")?;
        let mut members = [OperatorClassMember::Storage("bool"); MAX_LIST];
        let mut count = 0usize;
        loop {
            if count == members.len() {
                return Err(self.limit("operator class members", members.len()));
            }
            members[count] = if self.eat_ident("operator")? {
                let strategy = self.btree_strategy()?;
                let operator = self.operator_qual_name("operator class operator is invalid")?;
                let operand_types = if self.eat_op("(")? {
                    let left = self.unmodified_type_name()?;
                    self.expect_op(",")?;
                    let right = self.unmodified_type_name()?;
                    self.expect_op(")")?;
                    Some((left, right))
                } else {
                    None
                };
                if self.eat_ident("for")? {
                    if self.eat_ident("order")? {
                        self.expect_ident("by")?;
                        let _ = self.qual_name("sort operator family")?;
                        return Err(ParseError {
                            at: self.peek_at,
                            message: stack_format!(
                                96,
                                "btree ordering operators are not supported in a search operator class"
                            ),
                            sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                        });
                    }
                    self.expect_ident("search")?;
                }
                OperatorClassMember::Operator {
                    strategy,
                    operator,
                    operand_types,
                }
            } else if self.eat_ident("function")? {
                let Tok::Num(number) = self.peeked else {
                    return Err(self.err_here("btree support function number is required"));
                };
                if number != "1" {
                    return Err(self.err_here("btree comparison support function number must be 1"));
                }
                self.advance()?;
                let operand_types = if self.eat_op("(")? {
                    let left = self.unmodified_type_name()?;
                    let right = if self.eat_op(",")? {
                        self.unmodified_type_name()?
                    } else {
                        left
                    };
                    self.expect_op(")")?;
                    Some((left, right))
                } else {
                    None
                };
                let function = self.qual_name("btree comparison function")?;
                let argument_types = self.optional_type_signature()?;
                OperatorClassMember::CompareFunction {
                    operand_types,
                    function,
                    argument_types,
                }
            } else {
                self.expect_ident("storage")?;
                OperatorClassMember::Storage(self.unmodified_type_name()?)
            };
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(Stmt::CreateOperatorClass(CreateOperatorClass {
            name,
            default,
            input_type,
            method,
            family,
            members: self.arena_slice(&members[..count])?,
        }))
    }

    pub(super) fn alter_operator(&mut self) -> Result<Stmt<'a>, ParseError> {
        let identity = self.operator_identity()?;
        let action = if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterOperatorAction::Owner(self.any_ident("role name")?)
        } else if self.eat_ident("set")? {
            if self.eat_ident("schema")? {
                AlterOperatorAction::SetSchema(self.col_ident("schema name")?)
            } else {
                self.expect_op("(")?;
                let mut commutator = None;
                let mut negator = None;
                let mut hashes = false;
                let mut merges = false;
                loop {
                    if self.eat_ident("commutator")? {
                        self.expect_op("=")?;
                        if commutator
                            .replace(self.operator_qual_name("commutator name is invalid")?)
                            .is_some()
                        {
                            return Err(self.err_here("COMMUTATOR specified more than once"));
                        }
                    } else if self.eat_ident("negator")? {
                        self.expect_op("=")?;
                        if negator
                            .replace(self.operator_qual_name("negator name is invalid")?)
                            .is_some()
                        {
                            return Err(self.err_here("NEGATOR specified more than once"));
                        }
                    } else if self.eat_ident("hashes")? {
                        if hashes {
                            return Err(self.err_here("HASHES specified more than once"));
                        }
                        hashes = true;
                    } else if self.eat_ident("merges")? {
                        if merges {
                            return Err(self.err_here("MERGES specified more than once"));
                        }
                        merges = true;
                    } else if self.eat_ident("restrict")? || self.eat_ident("join")? {
                        self.expect_op("=")?;
                        if !self.eat_ident("none")? {
                            let _ = self.qual_name("selectivity function")?;
                        }
                        return Err(ParseError {
                            at: self.peek_at,
                            message: stack_format!(
                                96,
                                "custom selectivity functions are not supported by the bounded planner"
                            ),
                            sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                        });
                    } else {
                        return Err(self.err_here("invalid ALTER OPERATOR option"));
                    }
                    if self.eat_op(")")? {
                        break;
                    }
                    self.expect_op(",")?;
                }
                AlterOperatorAction::Set {
                    commutator,
                    negator,
                    hashes,
                    merges,
                }
            }
        } else {
            return Err(self.err_here("expected OWNER TO or SET in ALTER OPERATOR"));
        };
        Ok(Stmt::AlterOperator { identity, action })
    }

    fn operator_family_add_member(&mut self) -> Result<OperatorFamilyMember<'a>, ParseError> {
        if self.eat_ident("operator")? {
            let strategy = self.btree_strategy()?;
            let operator = self.operator_identity()?;
            if self.eat_ident("for")? {
                if self.eat_ident("order")? {
                    self.expect_ident("by")?;
                    let _ = self.qual_name("sort operator family")?;
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(
                            96,
                            "btree ordering operators are not supported in a search operator family"
                        ),
                        sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                    });
                }
                self.expect_ident("search")?;
            }
            Ok(OperatorFamilyMember::Operator { strategy, operator })
        } else {
            self.expect_ident("function")?;
            let Tok::Num(number) = self.peeked else {
                return Err(self.err_here("btree support function number is required"));
            };
            if number != "1" {
                return Err(self.err_here("btree comparison support function number must be 1"));
            }
            self.advance()?;
            self.expect_op("(")?;
            let left_type = self.unmodified_type_name()?;
            let right_type = if self.eat_op(",")? {
                self.unmodified_type_name()?
            } else {
                left_type
            };
            self.expect_op(")")?;
            let function = self.qual_name("btree comparison function")?;
            let argument_types = self.optional_type_signature()?;
            Ok(OperatorFamilyMember::CompareFunction {
                left_type,
                right_type,
                function,
                argument_types,
            })
        }
    }

    fn operator_family_drop_member(
        &mut self,
    ) -> Result<OperatorFamilyMemberIdentity<'a>, ParseError> {
        if self.eat_ident("operator")? {
            let strategy = self.btree_strategy()?;
            self.expect_op("(")?;
            let left_type = self.unmodified_type_name()?;
            let right_type = if self.eat_op(",")? {
                self.unmodified_type_name()?
            } else {
                left_type
            };
            self.expect_op(")")?;
            Ok(OperatorFamilyMemberIdentity::Operator {
                strategy,
                left_type,
                right_type,
            })
        } else {
            self.expect_ident("function")?;
            let Tok::Num(number) = self.peeked else {
                return Err(self.err_here("btree support function number is required"));
            };
            if number != "1" {
                return Err(self.err_here("btree comparison support function number must be 1"));
            }
            self.advance()?;
            self.expect_op("(")?;
            let left_type = self.unmodified_type_name()?;
            let right_type = if self.eat_op(",")? {
                self.unmodified_type_name()?
            } else {
                left_type
            };
            self.expect_op(")")?;
            Ok(OperatorFamilyMemberIdentity::CompareFunction {
                left_type,
                right_type,
            })
        }
    }

    pub(super) fn alter_operator_family(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("operator family name")?;
        self.expect_ident("using")?;
        let method = self.access_method()?;
        let action = if self.eat_ident("add")? {
            let mut members = [OperatorFamilyMember::CompareFunction {
                left_type: "bool",
                right_type: "bool",
                function: QualName::bare("boolcmp"),
                argument_types: &[],
            }; MAX_LIST];
            let mut count = 0usize;
            loop {
                if count == members.len() {
                    return Err(self.limit("operator family members", members.len()));
                }
                members[count] = self.operator_family_add_member()?;
                count += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            AlterOperatorFamilyAction::Add(self.arena_slice(&members[..count])?)
        } else if self.eat_ident("drop")? {
            let mut members = [OperatorFamilyMemberIdentity::CompareFunction {
                left_type: "bool",
                right_type: "bool",
            }; MAX_LIST];
            let mut count = 0usize;
            loop {
                if count == members.len() {
                    return Err(self.limit("operator family members", members.len()));
                }
                members[count] = self.operator_family_drop_member()?;
                count += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            AlterOperatorFamilyAction::Drop(self.arena_slice(&members[..count])?)
        } else if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterOperatorFamilyAction::Rename(self.col_ident("new operator family name")?)
        } else if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterOperatorFamilyAction::Owner(self.any_ident("role name")?)
        } else {
            self.expect_ident("set")?;
            self.expect_ident("schema")?;
            AlterOperatorFamilyAction::SetSchema(self.col_ident("schema name")?)
        };
        Ok(Stmt::AlterOperatorFamily {
            name,
            method,
            action,
        })
    }

    pub(super) fn alter_operator_class(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("operator class name")?;
        self.expect_ident("using")?;
        let method = self.access_method()?;
        let action = if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterOperatorClassAction::Rename(self.col_ident("new operator class name")?)
        } else if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterOperatorClassAction::Owner(self.any_ident("role name")?)
        } else {
            self.expect_ident("set")?;
            self.expect_ident("schema")?;
            AlterOperatorClassAction::SetSchema(self.col_ident("schema name")?)
        };
        Ok(Stmt::AlterOperatorClass {
            name,
            method,
            action,
        })
    }

    fn create_language(&mut self, or_replace: bool, trusted: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("language name")?;
        let mut handler = None;
        let mut inline = None;
        let mut validator = None;
        while !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
            if self.eat_ident("handler")? {
                if handler
                    .replace(self.qual_name("language handler")?)
                    .is_some()
                {
                    return Err(self.err_here("HANDLER specified more than once"));
                }
            } else if self.eat_ident("inline")? {
                if inline
                    .replace(self.qual_name("language inline handler")?)
                    .is_some()
                {
                    return Err(self.err_here("INLINE specified more than once"));
                }
            } else if self.eat_ident("validator")? {
                if validator
                    .replace(self.qual_name("language validator")?)
                    .is_some()
                {
                    return Err(self.err_here("VALIDATOR specified more than once"));
                }
            } else {
                return Err(self.unexpected("language option"));
            }
        }
        Ok(Stmt::CreateLanguage(crate::sql::ast::CreateLanguage {
            name,
            or_replace,
            trusted,
            handler,
            inline,
            validator,
        }))
    }

    fn extension_version(&mut self) -> Result<&'a str, ParseError> {
        let version = match self.peeked {
            Tok::Ident(value) | Tok::Str(value) | Tok::Num(value) => value,
            _ => return Err(self.err_here("extension version is required")),
        };
        if version.is_empty()
            || version.starts_with('-')
            || version.ends_with('-')
            || version.contains("--")
        {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "invalid extension version name: {}", version),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            });
        }
        self.advance()?;
        Ok(version)
    }

    fn create_extension(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_not_exists = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.col_ident("extension name")?;
        let _ = self.eat_ident("with")?;
        let mut schema = None;
        let mut version = None;
        let mut cascade = false;
        while self.peeked != Tok::Eof && self.peeked != Tok::Op(";") {
            if self.eat_ident("schema")? {
                if schema.replace(self.col_ident("schema name")?).is_some() {
                    return Err(self.err_here("SCHEMA specified more than once"));
                }
            } else if self.eat_ident("version")? {
                let parsed = self.extension_version()?;
                if version.replace(parsed).is_some() {
                    return Err(self.err_here("VERSION specified more than once"));
                }
            } else if self.eat_ident("cascade")? {
                if cascade {
                    return Err(self.err_here("CASCADE specified more than once"));
                }
                cascade = true;
            } else {
                return Err(self.unexpected("expected SCHEMA, VERSION, or CASCADE"));
            }
        }
        Ok(Stmt::CreateExtension {
            name,
            if_not_exists,
            schema,
            version,
            cascade,
        })
    }

    pub(super) fn routine_identity(&mut self) -> Result<RoutineIdentity<'a>, ParseError> {
        let name = self.qual_name("routine name")?;
        let mut argument_types = [""; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut count = 0usize;
        let signature_is_explicit = self.eat_op("(")?;
        if signature_is_explicit && !self.eat_op(")")? {
            loop {
                let (argument_type, input) = self.routine_identity_argument()?;
                if input {
                    if count == argument_types.len() {
                        return Err(self.limit("routine arguments", argument_types.len()));
                    }
                    argument_types[count] = argument_type;
                    count += 1;
                }
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        Ok(RoutineIdentity {
            name,
            argument_types: self.arena_slice(&argument_types[..count])?,
            signature_is_explicit,
        })
    }

    fn extension_routine_identity(
        &mut self,
        kind: RoutineTargetKind,
    ) -> Result<ExtensionMemberIdentity<'a>, ParseError> {
        Ok(ExtensionMemberIdentity::Routine {
            kind,
            identity: self.routine_identity()?,
        })
    }

    /// Parses the identity-bearing portion of one routine argument. OUT-only
    /// arguments are consumed but excluded from the declared signature; the
    /// returned type has already crossed the ordinary typed-name boundary.
    pub(super) fn routine_identity_argument(&mut self) -> Result<(&'a str, bool), ParseError> {
        let input = match self.peeked {
            Tok::Ident("out") | Tok::Ident("table") => {
                self.advance()?;
                false
            }
            Tok::Ident("in") | Tok::Ident("inout") | Tok::Ident("variadic") => {
                self.advance()?;
                true
            }
            _ => true,
        };
        let mark = self.lexer.mark();
        let (saved_peeked, saved_peek_at) = (self.peeked, self.peek_at);
        let candidate_type = self.type_name()?;
        let argument_type = if matches!(self.peeked, Tok::Op(",") | Tok::Op(")")) {
            candidate_type
        } else {
            self.lexer.reset(mark);
            self.peeked = saved_peeked;
            self.peek_at = saved_peek_at;
            let _ = self.type_function_ident("routine argument name")?;
            self.type_name()?
        };
        Ok((argument_type, input))
    }

    fn extension_member(&mut self) -> Result<ExtensionMemberIdentity<'a>, ParseError> {
        if self.eat_ident("aggregate")? {
            return Ok(ExtensionMemberIdentity::Aggregate(
                self.aggregate_identity()?,
            ));
        }
        if self.eat_ident("function")? {
            return self.extension_routine_identity(RoutineTargetKind::Function);
        }
        if self.eat_ident("procedure")? {
            return self.extension_routine_identity(RoutineTargetKind::Procedure);
        }
        if self.eat_ident("routine")? {
            return self.extension_routine_identity(RoutineTargetKind::Either);
        }
        let relation = if self.eat_ident("table")? {
            Some(ExtensionRelationKind::Table)
        } else if self.eat_ident("view")? {
            Some(ExtensionRelationKind::View)
        } else if self.eat_ident("materialized")? {
            self.expect_ident("view")?;
            Some(ExtensionRelationKind::MaterializedView)
        } else if self.eat_ident("sequence")? {
            Some(ExtensionRelationKind::Sequence)
        } else {
            None
        };
        if let Some(kind) = relation {
            return Ok(ExtensionMemberIdentity::Relation {
                kind,
                name: self.qual_name("extension member name")?,
            });
        }
        if self.eat_ident("schema")? {
            return Ok(ExtensionMemberIdentity::Schema(
                self.col_ident("schema name")?,
            ));
        }
        if self.eat_ident("domain")? {
            return Ok(ExtensionMemberIdentity::Domain(
                self.qual_name("domain name")?,
            ));
        }
        if self.eat_ident("type")? {
            return Ok(ExtensionMemberIdentity::Type(self.qual_name("type name")?));
        }
        Err(ParseError {
            at: self.peek_at,
            message: stack_format!(96, "unsupported extension member object kind"),
            sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
        })
    }

    pub(super) fn alter_extension(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("extension name")?;
        let action = if self.eat_ident("update")? {
            let version = if self.eat_ident("to")? {
                Some(self.extension_version()?)
            } else {
                None
            };
            AlterExtensionAction::Update { version }
        } else if self.eat_ident("set")? {
            self.expect_ident("schema")?;
            AlterExtensionAction::SetSchema(self.col_ident("schema name")?)
        } else {
            let add = if self.eat_ident("add")? {
                true
            } else {
                self.expect_ident("drop")?;
                false
            };
            AlterExtensionAction::Member {
                add,
                object: self.extension_member()?,
            }
        };
        Ok(Stmt::AlterExtension { name, action })
    }

    fn create_statistics(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            StatisticsName::Explicit {
                name: self.qual_name("statistics name")?,
                if_not_exists: true,
            }
        } else if self.peeked == Tok::Ident("on") {
            StatisticsName::Generated
        } else {
            StatisticsName::Explicit {
                name: self.qual_name("statistics name")?,
                if_not_exists: false,
            }
        };
        let explicit_kinds = if self.eat_op("(")? {
            let mut kinds = StatisticsKinds::empty();
            loop {
                let kind = self.any_ident("statistics kind")?;
                let fresh = if kind.eq_ignore_ascii_case("ndistinct") {
                    kinds.insert_ndistinct()
                } else if kind.eq_ignore_ascii_case("dependencies") {
                    kinds.insert_dependencies()
                } else if kind.eq_ignore_ascii_case("mcv") {
                    kinds.insert_mcv()
                } else {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(96, "unrecognized statistics kind \"{}\"", kind),
                        sqlstate: sqlstate::SYNTAX_ERROR,
                    });
                };
                if !fresh {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(
                            96,
                            "statistics kind \"{}\" specified more than once",
                            kind
                        ),
                        sqlstate: sqlstate::SYNTAX_ERROR,
                    });
                }
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
            Some(kinds)
        } else {
            None
        };
        self.expect_ident("on")?;
        let mut keys = [StatisticsKey::Column(""); crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
        let mut count = 0usize;
        loop {
            if count == keys.len() {
                return Err(self.limit("statistics expressions", keys.len()));
            }
            keys[count] = if self.eat_op("(")? {
                let start = self.peek_at;
                let expression = self.expression(0)?;
                let source =
                    index_expression_source(expression, self.text[start..self.peek_at].trim());
                self.expect_op(")")?;
                StatisticsKey::Expression(StatisticsExpression { expression, source })
            } else {
                StatisticsKey::Column(self.col_ident("column name")?)
            };
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_ident("from")?;
        let table = self.qual_name("table name")?;
        let keys = if count == 1 {
            if explicit_kinds.is_some() {
                return Err(self.err_here(
                    "when building statistics on a single expression, statistics kinds may not be specified",
                ));
            }
            match keys[0] {
                StatisticsKey::Expression(expression) => StatisticsKeys::Expression(expression),
                StatisticsKey::Column(_) => {
                    return Err(self.err_here("extended statistics require at least 2 columns"));
                }
            }
        } else {
            StatisticsKeys::Multivariate {
                kinds: explicit_kinds.unwrap_or(StatisticsKinds::ALL),
                keys: self.arena_slice(&keys[..count])?,
            }
        };
        Ok(Stmt::CreateStatistics(CreateStatistics {
            name,
            keys,
            table,
        }))
    }

    pub(super) fn alter_statistics(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("statistics name")?;
        let action = if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterStatisticsAction::Owner(self.any_ident("role name")?)
        } else if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterStatisticsAction::Rename(self.col_ident("new statistics name")?)
        } else if self.eat_ident("set")? {
            if self.eat_ident("schema")? {
                AlterStatisticsAction::SetSchema(self.col_ident("schema name")?)
            } else {
                self.expect_ident("statistics")?;
                let target = if self.eat_ident("default")? {
                    StatisticsTarget::Default
                } else {
                    let raw = self.seq_int()?;
                    if raw == -1 {
                        StatisticsTarget::Default
                    } else {
                        StatisticsTarget::Value(
                            u16::try_from(raw)
                                .ok()
                                .filter(|target| *target <= 10_000)
                                .ok_or_else(|| {
                                    self.err_here("statistics target is out of range")
                                })?,
                        )
                    }
                };
                AlterStatisticsAction::SetTarget(target)
            }
        } else {
            return Err(self.err_here("expected OWNER, RENAME, or SET after ALTER STATISTICS"));
        };
        Ok(Stmt::AlterStatistics { name, action })
    }

    fn tablespace_cost(&mut self) -> Result<crate::sql::ast::TablespaceCost, ParseError> {
        let raw = match self.peeked {
            Tok::Num(raw) | Tok::Str(raw) => raw,
            _ => return Err(self.err_here("tablespace cost must be numeric")),
        };
        let value = raw
            .parse::<f64>()
            .ok()
            .and_then(crate::sql::ast::TablespaceCost::new)
            .ok_or_else(|| self.err_here("tablespace cost is out of range"))?;
        self.advance()?;
        Ok(value)
    }

    fn tablespace_options(&mut self) -> Result<TablespaceOptions, ParseError> {
        let mut options = TablespaceOptions::DEFAULT;
        loop {
            let option = self.any_ident("tablespace parameter")?;
            let _ = self.eat_op("=")?;
            if option.eq_ignore_ascii_case("random_page_cost") {
                if options.random_page_cost.is_some() {
                    return Err(self.err_here("tablespace parameter specified more than once"));
                }
                options.random_page_cost = Some(self.tablespace_cost()?);
            } else if option.eq_ignore_ascii_case("seq_page_cost") {
                if options.seq_page_cost.is_some() {
                    return Err(self.err_here("tablespace parameter specified more than once"));
                }
                options.seq_page_cost = Some(self.tablespace_cost()?);
            } else if option.eq_ignore_ascii_case("effective_io_concurrency")
                || option.eq_ignore_ascii_case("maintenance_io_concurrency")
            {
                let value = self.seq_int()?;
                let value = i32::try_from(value)
                    .ok()
                    .filter(|value| (0..=1000).contains(value))
                    .ok_or_else(|| self.err_here("tablespace concurrency is out of range"))?;
                let target = if option.eq_ignore_ascii_case("effective_io_concurrency") {
                    &mut options.effective_io_concurrency
                } else {
                    &mut options.maintenance_io_concurrency
                };
                if target.replace(value).is_some() {
                    return Err(self.err_here("tablespace parameter specified more than once"));
                }
            } else {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "unrecognized parameter \"{}\"", option),
                    sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                });
            }
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(options)
    }

    fn tablespace_option_names(&mut self) -> Result<TablespaceOptionNames, ParseError> {
        let mut names = TablespaceOptionNames::EMPTY;
        loop {
            let option = self.any_ident("tablespace parameter")?;
            let target = if option.eq_ignore_ascii_case("random_page_cost") {
                &mut names.random_page_cost
            } else if option.eq_ignore_ascii_case("seq_page_cost") {
                &mut names.seq_page_cost
            } else if option.eq_ignore_ascii_case("effective_io_concurrency") {
                &mut names.effective_io_concurrency
            } else if option.eq_ignore_ascii_case("maintenance_io_concurrency") {
                &mut names.maintenance_io_concurrency
            } else {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "unrecognized parameter \"{}\"", option),
                    sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                });
            };
            if core::mem::replace(target, true) {
                return Err(self.err_here("tablespace parameter specified more than once"));
            }
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(names)
    }

    fn create_tablespace(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.any_ident("tablespace name")?;
        let owner = if self.eat_ident("owner")? {
            Some(self.any_ident("tablespace owner")?)
        } else {
            None
        };
        self.expect_ident("location")?;
        let location = self.str_literal("tablespace location")?;
        let options = if self.eat_ident("with")? {
            self.expect_op("(")?;
            let options = self.tablespace_options()?;
            self.expect_op(")")?;
            options
        } else {
            TablespaceOptions::DEFAULT
        };
        Ok(Stmt::CreateTablespace {
            name,
            owner,
            location,
            options,
        })
    }

    fn database_option_value(&mut self, what: &'static str) -> Result<&'a str, ParseError> {
        match self.peeked {
            Tok::Ident(value) | Tok::QuotedIdent(value) | Tok::Str(value) | Tok::Num(value) => {
                self.advance()?;
                Ok(value)
            }
            _ => Err(self.err_here(what)),
        }
    }

    fn create_database(&mut self) -> Result<Stmt<'a>, ParseError> {
        use crate::sql::ast::{CreateDatabaseOptions, DatabaseLocaleProvider, DatabaseStrategy};
        let name = self.any_ident("database name")?;
        let _ = self.eat_ident("with")?;
        let mut options = CreateDatabaseOptions::EMPTY;
        while !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
            let option = self.any_ident("database option")?;
            let _ = self.eat_op("=")?;
            if option.eq_ignore_ascii_case("owner") {
                if options.owner.is_some() {
                    return Err(self.err_here("OWNER specified more than once"));
                }
                options.owner = Some(self.any_ident("database owner")?);
            } else if option.eq_ignore_ascii_case("template") {
                if options.template.is_some() {
                    return Err(self.err_here("TEMPLATE specified more than once"));
                }
                options.template = Some(self.any_ident("template database")?);
            } else if option.eq_ignore_ascii_case("encoding") {
                if options.encoding.is_some() {
                    return Err(self.err_here("ENCODING specified more than once"));
                }
                options.encoding = Some(self.database_option_value("invalid database encoding")?);
            } else if option.eq_ignore_ascii_case("strategy") {
                if options.strategy.is_some() {
                    return Err(self.err_here("STRATEGY specified more than once"));
                }
                let value = self.database_option_value("invalid database strategy")?;
                options.strategy = Some(if value.eq_ignore_ascii_case("wal_log") {
                    DatabaseStrategy::WalLog
                } else if value.eq_ignore_ascii_case("file_copy") {
                    DatabaseStrategy::FileCopy
                } else {
                    return Err(self.err_here("invalid database creation strategy"));
                });
            } else if option.eq_ignore_ascii_case("locale_provider") {
                if options.locale_provider.is_some() {
                    return Err(self.err_here("LOCALE_PROVIDER specified more than once"));
                }
                let value = self.database_option_value("invalid locale provider")?;
                options.locale_provider = Some(if value.eq_ignore_ascii_case("builtin") {
                    DatabaseLocaleProvider::Builtin
                } else if value.eq_ignore_ascii_case("libc") {
                    DatabaseLocaleProvider::Libc
                } else if value.eq_ignore_ascii_case("icu") {
                    DatabaseLocaleProvider::Icu
                } else {
                    return Err(self.err_here("invalid locale provider"));
                });
            } else if option.eq_ignore_ascii_case("locale") {
                if options
                    .locale
                    .replace(self.database_option_value("invalid locale")?)
                    .is_some()
                {
                    return Err(self.err_here("LOCALE specified more than once"));
                }
            } else if option.eq_ignore_ascii_case("lc_collate") {
                if options
                    .collate
                    .replace(self.database_option_value("invalid LC_COLLATE")?)
                    .is_some()
                {
                    return Err(self.err_here("LC_COLLATE specified more than once"));
                }
            } else if option.eq_ignore_ascii_case("lc_ctype") {
                if options
                    .ctype
                    .replace(self.database_option_value("invalid LC_CTYPE")?)
                    .is_some()
                {
                    return Err(self.err_here("LC_CTYPE specified more than once"));
                }
            } else if option.eq_ignore_ascii_case("builtin_locale")
                || option.eq_ignore_ascii_case("icu_locale")
                || option.eq_ignore_ascii_case("icu_rules")
            {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "database option {} is not supported", option),
                    sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                });
            } else if option.eq_ignore_ascii_case("collation_version") {
                if options
                    .collation_version
                    .replace(self.database_option_value("invalid collation version")?)
                    .is_some()
                {
                    return Err(self.err_here("COLLATION_VERSION specified more than once"));
                }
            } else if option.eq_ignore_ascii_case("tablespace") {
                if options.tablespace.is_some() {
                    return Err(self.err_here("TABLESPACE specified more than once"));
                }
                options.tablespace = Some(self.any_ident("tablespace name")?);
            } else if option.eq_ignore_ascii_case("allow_connections") {
                if options
                    .allow_connections
                    .replace(self.role_option_boolean()?)
                    .is_some()
                {
                    return Err(self.err_here("ALLOW_CONNECTIONS specified more than once"));
                }
            } else if option.eq_ignore_ascii_case("connection_limit") {
                let value = i32::try_from(self.seq_int()?)
                    .ok()
                    .filter(|value| *value >= -1)
                    .ok_or_else(|| self.err_here("connection limit is out of range"))?;
                if options.connection_limit.replace(value).is_some() {
                    return Err(self.err_here("CONNECTION LIMIT specified more than once"));
                }
            } else if option.eq_ignore_ascii_case("is_template") {
                if options
                    .is_template
                    .replace(self.role_option_boolean()?)
                    .is_some()
                {
                    return Err(self.err_here("IS_TEMPLATE specified more than once"));
                }
            } else if option.eq_ignore_ascii_case("oid") {
                let value = i32::try_from(self.seq_int()?)
                    .map_err(|_| self.err_here("database OID is out of range"))?;
                if options.oid.replace(value).is_some() {
                    return Err(self.err_here("OID specified more than once"));
                }
            } else {
                return Err(self.err_here("unrecognized CREATE DATABASE option"));
            }
        }
        Ok(Stmt::CreateDatabase { name, options })
    }

    pub(super) fn alter_database(&mut self) -> Result<Stmt<'a>, ParseError> {
        use crate::sql::ast::{AlterDatabaseAction, RoutineConfigValue};
        let name = self.any_ident("database name")?;
        let action = if self.eat_ident("with")? {
            let mut allow_connections = None;
            let mut connection_limit = None;
            let mut is_template = None;
            loop {
                let option = self.any_ident("database option")?;
                let _ = self.eat_op("=")?;
                if option.eq_ignore_ascii_case("allow_connections") {
                    if allow_connections
                        .replace(self.role_option_boolean()?)
                        .is_some()
                    {
                        return Err(self.err_here("ALLOW_CONNECTIONS specified more than once"));
                    }
                } else if option.eq_ignore_ascii_case("connection_limit") {
                    let value = i32::try_from(self.seq_int()?)
                        .ok()
                        .filter(|value| *value >= -1)
                        .ok_or_else(|| self.err_here("connection limit is out of range"))?;
                    if connection_limit.replace(value).is_some() {
                        return Err(self.err_here("CONNECTION LIMIT specified more than once"));
                    }
                } else if option.eq_ignore_ascii_case("is_template") {
                    if is_template.replace(self.role_option_boolean()?).is_some() {
                        return Err(self.err_here("IS_TEMPLATE specified more than once"));
                    }
                } else {
                    return Err(self.err_here("unrecognized ALTER DATABASE option"));
                }
                if matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
                    break;
                }
            }
            AlterDatabaseAction::Options {
                allow_connections,
                connection_limit,
                is_template,
            }
        } else if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterDatabaseAction::Rename(self.any_ident("new database name")?)
        } else if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterDatabaseAction::SetOwner(self.any_ident("database owner")?)
        } else if self.eat_ident("set")? {
            if self.eat_ident("tablespace")? {
                AlterDatabaseAction::SetTablespace(self.any_ident("tablespace name")?)
            } else {
                let name = self.any_ident("configuration parameter")?;
                let value = if self.eat_ident("from")? {
                    self.expect_ident("current")?;
                    RoutineConfigValue::Current
                } else {
                    if !self.eat_op("=")? {
                        self.expect_ident("to")?;
                    }
                    let start = self.peek_at;
                    while !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
                        self.advance()?;
                    }
                    RoutineConfigValue::Value(self.text[start..self.peek_at].trim())
                };
                AlterDatabaseAction::Set { name, value }
            }
        } else if self.eat_ident("reset")? {
            AlterDatabaseAction::Reset(if self.eat_ident("all")? {
                None
            } else {
                Some(self.any_ident("configuration parameter")?)
            })
        } else if self.eat_ident("refresh")? {
            self.expect_ident("collation")?;
            self.expect_ident("version")?;
            AlterDatabaseAction::RefreshCollationVersion
        } else {
            return Err(self.err_here("expected an ALTER DATABASE action"));
        };
        Ok(Stmt::AlterDatabase { name, action })
    }

    pub(super) fn alter_tablespace(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.any_ident("tablespace name")?;
        let action = if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterTablespaceAction::Rename(self.any_ident("new tablespace name")?)
        } else if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterTablespaceAction::SetOwner(self.any_ident("tablespace owner")?)
        } else if self.eat_ident("set")? {
            self.expect_op("(")?;
            let options = self.tablespace_options()?;
            self.expect_op(")")?;
            AlterTablespaceAction::SetOptions(options)
        } else if self.eat_ident("reset")? {
            self.expect_op("(")?;
            let options = self.tablespace_option_names()?;
            self.expect_op(")")?;
            AlterTablespaceAction::ResetOptions(options)
        } else {
            return Err(self.err_here("expected a tablespace alteration"));
        };
        Ok(Stmt::AlterTablespace { name, action })
    }

    fn policy_expression(&mut self) -> Result<PolicyExpression<'a>, ParseError> {
        self.expect_op("(")?;
        let start = self.peek_at;
        let expression = self.expression(0)?;
        let source = self.arena_str(self.text[start..self.peek_at].trim_end())?;
        self.expect_op(")")?;
        Ok(PolicyExpression { expression, source })
    }

    fn policy_roles(&mut self) -> Result<&'a [PolicyRole<'a>], ParseError> {
        let mut roles = [PolicyRole::Public; MAX_LIST];
        let mut count = 0usize;
        loop {
            if count == roles.len() {
                return Err(self.limit("policy roles", roles.len()));
            }
            roles[count] = if self.eat_ident("public")? {
                PolicyRole::Public
            } else if self.eat_ident("current_role")? {
                PolicyRole::CurrentRole
            } else if self.eat_ident("current_user")? {
                PolicyRole::CurrentUser
            } else if self.eat_ident("session_user")? {
                PolicyRole::SessionUser
            } else {
                PolicyRole::Named(self.any_ident("policy role")?)
            };
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.arena_slice(&roles[..count])
    }

    /// CREATE POLICY's command kind decides which expression clauses can
    /// exist, so illegal INSERT/SELECT/DELETE shapes never enter the AST.
    fn create_policy(&mut self) -> Result<Stmt<'a>, ParseError> {
        #[derive(Clone, Copy)]
        enum Kind {
            All,
            Select,
            Insert,
            Update,
            Delete,
        }

        let name = self.col_ident("policy name")?;
        self.expect_ident("on")?;
        let table = self.qual_name("policy table")?;
        let permissiveness = if self.eat_ident("as")? {
            if self.eat_ident("permissive")? {
                PolicyPermissiveness::Permissive
            } else {
                self.expect_ident("restrictive")?;
                PolicyPermissiveness::Restrictive
            }
        } else {
            PolicyPermissiveness::Permissive
        };
        let kind = if self.eat_ident("for")? {
            if self.eat_ident("all")? {
                Kind::All
            } else if self.eat_ident("select")? {
                Kind::Select
            } else if self.eat_ident("insert")? {
                Kind::Insert
            } else if self.eat_ident("update")? {
                Kind::Update
            } else if self.eat_ident("delete")? {
                Kind::Delete
            } else {
                return Err(self.unexpected("expected ALL, SELECT, INSERT, UPDATE, or DELETE"));
            }
        } else {
            Kind::All
        };
        let roles = if self.eat_ident("to")? {
            self.policy_roles()?
        } else {
            self.arena_slice(&[PolicyRole::Public])?
        };
        let using = if self.eat_ident("using")? {
            Some(self.policy_expression()?)
        } else {
            None
        };
        let with_check = if self.eat_ident("with")? {
            self.expect_ident("check")?;
            Some(self.policy_expression()?)
        } else {
            None
        };
        let command = match kind {
            Kind::All => PolicyCommand::All { using, with_check },
            Kind::Select if with_check.is_none() => PolicyCommand::Select { using },
            Kind::Insert if using.is_none() => PolicyCommand::Insert { with_check },
            Kind::Update => PolicyCommand::Update { using, with_check },
            Kind::Delete if with_check.is_none() => PolicyCommand::Delete { using },
            Kind::Select | Kind::Delete => {
                return Err(self.err_here("WITH CHECK cannot be applied to SELECT or DELETE"));
            }
            Kind::Insert => {
                return Err(self.err_here("USING cannot be applied to INSERT"));
            }
        };
        Ok(Stmt::CreatePolicy(crate::sql::ast::CreatePolicy {
            name,
            table,
            permissiveness,
            roles,
            command,
        }))
    }

    pub(super) fn alter_policy(&mut self) -> Result<Stmt<'a>, ParseError> {
        let identity = PolicyIdentity {
            name: self.col_ident("policy name")?,
            table: {
                self.expect_ident("on")?;
                self.qual_name("policy table")?
            },
        };
        let roles = if self.eat_ident("to")? {
            Some(self.policy_roles()?)
        } else {
            None
        };
        let using = if self.eat_ident("using")? {
            Some(self.policy_expression()?)
        } else {
            None
        };
        let with_check = if self.eat_ident("with")? {
            self.expect_ident("check")?;
            Some(self.policy_expression()?)
        } else {
            None
        };
        Ok(Stmt::AlterPolicy(crate::sql::ast::AlterPolicy {
            identity,
            roles,
            using,
            with_check,
        }))
    }

    /// CREATE TRIGGER forms with a complete durable execution model.
    fn create_trigger(
        &mut self,
        or_replace: bool,
        constraint: bool,
    ) -> Result<Stmt<'a>, ParseError> {
        if or_replace && constraint {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "CREATE OR REPLACE CONSTRAINT TRIGGER is not supported"),
                sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
            });
        }
        let name = self.col_ident("trigger name")?;
        let timing = if self.eat_ident("before")? {
            TriggerTiming::Before
        } else if self.eat_ident("after")? {
            TriggerTiming::After
        } else if self.eat_ident("instead")? {
            self.expect_ident("of")?;
            TriggerTiming::InsteadOf
        } else {
            return Err(self.err_here("expected BEFORE, AFTER, or INSTEAD OF for CREATE TRIGGER"));
        };
        let mut events = [TriggerEvent::Insert; 4];
        let mut event_count = 0usize;
        let mut update_columns = [""; MAX_LIST];
        let mut update_column_count = 0usize;
        loop {
            let event = if self.eat_ident("insert")? {
                TriggerEvent::Insert
            } else if self.eat_ident("update")? {
                if self.eat_ident("of")? {
                    loop {
                        if update_column_count == update_columns.len() {
                            return Err(self.limit("UPDATE OF columns", update_columns.len()));
                        }
                        let column = self.col_ident("UPDATE OF column")?;
                        if update_columns[..update_column_count].contains(&column) {
                            return Err(ParseError {
                                at: self.peek_at,
                                message: stack_format!(
                                    96,
                                    "column \"{}\" specified more than once",
                                    column
                                ),
                                sqlstate: sqlstate::DUPLICATE_COLUMN,
                            });
                        }
                        update_columns[update_column_count] = column;
                        update_column_count += 1;
                        if !self.eat_op(",")? {
                            break;
                        }
                    }
                }
                TriggerEvent::Update
            } else if self.eat_ident("delete")? {
                TriggerEvent::Delete
            } else if self.eat_ident("truncate")? {
                TriggerEvent::Truncate
            } else {
                return Err(self
                    .err_here("expected INSERT, UPDATE, DELETE, or TRUNCATE for CREATE TRIGGER"));
            };
            if events[..event_count].contains(&event) {
                return Err(self.err_here("trigger event specified more than once"));
            }
            events[event_count] = event;
            event_count += 1;
            if !self.eat_ident("or")? {
                break;
            }
        }
        self.expect_ident("on")?;
        let table = self.qual_name("trigger table")?;
        let referenced_table = if self.eat_ident("from")? {
            Some(self.qual_name("referenced table")?)
        } else {
            None
        };
        let mut timing_clause = false;
        let constraint_timing = if self.eat_ident("not")? {
            timing_clause = true;
            self.expect_ident("deferrable")?;
            ConstraintTiming::NotDeferrable
        } else {
            let deferrable = self.eat_ident("deferrable")?;
            timing_clause |= deferrable;
            let mut initially_written = false;
            let initially = if self.eat_ident("initially")? {
                timing_clause = true;
                initially_written = true;
                if self.eat_ident("deferred")? {
                    ConstraintMode::Deferred
                } else {
                    self.expect_ident("immediate")?;
                    ConstraintMode::Immediate
                }
            } else {
                ConstraintMode::Immediate
            };
            if deferrable || initially_written {
                ConstraintTiming::Deferrable(initially)
            } else {
                ConstraintTiming::NotDeferrable
            }
        };
        if !constraint && (referenced_table.is_some() || timing_clause) {
            return Err(
                self.err_here("FROM and deferrability clauses require CREATE CONSTRAINT TRIGGER")
            );
        }
        let transition_tables = if self.eat_ident("referencing")? {
            let mut old = None;
            let mut new = None;
            loop {
                if self.eat_ident("old")? {
                    self.expect_ident("table")?;
                    let _ = self.eat_ident("as")?;
                    if old.replace(self.col_ident("OLD TABLE name")?).is_some() {
                        return Err(self.err_here("OLD TABLE specified more than once"));
                    }
                } else if self.eat_ident("new")? {
                    self.expect_ident("table")?;
                    let _ = self.eat_ident("as")?;
                    if new.replace(self.col_ident("NEW TABLE name")?).is_some() {
                        return Err(self.err_here("NEW TABLE specified more than once"));
                    }
                } else {
                    break;
                }
            }
            match (old, new) {
                (Some(old), Some(new)) if old.eq_ignore_ascii_case(new) => {
                    return Err(self.err_here("OLD TABLE and NEW TABLE names must differ"));
                }
                (Some(old), Some(new)) => TriggerTransitionTables::OldNew { old, new },
                (Some(old), None) => TriggerTransitionTables::Old(old),
                (None, Some(new)) => TriggerTransitionTables::New(new),
                (None, None) => return Err(self.err_here("expected OLD TABLE or NEW TABLE")),
            }
        } else {
            TriggerTransitionTables::None
        };
        let level = if self.eat_ident("for")? {
            let _ = self.eat_ident("each")?;
            if self.eat_ident("statement")? {
                crate::sql::ast::TriggerLevel::Statement
            } else {
                self.expect_ident("row")?;
                crate::sql::ast::TriggerLevel::Row
            }
        } else {
            crate::sql::ast::TriggerLevel::Statement
        };
        if matches!(level, crate::sql::ast::TriggerLevel::Row)
            && events[..event_count].contains(&TriggerEvent::Truncate)
        {
            return Err(self.err_here("TRUNCATE triggers must be FOR EACH STATEMENT"));
        }
        if matches!(timing, TriggerTiming::InsteadOf) {
            if !matches!(level, crate::sql::ast::TriggerLevel::Row) {
                return Err(self.err_here("INSTEAD OF triggers must be FOR EACH ROW"));
            }
            if events[..event_count].contains(&TriggerEvent::Truncate) {
                return Err(self.err_here("INSTEAD OF triggers cannot have TRUNCATE events"));
            }
            if update_column_count != 0 {
                return Err(self.err_here("INSTEAD OF triggers cannot have column lists"));
            }
        }
        if !matches!(transition_tables, TriggerTransitionTables::None) {
            if event_count != 1 {
                return Err(self.err_here(
                    "transition tables cannot be specified for triggers with more than one event",
                ));
            }
            if !matches!(timing, TriggerTiming::After) {
                return Err(self.err_here("transition tables are only valid for AFTER triggers"));
            }
            if events[..event_count].contains(&TriggerEvent::Truncate) {
                return Err(self.err_here("TRUNCATE triggers cannot have transition tables"));
            }
            if transition_tables.old().is_some()
                && !events[..event_count]
                    .iter()
                    .any(|event| matches!(event, TriggerEvent::Update | TriggerEvent::Delete))
            {
                return Err(self.err_here("OLD TABLE requires UPDATE or DELETE"));
            }
            if transition_tables.new_table().is_some()
                && !events[..event_count]
                    .iter()
                    .any(|event| matches!(event, TriggerEvent::Insert | TriggerEvent::Update))
            {
                return Err(self.err_here("NEW TABLE requires INSERT or UPDATE"));
            }
            if update_column_count != 0 {
                return Err(
                    self.err_here("UPDATE OF column lists cannot be used with transition tables")
                );
            }
        }
        let when = self
            .eat_ident("when")?
            .then(|| self.check_text())
            .transpose()?;
        if when.is_some() && matches!(timing, TriggerTiming::InsteadOf) {
            return Err(self.err_here("INSTEAD OF triggers cannot have WHEN conditions"));
        }
        self.expect_ident("execute")?;
        if !self.eat_ident("function")? && !self.eat_ident("procedure")? {
            return Err(self.err_here("expected FUNCTION or PROCEDURE after EXECUTE"));
        }
        let function = self.qual_name("trigger function")?;
        self.expect_op("(")?;
        let mut arguments = [""; MAX_LIST];
        let mut argument_count = 0usize;
        if !self.eat_op(")")? {
            loop {
                if argument_count == arguments.len() {
                    return Err(self.limit("trigger arguments", arguments.len()));
                }
                arguments[argument_count] = match self.peeked {
                    crate::sql::lexer::Tok::Str(value)
                    | crate::sql::lexer::Tok::Num(value)
                    | crate::sql::lexer::Tok::Ident(value)
                    | crate::sql::lexer::Tok::QuotedIdent(value) => {
                        self.advance()?;
                        value
                    }
                    _ => return Err(self.unexpected("expected trigger argument")),
                };
                argument_count += 1;
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        Ok(Stmt::CreateTrigger(CreateTrigger {
            or_replace,
            name,
            kind: if constraint {
                if !matches!(timing, TriggerTiming::After)
                    || !matches!(level, crate::sql::ast::TriggerLevel::Row)
                {
                    return Err(
                        self.err_here("constraint triggers must be AFTER FOR EACH ROW triggers")
                    );
                }
                if !matches!(transition_tables, TriggerTransitionTables::None) {
                    return Err(self.err_here("constraint triggers cannot have transition tables"));
                }
                TriggerKind::Constraint {
                    referenced_table,
                    timing: constraint_timing,
                }
            } else {
                TriggerKind::Ordinary
            },
            timing,
            level,
            events: self.arena_slice(&events[..event_count])?,
            update_columns: self.arena_slice(&update_columns[..update_column_count])?,
            table,
            transition_tables,
            when,
            function,
            arguments: self.arena_slice(&arguments[..argument_count])?,
        }))
    }

    fn routine_argument(&mut self) -> Result<RoutineArgument<'a>, ParseError> {
        #[derive(Clone, Copy)]
        enum WrittenMode {
            In,
            Out,
            InOut,
            Variadic,
        }

        let written_mode = if self.eat_ident("in")? {
            WrittenMode::In
        } else if self.eat_ident("out")? {
            WrittenMode::Out
        } else if self.eat_ident("inout")? {
            WrittenMode::InOut
        } else if self.eat_ident("variadic")? {
            WrittenMode::Variadic
        } else {
            WrittenMode::In
        };

        // A parameter name and a user type name occupy the same lexical
        // category. Parse a type first and keep it only when the next token is
        // a parameter boundary; otherwise restore and parse `name type`.
        let mark = self.lexer.mark();
        let (saved_peeked, saved_peek_at) = (self.peeked, self.peek_at);
        let candidate_type = self.type_name()?;
        let candidate_is_complete = matches!(
            self.peeked,
            Tok::Op("," | ")" | "=") | Tok::Ident("default")
        );
        let (name, type_name) = if candidate_is_complete {
            (None, candidate_type)
        } else {
            self.lexer.reset(mark);
            self.peeked = saved_peeked;
            self.peek_at = saved_peek_at;
            let name = self.type_function_ident("routine argument name")?;
            (Some(name), self.type_name()?)
        };

        let has_default = self.eat_ident("default")? || self.eat_op("=")?;
        let default_text = if has_default {
            let start = self.peek_at;
            let _ = self.expression(0)?;
            Some(self.arena_str(self.text[start..self.peek_at].trim_end())?)
        } else {
            None
        };
        let mode = match written_mode {
            WrittenMode::In => RoutineArgumentMode::In { default_text },
            WrittenMode::Out if default_text.is_none() => RoutineArgumentMode::Out,
            WrittenMode::Out => {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "only input parameters can have default values"),
                    sqlstate: sqlstate::INVALID_FUNCTION_DEFINITION,
                });
            }
            WrittenMode::InOut => RoutineArgumentMode::InOut { default_text },
            WrittenMode::Variadic => RoutineArgumentMode::Variadic { default_text },
        };
        Ok(RoutineArgument {
            mode,
            name,
            type_name,
        })
    }

    /// SQL-language routine definition. The parsed parameter modes separate
    /// call identity from output shape before catalog resolution.
    fn create_routine(&mut self, or_replace: bool, function: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name(if function {
            "function name"
        } else {
            "procedure name"
        })?;
        self.expect_op("(")?;
        let mut arguments = [RoutineArgument {
            mode: RoutineArgumentMode::In { default_text: None },
            name: None,
            type_name: "",
        }; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut count = 0;
        let mut saw_default = false;
        let mut saw_variadic = false;
        let mut output_count = 0usize;
        if !self.eat_op(")")? {
            loop {
                if count == arguments.len() {
                    return Err(self.limit("function arguments", arguments.len()));
                }
                let argument = self.routine_argument()?;
                if saw_variadic && !matches!(argument.mode, RoutineArgumentMode::Out) {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(
                            96,
                            "VARIADIC parameter must be the last input parameter"
                        ),
                        sqlstate: sqlstate::INVALID_FUNCTION_DEFINITION,
                    });
                }
                if argument.mode.is_input() {
                    if argument.mode.default_text().is_some() {
                        saw_default = true;
                    } else if saw_default {
                        return Err(ParseError {
                            at: self.peek_at,
                            message: stack_format!(
                                96,
                                "input parameters after one with a default value must also have defaults"
                            ),
                            sqlstate: sqlstate::INVALID_FUNCTION_DEFINITION,
                        });
                    }
                }
                saw_variadic |= matches!(argument.mode, RoutineArgumentMode::Variadic { .. });
                output_count += usize::from(argument.mode.is_output());
                arguments[count] = argument;
                count += 1;
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        let kind = if function {
            let has_returns = self.eat_ident("returns")?;
            if has_returns && self.eat_ident("trigger")? {
                if output_count != 0 {
                    return Err(self.err_here("trigger functions cannot have OUT parameters"));
                }
                RoutineCreateKind::Trigger
            } else if has_returns && self.eat_ident("event_trigger")? {
                if output_count != 0 {
                    return Err(self.err_here("event trigger functions cannot have OUT parameters"));
                }
                RoutineCreateKind::EventTrigger
            } else if has_returns && self.eat_ident("table")? {
                if output_count != 0 {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(
                            96,
                            "OUT and INOUT arguments cannot be used with RETURNS TABLE"
                        ),
                        sqlstate: sqlstate::INVALID_FUNCTION_DEFINITION,
                    });
                }
                self.expect_op("(")?;
                let mut columns = [RoutineResultColumn {
                    name: "",
                    type_name: "",
                }; crate::storage::MAX_ROUTINE_ARGUMENTS];
                let mut column_count = 0;
                loop {
                    if column_count == columns.len() {
                        return Err(self.limit("function result columns", columns.len()));
                    }
                    columns[column_count] = RoutineResultColumn {
                        name: self.any_ident("function result column")?,
                        type_name: self.type_name()?,
                    };
                    column_count += 1;
                    if self.eat_op(")")? {
                        break;
                    }
                    self.expect_op(",")?;
                }
                RoutineCreateKind::TableFunction {
                    columns: self.arena_slice(&columns[..column_count])?,
                }
            } else if output_count != 0 {
                let set_returning = has_returns && self.eat_ident("setof")?;
                let declared_result_type = if has_returns {
                    Some(self.type_name()?)
                } else {
                    None
                };
                RoutineCreateKind::OutputFunction {
                    declared_result_type,
                    set_returning,
                }
            } else {
                if !has_returns {
                    return Err(self.unexpected("RETURNS clause or OUT parameters"));
                }
                RoutineCreateKind::Function {
                    set_returning: self.eat_ident("setof")?,
                    result_type: self.type_name()?,
                }
            }
        } else {
            RoutineCreateKind::Procedure
        };
        let mut language = None;
        let mut body = None;
        let mut attributes = crate::sql::ast::RoutineAttributes::default();
        let mut strict_seen = false;
        let mut volatility_seen = false;
        let mut parallel_seen = false;
        let mut security_seen = false;
        let mut leakproof_seen = false;
        let mut cost_seen = false;
        let mut rows_seen = false;
        let mut configs = [crate::sql::ast::RoutineConfigClause {
            name: "",
            value: crate::sql::ast::RoutineConfigValue::Current,
        }; crate::storage::MAX_ROUTINE_CONFIGS];
        let mut config_count = 0usize;
        while !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
            if self.eat_ident("language")? {
                if language.is_some() {
                    return Err(self.unexpected("one LANGUAGE clause"));
                }
                language = Some(if self.eat_ident("sql")? {
                    crate::sql::ast::RoutineLanguage::Sql
                } else if self.eat_ident("plpgsql")? {
                    crate::sql::ast::RoutineLanguage::PlPgSql
                } else {
                    let unsupported = self.any_ident("routine language")?;
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(
                            96,
                            "routine language \"{}\" is not supported",
                            unsupported
                        ),
                        sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                    });
                });
            } else if self.eat_ident("as")? {
                if body.is_some() {
                    return Err(self.unexpected("one AS clause"));
                }
                let source = self.str_literal("function body")?;
                if self.eat_op(",")? {
                    let _symbol = self.str_literal("function link symbol")?;
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(
                            96,
                            "native-library routine bodies are not supported"
                        ),
                        sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                    });
                }
                body = Some(crate::sql::ast::RoutineBody::String(source));
            } else if self.eat_ident("return")? {
                if body.is_some() {
                    return Err(self.unexpected("one routine body"));
                }
                let start = self.peek_at;
                let _ = self.expression(0)?;
                body = Some(crate::sql::ast::RoutineBody::Return(
                    self.text[start..self.peek_at].trim(),
                ));
            } else if self.eat_ident("begin")? {
                if body.is_some() {
                    return Err(self.unexpected("one routine body"));
                }
                self.expect_ident("atomic")?;
                let start = self.peek_at;
                let end = loop {
                    if self.peeked == Tok::Ident("end") {
                        let end = self.peek_at;
                        self.advance()?;
                        break end;
                    }
                    if self.peeked == Tok::Eof {
                        return Err(self.unexpected("END"));
                    }
                    let _ = self.statement()?;
                    self.expect_op(";")?;
                };
                body = Some(crate::sql::ast::RoutineBody::Atomic(
                    self.text[start..end]
                        .trim_end_matches(|c: char| c.is_ascii_whitespace() || c == ';'),
                ));
            } else if self.eat_ident("strict")? {
                if strict_seen {
                    return Err(self.unexpected("one null-input clause"));
                }
                strict_seen = true;
                attributes.strict = true;
            } else if self.eat_ident("returns")? {
                self.expect_ident("null")?;
                self.expect_ident("on")?;
                self.expect_ident("null")?;
                self.expect_ident("input")?;
                if strict_seen {
                    return Err(self.unexpected("one null-input clause"));
                }
                strict_seen = true;
                attributes.strict = true;
            } else if self.eat_ident("called")? {
                self.expect_ident("on")?;
                self.expect_ident("null")?;
                self.expect_ident("input")?;
                if strict_seen {
                    return Err(self.unexpected("one null-input clause"));
                }
                strict_seen = true;
                attributes.strict = false;
            } else if self.eat_ident("immutable")? {
                if volatility_seen {
                    return Err(self.unexpected("one volatility clause"));
                }
                volatility_seen = true;
                attributes.volatility = crate::sql::ast::RoutineVolatility::Immutable;
            } else if self.eat_ident("stable")? {
                if volatility_seen {
                    return Err(self.unexpected("one volatility clause"));
                }
                volatility_seen = true;
                attributes.volatility = crate::sql::ast::RoutineVolatility::Stable;
            } else if self.eat_ident("volatile")? {
                if volatility_seen {
                    return Err(self.unexpected("one volatility clause"));
                }
                volatility_seen = true;
                attributes.volatility = crate::sql::ast::RoutineVolatility::Volatile;
            } else if self.eat_ident("parallel")? {
                if parallel_seen {
                    return Err(self.unexpected("one PARALLEL clause"));
                }
                parallel_seen = true;
                attributes.parallel = if self.eat_ident("safe")? {
                    crate::sql::ast::RoutineParallel::Safe
                } else if self.eat_ident("restricted")? {
                    crate::sql::ast::RoutineParallel::Restricted
                } else {
                    self.expect_ident("unsafe")?;
                    crate::sql::ast::RoutineParallel::Unsafe
                };
            } else if self.eat_ident("security")? {
                if security_seen {
                    return Err(self.unexpected("one SECURITY clause"));
                }
                security_seen = true;
                attributes.security_definer = if self.eat_ident("definer")? {
                    true
                } else {
                    self.expect_ident("invoker")?;
                    false
                };
            } else if self.eat_ident("external")? {
                self.expect_ident("security")?;
                if security_seen {
                    return Err(self.unexpected("one SECURITY clause"));
                }
                security_seen = true;
                attributes.security_definer = if self.eat_ident("definer")? {
                    true
                } else {
                    self.expect_ident("invoker")?;
                    false
                };
            } else if self.eat_ident("leakproof")? {
                if leakproof_seen {
                    return Err(self.unexpected("one LEAKPROOF clause"));
                }
                leakproof_seen = true;
                attributes.leakproof = true;
            } else if self.eat_ident("not")? {
                self.expect_ident("leakproof")?;
                if leakproof_seen {
                    return Err(self.unexpected("one LEAKPROOF clause"));
                }
                leakproof_seen = true;
                attributes.leakproof = false;
            } else if self.eat_ident("cost")? {
                if cost_seen {
                    return Err(self.unexpected("one COST clause"));
                }
                cost_seen = true;
                let Tok::Num(raw) = self.peeked else {
                    return Err(self.err_here("COST must be a positive number"));
                };
                attributes.cost = Some(
                    raw.parse::<f64>()
                        .ok()
                        .and_then(crate::sql::ast::RoutineEstimate::new)
                        .ok_or_else(|| self.err_here("COST must be a positive number"))?,
                );
                self.advance()?;
            } else if self.eat_ident("rows")? {
                if rows_seen {
                    return Err(self.unexpected("one ROWS clause"));
                }
                rows_seen = true;
                let Tok::Num(raw) = self.peeked else {
                    return Err(self.err_here("ROWS must be a positive number"));
                };
                attributes.rows = Some(
                    raw.parse::<f64>()
                        .ok()
                        .and_then(crate::sql::ast::RoutineEstimate::new)
                        .ok_or_else(|| self.err_here("ROWS must be a positive number"))?,
                );
                self.advance()?;
            } else if self.eat_ident("window")? {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "user-defined window functions are not supported"),
                    sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                });
            } else if self.eat_ident("transform")? {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "routine transforms are not supported"),
                    sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                });
            } else if self.eat_ident("support")? {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "planner support functions are not supported"),
                    sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                });
            } else if self.eat_ident("set")? {
                let name_start = self.peek_at;
                let _ = self.any_ident("configuration parameter")?;
                while self.eat_op(".")? {
                    let _ = self.any_ident("configuration parameter")?;
                }
                let config_name = self.text[name_start..self.peek_at].trim();
                let value = if self.eat_ident("from")? {
                    self.expect_ident("current")?;
                    crate::sql::ast::RoutineConfigValue::Current
                } else {
                    if !self.eat_op("=")? {
                        self.expect_ident("to")?;
                    }
                    let value_start = self.peek_at;
                    loop {
                        if matches!(self.peeked, Tok::Op("+") | Tok::Op("-")) {
                            self.advance()?;
                        }
                        if !matches!(self.peeked, Tok::Ident(_) | Tok::Num(_) | Tok::Str(_)) {
                            return Err(self.unexpected("configuration value"));
                        }
                        self.advance()?;
                        if !self.eat_op(",")? {
                            break;
                        }
                    }
                    crate::sql::ast::RoutineConfigValue::Value(
                        self.text[value_start..self.peek_at].trim(),
                    )
                };
                if let Some(existing) = configs[..config_count]
                    .iter_mut()
                    .find(|config| config.name.eq_ignore_ascii_case(config_name))
                {
                    existing.value = value;
                } else {
                    if config_count == configs.len() {
                        return Err(self.limit("routine configuration clauses", configs.len()));
                    }
                    configs[config_count] = crate::sql::ast::RoutineConfigClause {
                        name: config_name,
                        value,
                    };
                    config_count += 1;
                }
            } else {
                return Err(self.unexpected("routine option"));
            }
        }
        let language = language.ok_or_else(|| self.unexpected("LANGUAGE clause"))?;
        if body.is_some_and(|body| {
            !matches!(body, crate::sql::ast::RoutineBody::String(_))
                && language != crate::sql::ast::RoutineLanguage::Sql
        }) {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "SQL-standard bodies require LANGUAGE SQL"),
                sqlstate: sqlstate::INVALID_FUNCTION_DEFINITION,
            });
        }
        if !function
            && (strict_seen
                || volatility_seen
                || parallel_seen
                || leakproof_seen
                || cost_seen
                || rows_seen)
        {
            return Err(self.unexpected("procedure option"));
        }
        if attributes.rows.is_some()
            && !matches!(
                kind,
                RoutineCreateKind::Function {
                    set_returning: true,
                    ..
                } | RoutineCreateKind::OutputFunction {
                    set_returning: true,
                    ..
                } | RoutineCreateKind::TableFunction { .. }
            )
        {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(
                    96,
                    "ROWS is not applicable when function does not return a set"
                ),
                sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            });
        }
        if matches!(
            kind,
            RoutineCreateKind::Trigger | RoutineCreateKind::EventTrigger
        ) && language != crate::sql::ast::RoutineLanguage::PlPgSql
        {
            return Err(self.unexpected("trigger functions require LANGUAGE plpgsql"));
        }
        if language == crate::sql::ast::RoutineLanguage::PlPgSql
            && !matches!(
                kind,
                RoutineCreateKind::Function { .. }
                    | RoutineCreateKind::OutputFunction { .. }
                    | RoutineCreateKind::TableFunction { .. }
                    | RoutineCreateKind::Trigger
                    | RoutineCreateKind::EventTrigger
                    | RoutineCreateKind::Procedure
            )
        {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "PL/pgSQL function execution is not supported"),
                sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
            });
        }
        Ok(Stmt::CreateRoutine(CreateRoutine {
            name,
            or_replace,
            arguments: self.arena_slice(&arguments[..count])?,
            kind,
            language,
            attributes,
            configs: self.arena_slice(&configs[..config_count])?,
            body: body.ok_or_else(|| self.unexpected("routine body"))?,
        }))
    }

    fn create_role(&mut self, user_spelling: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.any_ident("role name")?;
        let _ = self.eat_ident("with")?;
        let mut options = RoleOptions::EMPTY;
        let mut in_roles: &'a [&'a str] = &[];
        let mut role_members: &'a [&'a str] = &[];
        let mut admin_members: &'a [&'a str] = &[];
        loop {
            if self.role_option(&mut options)? {
                continue;
            }
            if self.eat_ident("in")? {
                if !self.eat_ident("role")? {
                    self.expect_ident("group")?;
                }
                in_roles = self.role_name_list("role name")?;
            } else if self.eat_ident("role")? {
                role_members = self.role_name_list("member role name")?;
            } else if self.eat_ident("admin")? {
                admin_members = self.role_name_list("member role name")?;
            } else {
                break;
            }
        }
        if options.can_login.is_none() {
            options.can_login = Some(user_spelling);
        }
        Ok(Stmt::CreateRole {
            name,
            options,
            memberships: crate::sql::ast::RoleMembershipClauses {
                in_roles,
                role_members,
                admin_members,
            },
        })
    }

    pub(super) fn alter_role(&mut self) -> Result<Stmt<'a>, ParseError> {
        let written_name = self.any_ident("role name")?;
        let role = (!written_name.eq_ignore_ascii_case("all")).then_some(written_name);
        let database = if self.eat_ident("in")? {
            self.expect_ident("database")?;
            Some(self.any_ident("database name")?)
        } else {
            None
        };
        if self.eat_ident("set")? {
            let name = self.any_ident("configuration parameter")?;
            let value = if self.eat_ident("from")? {
                self.expect_ident("current")?;
                crate::sql::ast::RoutineConfigValue::Current
            } else {
                if !self.eat_op("=")? {
                    self.expect_ident("to")?;
                }
                let start = self.peek_at;
                while !matches!(self.peeked, Tok::Op(";") | Tok::Eof) {
                    self.advance()?;
                }
                let value = self.text[start..self.peek_at].trim();
                if value.is_empty() {
                    return Err(self.unexpected("configuration value"));
                }
                crate::sql::ast::RoutineConfigValue::Value(value)
            };
            return Ok(Stmt::AlterRoleSetting {
                role,
                database,
                action: crate::sql::ast::RoleSettingAction::Set { name, value },
            });
        }
        if self.eat_ident("reset")? {
            return Ok(Stmt::AlterRoleSetting {
                role,
                database,
                action: crate::sql::ast::RoleSettingAction::Reset(
                    (!self.eat_ident("all")?)
                        .then(|| self.any_ident("configuration parameter"))
                        .transpose()?,
                ),
            });
        }
        let Some(name) = role else {
            return Err(self.unexpected("expected SET or RESET for ALTER ROLE ALL"));
        };
        if database.is_some() {
            return Err(self.unexpected("expected SET or RESET after IN DATABASE"));
        }
        if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            return Ok(Stmt::AlterRoleRename {
                name,
                new_name: self.any_ident("new role name")?,
            });
        }
        let _ = self.eat_ident("with")?;
        let options = self.role_options()?;
        if options == RoleOptions::EMPTY {
            return Err(self.unexpected("expected a role option"));
        }
        Ok(Stmt::AlterRole { name, options })
    }

    fn role_options(&mut self) -> Result<RoleOptions<'a>, ParseError> {
        let mut options = RoleOptions::EMPTY;
        while self.role_option(&mut options)? {}
        Ok(options)
    }

    fn role_option(&mut self, options: &mut RoleOptions<'a>) -> Result<bool, ParseError> {
        if self.eat_ident("superuser")? {
            options.superuser = Some(true);
        } else if self.eat_ident("nosuperuser")? {
            options.superuser = Some(false);
        } else if self.eat_ident("inherit")? {
            options.inherit = Some(true);
        } else if self.eat_ident("noinherit")? {
            options.inherit = Some(false);
        } else if self.eat_ident("createrole")? {
            options.create_role = Some(true);
        } else if self.eat_ident("nocreaterole")? {
            options.create_role = Some(false);
        } else if self.eat_ident("createdb")? {
            options.create_database = Some(true);
        } else if self.eat_ident("nocreatedb")? {
            options.create_database = Some(false);
        } else if self.eat_ident("login")? {
            options.can_login = Some(true);
        } else if self.eat_ident("nologin")? {
            options.can_login = Some(false);
        } else if self.eat_ident("replication")? {
            options.replication = Some(true);
        } else if self.eat_ident("noreplication")? {
            options.replication = Some(false);
        } else if self.eat_ident("bypassrls")? {
            options.bypass_row_level_security = Some(true);
        } else if self.eat_ident("nobypassrls")? {
            options.bypass_row_level_security = Some(false);
        } else if self.eat_ident("connection")? {
            self.expect_ident("limit")?;
            let negative = self.eat_op("-")?;
            let Tok::Num(raw) = self.peeked else {
                return Err(self.unexpected("expected connection limit"));
            };
            let parsed = raw
                .parse::<i32>()
                .map_err(|_| self.unexpected("connection limit is out of range"))?;
            self.advance()?;
            options.connection_limit = Some(if negative { -parsed } else { parsed });
        } else if self.eat_ident("password")? {
            options.password = Some(if self.eat_ident("null")? {
                None
            } else {
                Some(self.str_literal("password")?)
            });
        } else if self.eat_ident("valid")? {
            self.expect_ident("until")?;
            options.valid_until = Some(if self.eat_ident("null")? {
                None
            } else {
                Some(self.str_literal("VALID UNTIL")?)
            });
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    /// The shared tail of `CREATE TABLE ... AS` / `CREATE MATERIALIZED VIEW`:
    /// capture the SELECT text (validated now, re-run to populate the backing
    /// table), then an optional `WITH [NO] DATA`.
    fn create_table_as(
        &mut self,
        name: QualName<'a>,
        columns: &'a [&'a str],
        if_not_exists: bool,
        kind: crate::sql::ast::CreateTableAsKind,
    ) -> Result<Stmt<'a>, ParseError> {
        let start = self.peek_at;
        let _ = self.query_select()?;
        let end = self.peek_at;
        let sql = self.text[start..end].trim();
        let with_data = if self.eat_ident("with")? {
            let no = self.eat_ident("no")?;
            self.expect_ident("data")?;
            !no
        } else {
            true
        };
        Ok(Stmt::CreateTableAs {
            name,
            columns,
            sql,
            with_data,
            if_not_exists,
            kind,
        })
    }

    /// `CREATE SEQUENCE [IF NOT EXISTS] name [options]` ("create sequence"
    /// consumed).
    fn create_sequence(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_not_exists = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.qual_name("sequence name")?;
        let options = self.seq_options(false)?;
        Ok(Stmt::CreateSequence {
            name,
            if_not_exists,
            options,
        })
    }

    /// `CREATE DOMAIN name [AS] basetype[(typmod)] [constraint...]` ("domain"
    /// consumed). A constraint is `[CONSTRAINT c] { NOT NULL | NULL | CHECK
    /// (expr) }` or `DEFAULT expr`.
    fn create_domain(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("domain name")?;
        let _ = self.eat_ident("as")?;
        let (base_type, base_type_mod) = self.type_name_mod()?;
        let mut not_null = false;
        let mut default_text = None;
        let mut checks = [DomainCheck {
            name: None,
            expression: "",
            validation: ConstraintValidation::EnforcedValidated,
        }; MAX_LIST];
        let mut n_checks = 0;
        loop {
            let cname = if self.eat_ident("constraint")? {
                Some(self.col_ident("constraint name")?)
            } else {
                None
            };
            if self.eat_ident("not")? {
                self.expect_ident("null")?;
                not_null = true;
            } else if self.eat_ident("null")? {
                not_null = false;
            } else if self.eat_ident("check")? {
                if n_checks == MAX_LIST {
                    return Err(self.limit("domain CHECK constraints", MAX_LIST));
                }
                checks[n_checks] = DomainCheck {
                    name: cname,
                    expression: self.check_text()?,
                    validation: ConstraintValidation::EnforcedValidated,
                };
                n_checks += 1;
            } else if cname.is_none() && self.eat_ident("default")? {
                let start = self.peek_at;
                let _ = self.column_default_expression()?;
                default_text = Some(self.arena_str(self.text[start..self.peek_at].trim_end())?);
            } else if cname.is_some() {
                return Err(self.err_here("expected NOT NULL or CHECK after CONSTRAINT"));
            } else {
                break;
            }
        }
        let checks = self.arena_slice(&checks[..n_checks])?;
        Ok(Stmt::CreateDomain(CreateDomain {
            name,
            base_type,
            base_type_mod,
            not_null,
            default_text,
            checks,
        }))
    }

    /// Captures the raw source text of a `CHECK (expr)` predicate (the paren
    /// already implied by the caller having consumed `CHECK`).
    fn check_text(&mut self) -> Result<&'a str, ParseError> {
        self.expect_op("(")?;
        let start = self.peek_at;
        let _ = self.expression(0)?;
        let text = self.arena_str(self.text[start..self.peek_at].trim_end())?;
        self.expect_op(")")?;
        Ok(text)
    }

    /// `ALTER DOMAIN name <action>` ("alter domain" consumed).
    pub(super) fn alter_domain(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("domain name")?;
        if self.peeked == Tok::Ident("owner") {
            return self.alter_owner(crate::sql::ast::AlterOwnerKind::Domain, name, false);
        }
        let action = if self.eat_ident("add")? {
            let cname = if self.eat_ident("constraint")? {
                Some(self.col_ident("constraint name")?)
            } else {
                None
            };
            self.expect_ident("check")?;
            let expression = self.check_text()?;
            let validation = if self.eat_ident("not")? {
                self.expect_ident("valid")?;
                ConstraintValidation::EnforcedNotValid
            } else {
                ConstraintValidation::EnforcedValidated
            };
            AlterDomainAction::AddCheck(DomainCheck {
                name: cname,
                expression,
                validation,
            })
        } else if self.eat_ident("validate")? {
            self.expect_ident("constraint")?;
            AlterDomainAction::ValidateConstraint(self.col_ident("constraint name")?)
        } else if self.eat_ident("drop")? {
            if self.eat_ident("constraint")? {
                let if_exists = if self.eat_ident("if")? {
                    self.expect_ident("exists")?;
                    true
                } else {
                    false
                };
                let name = self.col_ident("constraint name")?;
                let cascade = if self.eat_ident("cascade")? {
                    true
                } else {
                    let _ = self.eat_ident("restrict")?;
                    false
                };
                AlterDomainAction::DropConstraint {
                    name,
                    if_exists,
                    cascade,
                }
            } else if self.eat_ident("not")? {
                self.expect_ident("null")?;
                AlterDomainAction::DropNotNull
            } else {
                self.expect_ident("default")?;
                AlterDomainAction::DropDefault
            }
        } else if self.eat_ident("set")? {
            if self.eat_ident("not")? {
                self.expect_ident("null")?;
                AlterDomainAction::SetNotNull
            } else if self.eat_ident("schema")? {
                AlterDomainAction::SetSchema(self.col_ident("schema name")?)
            } else {
                self.expect_ident("default")?;
                let start = self.peek_at;
                let _ = self.expression(0)?;
                AlterDomainAction::SetDefault(
                    self.arena_str(self.text[start..self.peek_at].trim_end())?,
                )
            }
        } else if self.eat_ident("rename")? {
            if self.eat_ident("constraint")? {
                let from = self.col_ident("constraint name")?;
                self.expect_ident("to")?;
                AlterDomainAction::RenameConstraint {
                    from,
                    to: self.col_ident("new constraint name")?,
                }
            } else {
                self.expect_ident("to")?;
                AlterDomainAction::Rename(self.col_ident("new domain name")?)
            }
        } else {
            return Err(
                self.err_here("expected ADD, DROP, RENAME, SET or VALIDATE after ALTER DOMAIN")
            );
        };
        Ok(Stmt::AlterDomain { name, action })
    }

    /// `DROP DOMAIN [IF EXISTS] name [, ...] [CASCADE|RESTRICT]` ("domain"
    /// consumed).
    pub(super) fn drop_domain(&mut self) -> Result<Stmt<'a>, ParseError> {
        let (names, if_exists) = self.drop_targets("domain name")?;
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        Ok(Stmt::DropDomain {
            names,
            if_exists,
            cascade,
        })
    }

    /// `CREATE TYPE name AS ENUM (...)` or `CREATE TYPE name AS (field type, ...)`.
    pub(super) fn create_type(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("type name")?;
        self.expect_ident("as")?;
        if self.eat_ident("enum")? {
            self.expect_op("(")?;
            let mut labels = [""; MAX_LIST];
            let mut n = 0;
            if self.peeked != Tok::Op(")") {
                loop {
                    if n == MAX_LIST {
                        return Err(self.limit("enum labels", MAX_LIST));
                    }
                    labels[n] = self.str_literal("enum label")?;
                    n += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
            }
            self.expect_op(")")?;
            return Ok(Stmt::CreateEnum {
                name,
                labels: self.arena_slice(&labels[..n])?,
            });
        }
        self.expect_op("(")?;
        let mut fields = [crate::sql::ast::CompositeField {
            name: "",
            type_name: "",
            type_mod: -1,
            collation: crate::sql::ast::ParsedCollation::DEFAULT,
        }; MAX_LIST];
        let mut n = 0;
        if self.peeked != Tok::Op(")") {
            loop {
                if n == MAX_LIST {
                    return Err(self.limit("composite fields", MAX_LIST));
                }
                let field_name = self.any_ident("composite field name")?;
                let (type_name, type_mod) = self.type_name_mod()?;
                let collation = if self.eat_ident("collate")? {
                    self.collation_name()?
                } else {
                    crate::sql::ast::ParsedCollation::DEFAULT
                };
                fields[n] = crate::sql::ast::CompositeField {
                    name: field_name,
                    type_name,
                    type_mod,
                    collation,
                };
                n += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
        }
        self.expect_op(")")?;
        Ok(Stmt::CreateComposite {
            name,
            fields: self.arena_slice(&fields[..n])?,
        })
    }

    /// `ALTER TYPE name <action>` ("alter type" consumed).
    pub(super) fn alter_type(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("type name")?;
        if self.peeked == Tok::Ident("owner") {
            return self.alter_owner(crate::sql::ast::AlterOwnerKind::Type, name, false);
        }
        let action = if self.eat_ident("add")? {
            if self.eat_ident("attribute")? {
                let field_name = self.any_ident("composite field name")?;
                let (type_name, type_mod) = self.type_name_mod()?;
                let collation = if self.eat_ident("collate")? {
                    self.collation_name()?
                } else {
                    crate::sql::ast::ParsedCollation::DEFAULT
                };
                AlterTypeAction::AddAttribute(crate::sql::ast::CompositeField {
                    name: field_name,
                    type_name,
                    type_mod,
                    collation,
                })
            } else {
                self.expect_ident("value")?;
                let if_not_exists = if self.eat_ident("if")? {
                    self.expect_ident("not")?;
                    self.expect_ident("exists")?;
                    true
                } else {
                    false
                };
                let label = self.str_literal("enum label")?;
                let (before, after) = if self.eat_ident("before")? {
                    (Some(self.str_literal("enum label")?), None)
                } else if self.eat_ident("after")? {
                    (None, Some(self.str_literal("enum label")?))
                } else {
                    (None, None)
                };
                AlterTypeAction::AddValue {
                    label,
                    if_not_exists,
                    before,
                    after,
                }
            }
        } else if self.eat_ident("set")? {
            self.expect_ident("schema")?;
            AlterTypeAction::SetSchema(self.col_ident("schema name")?)
        } else if self.eat_ident("rename")? {
            if self.eat_ident("attribute")? {
                let from = self.any_ident("composite field name")?;
                self.expect_ident("to")?;
                AlterTypeAction::RenameAttribute {
                    from,
                    to: self.any_ident("new composite field name")?,
                }
            } else if self.eat_ident("value")? {
                let from = self.str_literal("enum label")?;
                self.expect_ident("to")?;
                let to = self.str_literal("enum label")?;
                AlterTypeAction::RenameValue { from, to }
            } else {
                self.expect_ident("to")?;
                AlterTypeAction::RenameTo(self.col_ident("new type name")?)
            }
        } else if self.eat_ident("drop")? {
            self.expect_ident("attribute")?;
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            AlterTypeAction::DropAttribute {
                name: self.any_ident("composite field name")?,
                if_exists,
            }
        } else if self.eat_ident("alter")? {
            self.expect_ident("attribute")?;
            let field = self.any_ident("composite field name")?;
            if self.eat_ident("set")? {
                self.expect_ident("data")?;
                self.expect_ident("type")?;
                let (type_name, type_mod) = self.type_name_mod()?;
                let collation = if self.eat_ident("collate")? {
                    self.collation_name()?
                } else {
                    crate::sql::ast::ParsedCollation::DEFAULT
                };
                let cascade = if self.eat_ident("cascade")? {
                    true
                } else {
                    let _ = self.eat_ident("restrict")?;
                    false
                };
                AlterTypeAction::AlterAttributeType {
                    name: field,
                    type_name,
                    type_mod,
                    collation,
                    cascade,
                }
            } else if self.eat_ident("type")? {
                let (type_name, type_mod) = self.type_name_mod()?;
                let collation = if self.eat_ident("collate")? {
                    self.collation_name()?
                } else {
                    crate::sql::ast::ParsedCollation::DEFAULT
                };
                let cascade = if self.eat_ident("cascade")? {
                    true
                } else {
                    let _ = self.eat_ident("restrict")?;
                    false
                };
                AlterTypeAction::AlterAttributeType {
                    name: field,
                    type_name,
                    type_mod,
                    collation,
                    cascade,
                }
            } else {
                return Err(self.err_here("expected SET DATA TYPE after ALTER ATTRIBUTE"));
            }
        } else {
            return Err(
                self.err_here("expected composite attribute or enum action after ALTER TYPE")
            );
        };
        Ok(Stmt::AlterType { name, action })
    }

    /// `DROP TYPE [IF EXISTS] name [, ...] [CASCADE|RESTRICT]` ("type" consumed).
    pub(super) fn drop_type(&mut self) -> Result<Stmt<'a>, ParseError> {
        let (names, if_exists) = self.drop_targets("type name")?;
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        Ok(Stmt::DropType {
            names,
            if_exists,
            cascade,
        })
    }

    /// `ALTER SEQUENCE [IF EXISTS] name [options] [RESTART [WITH n]]` ("alter
    /// sequence" consumed).
    pub(super) fn alter_sequence(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.qual_name("sequence name")?;
        if self.peeked == Tok::Ident("owner") {
            return self.alter_owner(crate::sql::ast::AlterOwnerKind::Sequence, name, if_exists);
        }
        if self.eat_ident("set")? {
            self.expect_ident("schema")?;
            return Ok(Stmt::AlterSequence {
                name,
                if_exists,
                options: crate::sql::ast::SeqOptions::EMPTY,
                set_schema: Some(self.col_ident("schema name")?),
            });
        }
        let options = self.seq_options(true)?;
        Ok(Stmt::AlterSequence {
            name,
            if_exists,
            options,
            set_schema: None,
        })
    }

    /// The shared CREATE/ALTER SEQUENCE option list. `allow_restart` enables the
    /// ALTER-only `RESTART [WITH n]` clause.
    pub(super) fn seq_options(
        &mut self,
        allow_restart: bool,
    ) -> Result<crate::sql::ast::SeqOptions<'a>, ParseError> {
        use crate::sql::ast::{QualName, SeqBound, SeqOptions, SeqOwner};
        let mut o = SeqOptions::EMPTY;
        loop {
            if self.eat_ident("as")? {
                o.data_type = Some(self.any_ident("sequence data type")?);
            } else if self.eat_ident("increment")? {
                let _ = self.eat_ident("by")?;
                o.increment = Some(self.seq_int()?);
            } else if self.eat_ident("minvalue")? {
                o.min_value = SeqBound::Value(self.seq_int()?);
            } else if self.eat_ident("maxvalue")? {
                o.max_value = SeqBound::Value(self.seq_int()?);
            } else if self.eat_ident("start")? {
                let _ = self.eat_ident("with")?;
                o.start = Some(self.seq_int()?);
            } else if self.eat_ident("cache")? {
                o.cache = Some(self.seq_int()?);
            } else if self.eat_ident("cycle")? {
                o.cycle = Some(true);
            } else if self.eat_ident("no")? {
                if self.eat_ident("minvalue")? {
                    o.min_value = SeqBound::NoBound;
                } else if self.eat_ident("maxvalue")? {
                    o.max_value = SeqBound::NoBound;
                } else {
                    self.expect_ident("cycle")?;
                    o.cycle = Some(false);
                }
            } else if self.eat_ident("owned")? {
                self.expect_ident("by")?;
                o.owned_by = if self.eat_ident("none")? {
                    Some(None)
                } else {
                    let first = self.col_ident("owner relation")?;
                    self.expect_op(".")?;
                    let second = self.col_ident("owner column")?;
                    if self.eat_op(".")? {
                        let third = self.col_ident("owner column")?;
                        Some(Some(SeqOwner {
                            table: QualName {
                                schema: Some(first),
                                name: second,
                            },
                            column: third,
                        }))
                    } else {
                        Some(Some(SeqOwner {
                            table: QualName {
                                schema: None,
                                name: first,
                            },
                            column: second,
                        }))
                    }
                };
            } else if allow_restart && self.eat_ident("restart")? {
                // RESTART [WITH] n, or bare RESTART (reposition to the start).
                let value = if self.eat_ident("with")?
                    || matches!(self.peeked, Tok::Num(_))
                    || self.peeked == Tok::Op("-")
                {
                    Some(self.seq_int()?)
                } else {
                    None
                };
                o.restart = Some(value);
            } else {
                break;
            }
        }
        Ok(o)
    }

    /// A signed integer literal for a sequence option. Parses through `i128` so
    /// `MINVALUE -9223372036854775808` (i64::MIN) is representable.
    pub(super) fn seq_int(&mut self) -> Result<i64, ParseError> {
        let negative = self.eat_op("-")?;
        if !negative {
            let _ = self.eat_op("+")?;
        }
        match self.peeked {
            Tok::Num(text) => {
                let magnitude: i128 = text
                    .parse()
                    .map_err(|_| self.unexpected("expected an integer"))?;
                self.advance()?;
                let value = if negative { -magnitude } else { magnitude };
                i64::try_from(value)
                    .map_err(|_| self.unexpected("sequence option value out of range for bigint"))
            }
            _ => Err(self.unexpected("expected an integer")),
        }
    }

    /// The tail of a `GENERATED` column clause ("generated" consumed): either
    /// `ALWAYS AS (expr) STORED` or `{ ALWAYS | BY DEFAULT } AS IDENTITY
    /// [(sequence options)]`.
    pub(super) fn generated_clause(&mut self) -> Result<crate::sql::ast::ColGen<'a>, ParseError> {
        use crate::sql::ast::{ColGen, IdentitySpec};
        let always = if self.eat_ident("always")? {
            self.expect_ident("as")?;
            if self.eat_op("(")? {
                let start = self.peek_at;
                let _ = self.expression(0)?;
                let text = self.text[start..self.peek_at].trim_end();
                self.expect_op(")")?;
                self.expect_ident("stored")?;
                return Ok(ColGen::Generated(text));
            }
            true
        } else {
            self.expect_ident("by")?;
            self.expect_ident("default")?;
            self.expect_ident("as")?;
            false
        };
        self.expect_ident("identity")?;
        // Optional `(START WITH n INCREMENT BY n ...)` sequence options.
        let mut sequence_name = None;
        let mut options = crate::sql::ast::SeqOptions::EMPTY;
        if self.eat_op("(")? {
            while !self.eat_op(")")? {
                if self.eat_ident("sequence")? {
                    self.expect_ident("name")?;
                    sequence_name = Some(self.qual_name("identity sequence name")?);
                } else if self.eat_ident("start")? {
                    let _ = self.eat_ident("with")?;
                    options.start = Some(self.seq_int()?);
                } else if self.eat_ident("increment")? {
                    let _ = self.eat_ident("by")?;
                    options.increment = Some(self.seq_int()?);
                } else if self.eat_ident("minvalue")? {
                    options.min_value = crate::sql::ast::SeqBound::Value(self.seq_int()?);
                } else if self.eat_ident("maxvalue")? {
                    options.max_value = crate::sql::ast::SeqBound::Value(self.seq_int()?);
                } else if self.eat_ident("cache")? {
                    options.cache = Some(self.seq_int()?);
                } else if self.eat_ident("cycle")? {
                    options.cycle = Some(true);
                } else if self.eat_ident("no")? {
                    if self.eat_ident("minvalue")? {
                        options.min_value = crate::sql::ast::SeqBound::NoBound;
                    } else if self.eat_ident("maxvalue")? {
                        options.max_value = crate::sql::ast::SeqBound::NoBound;
                    } else {
                        self.expect_ident("cycle")?;
                        options.cycle = Some(false);
                    }
                } else {
                    return Err(self.err_here("expected an identity sequence option"));
                }
            }
        }
        Ok(ColGen::Identity(IdentitySpec {
            always,
            sequence_name,
            options,
        }))
    }

    /// `CREATE MATERIALIZED VIEW [IF NOT EXISTS] name [(col, ...)] AS <query>
    /// [WITH [NO] DATA]` ("create materialized view" consumed). A `(` here is
    /// always a column-name list (a materialized view has no column defs).
    fn create_materialized_view(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_not_exists = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.qual_name("materialized view name")?;
        let mut columns: &'a [&'a str] = &[];
        if self.peeked == Tok::Op("(") {
            self.expect_op("(")?;
            let mut list = [""; MAX_LIST];
            let mut m = 0;
            loop {
                if m == MAX_LIST {
                    return Err(self.limit("column list", MAX_LIST));
                }
                list[m] = self.col_ident("column name")?;
                m += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
            columns = self.arena_slice(&list[..m])?;
        }
        self.expect_ident("as")?;
        self.create_table_as(
            name,
            columns,
            if_not_exists,
            crate::sql::ast::CreateTableAsKind::MaterializedView,
        )
    }

    /// CREATE SCHEMA [IF NOT EXISTS] { name [AUTHORIZATION role] |
    /// AUTHORIZATION role } [schema_element ...] ("create schema" consumed).
    /// Schema elements are the embedded CREATE statements, run with the new
    /// schema as their creation target.
    fn create_schema(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_not_exists = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = if self.peeked == Tok::Ident("authorization") {
            None
        } else {
            Some(self.col_ident("schema name")?)
        };
        let mut authorization = None;
        let name = if self.eat_ident("authorization")? {
            let role = self.col_ident("role name")?;
            authorization = Some(role);
            // An omitted name defaults to the role's name, as PostgreSQL.
            name.unwrap_or(role)
        } else {
            let Some(n) = name else {
                return Err(self.err_here("expected schema name or AUTHORIZATION"));
            };
            n
        };
        static EMPTY_SCHEMA_ELEMENT: CreateSchemaElement<'static> =
            CreateSchemaElement::Table(CreateTable {
                name: QualName {
                    schema: None,
                    name: "",
                },
                columns: &[],
                constraints: &[],
                likes: &[],
                partition: PartitionClause::None,
                membership: TableMembership::None,
                persistence: RelationPersistence::Permanent,
                access_method: TableAccessMethod::Heap,
                tablespace: None,
                storage_options: RelationStorageOptions::DEFAULT,
                if_not_exists: false,
            });
        let mut elements: [&'a CreateSchemaElement<'a>; 16] = [&EMPTY_SCHEMA_ELEMENT; 16];
        let mut n = 0usize;
        while self.peeked == Tok::Ident("create") {
            if n == elements.len() {
                return Err(self.limit("schema elements", elements.len()));
            }
            let element = match self.create()? {
                Stmt::CreateTable(table) => CreateSchemaElement::Table(table),
                Stmt::CreateView {
                    name,
                    or_replace,
                    security,
                    check_option,
                    sql,
                } => CreateSchemaElement::View {
                    name,
                    or_replace,
                    security,
                    check_option,
                    sql,
                },
                Stmt::CreateIndex {
                    name,
                    table,
                    build,
                    scope,
                    if_not_exists,
                    columns,
                    include_columns,
                    nulls_not_distinct,
                    predicate,
                    predicate_text,
                    options,
                    tablespace,
                    unique,
                } => CreateSchemaElement::Index {
                    name,
                    table,
                    build,
                    scope,
                    if_not_exists,
                    columns,
                    include_columns,
                    nulls_not_distinct,
                    predicate,
                    predicate_text,
                    options,
                    tablespace,
                    unique,
                },
                Stmt::CreateSequence {
                    name,
                    if_not_exists,
                    options,
                } => CreateSchemaElement::Sequence {
                    name,
                    if_not_exists,
                    options,
                },
                Stmt::CreateDomain(domain) => CreateSchemaElement::Domain(domain),
                Stmt::CreateEnum { name, labels } => CreateSchemaElement::Enum { name, labels },
                Stmt::CreateComposite { name, fields } => {
                    CreateSchemaElement::Composite { name, fields }
                }
                _ => {
                    return Err(self.err_here(
                        "CREATE SCHEMA elements may be CREATE TABLE, VIEW, INDEX, SEQUENCE, DOMAIN, or TYPE",
                    ));
                }
            };
            elements[n] = self
                .arena
                .alloc(element)
                .map_err(|_| self.err_here("statement too large for SQL arena"))?;
            n += 1;
        }
        Ok(Stmt::CreateSchema {
            name,
            authorization,
            if_not_exists,
            elements: self.arena_slice(&elements[..n])?,
        })
    }

    /// CREATE INDEX after the INDEX keyword. Syntax defaults become explicit
    /// typed fields before execution sees the statement.
    fn create_index(&mut self, unique: bool) -> Result<Stmt<'a>, ParseError> {
        let build = if self.eat_ident("concurrently")? {
            IndexBuildMode::Concurrent
        } else {
            IndexBuildMode::Blocking
        };
        let if_not_exists = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = if self.peeked == Tok::Ident("on") {
            if if_not_exists {
                return Err(self.err_here("IF NOT EXISTS requires an index name"));
            }
            None
        } else {
            Some(self.col_ident("index name")?)
        };
        self.expect_ident("on")?;
        let scope = if self.eat_ident("only")? {
            IndexTargetScope::Only
        } else {
            IndexTargetScope::Recurse
        };
        let table = self.qual_name("table name")?;
        if self.eat_ident("using")? {
            let method = self.any_ident("index access method")?;
            if !method.eq_ignore_ascii_case("btree") {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(
                        96,
                        "index access method \"{}\" is not supported",
                        method
                    ),
                    sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                });
            }
        }
        self.expect_op("(")?;
        let null_expression = self.arena_expr(Expr::Null)?;
        let mut columns = [crate::sql::ast::IndexColumn {
            column: None,
            expression: null_expression,
            expression_text: "",
            collation: None,
            operator_class: None,
            descending: false,
            nulls_first: false,
        }; MAX_LIST];
        let mut n = 0;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("index columns", MAX_LIST));
            }
            let start = self.peek_at;
            let expression = self.expression(0)?;
            let expression_text =
                index_expression_source(expression, self.text[start..self.peek_at].trim_end());
            let column = match expression {
                Expr::Column {
                    qualifier: None,
                    name,
                } => Some(*name),
                _ => None,
            };
            let collation = if self.eat_ident("collate")? {
                Some(self.collation_name()?)
            } else {
                None
            };
            let operator_class = match self.peeked {
                Tok::Ident(word)
                    if !word.eq_ignore_ascii_case("asc")
                        && !word.eq_ignore_ascii_case("desc")
                        && !word.eq_ignore_ascii_case("nulls") =>
                {
                    Some(self.qual_name("operator class")?)
                }
                _ => None,
            };
            if operator_class.is_some() && self.peeked == Tok::Op("(") {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "operator class parameters are not supported"),
                    sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                });
            }
            let descending = if self.eat_ident("asc")? {
                false
            } else {
                self.eat_ident("desc")?
            };
            let nulls_first = if self.eat_ident("nulls")? {
                if self.eat_ident("first")? {
                    true
                } else {
                    self.expect_ident("last")?;
                    false
                }
            } else {
                descending
            };
            columns[n] = crate::sql::ast::IndexColumn {
                column,
                expression,
                expression_text,
                collation,
                operator_class,
                descending,
                nulls_first,
            };
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        let columns = self.arena_slice(&columns[..n])?;
        let include_columns = if self.eat_ident("include")? {
            self.expect_op("(")?;
            let mut names = [""; MAX_LIST];
            let mut count = 0;
            loop {
                if count == names.len() {
                    return Err(self.limit("index included columns", names.len()));
                }
                names[count] = self.col_ident("included column name")?;
                count += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
            self.arena_slice(&names[..count])?
        } else {
            &[]
        };
        let nulls_not_distinct = if self.eat_ident("nulls")? {
            let not = self.eat_ident("not")?;
            self.expect_ident("distinct")?;
            not
        } else {
            false
        };
        let options = if self.eat_ident("with")? {
            self.expect_op("(")?;
            let options = self.index_storage_options()?;
            self.expect_op(")")?;
            options
        } else {
            IndexStorageOptions::DEFAULT
        };
        let tablespace = if self.eat_ident("tablespace")? {
            Some(self.any_ident("tablespace name")?)
        } else {
            None
        };
        let (predicate, predicate_text) = if self.eat_ident("where")? {
            let start = self.peek_at;
            let predicate = self.expression(0)?;
            (
                Some(predicate),
                Some(self.text[start..self.peek_at].trim_end()),
            )
        } else {
            (None, None)
        };
        Ok(Stmt::CreateIndex {
            name,
            table,
            build,
            scope,
            if_not_exists,
            columns,
            include_columns,
            nulls_not_distinct,
            predicate,
            predicate_text,
            options,
            tablespace,
            unique,
        })
    }

    fn index_storage_options(&mut self) -> Result<IndexStorageOptions, ParseError> {
        let mut options = IndexStorageOptions::DEFAULT;
        if self.peeked == Tok::Op(")") {
            return Ok(options);
        }
        loop {
            let option = self.any_ident("index storage parameter")?;
            let _ = self.eat_op("=")?;
            if option.eq_ignore_ascii_case("fillfactor") {
                if options.fillfactor.is_some() {
                    return Err(self.err_here("parameter \"fillfactor\" specified more than once"));
                }
                let Tok::Num(raw) = self.peeked else {
                    return Err(self.unexpected("fillfactor must be an integer"));
                };
                let value = raw
                    .parse::<u8>()
                    .map_err(|_| self.unexpected("fillfactor is out of range"))?;
                self.advance()?;
                if !(10..=100).contains(&value) {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(96, "fillfactor must be between 10 and 100"),
                        sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                    });
                }
                options.fillfactor = Some(value);
            } else if option.eq_ignore_ascii_case("deduplicate_items") {
                if options.deduplicate_items.is_some() {
                    return Err(
                        self.err_here("parameter \"deduplicate_items\" specified more than once")
                    );
                }
                let value = match self.peeked {
                    Tok::Ident("true" | "on") | Tok::Str("true" | "on" | "1") => true,
                    Tok::Ident("false" | "off") | Tok::Str("false" | "off" | "0") => false,
                    _ => return Err(self.err_here("parameter requires a boolean value")),
                };
                self.advance()?;
                options.deduplicate_items = Some(value);
            } else {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "unrecognized parameter \"{}\"", option),
                    sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                });
            }
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(options)
    }

    fn index_storage_option_names(&mut self) -> Result<IndexStorageOptionNames, ParseError> {
        let mut names = IndexStorageOptionNames::EMPTY;
        loop {
            let option = self.any_ident("index storage parameter")?;
            let flag = if option.eq_ignore_ascii_case("fillfactor") {
                &mut names.fillfactor
            } else if option.eq_ignore_ascii_case("deduplicate_items") {
                &mut names.deduplicate_items
            } else {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "unrecognized parameter \"{}\"", option),
                    sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                });
            };
            if core::mem::replace(flag, true) {
                return Err(self.err_here("index storage parameter specified more than once"));
            }
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(names)
    }

    pub(super) fn relation_storage_options(
        &mut self,
    ) -> Result<RelationStorageOptions, ParseError> {
        let mut options = RelationStorageOptions::DEFAULT;
        if self.peeked == Tok::Op(")") {
            return Ok(options);
        }
        loop {
            let option = self.any_ident("table storage parameter")?;
            self.expect_op("=")?;
            if option.eq_ignore_ascii_case("fillfactor") {
                if options.fillfactor.is_some() {
                    return Err(self.err_here("parameter \"fillfactor\" specified more than once"));
                }
                let Tok::Num(raw) = self.peeked else {
                    return Err(self.unexpected("fillfactor must be an integer"));
                };
                let value = raw
                    .parse::<u8>()
                    .map_err(|_| self.unexpected("fillfactor is out of range"))?;
                self.advance()?;
                if !(10..=100).contains(&value) {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(96, "fillfactor must be between 10 and 100"),
                        sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                    });
                }
                options.fillfactor = Some(value);
            } else {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "unrecognized parameter \"{}\"", option),
                    sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                });
            }
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(options)
    }

    pub(super) fn relation_storage_option_names(
        &mut self,
    ) -> Result<RelationStorageOptionNames, ParseError> {
        let mut names = RelationStorageOptionNames::EMPTY;
        loop {
            let option = self.any_ident("table storage parameter")?;
            let flag = if option.eq_ignore_ascii_case("fillfactor") {
                &mut names.fillfactor
            } else {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "unrecognized parameter \"{}\"", option),
                    sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
                });
            };
            if core::mem::replace(flag, true) {
                return Err(self.err_here("table storage parameter specified more than once"));
            }
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok(names)
    }

    /// CREATE VIEW name AS <select> ("create [or replace] view" consumed).
    fn create_view(&mut self, or_replace: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("view name")?;
        let mut security = ViewSecurity::Definer;
        let mut check_option = None;
        if self.eat_ident("with")? {
            self.expect_op("(")?;
            for option in self.view_options()? {
                match option {
                    crate::sql::ast::ViewOption::SecurityInvoker(enabled) => {
                        security = if *enabled {
                            ViewSecurity::Invoker
                        } else {
                            ViewSecurity::Definer
                        };
                    }
                    crate::sql::ast::ViewOption::CheckOption(option) => {
                        check_option = Some(*option);
                    }
                    crate::sql::ast::ViewOption::SecurityBarrier(true) => {
                        return Err(self
                            .err_here("security_barrier requires a predicate-ordering boundary"));
                    }
                    crate::sql::ast::ViewOption::SecurityBarrier(false) => {}
                }
            }
        }
        self.expect_ident("as")?;
        // Capture the raw SELECT text (re-parsed at query time).
        let start = self.peek_at;
        // Validate the body parses now, so a bad view errors at CREATE time.
        let _ = self.query_select()?;
        let end = self.peek_at;
        let sql = self.text[start..end].trim();
        Ok(Stmt::CreateView {
            name,
            or_replace,
            security,
            check_option,
            sql,
        })
    }

    fn create_rule(&mut self, or_replace: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("rule name")?;
        self.expect_ident("as")?;
        self.expect_ident("on")?;
        let event = if self.eat_ident("select")? {
            RuleEvent::Select
        } else if self.eat_ident("insert")? {
            RuleEvent::Insert
        } else if self.eat_ident("update")? {
            RuleEvent::Update
        } else if self.eat_ident("delete")? {
            RuleEvent::Delete
        } else {
            return Err(self.err_here("expected SELECT, INSERT, UPDATE, or DELETE rule event"));
        };
        self.expect_ident("to")?;
        let table = self.qual_name("rule relation")?;
        let (condition, condition_sql) = if self.eat_ident("where")? {
            let start = self.peek_at;
            let condition = self.expression(0)?;
            let source = self.text[start..self.peek_at].trim();
            (Some(condition), Some(source))
        } else {
            (None, None)
        };
        self.expect_ident("do")?;
        let mode = if self.eat_ident("also")? {
            RuleMode::Also
        } else if self.eat_ident("instead")? {
            RuleMode::Instead
        } else {
            RuleMode::Also
        };
        let mut actions = [RuleAction {
            statement: &Stmt::Commit,
            sql: "",
        }; MAX_LIST];
        let mut count = 0usize;
        if !self.eat_ident("nothing")? {
            let parenthesized = self.eat_op("(")?;
            loop {
                if count == actions.len() {
                    return Err(self.limit("rule actions", actions.len()));
                }
                let start = self.peek_at;
                let statement = self.statement()?;
                if !rule_action_statement(&statement) {
                    return Err(ParseError {
                        at: start,
                        message: stack_format!(
                            96,
                            "rules cannot contain {} commands",
                            rule_action_name(&statement)
                        ),
                        sqlstate: sqlstate::INVALID_OBJECT_DEFINITION,
                    });
                }
                let source = self.text[start..self.peek_at].trim();
                let statement = self
                    .arena
                    .alloc(statement)
                    .map(|statement| &*statement)
                    .map_err(|_| self.err_here("rule action is too large"))?;
                actions[count] = RuleAction {
                    statement,
                    sql: source,
                };
                count += 1;
                if !parenthesized {
                    break;
                }
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(";")?;
                if self.eat_op(")")? {
                    break;
                }
            }
        }
        Ok(Stmt::CreateRule(CreateRule {
            name,
            or_replace,
            event,
            table,
            condition,
            condition_sql,
            mode,
            actions: self.arena_slice(&actions[..count])?,
        }))
    }

    fn create_publication(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.any_ident("publication name")?;
        let (all_tables, tables, schemas) = if self.eat_ident("for")? {
            if self.eat_ident("all")? {
                self.expect_ident("tables")?;
                (true, &[][..], &[][..])
            } else {
                let (tables, schemas) = self.publication_targets()?;
                (false, tables, schemas)
            }
        } else {
            // PostgreSQL permits an empty publication so tables can be added
            // transactionally later with ALTER PUBLICATION.
            (false, &[][..], &[][..])
        };
        let mut publish = None;
        let mut publish_via_partition_root = None;
        let mut publish_generated_columns = None;
        if self.eat_ident("with")? {
            self.expect_op("(")?;
            loop {
                if self.eat_ident("publish")? {
                    if publish.is_some() {
                        return Err(self.err_here("conflicting or redundant options"));
                    }
                    self.expect_op("=")?;
                    let value = self.str_literal("publication publish option")?;
                    publish = Some(self.publication_operations(value)?);
                } else if self.eat_ident("publish_via_partition_root")? {
                    if publish_via_partition_root.is_some() {
                        return Err(self.err_here("conflicting or redundant options"));
                    }
                    publish_via_partition_root =
                        Some(self.subscription_bool_option("publish_via_partition_root")?);
                } else {
                    self.expect_ident("publish_generated_columns")?;
                    if publish_generated_columns.is_some() {
                        return Err(self.err_here("conflicting or redundant options"));
                    }
                    publish_generated_columns = Some(
                        match self.subscription_option_value(
                            "publish_generated_columns must be none or stored",
                        )? {
                            value if value.eq_ignore_ascii_case("none") => {
                                crate::sql::ast::PublishGeneratedColumns::None
                            }
                            value if value.eq_ignore_ascii_case("stored") => {
                                crate::sql::ast::PublishGeneratedColumns::Stored
                            }
                            _ => {
                                return Err(self
                                    .err_here("publish_generated_columns must be none or stored"));
                            }
                        },
                    );
                }
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        let publish = match publish {
            Some(value) => value,
            None => PublicationOperations::ALL,
        };
        let publish_via_partition_root = publish_via_partition_root.unwrap_or_default();
        let publish_generated_columns =
            publish_generated_columns.unwrap_or(crate::sql::ast::PublishGeneratedColumns::None);
        Ok(Stmt::CreatePublication {
            name,
            all_tables,
            tables,
            schemas,
            publish,
            publish_via_partition_root,
            publish_generated_columns,
        })
    }

    fn create_subscription(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.any_ident("subscription name")?;
        self.expect_ident("connection")?;
        let connection = self.str_literal("subscription connection string")?;
        self.expect_ident("publication")?;
        let mut names = [""; MAX_LIST];
        let mut count = 0usize;
        loop {
            if count == names.len() {
                return Err(self.limit("subscription publications", names.len()));
            }
            names[count] = self.any_ident("publication name")?;
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        let mut connect = true;
        let mut enabled = true;
        let mut create_slot = true;
        let mut copy_data = true;
        let mut slot_name = Some(SubscriptionSlotName::Default);
        let mut behavior = SubscriptionBehavior::POSTGRESQL_18_DEFAULT;
        if self.eat_ident("with")? {
            self.expect_op("(")?;
            let mut seen_connect = false;
            let mut seen_enabled = false;
            let mut seen_create_slot = false;
            let mut seen_copy_data = false;
            let mut seen_slot_name = false;
            let mut seen_binary = false;
            let mut seen_streaming = false;
            let mut seen_synchronous_commit = false;
            let mut seen_two_phase = false;
            let mut seen_disable_on_error = false;
            let mut seen_password_required = false;
            let mut seen_run_as_owner = false;
            let mut seen_origin = false;
            let mut seen_failover = false;
            loop {
                let key = self.any_ident("subscription option")?;
                if key.eq_ignore_ascii_case("connect") {
                    let value = self.subscription_bool_option(key)?;
                    if core::mem::replace(&mut seen_connect, true) {
                        return Err(self.err_here("duplicate subscription option connect"));
                    }
                    connect = value;
                } else if key.eq_ignore_ascii_case("enabled") {
                    let value = self.subscription_bool_option(key)?;
                    if core::mem::replace(&mut seen_enabled, true) {
                        return Err(self.err_here("duplicate subscription option enabled"));
                    }
                    enabled = value;
                } else if key.eq_ignore_ascii_case("create_slot") {
                    let value = self.subscription_bool_option(key)?;
                    if core::mem::replace(&mut seen_create_slot, true) {
                        return Err(self.err_here("duplicate subscription option create_slot"));
                    }
                    create_slot = value;
                } else if key.eq_ignore_ascii_case("copy_data") {
                    let value = self.subscription_bool_option(key)?;
                    if core::mem::replace(&mut seen_copy_data, true) {
                        return Err(self.err_here("duplicate subscription option copy_data"));
                    }
                    copy_data = value;
                } else if key.eq_ignore_ascii_case("slot_name") {
                    if core::mem::replace(&mut seen_slot_name, true) {
                        return Err(self.err_here("duplicate subscription option slot_name"));
                    }
                    self.expect_op("=")?;
                    slot_name = if self.eat_ident("none")? {
                        None
                    } else if let Tok::Str(value) = self.peeked {
                        self.advance()?;
                        Some(SubscriptionSlotName::Named(value))
                    } else {
                        Some(SubscriptionSlotName::Named(
                            self.any_ident("subscription slot name")?,
                        ))
                    };
                } else if key.eq_ignore_ascii_case("binary") {
                    if core::mem::replace(&mut seen_binary, true) {
                        return Err(self.err_here("duplicate subscription option binary"));
                    }
                    behavior.binary = self.subscription_bool_option(key)?;
                } else if key.eq_ignore_ascii_case("streaming") {
                    if core::mem::replace(&mut seen_streaming, true) {
                        return Err(self.err_here("duplicate subscription option streaming"));
                    }
                    behavior.streaming = self.subscription_streaming()?;
                } else if key.eq_ignore_ascii_case("synchronous_commit") {
                    if core::mem::replace(&mut seen_synchronous_commit, true) {
                        return Err(
                            self.err_here("duplicate subscription option synchronous_commit")
                        );
                    }
                    behavior.synchronous_commit = self.subscription_synchronous_commit()?;
                } else if key.eq_ignore_ascii_case("two_phase") {
                    if core::mem::replace(&mut seen_two_phase, true) {
                        return Err(self.err_here("duplicate subscription option two_phase"));
                    }
                    behavior.two_phase = self.subscription_bool_option(key)?;
                } else if key.eq_ignore_ascii_case("disable_on_error") {
                    if core::mem::replace(&mut seen_disable_on_error, true) {
                        return Err(self.err_here("duplicate subscription option disable_on_error"));
                    }
                    behavior.disable_on_error = self.subscription_bool_option(key)?;
                } else if key.eq_ignore_ascii_case("password_required") {
                    if core::mem::replace(&mut seen_password_required, true) {
                        return Err(
                            self.err_here("duplicate subscription option password_required")
                        );
                    }
                    behavior.password_required = self.subscription_bool_option(key)?;
                } else if key.eq_ignore_ascii_case("run_as_owner") {
                    if core::mem::replace(&mut seen_run_as_owner, true) {
                        return Err(self.err_here("duplicate subscription option run_as_owner"));
                    }
                    behavior.run_as_owner = self.subscription_bool_option(key)?;
                } else if key.eq_ignore_ascii_case("origin") {
                    if core::mem::replace(&mut seen_origin, true) {
                        return Err(self.err_here("duplicate subscription option origin"));
                    }
                    behavior.origin = self.subscription_origin()?;
                } else if key.eq_ignore_ascii_case("failover") {
                    if core::mem::replace(&mut seen_failover, true) {
                        return Err(self.err_here("duplicate subscription option failover"));
                    }
                    behavior.failover = self.subscription_bool_option(key)?;
                } else {
                    return Err(self.err_here("unrecognized subscription option"));
                }
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
            if !connect {
                if (seen_enabled && enabled)
                    || (seen_create_slot && create_slot)
                    || (seen_copy_data && copy_data)
                {
                    return Err(self.err_here(
                        "connect = false requires enabled, create_slot, and copy_data to be false",
                    ));
                }
                enabled = false;
                create_slot = false;
                copy_data = false;
            }
        }
        if slot_name.is_none() && (enabled || create_slot) {
            return Err(
                self.err_here("slot_name = NONE requires enabled and create_slot to be false")
            );
        }
        let slot = match (slot_name, create_slot) {
            (Some(name), true) => SubscriptionSlotPlan::Managed(name),
            (Some(name), false) => SubscriptionSlotPlan::External(name),
            (None, false) => SubscriptionSlotPlan::Absent,
            (None, true) => unreachable!("slot_name NONE was rejected with create_slot"),
        };
        let options = SubscriptionOptions {
            connect: if connect {
                SubscriptionConnect::Now
            } else {
                SubscriptionConnect::Deferred
            },
            enabled,
            copy_data,
            slot,
            behavior,
        };
        Ok(Stmt::CreateSubscription {
            name,
            connection,
            publications: self.arena_slice(&names[..count])?,
            options,
        })
    }

    pub(super) fn subscription_bool_option(&mut self, option: &str) -> Result<bool, ParseError> {
        if !self.eat_op("=")? {
            return Ok(true);
        }
        self.subscription_bool(option)
    }

    fn subscription_bool(&mut self, _option: &str) -> Result<bool, ParseError> {
        let value = match self.peeked {
            Tok::Ident("true" | "on") | Tok::Str("true" | "on" | "1") => true,
            Tok::Ident("false" | "off") | Tok::Str("false" | "off" | "0") => false,
            _ => return Err(self.err_here("option requires a boolean value")),
        };
        self.advance()?;
        Ok(value)
    }

    pub(super) fn subscription_option_value(
        &mut self,
        expected: &'static str,
    ) -> Result<&'a str, ParseError> {
        self.expect_op("=")?;
        let value = match self.peeked {
            Tok::Ident(value) | Tok::Str(value) => value,
            _ => return Err(self.err_here(expected)),
        };
        self.advance()?;
        Ok(value)
    }

    pub(super) fn subscription_streaming(&mut self) -> Result<SubscriptionStreaming, ParseError> {
        let value = self.subscription_option_value("streaming requires off, on, or parallel")?;
        if value.eq_ignore_ascii_case("off") || value == "false" || value == "0" {
            Ok(SubscriptionStreaming::Off)
        } else if value.eq_ignore_ascii_case("on") || value == "true" || value == "1" {
            Ok(SubscriptionStreaming::On)
        } else if value.eq_ignore_ascii_case("parallel") {
            Ok(SubscriptionStreaming::Parallel)
        } else {
            Err(self.err_here("streaming requires off, on, or parallel"))
        }
    }

    pub(super) fn subscription_synchronous_commit(
        &mut self,
    ) -> Result<SubscriptionSynchronousCommit, ParseError> {
        let value = self.subscription_option_value("invalid synchronous_commit value")?;
        if value.eq_ignore_ascii_case("off") {
            Ok(SubscriptionSynchronousCommit::Off)
        } else if value.eq_ignore_ascii_case("local") {
            Ok(SubscriptionSynchronousCommit::Local)
        } else if value.eq_ignore_ascii_case("remote_write") {
            Ok(SubscriptionSynchronousCommit::RemoteWrite)
        } else if value.eq_ignore_ascii_case("on") {
            Ok(SubscriptionSynchronousCommit::On)
        } else if value.eq_ignore_ascii_case("remote_apply") {
            Ok(SubscriptionSynchronousCommit::RemoteApply)
        } else {
            Err(self.err_here("invalid synchronous_commit value"))
        }
    }

    pub(super) fn subscription_origin(&mut self) -> Result<SubscriptionOrigin, ParseError> {
        let value = self.subscription_option_value("origin requires none or any")?;
        if value.eq_ignore_ascii_case("none") {
            Ok(SubscriptionOrigin::None)
        } else if value.eq_ignore_ascii_case("any") {
            Ok(SubscriptionOrigin::Any)
        } else {
            Err(self.err_here("origin requires none or any"))
        }
    }

    /// `ALTER PUBLICATION name { SET (publish = ...) | { ADD | SET | DROP }
    /// TABLE table [, ...] }`.  The AST retains the operation distinction so
    /// execution cannot accidentally turn ADD or DROP into replacement.
    pub(super) fn alter_publication(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.any_ident("publication name")?;
        let action = if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterPublicationAction::SetOwner(self.any_ident("role name")?)
        } else if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterPublicationAction::Rename(self.any_ident("new publication name")?)
        } else if self.eat_ident("set")? {
            if self.eat_op("(")? {
                let mut publish = None;
                let mut publish_via_partition_root = None;
                let mut publish_generated_columns = None;
                loop {
                    if self.eat_ident("publish")? {
                        if publish.is_some() {
                            return Err(self.err_here("conflicting or redundant options"));
                        }
                        self.expect_op("=")?;
                        let value = self.str_literal("publication publish option")?;
                        publish = Some(self.publication_operations(value)?);
                    } else if self.eat_ident("publish_via_partition_root")? {
                        if publish_via_partition_root.is_some() {
                            return Err(self.err_here("conflicting or redundant options"));
                        }
                        publish_via_partition_root =
                            Some(self.subscription_bool_option("publish_via_partition_root")?);
                    } else {
                        self.expect_ident("publish_generated_columns")?;
                        if publish_generated_columns.is_some() {
                            return Err(self.err_here("conflicting or redundant options"));
                        }
                        publish_generated_columns = Some(
                            match self.subscription_option_value(
                                "publish_generated_columns must be none or stored",
                            )? {
                                value if value.eq_ignore_ascii_case("none") => {
                                    crate::sql::ast::PublishGeneratedColumns::None
                                }
                                value if value.eq_ignore_ascii_case("stored") => {
                                    crate::sql::ast::PublishGeneratedColumns::Stored
                                }
                                _ => {
                                    return Err(self.err_here(
                                        "publish_generated_columns must be none or stored",
                                    ));
                                }
                            },
                        );
                    }
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                AlterPublicationAction::SetOptions {
                    publish,
                    publish_via_partition_root,
                    publish_generated_columns,
                }
            } else {
                let (tables, schemas) = self.publication_targets()?;
                AlterPublicationAction::SetTargets { tables, schemas }
            }
        } else if self.eat_ident("add")? {
            let (tables, schemas) = self.publication_targets()?;
            AlterPublicationAction::AddTargets { tables, schemas }
        } else if self.eat_ident("drop")? {
            let (tables, schemas) = self.publication_targets()?;
            AlterPublicationAction::DropTargets { tables, schemas }
        } else {
            return Err(self.err_here("expected SET, ADD, or DROP after ALTER PUBLICATION"));
        };
        Ok(Stmt::AlterPublication { name, action })
    }

    fn publication_targets(
        &mut self,
    ) -> Result<(&'a [PublicationTarget<'a>], &'a [&'a str]), ParseError> {
        let mut tables = [PublicationTarget {
            relation: QualName {
                schema: None,
                name: "",
            },
            columns: &[],
            filter: None,
            filter_text: None,
        }; MAX_LIST];
        let mut schemas = [""; MAX_LIST];
        let mut table_count = 0;
        let mut schema_count = 0;
        loop {
            if self.eat_ident("table")? {
                loop {
                    if table_count == MAX_LIST {
                        return Err(self.limit("publication tables", MAX_LIST));
                    }
                    let relation = self.qual_name("table name")?;
                    let mut columns = [""; MAX_LIST];
                    let mut column_count = 0usize;
                    if self.eat_op("(")? {
                        loop {
                            if column_count == columns.len() {
                                return Err(self.limit("publication columns", columns.len()));
                            }
                            columns[column_count] = self.any_ident("column name")?;
                            column_count += 1;
                            if !self.eat_op(",")? {
                                break;
                            }
                        }
                        self.expect_op(")")?;
                    }
                    let (filter, filter_text) = if self.eat_ident("where")? {
                        self.expect_op("(")?;
                        let start = self.peek_at;
                        let filter = self.expression(0)?;
                        let filter_text = self.text[start..self.peek_at].trim_end();
                        self.expect_op(")")?;
                        (Some(filter), Some(filter_text))
                    } else {
                        (None, None)
                    };
                    tables[table_count] = PublicationTarget {
                        relation,
                        columns: self.arena_slice(&columns[..column_count])?,
                        filter,
                        filter_text,
                    };
                    table_count += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                    if self.peeked == Tok::Ident("tables") {
                        break;
                    }
                }
            } else if self.eat_ident("tables")? {
                self.expect_ident("in")?;
                self.expect_ident("schema")?;
                loop {
                    if schema_count == MAX_LIST {
                        return Err(self.limit("publication schemas", MAX_LIST));
                    }
                    schemas[schema_count] = if self.eat_ident("current_schema")? {
                        self.expect_op("(")?;
                        self.expect_op(")")?;
                        "public"
                    } else {
                        self.any_ident("schema name")?
                    };
                    schema_count += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                    if self.peeked == Tok::Ident("table") || self.peeked == Tok::Ident("tables") {
                        break;
                    }
                }
            } else {
                return Err(self.err_here("expected TABLE or TABLES IN SCHEMA in publication"));
            }
            if self.peeked == Tok::Ident("table") || self.peeked == Tok::Ident("tables") {
                continue;
            }
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok((
            self.arena_slice(&tables[..table_count])?,
            self.arena_slice(&schemas[..schema_count])?,
        ))
    }

    fn publication_operations(&self, value: &'a str) -> Result<PublicationOperations, ParseError> {
        let mut publish = PublicationOperations {
            insert: false,
            update: false,
            delete: false,
            truncate: false,
        };
        for operation in value.split(',').map(str::trim) {
            if operation.eq_ignore_ascii_case("insert") {
                publish.insert = true;
            } else if operation.eq_ignore_ascii_case("update") {
                publish.update = true;
            } else if operation.eq_ignore_ascii_case("delete") {
                publish.delete = true;
            } else if operation.eq_ignore_ascii_case("truncate") {
                publish.truncate = true;
            } else {
                return Err(self.err_here("invalid publication publish operation"));
            }
        }
        Ok(publish)
    }

    /// Dispatches DROP: `VIEW` or `TABLE` ("drop" consumed here).
    pub(super) fn drop_stmt(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("drop")?;
        if self.eat_ident("foreign")? {
            if self.eat_ident("data")? {
                self.expect_ident("wrapper")?;
                let (names, if_exists) = self.drop_bare_targets("foreign-data wrapper name")?;
                let cascade = if self.eat_ident("cascade")? {
                    true
                } else {
                    let _ = self.eat_ident("restrict")?;
                    false
                };
                return Ok(Stmt::DropForeignDataWrapper {
                    names,
                    if_exists,
                    cascade,
                });
            }
            self.expect_ident("table")?;
            let (names, if_exists) = self.drop_targets("foreign table name")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropForeignTable(DropTable {
                names,
                if_exists,
                cascade,
            }));
        }
        if self.eat_ident("server")? {
            let (names, if_exists) = self.drop_bare_targets("foreign server name")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropForeignServer {
                names,
                if_exists,
                cascade,
            });
        }
        if self.eat_ident("user")? {
            if self.eat_ident("mapping")? {
                let if_exists = if self.eat_ident("if")? {
                    self.expect_ident("exists")?;
                    true
                } else {
                    false
                };
                self.expect_ident("for")?;
                let user = self.foreign_user()?;
                self.expect_ident("server")?;
                return Ok(Stmt::DropUserMapping(crate::sql::ast::DropUserMapping {
                    user,
                    server: self.col_ident("foreign server name")?,
                    if_exists,
                }));
            }
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let mut names = [""; 16];
            let mut count = 0usize;
            loop {
                if count == names.len() {
                    return Err(self.limit("roles", names.len()));
                }
                names[count] = self.any_ident("role name")?;
                count += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            return Ok(Stmt::DropRole {
                names: self.arena_slice(&names[..count])?,
                if_exists,
            });
        }
        if self.eat_ident("rule")? {
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let name = self.col_ident("rule name")?;
            self.expect_ident("on")?;
            let table = self.qual_name("rule relation")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropRule(crate::sql::ast::DropRule {
                name,
                table,
                if_exists,
                cascade,
            }));
        }
        if self.eat_ident("owned")? {
            self.expect_ident("by")?;
            let roles = self.role_name_list("role name")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropOwned { roles, cascade });
        }
        if self.eat_ident("collation")? {
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let name = self.qual_name("collation name")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropCollation {
                name,
                if_exists,
                cascade,
            });
        }
        if self.eat_ident("text")? {
            self.expect_ident("search")?;
            let kind = if self.eat_ident("parser")? {
                TextSearchObjectKind::Parser
            } else if self.eat_ident("template")? {
                TextSearchObjectKind::Template
            } else if self.eat_ident("dictionary")? {
                TextSearchObjectKind::Dictionary
            } else if self.eat_ident("configuration")? {
                TextSearchObjectKind::Configuration
            } else {
                return Err(
                    self.err_here("expected PARSER, TEMPLATE, DICTIONARY, or CONFIGURATION")
                );
            };
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let name = self.any_qual_name(kind.noun())?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropTextSearch {
                kind,
                name,
                if_exists,
                cascade,
            });
        }
        if self.eat_ident("conversion")? {
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let name = self.qual_name("conversion name")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropConversion {
                name,
                if_exists,
                cascade,
            });
        }
        if self.eat_ident("cast")? {
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            self.expect_op("(")?;
            let source_type = self.unmodified_type_name()?;
            self.expect_ident("as")?;
            let target_type = self.unmodified_type_name()?;
            self.expect_op(")")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropCast(crate::sql::ast::DropCast {
                source_type,
                target_type,
                if_exists,
                cascade,
            }));
        }
        if self.eat_ident("operator")? {
            if self.eat_ident("family")? {
                let if_exists = if self.eat_ident("if")? {
                    self.expect_ident("exists")?;
                    true
                } else {
                    false
                };
                let name = self.qual_name("operator family name")?;
                self.expect_ident("using")?;
                let method = self.access_method()?;
                let cascade = if self.eat_ident("cascade")? {
                    true
                } else {
                    let _ = self.eat_ident("restrict")?;
                    false
                };
                return Ok(Stmt::DropOperatorFamily {
                    names: self.arena_slice(&[name])?,
                    method,
                    if_exists,
                    cascade,
                });
            }
            if self.eat_ident("class")? {
                let if_exists = if self.eat_ident("if")? {
                    self.expect_ident("exists")?;
                    true
                } else {
                    false
                };
                let name = self.qual_name("operator class name")?;
                self.expect_ident("using")?;
                let method = self.access_method()?;
                let cascade = if self.eat_ident("cascade")? {
                    true
                } else {
                    let _ = self.eat_ident("restrict")?;
                    false
                };
                return Ok(Stmt::DropOperatorClass {
                    names: self.arena_slice(&[name])?,
                    method,
                    if_exists,
                    cascade,
                });
            }
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let mut identities = [OperatorIdentity {
                name: QualName::bare("="),
                operands: OperatorOperands::Prefix("bool"),
            }; MAX_LIST];
            let mut count = 0usize;
            loop {
                if count == identities.len() {
                    return Err(self.limit("operators", identities.len()));
                }
                identities[count] = self.operator_identity()?;
                count += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropOperator {
                identities: self.arena_slice(&identities[..count])?,
                if_exists,
                cascade,
            });
        }
        if self.eat_ident("language")? {
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let mut names = [""; MAX_LIST];
            let mut count = 0usize;
            loop {
                if count == names.len() {
                    return Err(self.limit("languages", names.len()));
                }
                names[count] = self.col_ident("language name")?;
                count += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropLanguage {
                names: self.arena_slice(&names[..count])?,
                if_exists,
                cascade,
            });
        }
        if self.eat_ident("tablespace")? {
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let name = self.any_ident("tablespace name")?;
            return Ok(Stmt::DropTablespace { name, if_exists });
        }
        if self.eat_ident("database")? {
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let name = self.any_ident("database name")?;
            let force = if self.eat_ident("with")? {
                self.expect_op("(")?;
                self.expect_ident("force")?;
                self.expect_op(")")?;
                true
            } else {
                false
            };
            return Ok(Stmt::DropDatabase {
                name,
                if_exists,
                force,
            });
        }
        if self.eat_ident("extension")? {
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let mut names = [""; MAX_LIST];
            let mut count = 0usize;
            loop {
                if count == names.len() {
                    return Err(self.limit("extensions", names.len()));
                }
                names[count] = self.col_ident("extension name")?;
                count += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropExtension {
                names: self.arena_slice(&names[..count])?,
                if_exists,
                cascade,
            });
        }
        if self.eat_ident("view")? {
            let (names, if_exists) = self.drop_targets("view name")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropView {
                names,
                if_exists,
                cascade,
            });
        }
        if self.eat_ident("publication")? {
            let (names, if_exists) = self.drop_targets("publication name")?;
            let mut publication_names: [&str; MAX_LIST] = [""; MAX_LIST];
            for (index, name) in names.iter().enumerate() {
                if name.schema.is_some() {
                    return Err(self.err_here("publication names cannot be schema-qualified"));
                }
                publication_names[index] = name.name;
            }
            return Ok(Stmt::DropPublication {
                names: self.arena_slice(&publication_names[..names.len()])?,
                if_exists,
            });
        }
        if self.eat_ident("subscription")? {
            let (names, if_exists) = self.drop_targets("subscription name")?;
            let mut subscription_names: [&str; MAX_LIST] = [""; MAX_LIST];
            for (index, name) in names.iter().enumerate() {
                subscription_names[index] = name.name;
            }
            return Ok(Stmt::DropSubscription {
                names: self.arena_slice(&subscription_names[..names.len()])?,
                if_exists,
            });
        }
        if self.eat_ident("event")? {
            self.expect_ident("trigger")?;
            return self.drop_event_trigger();
        }
        if self.eat_ident("trigger")? {
            return self.drop_trigger();
        }
        if self.eat_ident("policy")? {
            return self.drop_policy();
        }
        if self.eat_ident("statistics")? {
            let (names, if_exists) = self.drop_targets("statistics name")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropStatistics {
                names,
                if_exists,
                cascade,
            });
        }
        if self.eat_ident("materialized")? {
            self.expect_ident("view")?;
            let (names, if_exists) = self.drop_targets("materialized view name")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropMaterializedView {
                names,
                if_exists,
                cascade,
            });
        }
        if self.eat_ident("index")? {
            let build = if self.eat_ident("concurrently")? {
                IndexBuildMode::Concurrent
            } else {
                IndexBuildMode::Blocking
            };
            let (names, if_exists) = self.drop_targets("index name")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropIndex {
                names,
                if_exists,
                build,
                cascade,
            });
        }
        if self.eat_ident("schema")? {
            return self.drop_schema();
        }
        if self.eat_ident("sequence")? {
            let (names, if_exists) = self.drop_targets("sequence name")?;
            let cascade = if self.eat_ident("cascade")? {
                true
            } else {
                let _ = self.eat_ident("restrict")?;
                false
            };
            return Ok(Stmt::DropSequence {
                names,
                if_exists,
                cascade,
            });
        }
        if self.eat_ident("domain")? {
            return self.drop_domain();
        }
        if self.eat_ident("aggregate")? {
            return self.drop_aggregate();
        }
        if self.eat_ident("function")? {
            return self.drop_routine(RoutineTargetKind::Function);
        }
        if self.eat_ident("procedure")? {
            return self.drop_routine(RoutineTargetKind::Procedure);
        }
        if self.eat_ident("routine")? {
            return self.drop_routine(RoutineTargetKind::Either);
        }
        if self.eat_ident("type")? {
            return self.drop_type();
        }
        if self.eat_ident("role")? || self.eat_ident("group")? {
            let if_exists = if self.eat_ident("if")? {
                self.expect_ident("exists")?;
                true
            } else {
                false
            };
            let mut names = [""; 16];
            let mut count = 0usize;
            loop {
                if count == names.len() {
                    return Err(self.limit("roles", names.len()));
                }
                names[count] = self.any_ident("role name")?;
                count += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            return Ok(Stmt::DropRole {
                names: self.arena_slice(&names[..count])?,
                if_exists,
            });
        }
        self.drop_table()
    }

    /// DROP SCHEMA [IF EXISTS] name [, ...] [CASCADE | RESTRICT]
    /// ("drop schema" consumed).
    fn drop_schema(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let mut names: [&'a str; 16] = [""; 16];
        let mut n = 0usize;
        loop {
            if n == names.len() {
                return Err(self.limit("schemas", names.len()));
            }
            names[n] = self.col_ident("schema name")?;
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        Ok(Stmt::DropSchema {
            names: self.arena_slice(&names[..n])?,
            if_exists,
            cascade,
        })
    }

    fn drop_routine(&mut self, kind: RoutineTargetKind) -> Result<Stmt<'a>, ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let mut routines = [RoutineIdentity {
            name: QualName::bare(""),
            argument_types: &[],
            signature_is_explicit: false,
        }; MAX_LIST];
        let mut count = 0;
        loop {
            if count == routines.len() {
                return Err(self.limit("routines", routines.len()));
            }
            let name = self.qual_name("function name")?;
            let mut argument_types = [""; crate::storage::MAX_ROUTINE_ARGUMENTS];
            let mut argument_count = 0;
            let signature_is_explicit = self.eat_op("(")?;
            if signature_is_explicit && !self.eat_op(")")? {
                loop {
                    let (argument_type, input) = self.routine_identity_argument()?;
                    if input {
                        if argument_count == argument_types.len() {
                            return Err(self.limit("function arguments", argument_types.len()));
                        }
                        argument_types[argument_count] = argument_type;
                        argument_count += 1;
                    }
                    if self.eat_op(")")? {
                        break;
                    }
                    self.expect_op(",")?;
                }
            }
            routines[count] = RoutineIdentity {
                name,
                argument_types: self.arena_slice(&argument_types[..argument_count])?,
                signature_is_explicit,
            };
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        let routines = self.arena_slice(&routines[..count])?;
        match kind {
            RoutineTargetKind::Function => Ok(Stmt::DropFunction {
                functions: routines,
                if_exists,
                cascade,
            }),
            RoutineTargetKind::Procedure => Ok(Stmt::DropProcedure {
                procedures: routines,
                if_exists,
                cascade,
            }),
            RoutineTargetKind::Either => Ok(Stmt::DropRoutine {
                routines,
                if_exists,
                cascade,
            }),
            RoutineTargetKind::Aggregate => {
                unreachable!("aggregate DROP has its own typed identity parser")
            }
        }
    }

    fn aggregate_identity(&mut self) -> Result<AggregateIdentity<'a>, ParseError> {
        let name = self.qual_name("aggregate name")?;
        self.expect_op("(")?;
        let mut direct = [""; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut aggregated = [""; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut direct_count = 0usize;
        let mut aggregated_count = 0usize;
        let mut ordered_set = false;
        if self.eat_op("*")? {
            self.expect_op(")")?;
        } else if !self.eat_op(")")? {
            loop {
                if self.eat_ident("order")? {
                    self.expect_ident("by")?;
                    ordered_set = true;
                    if self.eat_op(")")? {
                        break;
                    }
                    continue;
                }
                let _ = self.eat_ident("in")?;
                let _ = self.eat_ident("variadic")?;
                let target = if ordered_set {
                    &mut aggregated
                } else {
                    &mut direct
                };
                let count = if ordered_set {
                    &mut aggregated_count
                } else {
                    &mut direct_count
                };
                if *count == target.len() {
                    return Err(self.limit("aggregate arguments", target.len()));
                }
                target[*count] = self.type_name()?;
                *count += 1;
                if self.eat_op(")")? {
                    break;
                }
                if !ordered_set && self.eat_ident("order")? {
                    self.expect_ident("by")?;
                    ordered_set = true;
                    if self.eat_op(")")? {
                        break;
                    }
                    continue;
                }
                self.expect_op(",")?;
            }
        }
        Ok(if ordered_set {
            AggregateIdentity {
                name,
                direct_argument_types: self.arena_slice(&direct[..direct_count])?,
                aggregated_argument_types: self.arena_slice(&aggregated[..aggregated_count])?,
                ordered_set: true,
            }
        } else {
            AggregateIdentity {
                name,
                direct_argument_types: &[],
                aggregated_argument_types: self.arena_slice(&direct[..direct_count])?,
                ordered_set: false,
            }
        })
    }

    fn drop_aggregate(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let mut aggregates = [AggregateIdentity {
            name: QualName::bare(""),
            direct_argument_types: &[],
            aggregated_argument_types: &[],
            ordered_set: false,
        }; MAX_LIST];
        let mut count = 0usize;
        loop {
            if count == aggregates.len() {
                return Err(self.limit("aggregates", aggregates.len()));
            }
            aggregates[count] = self.aggregate_identity()?;
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        Ok(Stmt::DropAggregate {
            aggregates: self.arena_slice(&aggregates[..count])?,
            if_exists,
            cascade,
        })
    }

    pub(super) fn alter_aggregate(&mut self) -> Result<Stmt<'a>, ParseError> {
        let aggregate = self.aggregate_identity()?;
        let action = if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterRoutineAction::SetOwner(self.any_ident("role name")?)
        } else if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterRoutineAction::Rename(self.col_ident("aggregate name")?)
        } else if self.eat_ident("set")? {
            self.expect_ident("schema")?;
            AlterRoutineAction::SetSchema(self.col_ident("schema name")?)
        } else {
            return Err(self.unexpected("expected OWNER, RENAME, or SET SCHEMA"));
        };
        Ok(Stmt::AlterAggregate { aggregate, action })
    }

    pub(super) fn drop_trigger(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.col_ident("trigger name")?;
        self.expect_ident("on")?;
        let trigger = TriggerIdentity {
            name,
            table: self.qual_name("trigger table")?,
        };
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        Ok(Stmt::DropTrigger {
            trigger,
            if_exists,
            cascade,
        })
    }

    fn drop_policy(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.col_ident("policy name")?;
        self.expect_ident("on")?;
        let table = self.qual_name("policy table")?;
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        Ok(Stmt::DropPolicy {
            policy: PolicyIdentity { name, table },
            if_exists,
            cascade,
        })
    }

    pub(super) fn alter_routine(
        &mut self,
        kind: RoutineTargetKind,
    ) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("routine name")?;
        let mut argument_types = [""; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut count = 0;
        let signature_is_explicit = self.eat_op("(")?;
        if signature_is_explicit && !self.eat_op(")")? {
            loop {
                let (argument_type, input) = self.routine_identity_argument()?;
                if input {
                    if count == argument_types.len() {
                        return Err(self.limit("routine arguments", argument_types.len()));
                    }
                    argument_types[count] = argument_type;
                    count += 1;
                }
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        let mut actions = [AlterRoutineAction::SetStrict(false); 32];
        let mut action_count = 0usize;
        loop {
            let action = if self.eat_ident("owner")? {
                self.expect_ident("to")?;
                AlterRoutineAction::SetOwner(self.any_ident("role name")?)
            } else if self.eat_ident("rename")? {
                self.expect_ident("to")?;
                AlterRoutineAction::Rename(self.col_ident("routine name")?)
            } else if self.eat_ident("set")? {
                if self.eat_ident("schema")? {
                    AlterRoutineAction::SetSchema(self.col_ident("schema name")?)
                } else {
                    let name_start = self.peek_at;
                    let _ = self.any_ident("configuration parameter")?;
                    while self.eat_op(".")? {
                        let _ = self.any_ident("configuration parameter")?;
                    }
                    let name = self.text[name_start..self.peek_at].trim();
                    let value = if self.eat_ident("from")? {
                        self.expect_ident("current")?;
                        crate::sql::ast::RoutineConfigValue::Current
                    } else {
                        if !self.eat_op("=")? {
                            self.expect_ident("to")?;
                        }
                        let start = self.peek_at;
                        if matches!(self.peeked, Tok::Op("+") | Tok::Op("-")) {
                            self.advance()?;
                        }
                        if !matches!(self.peeked, Tok::Ident(_) | Tok::Num(_) | Tok::Str(_)) {
                            return Err(self.unexpected("configuration value"));
                        }
                        self.advance()?;
                        while self.eat_op(",")? {
                            if matches!(self.peeked, Tok::Op("+") | Tok::Op("-")) {
                                self.advance()?;
                            }
                            if !matches!(self.peeked, Tok::Ident(_) | Tok::Num(_) | Tok::Str(_)) {
                                return Err(self.unexpected("configuration value"));
                            }
                            self.advance()?;
                        }
                        crate::sql::ast::RoutineConfigValue::Value(
                            self.text[start..self.peek_at].trim(),
                        )
                    };
                    AlterRoutineAction::SetConfig { name, value }
                }
            } else if self.eat_ident("reset")? {
                AlterRoutineAction::ResetConfig(if self.eat_ident("all")? {
                    None
                } else {
                    Some(self.any_ident("configuration parameter")?)
                })
            } else if self.eat_ident("strict")? {
                AlterRoutineAction::SetStrict(true)
            } else if self.eat_ident("called")? {
                self.expect_ident("on")?;
                self.expect_ident("null")?;
                self.expect_ident("input")?;
                AlterRoutineAction::SetStrict(false)
            } else if self.eat_ident("returns")? {
                self.expect_ident("null")?;
                self.expect_ident("on")?;
                self.expect_ident("null")?;
                self.expect_ident("input")?;
                AlterRoutineAction::SetStrict(true)
            } else if self.eat_ident("immutable")? {
                AlterRoutineAction::SetVolatility(crate::sql::ast::RoutineVolatility::Immutable)
            } else if self.eat_ident("stable")? {
                AlterRoutineAction::SetVolatility(crate::sql::ast::RoutineVolatility::Stable)
            } else if self.eat_ident("volatile")? {
                AlterRoutineAction::SetVolatility(crate::sql::ast::RoutineVolatility::Volatile)
            } else if self.eat_ident("leakproof")? {
                AlterRoutineAction::SetLeakproof(true)
            } else if self.eat_ident("not")? {
                self.expect_ident("leakproof")?;
                AlterRoutineAction::SetLeakproof(false)
            } else if self.eat_ident("security")? {
                AlterRoutineAction::SetSecurityDefiner(if self.eat_ident("definer")? {
                    true
                } else {
                    self.expect_ident("invoker")?;
                    false
                })
            } else if self.eat_ident("external")? {
                self.expect_ident("security")?;
                AlterRoutineAction::SetSecurityDefiner(if self.eat_ident("definer")? {
                    true
                } else {
                    self.expect_ident("invoker")?;
                    false
                })
            } else if self.eat_ident("parallel")? {
                AlterRoutineAction::SetParallel(if self.eat_ident("safe")? {
                    crate::sql::ast::RoutineParallel::Safe
                } else if self.eat_ident("restricted")? {
                    crate::sql::ast::RoutineParallel::Restricted
                } else {
                    self.expect_ident("unsafe")?;
                    crate::sql::ast::RoutineParallel::Unsafe
                })
            } else if self.eat_ident("cost")? {
                let Tok::Num(raw) = self.peeked else {
                    return Err(self.err_here("routine estimate must be a positive number"));
                };
                let value = raw
                    .parse::<f64>()
                    .ok()
                    .and_then(crate::sql::ast::RoutineEstimate::new)
                    .ok_or_else(|| self.err_here("routine estimate must be a positive number"))?;
                self.advance()?;
                AlterRoutineAction::SetCost(value)
            } else if self.eat_ident("rows")? {
                let Tok::Num(raw) = self.peeked else {
                    return Err(self.err_here("routine estimate must be a positive number"));
                };
                let value = raw
                    .parse::<f64>()
                    .ok()
                    .and_then(crate::sql::ast::RoutineEstimate::new)
                    .ok_or_else(|| self.err_here("routine estimate must be a positive number"))?;
                self.advance()?;
                AlterRoutineAction::SetRows(value)
            } else if self.peeked == Tok::Ident("depends") || self.peeked == Tok::Ident("no") {
                let enabled = !self.eat_ident("no")?;
                self.expect_ident("depends")?;
                self.expect_ident("on")?;
                self.expect_ident("extension")?;
                AlterRoutineAction::ExtensionDependency {
                    extension: self.col_ident("extension name")?,
                    enabled,
                }
            } else {
                break;
            };
            if action_count == actions.len() {
                return Err(self.limit("ALTER ROUTINE actions", actions.len()));
            }
            actions[action_count] = action;
            action_count += 1;
            if matches!(
                action,
                AlterRoutineAction::SetOwner(_)
                    | AlterRoutineAction::Rename(_)
                    | AlterRoutineAction::SetSchema(_)
                    | AlterRoutineAction::ExtensionDependency { .. }
            ) {
                break;
            }
        }
        let _ = self.eat_ident("restrict")?;
        if action_count == 0 {
            return Err(self.unexpected("routine alteration action"));
        }
        Ok(Stmt::AlterRoutine {
            kind,
            routine: RoutineIdentity {
                name,
                argument_types: self.arena_slice(&argument_types[..count])?,
                signature_is_explicit,
            },
            actions: self.arena_slice(&actions[..action_count])?,
        })
    }

    /// `[IF EXISTS] name [, ...]` after a DROP keyword.
    fn drop_targets(&mut self, what: &str) -> Result<(&'a [QualName<'a>], bool), ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let mut names: [QualName<'a>; 16] = [QualName::bare(""); 16];
        let mut n = 0usize;
        loop {
            if n == names.len() {
                return Err(self.limit("relations", names.len()));
            }
            let first = self.any_ident(what)?;
            names[n] = if self.eat_op(".")? {
                QualName {
                    schema: Some(first),
                    name: self.any_ident(what)?,
                }
            } else {
                QualName::bare(first)
            };
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok((self.arena_slice(&names[..n])?, if_exists))
    }

    fn drop_bare_targets(
        &mut self,
        what: &'static str,
    ) -> Result<(&'a [&'a str], bool), ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let mut names = [""; MAX_LIST];
        let mut count = 0usize;
        loop {
            if count == names.len() {
                return Err(self.limit(what, names.len()));
            }
            names[count] = self.col_ident(what)?;
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        Ok((self.arena_slice(&names[..count])?, if_exists))
    }

    fn create_table(
        &mut self,
        foreign: bool,
        persistence: RelationPersistence,
    ) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("table")?;
        let if_not_exists = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.qual_name("table name")?;
        let mut membership = if self.eat_ident("of")? {
            TableMembership::OfType(self.qual_name("row type name")?)
        } else {
            TableMembership::None
        };
        // A partition is a table whose column layout is inherited from its
        // parent; unlike ordinary CREATE TABLE it has no column list here.
        if self.eat_ident("partition")? {
            self.expect_ident("of")?;
            let parent = self.qual_name("partitioned table name")?;
            let bound = self.partition_bound()?;
            let subpartition = if self.eat_ident("partition")? {
                Some(self.partition_by_clause()?)
            } else {
                None
            };
            let access_method = if self.eat_ident("using")? {
                let method = self.any_ident("table access method")?;
                if method.eq_ignore_ascii_case("heap") {
                    TableAccessMethod::Heap
                } else {
                    TableAccessMethod::Named(method)
                }
            } else {
                TableAccessMethod::Heap
            };
            let tablespace = if self.eat_ident("tablespace")? {
                Some(self.col_ident("tablespace name")?)
            } else {
                None
            };
            let relation = CreateTable {
                name,
                columns: &[],
                constraints: &[],
                likes: &[],
                partition: PartitionClause::Of {
                    parent,
                    bound,
                    subpartition,
                },
                membership: TableMembership::None,
                persistence,
                access_method,
                tablespace,
                storage_options: RelationStorageOptions::DEFAULT,
                if_not_exists,
            };
            if foreign {
                self.expect_ident("server")?;
                let server = self.col_ident("foreign server name")?;
                let options = if self.eat_ident("options")? {
                    self.foreign_options()?
                } else {
                    &[]
                };
                return Ok(Stmt::CreateForeignTable(CreateForeignTable {
                    relation,
                    server,
                    options,
                }));
            }
            return Ok(Stmt::CreateTable(relation));
        }
        // `CREATE TABLE name AS <query>` — no explicit column list.
        if self.eat_ident("as")? {
            if foreign {
                return Err(self.err_here("CREATE FOREIGN TABLE AS is not supported by PostgreSQL"));
            }
            return self.create_table_as(
                name,
                &[],
                if_not_exists,
                crate::sql::ast::CreateTableAsKind::Table,
            );
        }
        if matches!(membership, TableMembership::OfType(_)) && self.peeked != Tok::Op("(") {
            if foreign {
                return Err(self.err_here("CREATE FOREIGN TABLE OF is not supported by PostgreSQL"));
            }
            let mut access_method = TableAccessMethod::Heap;
            let mut tablespace = None;
            let mut storage_options = RelationStorageOptions::DEFAULT;
            loop {
                if self.eat_ident("with")? {
                    self.expect_op("(")?;
                    storage_options = self.relation_storage_options()?;
                    self.expect_op(")")?;
                } else if self.eat_ident("using")? {
                    let method = self.any_ident("table access method")?;
                    access_method = if method.eq_ignore_ascii_case("heap") {
                        TableAccessMethod::Heap
                    } else {
                        TableAccessMethod::Named(method)
                    };
                } else if self.eat_ident("tablespace")? {
                    tablespace = Some(self.col_ident("tablespace name")?);
                } else {
                    break;
                }
            }
            return Ok(Stmt::CreateTable(CreateTable {
                name,
                columns: &[],
                constraints: &[],
                likes: &[],
                partition: PartitionClause::None,
                membership,
                persistence,
                access_method,
                tablespace,
                storage_options,
                if_not_exists,
            }));
        }
        self.expect_op("(")?;
        // A `(` is either column definitions or — for `CREATE TABLE ... AS` — a
        // column-name list. Only a plain identifier (not LIKE, CONSTRAINT, or a
        // table-constraint keyword) can begin a name list, so peek there: a name
        // list has that identifier immediately followed by `,` or `)`; a column
        // definition has a type after it.
        let mut pending_first_col = None;
        if matches!(self.peeked, Tok::Ident(w)
            if !matches!(w, "like" | "constraint" | "primary" | "unique" | "check" | "foreign"))
        {
            let first_ident = self.col_ident("column name")?;
            if matches!(self.peeked, Tok::Op(",") | Tok::Op(")")) {
                let mut list = [""; MAX_LIST];
                list[0] = first_ident;
                let mut m = 1;
                while self.eat_op(",")? {
                    if m == MAX_LIST {
                        return Err(self.limit("column list", MAX_LIST));
                    }
                    list[m] = self.col_ident("column name")?;
                    m += 1;
                }
                self.expect_op(")")?;
                self.expect_ident("as")?;
                if foreign {
                    return Err(
                        self.err_here("CREATE FOREIGN TABLE AS is not supported by PostgreSQL")
                    );
                }
                let cols = self.arena_slice(&list[..m])?;
                return self.create_table_as(
                    name,
                    cols,
                    if_not_exists,
                    crate::sql::ast::CreateTableAsKind::Table,
                );
            }
            // Otherwise it is a column definition whose name we already read.
            pending_first_col = Some(first_ident);
        }
        let mut columns = [ColumnDef {
            name: "",
            type_name: "",
            type_mod: -1,
            collation: crate::sql::ast::ParsedCollation::DEFAULT,
            foreign_options: &[],
            not_null: false,
            unique: false,
            primary: false,
            default: None,
            default_text: None,
            generated_text: None,
            identity: None,
        }; MAX_LIST];
        let mut n = 0;
        let mut cons = [TableConstraint::Unique {
            name: None,
            columns: &[],
            timing: ConstraintTiming::NotDeferrable,
        }; MAX_LIST];
        let mut n_cons = 0;
        let mut likes = [LikeClause {
            at: 0,
            source: QualName::bare(""),
            defaults: false,
            constraints: false,
            indexes: false,
            identity: false,
            generated: false,
        }; MAX_LIST];
        let mut n_likes = 0;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("column list", MAX_LIST));
            }
            // The first column's name was pre-read to tell a definition list
            // from a `CREATE TABLE ... AS` column-name list; use it, skipping the
            // LIKE / constraint forms it cannot be.
            let col_name = if let Some(pre_read) = pending_first_col.take() {
                pre_read
            } else {
                // `LIKE source [INCLUDING ...]` copies another table's columns in
                // at this position; the catalog is only consulted when it runs.
                if self.eat_ident("like")? {
                    if n_likes == MAX_LIST {
                        return Err(self.limit("LIKE clauses", MAX_LIST));
                    }
                    likes[n_likes] = self.like_clause(n)?;
                    n_likes += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                    continue;
                }
                // An optional CONSTRAINT <name> prefixes a table- or column-level
                // constraint; it names the following constraint.
                let cons_name = if self.eat_ident("constraint")? {
                    Some(self.col_ident("constraint name")?)
                } else {
                    None
                };
                // Table-level constraints: PRIMARY KEY / UNIQUE / CHECK / FOREIGN KEY.
                if matches!(
                    self.peeked,
                    Tok::Ident("primary")
                        | Tok::Ident("unique")
                        | Tok::Ident("check")
                        | Tok::Ident("foreign")
                        | Tok::Ident("exclude")
                ) {
                    let c = self.table_constraint(cons_name, false)?;
                    if n_cons == MAX_LIST {
                        return Err(self.limit("constraint list", MAX_LIST));
                    }
                    cons[n_cons] = c;
                    n_cons += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                    continue;
                }
                if cons_name.is_some() {
                    return Err(self.err_here("expected a table constraint after CONSTRAINT name"));
                }
                self.col_ident("column name")?
            };
            let warnings_before = self.n_warnings;
            let (type_name, type_mod) = self.type_name_mod()?;
            let foreign_options = if foreign && self.eat_ident("options")? {
                self.foreign_options()?
            } else {
                &[]
            };
            let collation = if self.eat_ident("collate")? {
                self.collation_name()?
            } else {
                crate::sql::ast::ParsedCollation::DEFAULT
            };
            // PostgreSQL resolves a column definition's type twice, so a
            // precision-clamp warning is reported twice per column here where
            // a cast reports it once. Faithfully duplicated.
            for w in warnings_before..self.n_warnings.min(super::MAX_PARSE_WARNINGS) {
                let again = self.warnings[w];
                self.warn(again);
            }
            let mut not_null = false;
            let mut unique = false;
            let mut primary = false;
            let mut default = None;
            let mut default_text = None;
            let mut generated_text = None;
            let mut identity = None;
            loop {
                // Column-level constraints may carry their own CONSTRAINT name.
                let col_cons_name = if self.eat_ident("constraint")? {
                    Some(self.col_ident("constraint name")?)
                } else {
                    None
                };
                if self.eat_ident("not")? {
                    self.expect_ident("null")?;
                    not_null = true;
                } else if self.eat_ident("null")? {
                    not_null = false;
                } else if self.eat_ident("default")? {
                    let start = self.peek_at;
                    default = Some(self.column_default_expression()?);
                    default_text = Some(self.text[start..self.peek_at].trim_end());
                } else if self.eat_ident("unique")? {
                    let timing = self.constraint_timing(false)?;
                    // A named or deferrable single-column constraint needs a
                    // durable key object; only the default unnamed form can use
                    // the compact column flag.
                    if col_cons_name.is_some() || timing.is_deferrable() {
                        if n_cons == MAX_LIST {
                            return Err(self.limit("constraint list", MAX_LIST));
                        }
                        cons[n_cons] = TableConstraint::Unique {
                            name: col_cons_name,
                            columns: self.arena_slice(&[col_name])?,
                            timing,
                        };
                        n_cons += 1;
                        continue;
                    }
                    unique = true;
                } else if self.eat_ident("primary")? {
                    self.expect_ident("key")?;
                    let timing = self.constraint_timing(false)?;
                    if col_cons_name.is_some() || timing.is_deferrable() {
                        if n_cons == MAX_LIST {
                            return Err(self.limit("constraint list", MAX_LIST));
                        }
                        cons[n_cons] = TableConstraint::PrimaryKey {
                            name: col_cons_name,
                            columns: self.arena_slice(&[col_name])?,
                            timing,
                        };
                        n_cons += 1;
                        // PRIMARY KEY implies NOT NULL; attach_constraints sets
                        // it, but a `LIKE` copy reads the flag, so set it here.
                        not_null = true;
                        continue;
                    }
                    primary = true;
                    unique = true;
                    not_null = true;
                } else if self.eat_ident("check")? {
                    // Desugar a column CHECK to a table-level CHECK.
                    let c = self.check_constraint(col_cons_name, false)?;
                    if n_cons == MAX_LIST {
                        return Err(self.limit("constraint list", MAX_LIST));
                    }
                    cons[n_cons] = c;
                    n_cons += 1;
                    continue;
                } else if self.eat_ident("references")? {
                    // Desugar a column REFERENCES to a single-column FK.
                    let child = self.arena_slice(&[col_name])?;
                    let c = self.references_tail(col_cons_name, child, false)?;
                    if n_cons == MAX_LIST {
                        return Err(self.limit("constraint list", MAX_LIST));
                    }
                    cons[n_cons] = c;
                    n_cons += 1;
                    continue;
                } else if self.eat_ident("generated")? {
                    match self.generated_clause()? {
                        crate::sql::ast::ColGen::Generated(text) => generated_text = Some(text),
                        crate::sql::ast::ColGen::Identity(spec) => identity = Some(spec),
                    }
                } else if col_cons_name.is_some() {
                    return Err(self.err_here("expected a column constraint after CONSTRAINT name"));
                } else {
                    break;
                }
            }
            columns[n] = ColumnDef {
                name: col_name,
                type_name,
                type_mod,
                collation,
                foreign_options,
                not_null,
                unique,
                primary,
                default,
                default_text,
                generated_text,
                identity,
            };
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        let partition = if self.eat_ident("partition")? {
            if foreign {
                return Err(
                    self.err_here("CREATE FOREIGN TABLE does not support a PARTITION BY clause")
                );
            }
            let by = self.partition_by_clause()?;
            PartitionClause::By {
                strategy: by.strategy,
                columns: by.columns,
            }
        } else {
            PartitionClause::None
        };
        let mut access_method = TableAccessMethod::Heap;
        let mut tablespace = None;
        let mut storage_options = RelationStorageOptions::DEFAULT;
        loop {
            if self.eat_ident("with")? {
                self.expect_op("(")?;
                storage_options = self.relation_storage_options()?;
                self.expect_op(")")?;
            } else if self.eat_ident("using")? {
                let method = self.any_ident("table access method")?;
                access_method = if method.eq_ignore_ascii_case("heap") {
                    TableAccessMethod::Heap
                } else {
                    TableAccessMethod::Named(method)
                };
            } else if self.eat_ident("tablespace")? {
                tablespace = Some(self.col_ident("tablespace name")?);
            } else if self.eat_ident("inherits")? {
                if !matches!(membership, TableMembership::None) {
                    return Err(self.err_here("a typed table cannot also specify INHERITS"));
                }
                self.expect_op("(")?;
                let mut parents = [QualName::bare(""); MAX_LIST];
                let mut n_parents = 0usize;
                loop {
                    if n_parents == parents.len() {
                        return Err(self.limit("inheritance parent", parents.len()));
                    }
                    parents[n_parents] = self.qual_name("parent relation name")?;
                    n_parents += 1;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                membership = TableMembership::Inherits(self.arena_slice(&parents[..n_parents])?);
            } else {
                break;
            }
        }
        let columns = self.arena_slice(&columns[..n])?;
        let constraints = self.arena_slice(&cons[..n_cons])?;
        let likes = self.arena_slice(&likes[..n_likes])?;
        let relation = CreateTable {
            name,
            columns,
            constraints,
            likes,
            partition,
            membership,
            persistence,
            access_method,
            tablespace,
            storage_options,
            if_not_exists,
        };
        if foreign {
            self.expect_ident("server")?;
            let server = self.col_ident("foreign server name")?;
            let options = if self.eat_ident("options")? {
                self.foreign_options()?
            } else {
                &[]
            };
            Ok(Stmt::CreateForeignTable(CreateForeignTable {
                relation,
                server,
                options,
            }))
        } else {
            Ok(Stmt::CreateTable(relation))
        }
    }

    pub(super) fn partition_bound(&mut self) -> Result<PartitionBound<'a>, ParseError> {
        if self.eat_ident("default")? {
            return Ok(PartitionBound::Default);
        }
        self.expect_ident("for")?;
        self.expect_ident("values")?;
        if self.eat_ident("from")? {
            let from = self.partition_bound_values()?;
            self.expect_ident("to")?;
            let to = self.partition_bound_values()?;
            return Ok(PartitionBound::Range { from, to });
        }
        if self.eat_ident("in")? {
            return Ok(PartitionBound::List {
                values: self.partition_bound_values()?,
            });
        }
        self.expect_ident("with")?;
        self.expect_op("(")?;
        self.expect_ident("modulus")?;
        let modulus = self.expression(0)?;
        self.expect_op(",")?;
        self.expect_ident("remainder")?;
        let remainder = self.expression(0)?;
        self.expect_op(")")?;
        Ok(PartitionBound::Hash { modulus, remainder })
    }

    fn partition_by_clause(&mut self) -> Result<crate::sql::ast::PartitionBy<'a>, ParseError> {
        self.expect_ident("by")?;
        let strategy = if self.eat_ident("range")? {
            PartitionStrategy::Range
        } else if self.eat_ident("list")? {
            PartitionStrategy::List
        } else if self.eat_ident("hash")? {
            PartitionStrategy::Hash
        } else {
            return Err(self.err_here("expected RANGE, LIST, or HASH after PARTITION BY"));
        };
        self.expect_op("(")?;
        let mut columns = [""; MAX_LIST];
        let mut n_columns = 0;
        loop {
            if n_columns == MAX_LIST {
                return Err(self.limit("partition key", MAX_LIST));
            }
            columns[n_columns] = self.col_ident("partition key column")?;
            n_columns += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(crate::sql::ast::PartitionBy {
            strategy,
            columns: self.arena_slice(&columns[..n_columns])?,
        })
    }

    fn partition_bound_values(&mut self) -> Result<&'a [&'a Expr<'a>], ParseError> {
        self.expect_op("(")?;
        let mut values: [&'a Expr<'a>; MAX_LIST] = [&Expr::Null; MAX_LIST];
        let mut n = 0;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("partition bound", MAX_LIST));
            }
            values[n] = self.expression(0)?;
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        self.arena_slice(&values[..n])
    }

    /// The rest of a `LIKE source [ { INCLUDING | EXCLUDING } option ]...`
    /// element, `LIKE` already consumed. `at` is how many columns precede it.
    fn like_clause(&mut self, at: usize) -> Result<LikeClause<'a>, ParseError> {
        let source = self.qual_name("source table name")?;
        let mut clause = LikeClause {
            at,
            source,
            defaults: false,
            constraints: false,
            indexes: false,
            identity: false,
            generated: false,
        };
        loop {
            let including = if self.eat_ident("including")? {
                true
            } else if self.eat_ident("excluding")? {
                false
            } else {
                return Ok(clause);
            };
            // PostgreSQL's option set. The four this engine has no notion of
            // are rejected rather than accepted and quietly dropped; ALL does
            // not name them, so it stays legal.
            match self.peeked {
                Tok::Ident("defaults") => clause.defaults = including,
                Tok::Ident("constraints") => clause.constraints = including,
                Tok::Ident("indexes") => clause.indexes = including,
                Tok::Ident("identity") => clause.identity = including,
                Tok::Ident("generated") => clause.generated = including,
                Tok::Ident("all") => {
                    clause.defaults = including;
                    clause.constraints = including;
                    clause.indexes = including;
                    clause.identity = including;
                    clause.generated = including;
                }
                Tok::Ident(other @ ("comments" | "compression" | "statistics" | "storage")) => {
                    return Err(ParseError {
                        at: self.peek_at,
                        message: stack_format!(
                            96,
                            "INCLUDING {} is not supported: this engine has no such column property",
                            other
                        ),
                        sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
                    });
                }
                _ => return Err(self.err_here("expected a LIKE option after INCLUDING/EXCLUDING")),
            }
            self.advance()?;
        }
    }

    /// Parses a parenthesized, comma-separated column-name list.
    pub(super) fn column_name_list(&mut self) -> Result<&'a [&'a str], ParseError> {
        self.expect_op("(")?;
        let mut columns: [&'a str; MAX_INDEX_COLS] = [""; MAX_INDEX_COLS];
        let mut k = 0;
        loop {
            if k == MAX_INDEX_COLS {
                return Err(self.limit("constraint column list", MAX_INDEX_COLS));
            }
            columns[k] = self.col_ident("column name")?;
            k += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        self.arena_slice(&columns[..k])
    }

    /// A table-level PRIMARY KEY / UNIQUE / CHECK / FOREIGN KEY constraint.
    pub(super) fn table_constraint(
        &mut self,
        name: Option<&'a str>,
        allow_not_valid: bool,
    ) -> Result<TableConstraint<'a>, ParseError> {
        if self.eat_ident("primary")? {
            self.expect_ident("key")?;
            let columns = self.column_name_list()?;
            let timing = self.constraint_timing(false)?;
            Ok(TableConstraint::PrimaryKey {
                name,
                columns,
                timing,
            })
        } else if self.eat_ident("unique")? {
            let columns = self.column_name_list()?;
            let timing = self.constraint_timing(false)?;
            Ok(TableConstraint::Unique {
                name,
                columns,
                timing,
            })
        } else if self.eat_ident("check")? {
            self.check_constraint(name, allow_not_valid)
        } else if self.eat_ident("exclude")? {
            self.exclusion_constraint(name)
        } else {
            self.expect_ident("foreign")?;
            self.expect_ident("key")?;
            let columns = self.column_name_list()?;
            self.expect_ident("references")?;
            self.references_tail(name, columns, allow_not_valid)
        }
    }

    fn exclusion_constraint(
        &mut self,
        name: Option<&'a str>,
    ) -> Result<TableConstraint<'a>, ParseError> {
        if self.eat_ident("using")? {
            let method = self.col_ident("exclusion index method")?;
            if method != "gist" {
                return Err(self.err_here("only GiST exclusion constraints are supported"));
            }
        }
        self.expect_op("(")?;
        let mut columns = [""; MAX_LIST];
        let mut operators = [ExclusionOperator::Equal; MAX_LIST];
        let mut count = 0;
        loop {
            if count == columns.len() {
                return Err(self.limit("exclusion elements", columns.len()));
            }
            columns[count] = self.col_ident("exclusion column")?;
            self.expect_ident("with")?;
            operators[count] = match self.any_op_token()? {
                "=" => ExclusionOperator::Equal,
                "&&" => ExclusionOperator::Overlaps,
                "-|-" => ExclusionOperator::Adjacent,
                _ => return Err(self.err_here("unsupported exclusion operator")),
            };
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        let (predicate, predicate_text) = if self.eat_ident("where")? {
            self.expect_op("(")?;
            let start = self.peek_at;
            let expression = self.expression(0)?;
            let text = self.arena_str(self.text[start..self.peek_at].trim_end())?;
            self.expect_op(")")?;
            (Some(expression), Some(text))
        } else {
            (None, None)
        };
        let timing = self.constraint_timing(false)?;
        Ok(TableConstraint::Exclusion {
            name,
            columns: self.arena_slice(&columns[..count])?,
            operators: self.arena_slice(&operators[..count])?,
            predicate,
            predicate_text,
            timing,
        })
    }

    /// A CHECK (predicate): captures the predicate's source text for durable
    /// storage alongside the parsed expression.
    fn check_constraint(
        &mut self,
        name: Option<&'a str>,
        allow_not_valid: bool,
    ) -> Result<TableConstraint<'a>, ParseError> {
        self.expect_op("(")?;
        let start = self.peek_at;
        let expression = self.expression(0)?;
        let text = self.text[start..self.peek_at].trim_end();
        let text = self.arena_str(text)?;
        self.expect_op(")")?;
        let (_, validation) = self.constraint_attributes(false, true, allow_not_valid)?;
        Ok(TableConstraint::Check {
            name,
            expression,
            text,
            validation,
        })
    }

    /// The part of a FOREIGN KEY after `REFERENCES`: parent table, optional
    /// parent columns, and ON DELETE / ON UPDATE actions.
    fn references_tail(
        &mut self,
        name: Option<&'a str>,
        columns: &'a [&'a str],
        allow_not_valid: bool,
    ) -> Result<TableConstraint<'a>, ParseError> {
        let parent = self.qual_name("referenced table")?;
        let parent_cols = if self.peeked == Tok::Op("(") {
            self.column_name_list()?
        } else {
            &[]
        };
        let mut on_delete = FkAction::NoAction;
        let mut on_update = FkAction::NoAction;
        while self.eat_ident("on")? {
            let is_delete = if self.eat_ident("delete")? {
                true
            } else {
                self.expect_ident("update")?;
                false
            };
            let action = self.fk_action()?;
            if is_delete {
                on_delete = action;
            } else {
                on_update = action;
            }
        }
        let (timing, validation) = self.constraint_attributes(true, true, allow_not_valid)?;
        Ok(TableConstraint::ForeignKey {
            name,
            columns,
            parent,
            parent_cols,
            on_delete,
            on_update,
            timing,
            validation,
        })
    }

    /// Parses the attributes shared by table constraints and returns only
    /// states that execution can represent. `ENFORCED` is accepted for every
    /// constraint; `NOT ENFORCED` is limited to CHECK and FOREIGN KEY by the
    /// caller.
    fn constraint_attributes(
        &mut self,
        can_defer: bool,
        can_disable: bool,
        allow_not_valid: bool,
    ) -> Result<(ConstraintTiming, ConstraintValidation), ParseError> {
        let mut deferrable = None;
        let mut initial = None;
        let mut enforced = None;
        let mut not_valid = false;
        loop {
            let not_attribute = if self.peeked == Tok::Ident("not") {
                let mut lookahead = self.lexer.clone();
                matches!(
                    lookahead.next_token()?,
                    Tok::Ident("deferrable") | Tok::Ident("enforced") | Tok::Ident("valid")
                )
            } else {
                false
            };
            if self.eat_ident("deferrable")? {
                if !can_defer || deferrable.replace(true).is_some() {
                    return Err(self.err_here("invalid or duplicate DEFERRABLE clause"));
                }
            } else if not_attribute {
                self.expect_ident("not")?;
                if self.eat_ident("deferrable")? {
                    if !can_defer || deferrable.replace(false).is_some() {
                        return Err(self.err_here("invalid or duplicate NOT DEFERRABLE clause"));
                    }
                } else if self.eat_ident("enforced")? {
                    if !can_disable || enforced.replace(false).is_some() {
                        return Err(self.err_here("invalid or duplicate NOT ENFORCED clause"));
                    }
                } else {
                    self.expect_ident("valid")?;
                    if !allow_not_valid || not_valid {
                        return Err(self.err_here("invalid or duplicate NOT VALID clause"));
                    }
                    not_valid = true;
                }
            } else if self.eat_ident("initially")? {
                let mode = if self.eat_ident("deferred")? {
                    ConstraintMode::Deferred
                } else {
                    self.expect_ident("immediate")?;
                    ConstraintMode::Immediate
                };
                if !can_defer || initial.replace(mode).is_some() {
                    return Err(self.err_here("invalid or duplicate INITIALLY clause"));
                }
            } else if self.eat_ident("enforced")? {
                if !can_disable || enforced.replace(true).is_some() {
                    return Err(self.err_here("invalid or duplicate ENFORCED clause"));
                }
            } else {
                break;
            }
        }
        if initial.is_some() && deferrable != Some(true) {
            return Err(self.err_here("INITIALLY requires DEFERRABLE"));
        }
        let timing = match deferrable {
            Some(true) => {
                ConstraintTiming::Deferrable(initial.unwrap_or(ConstraintMode::Immediate))
            }
            Some(false) | None => ConstraintTiming::NotDeferrable,
        };
        let validation = match (enforced, not_valid) {
            (Some(false), _) => ConstraintValidation::NotEnforced,
            (_, true) => ConstraintValidation::EnforcedNotValid,
            _ => ConstraintValidation::EnforcedValidated,
        };
        Ok((timing, validation))
    }

    pub(super) fn constraint_timing(
        &mut self,
        allow_not_valid: bool,
    ) -> Result<ConstraintTiming, ParseError> {
        let (timing, validation) = self.constraint_attributes(true, false, allow_not_valid)?;
        debug_assert_eq!(validation, ConstraintValidation::EnforcedValidated);
        Ok(timing)
    }

    fn fk_action(&mut self) -> Result<FkAction, ParseError> {
        if self.eat_ident("no")? {
            self.expect_ident("action")?;
            Ok(FkAction::NoAction)
        } else if self.eat_ident("restrict")? {
            Ok(FkAction::Restrict)
        } else if self.eat_ident("cascade")? {
            Ok(FkAction::Cascade)
        } else if self.eat_ident("set")? {
            if self.eat_ident("null")? {
                Ok(FkAction::SetNull)
            } else {
                self.expect_ident("default")?;
                Ok(FkAction::SetDefault)
            }
        } else {
            Err(self.err_here("expected NO ACTION, RESTRICT, CASCADE, SET NULL, or SET DEFAULT"))
        }
    }

    fn drop_table(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("table")?;
        let (names, if_exists) = self.drop_targets("table name")?;
        let cascade = if self.eat_ident("cascade")? {
            true
        } else {
            let _ = self.eat_ident("restrict")?;
            false
        };
        Ok(Stmt::DropTable(DropTable {
            names,
            if_exists,
            cascade,
        }))
    }
}
