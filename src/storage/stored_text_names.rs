//! Stored expressions that keep routine and type names as SQL text without a
//! captured binding (exclusion elements and predicates, statistics
//! expressions, trigger conditions and domain constraints) would be left
//! unresolvable by renaming or moving a routine or type they name, so the
//! rename is refused. CHECK
//! constraints, column defaults and generation expressions, and index keys
//! and predicates are bound to what they name and refuse only enum label
//! renames, whose spellings stay text.

use super::{CatalogDdlState, ExtendedStatisticsKey, SqlName, Storage, current_database};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::lexer::{Lexer, Tok};
use crate::sql_err;

/// A routine or type that stored expression text may name.
#[derive(Clone, Copy)]
pub(crate) enum StoredTextName {
    Routine {
        schema: SqlName,
        name: SqlName,
    },
    Type {
        schema: SqlName,
        name: SqlName,
    },
    /// An enum whose labels a cast in stored text spells.
    EnumLabels {
        schema: SqlName,
        name: SqlName,
    },
}

impl StoredTextName {
    fn action(self) -> &'static str {
        match self {
            Self::Routine { .. } => "rename or move function",
            Self::Type { .. } => "rename or move type",
            Self::EnumLabels { .. } => "rename a value of type",
        }
    }

    fn identity(self) -> (SqlName, SqlName) {
        match self {
            Self::Routine { schema, name }
            | Self::Type { schema, name }
            | Self::EnumLabels { schema, name } => (schema, name),
        }
    }

    /// Whether `text` may name this object. An unqualified name counts, since
    /// the search path that resolves it is not part of the stored text.
    fn named_by(self, text: &str, arena: &crate::mem::arena::Arena) -> Result<bool, SqlError> {
        let (schema, name) = self.identity();
        let identifier = |token: Tok<'_>, expected: SqlName| match token {
            Tok::Ident(value) | Tok::QuotedIdent(value) => value == expected.as_str(),
            _ => false,
        };
        let type_context =
            |token: Tok<'_>| matches!(token, Tok::Op("::") | Tok::Ident("as" | "of"));
        // A bound enum expression marks each cast it binds by type identity
        // (`/*p3t:N*/`); those spellings follow the type.
        let bound_cast = |start: usize| {
            let before = text[..start].trim_end();
            before.ends_with("*/")
                && before
                    .rfind("/*")
                    .is_some_and(|comment| before[comment..].starts_with("/*p3t:"))
        };
        let mark = arena.mark();
        let result = (|| {
            let mut lexer = Lexer::new(text, arena);
            let mut previous = [Tok::Eof; 3];
            let mut starts = [0usize; 3];
            loop {
                let token = lexer.next_token().map_err(|error| {
                    sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "invalid stored expression at byte {}: {}",
                        error.at,
                        error.message
                    )
                })?;
                if token == Tok::Eof {
                    return Ok(false);
                }
                let start = lexer.token_start();
                if identifier(token, name) {
                    let qualified = previous[0] == Tok::Op(".");
                    if qualified && !identifier(previous[1], schema) {
                        previous = [token, previous[0], previous[1]];
                        starts = [start, starts[0], starts[1]];
                        continue;
                    }
                    let found = match self {
                        Self::Routine { .. } => {
                            let mut lookahead = lexer.clone();
                            lookahead.next_token().ok() == Some(Tok::Op("("))
                        }
                        Self::Type { .. } | Self::EnumLabels { .. } => {
                            let (context, first) = if qualified {
                                (previous[2], starts[1])
                            } else {
                                (previous[0], start)
                            };
                            type_context(context) && !bound_cast(first)
                        }
                    };
                    if found {
                        return Ok(true);
                    }
                }
                previous = [token, previous[0], previous[1]];
                starts = [start, starts[0], starts[1]];
            }
        })();
        // SAFETY: lexing borrows only `text`; no token outlives this call.
        unsafe { arena.rewind_to(mark) };
        result
    }
}

impl Storage {
    /// Refuses a rename or schema move of `target` while a table-level stored
    /// expression names it as text.
    pub(crate) fn require_unnamed_by_stored_text(
        &self,
        target: StoredTextName,
        txid: u32,
        arena: &crate::mem::arena::Arena,
    ) -> Result<(), SqlError> {
        let refuse = |owner: &str, owner_name: &str| {
            let (schema, name) = target.identity();
            Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "cannot {} {}.{} because {} \"{}\" names it in a stored expression",
                target.action(),
                schema.as_str(),
                name.as_str(),
                owner,
                owner_name
            ))
        };
        for slot in 0..self.table_count() {
            if !self.table_slot_visible_to(slot, txid) {
                continue;
            }
            let definition = self.table_def(slot, txid);
            // CHECK constraints and column expressions are bound to the
            // routines and types they name, so only their enum labels, which
            // stay text, block a rename here.
            let bound = definition
                .columns()
                .iter()
                .filter_map(|column| column.default.expression().map(|text| text.as_str()))
                .chain(
                    definition
                        .checks()
                        .iter()
                        .map(|check| check.expression.as_str()),
                )
                .filter(|_| matches!(target, StoredTextName::EnumLabels { .. }));
            let named = bound
                .chain(definition.exclusions().iter().flat_map(|exclusion| {
                    exclusion
                        .elements()
                        .filter_map(|element| match element {
                            super::ExclusionElement::Expression(text) => Some(text),
                            super::ExclusionElement::Column(_) => None,
                        })
                        .chain(exclusion.predicate.as_ref().map(|text| text.as_str()))
                }))
                .try_fold(false, |found, text| {
                    Ok::<_, SqlError>(found || target.named_by(text, arena)?)
                })?;
            if named {
                return refuse("table", definition.name.as_str());
            }
        }
        for slot in 0..self.index_count() {
            let Some(index) = self.index_visible_to(slot, txid) else {
                continue;
            };
            let index = index.definition_for(txid);
            // Key expressions and predicates are bound like CHECK
            // constraints; only enum labels stay text.
            if !matches!(target, StoredTextName::EnumLabels { .. }) {
                continue;
            }
            for text in index
                .expressions
                .iter()
                .flatten()
                .map(|text| text.as_str())
                .chain(index.predicate.as_ref().map(|text| text.as_str()))
            {
                if target.named_by(text, arena)? {
                    return refuse("index", index.name_for(txid).as_str());
                }
            }
        }
        for slot in 0..self.extended_statistics_count() {
            let statistics = self.extended_statistics(slot);
            if statistics.database != current_database() || !statistics.visible_to(txid) {
                continue;
            }
            for key in statistics.keys_for(txid) {
                if let ExtendedStatisticsKey::Expression(text) = key
                    && target.named_by(text.as_str(), arena)?
                {
                    return refuse(
                        "statistics object",
                        statistics.definition_for(txid).name.as_str(),
                    );
                }
            }
        }
        for slot in 0..self.trigger_capacity() {
            let trigger = self.trigger(slot);
            if trigger.database != current_database() || !trigger.visible_to(txid) {
                continue;
            }
            let definition = trigger.definition_to(txid);
            if let Some(when) = definition.when
                && target.named_by(when.as_str(), arena)?
            {
                return refuse("trigger", definition.name.as_str());
            }
        }
        for slot in 0..self.domain_count() {
            let domain = self.domain_for(slot, txid);
            if domain.database != current_database() || domain.ddl_state == CatalogDdlState::Absent
            {
                continue;
            }
            if !domain.visible_to(txid) {
                continue;
            }
            let default = domain.default_expr.as_ref().map(|text| text.as_str());
            for text in default.into_iter().chain(
                domain
                    .checks()
                    .iter()
                    .map(|check| check.expression.as_str()),
            ) {
                if target.named_by(text, arena)? {
                    return refuse("type", domain.name.as_str());
                }
            }
        }
        Ok(())
    }
}
