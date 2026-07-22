//! Parsing the data-definition statements.
//!
//! `CREATE TABLE` and its column definitions, constraints and `LIKE` clauses;
//! `CREATE INDEX`; `CREATE VIEW`; and the `DROP` family. Split from the rest of
//! the parser as a second `impl Parser` block: these share the cursor and the
//! token helpers with every other statement, but nothing else refers to them.

use super::{
    ColumnDef, CreateTable, DropTable, FkAction, LikeClause, ParseError, Parser, Stmt,
    TableConstraint, Tok, MAX_LIST,
};
use crate::stack_format;
use crate::storage::MAX_INDEX_COLS;

impl<'a> Parser<'a> {
    /// Dispatches CREATE: `[OR REPLACE] VIEW` or `TABLE` ("create" consumed here).
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
        self.create_table()
    }

    /// CREATE [UNIQUE] INDEX name ON table (col, ...) ("create [unique] index"
    /// consumed).
    fn create_index(&mut self, unique: bool) -> Result<Stmt<'a>, ParseError> {
        let name = self.col_ident("index name")?;
        self.expect_ident("on")?;
        let table = self.col_ident("table name")?;
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
        let name = self.col_ident("view name")?;
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
            let (name, if_exists) = self.drop_target("view name")?;
            return Ok(Stmt::DropView { name, if_exists });
        }
        if self.eat_ident("index")? {
            let (name, if_exists) = self.drop_target("index name")?;
            return Ok(Stmt::DropIndex { name, if_exists });
        }
        self.drop_table()
    }

    /// `[IF EXISTS] name` after a DROP keyword.
    fn drop_target(&mut self, what: &str) -> Result<(&'a str, bool), ParseError> {
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        Ok((self.any_ident(what)?, if_exists))
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
        let name = self.col_ident("table name")?;
        self.expect_op("(")?;
        let mut columns = [ColumnDef { name: "", type_name: "", type_mod: -1, not_null: false, unique: false, primary: false, default: None }; MAX_LIST];
        let mut n = 0;
        let mut cons = [TableConstraint::Unique { name: None, columns: &[] }; MAX_LIST];
        let mut n_cons = 0;
        let mut likes =
            [LikeClause { at: 0, source: "", defaults: false, constraints: false, indexes: false, identity: false };
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
            let (type_name, type_mod) = self.type_name_mod()?;
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
        let source = self.col_ident("source table name")?;
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
                        sqlstate: "0A000",
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
        let parent = self.col_ident("referenced table")?;
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
        let if_exists = if self.eat_ident("if")? {
            self.expect_ident("exists")?;
            true
        } else {
            false
        };
        let name = self.col_ident("table name")?;
        Ok(Stmt::DropTable(DropTable { name, if_exists }))
    }}
