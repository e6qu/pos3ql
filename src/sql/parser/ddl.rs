//! Parsing the data-definition statements.
//!
//! `CREATE TABLE` and its column definitions, constraints and `LIKE` clauses;
//! `CREATE INDEX`; `CREATE VIEW`; and the `DROP` family. Split from the rest of
//! the parser as a second `impl Parser` block: these share the cursor and the
//! token helpers with every other statement, but nothing else refers to them.

use crate::sql::eval::sqlstate;
use super::{
    ColumnDef, CreateTable, DropTable, FkAction, LikeClause, ParseError, Parser, QualName,
    Stmt, TableConstraint, Tok, MAX_LIST,
};
use crate::stack_format;
use crate::storage::MAX_INDEX_COLS;

impl<'a> Parser<'a> {
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
            self.expect_ident("view")?;
            return self.create_view(true);
        }
        if self.eat_ident("unique")? {
            self.expect_ident("index")?;
            return self.create_index(true);
        }
        if self.eat_ident("view")? {
            return self.create_view(false);
        }
        if self.eat_ident("index")? {
            return self.create_index(false);
        }
        if self.eat_ident("schema")? {
            return self.create_schema();
        }
        self.create_table()
    }

    /// CREATE SCHEMA [IF NOT EXISTS] { name [AUTHORIZATION role] |
    /// AUTHORIZATION role } [schema_element ...] ("create schema" consumed).
    /// The only role this engine has is the session's bootstrap user; naming
    /// any other errors as PostgreSQL does. Schema elements are the embedded
    /// CREATE statements, run with the new schema as their creation target.
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
        let name = if self.eat_ident("authorization")? {
            let role = self.col_ident("role name")?;
            if role != "postgres" {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "role \"{}\" does not exist", role),
                    sqlstate: sqlstate::UNDEFINED_OBJECT,
                });
            }
            // An omitted name defaults to the role's name, as PostgreSQL.
            name.unwrap_or(role)
        } else {
            let Some(n) = name else {
                return Err(self.err_here("expected schema name or AUTHORIZATION"));
            };
            n
        };
        let mut elements: [&'a Stmt<'a>; 16] = [&Stmt::Begin; 16];
        let mut n = 0usize;
        while self.peeked == Tok::Ident("create") {
            if n == elements.len() {
                return Err(self.limit("schema elements", elements.len()));
            }
            let element = self.create()?;
            if !matches!(
                element,
                Stmt::CreateTable(_) | Stmt::CreateView { .. } | Stmt::CreateIndex { .. }
            ) {
                return Err(self.err_here(
                    "CREATE SCHEMA elements may be CREATE TABLE, VIEW, or INDEX",
                ));
            }
            elements[n] = self
                .arena
                .alloc(element)
                .map_err(|_| self.err_here("statement too large for SQL arena"))?;
            n += 1;
        }
        Ok(Stmt::CreateSchema {
            name,
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
        self.expect_op("(")?;
        let mut columns = [""; MAX_LIST];
        let mut n = 0;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("index columns", MAX_LIST));
            }
            columns[n] = self.col_ident("column name")?;
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        let columns = self.arena_slice(&columns[..n])?;
        Ok(Stmt::CreateIndex { name, table, columns, unique })
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
        Ok(Stmt::CreateView { name, or_replace, sql })
    }

    /// Dispatches DROP: `VIEW` or `TABLE` ("drop" consumed here).
    pub(super) fn drop_stmt(&mut self) -> Result<Stmt<'a>, ParseError> {
        self.expect_ident("drop")?;
        if self.eat_ident("view")? {
            let (names, if_exists) = self.drop_targets("view name")?;
            return Ok(Stmt::DropView { names, if_exists });
        }
        if self.eat_ident("index")? {
            let (names, if_exists) = self.drop_targets("index name")?;
            return Ok(Stmt::DropIndex { names, if_exists });
        }
        if self.eat_ident("schema")? {
            return self.drop_schema();
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
        Ok(Stmt::DropSchema { names: self.arena_slice(&names[..n])?, if_exists, cascade })
    }

    /// `[IF EXISTS] name [, ...]` after a DROP keyword.
    fn drop_targets(
        &mut self,
        what: &str,
    ) -> Result<(&'a [QualName<'a>], bool), ParseError> {
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
                QualName { schema: Some(first), name: self.any_ident(what)? }
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
        self.expect_op("(")?;
        let mut columns = [ColumnDef { name: "", type_name: "", type_mod: -1, not_null: false, unique: false, primary: false, default: None }; MAX_LIST];
        let mut n = 0;
        let mut cons = [TableConstraint::Unique { name: None, columns: &[] }; MAX_LIST];
        let mut n_cons = 0;
        let mut likes =
            [LikeClause { at: 0, source: QualName::bare(""), defaults: false, constraints: false, indexes: false, identity: false };
                MAX_LIST];
        let mut n_likes = 0;
        loop {
            if n == MAX_LIST {
                return Err(self.limit("column list", MAX_LIST));
            }
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
                Tok::Ident("primary") | Tok::Ident("unique") | Tok::Ident("check") | Tok::Ident("foreign")
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
            let col_name = self.col_ident("column name")?;
            let warnings_before = self.n_warnings;
            let (type_name, type_mod) = self.type_name_mod()?;
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
                    default = Some(self.expression(0)?);
                } else if self.eat_ident("unique")? {
                    unique = true;
                } else if self.eat_ident("primary")? {
                    self.expect_ident("key")?;
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
                } else if col_cons_name.is_some() {
                    return Err(self.err_here("expected a column constraint after CONSTRAINT name"));
                } else {
                    break;
                }
            }
            columns[n] = ColumnDef { name: col_name, type_name, type_mod, not_null, unique, primary, default };
            n += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        let columns = self.arena_slice(&columns[..n])?;
        let constraints = self.arena_slice(&cons[..n_cons])?;
        let likes = self.arena_slice(&likes[..n_likes])?;
        Ok(Stmt::CreateTable(CreateTable { name, columns, constraints, likes, if_not_exists }))
    }

    /// The rest of a `LIKE source [ { INCLUDING | EXCLUDING } option ]...`
    /// element, `LIKE` already consumed. `at` is how many columns precede it.
    fn like_clause(&mut self, at: usize) -> Result<LikeClause<'a>, ParseError> {
        let source = self.qual_name("source table name")?;
        let mut clause =
            LikeClause { at, source, defaults: false, constraints: false, indexes: false, identity: false };
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
                Tok::Ident("identity" | "generated") => clause.identity = including,
                Tok::Ident("all") => {
                    clause.defaults = including;
                    clause.constraints = including;
                    clause.indexes = including;
                    clause.identity = including;
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
                    })
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
    fn table_constraint(&mut self, name: Option<&'a str>) -> Result<TableConstraint<'a>, ParseError> {
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
    fn check_constraint(&mut self, name: Option<&'a str>) -> Result<TableConstraint<'a>, ParseError> {
        self.expect_op("(")?;
        let start = self.peek_at;
        let expression = self.expression(0)?;
        let text = self.text[start..self.peek_at].trim_end();
        let text = self.arena_str(text)?;
        self.expect_op(")")?;
        Ok(TableConstraint::Check { name, expression, text })
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
        Ok(Stmt::DropTable(DropTable { names, if_exists }))
    }}
