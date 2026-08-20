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
    AlterDomainAction, AlterIndexAction, AlterPublicationAction, AlterRoutineAction,
    AlterTriggerAction, AlterTypeAction, CreateDomain, CreateRoutine, CreateSchemaElement,
    CreateTrigger, DomainCheck, Expr, PartitionBound, PartitionClause, PartitionStrategy,
    PublicationOperations, PublicationTarget, RoleOptions, RoutineArgument, RoutineCreateKind,
    RoutineIdentity, RoutineTargetKind, SubscriptionOptions, SubscriptionSlotName, TriggerEvent,
    TriggerIdentity, TriggerTiming, TriggerTransitionTables,
};
use crate::sql::eval::sqlstate;
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

impl<'a> Parser<'a> {
    pub(super) fn alter_trigger(&mut self) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("trigger name")?;
        self.expect_ident("on")?;
        let table = self.qual_name("trigger table")?;
        let action = if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterTriggerAction::Rename(self.col_ident("new trigger name")?)
        } else {
            return Err(self.err_here("expected RENAME after ALTER TRIGGER"));
        };
        Ok(Stmt::AlterTrigger {
            trigger: TriggerIdentity { name, table },
            action,
        })
    }

    pub(super) fn alter_index(&mut self) -> Result<Stmt<'a>, ParseError> {
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
        Err(ParseError {
            at: self.peek_at,
            message: stack_format!(96, "ALTER INDEX form is not implemented"),
            sqlstate: sqlstate::FEATURE_NOT_SUPPORTED,
        })
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
            if self.eat_ident("view")? {
                return self.create_view(true);
            }
            if self.eat_ident("function")? {
                return self.create_routine(true, true);
            }
            if self.eat_ident("procedure")? {
                return self.create_routine(true, false);
            }
            return Err(
                self.unexpected("expected VIEW, FUNCTION, or PROCEDURE after CREATE OR REPLACE")
            );
        }
        if self.eat_ident("unique")? {
            self.expect_ident("index")?;
            return self.create_index(true);
        }
        if self.eat_ident("view")? {
            return self.create_view(false);
        }
        if self.eat_ident("function")? {
            return self.create_routine(false, true);
        }
        if self.eat_ident("procedure")? {
            return self.create_routine(false, false);
        }
        if self.eat_ident("publication")? {
            return self.create_publication();
        }
        if self.eat_ident("subscription")? {
            return self.create_subscription();
        }
        if self.eat_ident("trigger")? {
            return self.create_trigger();
        }
        if self.eat_ident("materialized")? {
            self.expect_ident("view")?;
            return self.create_materialized_view();
        }
        if self.eat_ident("index")? {
            return self.create_index(false);
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
        if self.eat_ident("user")? {
            return self.create_role(true);
        }
        if self.eat_ident("group")? {
            return self.create_role(false);
        }
        self.create_table()
    }

    /// CREATE TRIGGER forms with a complete durable execution model.
    fn create_trigger(&mut self) -> Result<Stmt<'a>, ParseError> {
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
                        update_columns[update_column_count] = self.col_ident("UPDATE OF column")?;
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
            if !matches!(timing, TriggerTiming::After)
                || !matches!(level, crate::sql::ast::TriggerLevel::Statement)
            {
                return Err(self.err_here(
                    "transition tables are only valid for AFTER FOR EACH STATEMENT triggers",
                ));
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
        if when.is_some() && matches!(level, crate::sql::ast::TriggerLevel::Statement) {
            return Err(self.err_here("statement triggers cannot have WHEN conditions"));
        }
        self.expect_ident("execute")?;
        if !self.eat_ident("function")? {
            return Err(self.err_here("trigger procedures are not supported; use EXECUTE FUNCTION"));
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
                arguments[argument_count] = self.str_literal("trigger argument")?;
                argument_count += 1;
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        Ok(Stmt::CreateTrigger(CreateTrigger {
            name,
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

    /// SQL-language routine definition. Its parsed kind makes omitting a
    /// function return type or assigning one to a procedure impossible.
    fn create_routine(&mut self, or_replace: bool, function: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name(if function {
            "function name"
        } else {
            "procedure name"
        })?;
        self.expect_op("(")?;
        let mut arguments = [RoutineArgument {
            name: "",
            type_name: "",
        }; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut count = 0;
        if !self.eat_op(")")? {
            loop {
                if count == arguments.len() {
                    return Err(self.limit("function arguments", arguments.len()));
                }
                let first = self.any_ident("function argument")?;
                let type_name = if self.peeked == Tok::Op("[") {
                    self.advance()?;
                    self.expect_op("]")?;
                    while self.peeked == Tok::Op("[") {
                        self.advance()?;
                        self.expect_op("]")?;
                    }
                    self.arena_str(stack_format!(132, "{}[]", first).as_str())?
                } else if matches!(self.peeked, Tok::Op(",") | Tok::Op(")")) {
                    first
                } else {
                    let type_name = self.type_name()?;
                    arguments[count].name = first;
                    type_name
                };
                arguments[count].type_name = type_name;
                count += 1;
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        let kind = if function {
            self.expect_ident("returns")?;
            if self.eat_ident("trigger")? {
                RoutineCreateKind::Trigger
            } else if self.eat_ident("table")? {
                self.expect_op("(")?;
                let mut columns = [RoutineArgument {
                    name: "",
                    type_name: "",
                }; crate::storage::MAX_ROUTINE_ARGUMENTS];
                let mut column_count = 0;
                loop {
                    if column_count == columns.len() {
                        return Err(self.limit("function result columns", columns.len()));
                    }
                    columns[column_count] = RoutineArgument {
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
            } else {
                RoutineCreateKind::Function {
                    set_returning: self.eat_ident("setof")?,
                    result_type: self.type_name()?,
                }
            }
        } else {
            RoutineCreateKind::Procedure
        };
        self.expect_ident("language")?;
        let language = if self.eat_ident("sql")? {
            crate::sql::ast::RoutineLanguage::Sql
        } else if self.eat_ident("plpgsql")? {
            crate::sql::ast::RoutineLanguage::PlPgSql
        } else {
            return Err(self.unexpected("supported routine language"));
        };
        if matches!(kind, RoutineCreateKind::Trigger)
            != matches!(language, crate::sql::ast::RoutineLanguage::PlPgSql)
        {
            return Err(
                self.unexpected(if matches!(kind, RoutineCreateKind::Trigger) {
                    "trigger functions require LANGUAGE plpgsql"
                } else {
                    "only trigger functions support LANGUAGE plpgsql"
                }),
            );
        }
        self.expect_ident("as")?;
        let body = self.str_literal("function body")?;
        Ok(Stmt::CreateRoutine(CreateRoutine {
            name,
            or_replace,
            arguments: self.arena_slice(&arguments[..count])?,
            kind,
            language,
            body,
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
        let name = self.any_ident("role name")?;
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
        materialized: bool,
    ) -> Result<Stmt<'a>, ParseError> {
        let start = self.peek_at;
        let _ = self.query()?;
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
            materialized,
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
            AlterDomainAction::AddCheck(DomainCheck {
                name: cname,
                expression: self.check_text()?,
            })
        } else if self.eat_ident("drop")? {
            if self.eat_ident("constraint")? {
                let if_exists = if self.eat_ident("if")? {
                    self.expect_ident("exists")?;
                    true
                } else {
                    false
                };
                AlterDomainAction::DropConstraint {
                    name: self.col_ident("constraint name")?,
                    if_exists,
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
            self.expect_ident("to")?;
            AlterDomainAction::Rename(self.col_ident("new domain name")?)
        } else {
            return Err(self.err_here("expected ADD, DROP, RENAME or SET after ALTER DOMAIN"));
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
        }; MAX_LIST];
        let mut n = 0;
        if self.peeked != Tok::Op(")") {
            loop {
                if n == MAX_LIST {
                    return Err(self.limit("composite fields", MAX_LIST));
                }
                let field_name = self.any_ident("composite field name")?;
                let (type_name, type_mod) = self.type_name_mod()?;
                fields[n] = crate::sql::ast::CompositeField {
                    name: field_name,
                    type_name,
                    type_mod,
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
                AlterTypeAction::AddAttribute(crate::sql::ast::CompositeField {
                    name: field_name,
                    type_name,
                    type_mod,
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
                if self.eat_ident("data")? {
                    self.expect_ident("type")?;
                    let (type_name, type_mod) = self.type_name_mod()?;
                    AlterTypeAction::AlterAttributeType {
                        name: field,
                        type_name,
                        type_mod,
                    }
                } else {
                    self.expect_ident("not")?;
                    self.expect_ident("null")?;
                    AlterTypeAction::SetAttributeNotNull(field)
                }
            } else if self.eat_ident("type")? {
                let (type_name, type_mod) = self.type_name_mod()?;
                AlterTypeAction::AlterAttributeType {
                    name: field,
                    type_name,
                    type_mod,
                }
            } else if self.eat_ident("drop")? {
                self.expect_ident("not")?;
                self.expect_ident("null")?;
                AlterTypeAction::DropAttributeNotNull(field)
            } else {
                return Err(self.err_here(
                    "expected SET DATA TYPE, SET NOT NULL, or DROP NOT NULL after ALTER ATTRIBUTE",
                ));
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
        let options = self.seq_options(true)?;
        Ok(Stmt::AlterSequence {
            name,
            if_exists,
            options,
        })
    }

    /// The shared CREATE/ALTER SEQUENCE option list. `allow_restart` enables the
    /// ALTER-only `RESTART [WITH n]` clause.
    fn seq_options(
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
    fn seq_int(&mut self) -> Result<i64, ParseError> {
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
        self.create_table_as(name, columns, if_not_exists, true)
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
                    sql,
                } => CreateSchemaElement::View {
                    name,
                    or_replace,
                    sql,
                },
                Stmt::CreateIndex {
                    name,
                    table,
                    columns,
                    include_columns,
                    nulls_not_distinct,
                    predicate,
                    predicate_text,
                    unique,
                } => CreateSchemaElement::Index {
                    name,
                    table,
                    columns,
                    include_columns,
                    nulls_not_distinct,
                    predicate,
                    predicate_text,
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

    /// CREATE [UNIQUE] INDEX name ON table (col, ...) ("create [unique] index"
    /// consumed).
    fn create_index(&mut self, unique: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("index name")?;
        self.expect_ident("on")?;
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
            self.expect_ident("not")?;
            self.expect_ident("distinct")?;
            true
        } else {
            false
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
            columns,
            include_columns,
            nulls_not_distinct,
            predicate,
            predicate_text,
            unique,
        })
    }

    /// CREATE VIEW name AS <select> ("create [or replace] view" consumed).
    fn create_view(&mut self, or_replace: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("view name")?;
        self.expect_ident("as")?;
        // Capture the raw SELECT text (re-parsed at query time).
        let start = self.peek_at;
        // Validate the body parses now, so a bad view errors at CREATE time.
        let _ = self.query()?;
        let end = self.peek_at;
        let sql = self.text[start..end].trim();
        Ok(Stmt::CreateView {
            name,
            or_replace,
            sql,
        })
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
        let mut publish = PublicationOperations::ALL;
        if self.eat_ident("with")? {
            self.expect_op("(")?;
            self.expect_ident("publish")?;
            self.expect_op("=")?;
            let value = self.str_literal("publication publish option")?;
            publish = self.publication_operations(value)?;
            self.expect_op(")")?;
        }
        Ok(Stmt::CreatePublication {
            name,
            all_tables,
            tables,
            schemas,
            publish,
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
        let mut options = SubscriptionOptions {
            enabled: true,
            create_slot: true,
            copy_data: true,
            slot_name: SubscriptionSlotName::Default,
        };
        if self.eat_ident("with")? {
            self.expect_op("(")?;
            let mut seen_connect = false;
            let mut seen_enabled = false;
            let mut seen_create_slot = false;
            let mut seen_copy_data = false;
            let mut seen_slot_name = false;
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
                    options.enabled = value;
                } else if key.eq_ignore_ascii_case("create_slot") {
                    let value = self.subscription_bool_option(key)?;
                    if core::mem::replace(&mut seen_create_slot, true) {
                        return Err(self.err_here("duplicate subscription option create_slot"));
                    }
                    options.create_slot = value;
                } else if key.eq_ignore_ascii_case("copy_data") {
                    let value = self.subscription_bool_option(key)?;
                    if core::mem::replace(&mut seen_copy_data, true) {
                        return Err(self.err_here("duplicate subscription option copy_data"));
                    }
                    options.copy_data = value;
                } else if key.eq_ignore_ascii_case("slot_name") {
                    if core::mem::replace(&mut seen_slot_name, true) {
                        return Err(self.err_here("duplicate subscription option slot_name"));
                    }
                    self.expect_op("=")?;
                    options.slot_name = if self.eat_ident("none")? {
                        SubscriptionSlotName::None
                    } else if let Tok::Str(value) = self.peeked {
                        self.advance()?;
                        SubscriptionSlotName::Named(value)
                    } else {
                        SubscriptionSlotName::Named(self.any_ident("subscription slot name")?)
                    };
                } else {
                    return Err(self.err_here("subscription option is not implemented"));
                }
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
            if !connect {
                if (seen_enabled && options.enabled)
                    || (seen_create_slot && options.create_slot)
                    || (seen_copy_data && options.copy_data)
                {
                    return Err(self.err_here(
                        "connect = false requires enabled, create_slot, and copy_data to be false",
                    ));
                }
                options.enabled = false;
                options.create_slot = false;
                options.copy_data = false;
            }
        }
        if options.slot_name == SubscriptionSlotName::None
            && (options.enabled || options.create_slot)
        {
            return Err(
                self.err_here("slot_name = NONE requires enabled and create_slot to be false")
            );
        }
        Ok(Stmt::CreateSubscription {
            name,
            connection,
            publications: self.arena_slice(&names[..count])?,
            options,
        })
    }

    fn subscription_bool_option(&mut self, option: &str) -> Result<bool, ParseError> {
        if !self.eat_op("=")? {
            return Ok(true);
        }
        self.subscription_bool(option)
    }

    fn subscription_bool(&mut self, _option: &str) -> Result<bool, ParseError> {
        let value = match self.peeked {
            Tok::Ident("true" | "on") | Tok::Str("true" | "on" | "1") => true,
            Tok::Ident("false" | "off") | Tok::Str("false" | "off" | "0") => false,
            _ => return Err(self.err_here("subscription option requires a boolean value")),
        };
        self.advance()?;
        Ok(value)
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
                self.expect_ident("publish")?;
                self.expect_op("=")?;
                let value = self.str_literal("publication publish option")?;
                let publish = self.publication_operations(value)?;
                self.expect_op(")")?;
                AlterPublicationAction::SetOperations(publish)
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
        if self.eat_ident("trigger")? {
            return self.drop_trigger();
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
            let (names, if_exists) = self.drop_targets("index name")?;
            return Ok(Stmt::DropIndex { names, if_exists });
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
        if self.eat_ident("role")? || self.eat_ident("user")? || self.eat_ident("group")? {
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
                    if argument_count == argument_types.len() {
                        return Err(self.limit("function arguments", argument_types.len()));
                    }
                    argument_types[argument_count] = self.any_ident("function argument type")?;
                    argument_count += 1;
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
        }
    }

    pub(super) fn drop_trigger(&mut self) -> Result<Stmt<'a>, ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let mut triggers = [TriggerIdentity {
            name: "",
            table: QualName::bare(""),
        }; MAX_LIST];
        let mut count = 0usize;
        loop {
            if count == triggers.len() {
                return Err(self.limit("triggers", triggers.len()));
            }
            let name = self.col_ident("trigger name")?;
            self.expect_ident("on")?;
            triggers[count] = TriggerIdentity {
                name,
                table: self.qual_name("trigger table")?,
            };
            count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        if self.eat_ident("cascade")? {
            return Err(self.err_here("DROP TRIGGER CASCADE is not supported"));
        }
        let _ = self.eat_ident("restrict")?;
        Ok(Stmt::DropTrigger {
            triggers: self.arena_slice(&triggers[..count])?,
            if_exists,
        })
    }

    pub(super) fn alter_routine(
        &mut self,
        kind: RoutineTargetKind,
    ) -> Result<Stmt<'a>, ParseError> {
        let name = self.qual_name("routine name")?;
        self.expect_op("(")?;
        let mut argument_types = [""; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut count = 0;
        if !self.eat_op(")")? {
            loop {
                if count == argument_types.len() {
                    return Err(self.limit("routine arguments", argument_types.len()));
                }
                argument_types[count] = self.any_ident("routine argument type")?;
                count += 1;
                if self.eat_op(")")? {
                    break;
                }
                self.expect_op(",")?;
            }
        }
        let action = if self.eat_ident("owner")? {
            self.expect_ident("to")?;
            AlterRoutineAction::SetOwner(self.any_ident("role name")?)
        } else if self.eat_ident("rename")? {
            self.expect_ident("to")?;
            AlterRoutineAction::Rename(self.col_ident("routine name")?)
        } else if self.eat_ident("set")? {
            self.expect_ident("schema")?;
            AlterRoutineAction::SetSchema(self.col_ident("schema name")?)
        } else {
            return Err(self.unexpected("expected OWNER, RENAME, or SET SCHEMA"));
        };
        Ok(Stmt::AlterRoutine {
            kind,
            routine: RoutineIdentity {
                name,
                argument_types: self.arena_slice(&argument_types[..count])?,
                signature_is_explicit: true,
            },
            action,
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

    fn create_table(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("table")?;
        let if_not_exists = if self.eat_ident("if")? {
            self.expect_ident("not")?;
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.qual_name("table name")?;
        // A partition is a table whose column layout is inherited from its
        // parent; unlike ordinary CREATE TABLE it has no column list here.
        if self.eat_ident("partition")? {
            self.expect_ident("of")?;
            let parent = self.qual_name("partitioned table name")?;
            let bound = self.partition_bound()?;
            return Ok(Stmt::CreateTable(CreateTable {
                name,
                columns: &[],
                constraints: &[],
                likes: &[],
                partition: PartitionClause::Of { parent, bound },
                if_not_exists,
            }));
        }
        // `CREATE TABLE name AS <query>` — no explicit column list.
        if self.eat_ident("as")? {
            return self.create_table_as(name, &[], if_not_exists, false);
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
                let cols = self.arena_slice(&list[..m])?;
                return self.create_table_as(name, cols, if_not_exists, false);
            }
            // Otherwise it is a column definition whose name we already read.
            pending_first_col = Some(first_ident);
        }
        let mut columns = [ColumnDef {
            name: "",
            type_name: "",
            type_mod: -1,
            collation: crate::sql::ast::Collation::Default,
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
                ) {
                    let c = self.table_constraint(cons_name)?;
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
            let collation = if self.eat_ident("collate")? {
                self.collation_name()?
            } else {
                crate::sql::ast::Collation::Default
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
                    // An explicitly named single-column UNIQUE desugars to a
                    // table constraint so the name is retained; an unnamed one
                    // rides the column flag with a synthesized name.
                    if let Some(cons_name) = col_cons_name {
                        if n_cons == MAX_LIST {
                            return Err(self.limit("constraint list", MAX_LIST));
                        }
                        cons[n_cons] = TableConstraint::Unique {
                            name: Some(cons_name),
                            columns: self.arena_slice(&[col_name])?,
                        };
                        n_cons += 1;
                        continue;
                    }
                    unique = true;
                } else if self.eat_ident("primary")? {
                    self.expect_ident("key")?;
                    if let Some(cons_name) = col_cons_name {
                        if n_cons == MAX_LIST {
                            return Err(self.limit("constraint list", MAX_LIST));
                        }
                        cons[n_cons] = TableConstraint::PrimaryKey {
                            name: Some(cons_name),
                            columns: self.arena_slice(&[col_name])?,
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
                    let c = self.check_constraint(col_cons_name)?;
                    if n_cons == MAX_LIST {
                        return Err(self.limit("constraint list", MAX_LIST));
                    }
                    cons[n_cons] = c;
                    n_cons += 1;
                    continue;
                } else if self.eat_ident("references")? {
                    // Desugar a column REFERENCES to a single-column FK.
                    let child = self.arena_slice(&[col_name])?;
                    let c = self.references_tail(col_cons_name, child)?;
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
            PartitionClause::By {
                strategy,
                columns: self.arena_slice(&columns[..n_columns])?,
            }
        } else {
            PartitionClause::None
        };
        let columns = self.arena_slice(&columns[..n])?;
        let constraints = self.arena_slice(&cons[..n_cons])?;
        let likes = self.arena_slice(&likes[..n_likes])?;
        Ok(Stmt::CreateTable(CreateTable {
            name,
            columns,
            constraints,
            likes,
            partition,
            if_not_exists,
        }))
    }

    fn partition_bound(&mut self) -> Result<PartitionBound<'a>, ParseError> {
        self.expect_ident("for")?;
        self.expect_ident("values")?;
        if self.eat_ident("default")? {
            return Ok(PartitionBound::Default);
        }
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
    fn column_name_list(&mut self) -> Result<&'a [&'a str], ParseError> {
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
    ) -> Result<TableConstraint<'a>, ParseError> {
        if self.eat_ident("primary")? {
            self.expect_ident("key")?;
            let columns = self.column_name_list()?;
            Ok(TableConstraint::PrimaryKey { name, columns })
        } else if self.eat_ident("unique")? {
            let columns = self.column_name_list()?;
            Ok(TableConstraint::Unique { name, columns })
        } else if self.eat_ident("check")? {
            self.check_constraint(name)
        } else {
            self.expect_ident("foreign")?;
            self.expect_ident("key")?;
            let columns = self.column_name_list()?;
            self.expect_ident("references")?;
            self.references_tail(name, columns)
        }
    }

    /// A CHECK (predicate): captures the predicate's source text for durable
    /// storage alongside the parsed expression.
    fn check_constraint(
        &mut self,
        name: Option<&'a str>,
    ) -> Result<TableConstraint<'a>, ParseError> {
        self.expect_op("(")?;
        let start = self.peek_at;
        let expression = self.expression(0)?;
        let text = self.text[start..self.peek_at].trim_end();
        let text = self.arena_str(text)?;
        self.expect_op(")")?;
        Ok(TableConstraint::Check {
            name,
            expression,
            text,
        })
    }

    /// The part of a FOREIGN KEY after `REFERENCES`: parent table, optional
    /// parent columns, and ON DELETE / ON UPDATE actions.
    fn references_tail(
        &mut self,
        name: Option<&'a str>,
        columns: &'a [&'a str],
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
        Ok(TableConstraint::ForeignKey {
            name,
            columns,
            parent,
            parent_cols,
            on_delete,
            on_update,
        })
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
