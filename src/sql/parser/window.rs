//! The window clause: `OVER (...)`, `WINDOW` definitions, and the frame.
//!
//! A window specification is written in three places that must agree — inline
//! after `OVER`, by name against a `WINDOW` clause, and as a named spec that
//! another one extends — so all three resolve through one body parser here.

use crate::sql::eval::sqlstate;
use crate::sql::lexer::Tok;
use crate::stack_format;

use super::{ParseError, Parser, MAX_LIST, MAX_WINDOW_DEFS};
use crate::sql::ast::{Expr, FrameBound, FrameExclusion, FrameUnits, OrderBy, WindowFrame, WindowSpec};

impl<'a> Parser<'a> {
    /// Parses an optional aggregate `FILTER (WHERE cond)` clause.
    pub(super) fn parse_filter(&mut self) -> Result<Option<&'a Expr<'a>>, ParseError> {
        if self.peeked != Tok::Ident("filter") {
            return Ok(None);
        }
        self.advance()?;
        self.expect_op("(")?;
        self.expect_ident("where")?;
        let cond = self.expression(0)?;
        self.expect_op(")")?;
        Ok(Some(cond))
    }

    /// Parses a comma-separated ORDER BY item list (the `ORDER BY` keyword
    /// already consumed).
    pub(super) fn order_by_items(&mut self) -> Result<&'a [OrderBy<'a>], ParseError> {
        let null_expr: &'a Expr<'a> = self.arena_expr(Expr::Null)?;
        let mut ord = [OrderBy { expression: null_expr, descending: false, nulls_first: false }; MAX_LIST];
        let mut m = 0;
        loop {
            if m == MAX_LIST {
                return Err(self.limit("ORDER BY", MAX_LIST));
            }
            let expression = self.expression(0)?;
            let descending = if self.eat_ident("desc")? {
                true
            } else {
                self.eat_ident("asc")?;
                false
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
            ord[m] = OrderBy { expression, descending, nulls_first };
            m += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.arena_slice(&ord[..m])
    }

    /// One frame bound: UNBOUNDED PRECEDING/FOLLOWING, CURRENT ROW, or
    /// `<expression> PRECEDING/FOLLOWING`.
    pub(super) fn frame_bound(&mut self) -> Result<FrameBound<'a>, ParseError> {
        if self.eat_ident("unbounded")? {
            if self.eat_ident("preceding")? {
                return Ok(FrameBound::UnboundedPreceding);
            }
            self.expect_ident("following")?;
            return Ok(FrameBound::UnboundedFollowing);
        }
        if self.eat_ident("current")? {
            self.expect_ident("row")?;
            return Ok(FrameBound::CurrentRow);
        }
        let offset = self.expression(0)?;
        if self.eat_ident("preceding")? {
            return Ok(FrameBound::Preceding(offset));
        }
        self.expect_ident("following")?;
        Ok(FrameBound::Following(offset))
    }

    /// An optional `OVER` clause: either `OVER name`, naming one of the query's
    /// `WINDOW` definitions, or an inline `OVER (...)` specification.
    pub(super) fn parse_over(&mut self) -> Result<Option<&'a WindowSpec<'a>>, ParseError> {
        if !self.eat_ident("over")? {
            return Ok(None);
        }
        // `OVER name` uses the named window as it stands, frame included; only
        // the parenthesized form copies it, and a copy may not carry a frame.
        if !matches!(self.peeked, Tok::Op("(")) {
            let name = self.any_ident("window name or '('")?;
            return Ok(Some(self.named_window(name)?));
        }
        self.expect_op("(")?;
        let spec = self.window_spec_body()?;
        Ok(Some(self.arena_window(spec)?))
    }

    /// Resolves a name against the query's `WINDOW` definitions.
    pub(super) fn named_window(&mut self, name: &str) -> Result<&'a WindowSpec<'a>, ParseError> {
        for entry in &self.windows[..self.n_windows] {
            match entry {
                Some((defined, spec)) if *defined == name => return Ok(spec),
                _ => {}
            }
        }
        Err(ParseError {
            at: self.peek_at,
            message: stack_format!(96, "window \"{}\" does not exist", name),
            sqlstate: sqlstate::UNDEFINED_OBJECT,
        })
    }

    pub(super) fn arena_window(&self, spec: WindowSpec<'a>) -> Result<&'a WindowSpec<'a>, ParseError> {
        let spec = self.arena.alloc(spec).map_err(|_| self.err_here("window spec too large for arena"))?;
        Ok(&*spec)
    }

    /// `WINDOW name AS ( ... ) [, ...]`, replacing any definitions already
    /// parsed (the lookahead in [`Self::prescan_windows`] parses the same
    /// clause, and re-parsing it in its written position must not see itself
    /// as a redefinition).
    pub(super) fn window_definitions(&mut self) -> Result<(), ParseError> {
        self.n_windows = 0;
        loop {
            let name = self.any_ident("window name")?;
            if self.named_window(name).is_ok() {
                return Err(ParseError {
                    at: self.peek_at,
                    message: stack_format!(96, "window \"{}\" is already defined", name),
                    sqlstate: sqlstate::WINDOWING_ERROR,
                });
            }
            if self.n_windows == MAX_WINDOW_DEFS {
                return Err(self.limit("WINDOW definitions", MAX_WINDOW_DEFS));
            }
            self.expect_ident("as")?;
            self.expect_op("(")?;
            let spec = self.window_spec_body()?;
            self.windows[self.n_windows] = Some((name, self.arena_window(spec)?));
            self.n_windows += 1;
            if !self.eat_op(",")? {
                return Ok(());
            }
        }
    }

    /// The inside of a window specification, `(` already consumed: an optional
    /// existing window name to copy, then PARTITION BY / ORDER BY / frame.
    pub(super) fn window_spec_body(&mut self) -> Result<WindowSpec<'a>, ParseError> {
        // A leading identifier that is not one of the clause keywords names an
        // existing window to copy, as in `OVER (w ORDER BY x)`.
        let copied = match self.peeked {
            // `exclude` is not a window name either: it only follows a frame,
            // so a leading one is the syntax error PostgreSQL reports.
            Tok::Ident(name)
                if !matches!(
                    name,
                    "partition" | "order" | "rows" | "range" | "groups" | "exclude"
                ) =>
            {
                self.advance()?;
                Some((name, *self.named_window(name)?))
            }
            Tok::QuotedIdent(name) => {
                self.advance()?;
                Some((name, *self.named_window(name)?))
            }
            _ => None,
        };
        let partition_by = if self.eat_ident("partition")? {
            self.expect_ident("by")?;
            let mut parts: [&'a Expr<'a>; MAX_LIST] = [self.arena_expr(Expr::Null)?; MAX_LIST];
            let mut n = 0;
            loop {
                if n == MAX_LIST {
                    return Err(self.limit("PARTITION BY", MAX_LIST));
                }
                parts[n] = self.expression(0)?;
                n += 1;
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.arena_slice(&parts[..n])?
        } else {
            &[]
        };
        let order_by = if self.eat_ident("order")? {
            self.expect_ident("by")?;
            self.order_by_items()?
        } else {
            &[]
        };
        let frame = if matches!(
            self.peeked,
            Tok::Ident("rows") | Tok::Ident("range") | Tok::Ident("groups")
        ) {
            let units = match self.peeked {
                Tok::Ident("rows") => FrameUnits::Rows,
                Tok::Ident("range") => FrameUnits::Range,
                _ => FrameUnits::Groups,
            };
            self.advance()?;
            let (start, end) = if self.eat_ident("between")? {
                let start = self.frame_bound()?;
                self.expect_ident("and")?;
                let end = self.frame_bound()?;
                (start, end)
            } else {
                (self.frame_bound()?, FrameBound::CurrentRow)
            };
            // PostgreSQL's frame-shape validation (42P20).
            if matches!(start, FrameBound::UnboundedFollowing) {
                return Err(ParseError { sqlstate: sqlstate::WINDOWING_ERROR, ..self.err_here("frame start cannot be UNBOUNDED FOLLOWING") });
            }
            if matches!(end, FrameBound::UnboundedPreceding) {
                return Err(ParseError { sqlstate: sqlstate::WINDOWING_ERROR, ..self.err_here("frame end cannot be UNBOUNDED PRECEDING") });
            }
            if matches!(start, FrameBound::CurrentRow) && matches!(end, FrameBound::Preceding(_)) {
                return Err(
                    ParseError { sqlstate: sqlstate::WINDOWING_ERROR, ..self.err_here("frame starting from current row cannot have preceding rows") }
                );
            }
            if matches!(start, FrameBound::Following(_))
                && matches!(end, FrameBound::Preceding(_) | FrameBound::CurrentRow)
            {
                return Err(
                    ParseError { sqlstate: sqlstate::WINDOWING_ERROR, ..self.err_here("frame starting from following row cannot have preceding rows") }
                );
            }
            let exclusion = if self.eat_ident("exclude")? {
                if self.eat_ident("no")? {
                    self.expect_ident("others")?;
                    FrameExclusion::NoOthers
                } else if self.eat_ident("current")? {
                    self.expect_ident("row")?;
                    FrameExclusion::CurrentRow
                } else if self.eat_ident("group")? {
                    FrameExclusion::Group
                } else if self.eat_ident("ties")? {
                    FrameExclusion::Ties
                } else {
                    return Err(self.err_here(
                        "expected NO OTHERS, CURRENT ROW, GROUP, or TIES after EXCLUDE",
                    ));
                }
            } else {
                FrameExclusion::NoOthers
            };
            Some(WindowFrame { units, start, end, exclusion })
        } else {
            None
        };
        self.expect_op(")")?;
        let Some((name, base)) = copied else {
            return Ok(WindowSpec { partition_by, order_by, frame });
        };
        // A copy inherits the partitioning, may add an ORDER BY only where the
        // copied window has none, and may not copy a window that has a frame.
        if !partition_by.is_empty() {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "cannot override PARTITION BY clause of window \"{}\"", name),
                sqlstate: sqlstate::WINDOWING_ERROR,
            });
        }
        if !base.order_by.is_empty() && !order_by.is_empty() {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "cannot override ORDER BY clause of window \"{}\"", name),
                sqlstate: sqlstate::WINDOWING_ERROR,
            });
        }
        if base.frame.is_some() {
            return Err(ParseError {
                at: self.peek_at,
                message: stack_format!(96, "cannot copy window \"{}\" because it has a frame clause", name),
                sqlstate: sqlstate::WINDOWING_ERROR,
            });
        }
        Ok(WindowSpec {
            partition_by: base.partition_by,
            order_by: if order_by.is_empty() { base.order_by } else { order_by },
            frame,
        })
    }
}
