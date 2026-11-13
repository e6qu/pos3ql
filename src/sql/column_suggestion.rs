//! PostgreSQL's spelling suggestion for a missing column (`errorMissingColumn`
//! in `parse_relation.c`): every FROM item of the failing query and of the
//! queries enclosing it is searched, innermost first, for the column whose
//! name is closest to the one written by Levenshtein distance.
//!
//! A FROM item whose name differs from a written qualifier adds that
//! distance as a penalty. A candidate farther than three edits, or differing
//! in more than half of the written name's bytes, is never suggested. The
//! hint names the single closest column, or two equally close ones; three or
//! more at the closest distance give no hint.

use crate::util::StackStr;

/// PostgreSQL's `MAX_FUZZY_DISTANCE`.
const MAX_FUZZY_DISTANCE: usize = 3;

/// Longest name compared, in characters; `NAMEDATALEN - 1` bytes bounds
/// every SQL identifier.
const MAX_NAME_CHARS: usize = crate::sql::lexer::MAX_IDENTIFIER_BYTES;

#[derive(Clone, Copy)]
struct Candidate {
    relation: StackStr<{ crate::sql::lexer::MAX_IDENTIFIER_BYTES }>,
    column: StackStr<{ crate::sql::lexer::MAX_IDENTIFIER_BYTES }>,
}

/// PostgreSQL's `FuzzyAttrMatchState` for one missing column reference.
pub struct ColumnSuggestion<'n> {
    qualifier: Option<&'n str>,
    name: &'n str,
    distance: usize,
    first: Option<Candidate>,
    second: Option<Candidate>,
    /// A FROM item answering the qualifier has the exact column, so the
    /// column exists but is out of reach; PostgreSQL then explains why
    /// instead of suggesting a spelling.
    exact: bool,
}

impl<'n> ColumnSuggestion<'n> {
    pub(crate) fn new(qualifier: Option<&'n str>, name: &'n str) -> Self {
        Self {
            qualifier,
            name,
            distance: MAX_FUZZY_DISTANCE + 1,
            first: None,
            second: None,
            exact: false,
        }
    }

    /// Considers the columns of the FROM item named `relation`, in order.
    /// Dropped columns are passed as empty names and never match.
    pub(crate) fn relation<'c>(
        &mut self,
        relation: &str,
        columns: impl IntoIterator<Item = &'c str>,
    ) {
        let penalty = self
            .qualifier
            .map_or(0, |qualifier| levenshtein(qualifier, relation));
        for column in columns {
            if penalty > self.distance {
                return;
            }
            if column.is_empty() {
                continue;
            }
            if penalty == 0 && column == self.name {
                self.exact = true;
            }
            let distance = levenshtein(column, self.name);
            if distance > self.name.len() / 2 {
                continue;
            }
            let distance = distance + penalty;
            let candidate = || Candidate {
                relation: StackStr::from_str(relation),
                column: StackStr::from_str(column),
            };
            if distance < self.distance {
                self.distance = distance;
                self.first = Some(candidate());
                self.second = None;
            } else if distance == self.distance {
                if self.second.is_some() {
                    // Too many equally close: only a closer one is useful.
                    self.first = None;
                    self.second = None;
                } else if self.first.is_some() {
                    self.second = Some(candidate());
                }
            }
        }
    }

    /// PostgreSQL's hint, when one column or two equally close columns are
    /// the closest and none matches exactly.
    pub(crate) fn hint(&self) -> Option<StackStr<{ crate::sql::error_position::NOTE_BYTES }>> {
        use core::fmt::Write as _;
        if self.exact {
            return None;
        }
        let first = self.first?;
        let mut hint = StackStr::new();
        let _ = match self.second {
            None => write!(
                hint,
                "Perhaps you meant to reference the column \"{}.{}\".",
                first.relation.as_str(),
                first.column.as_str()
            ),
            Some(second) => write!(
                hint,
                "Perhaps you meant to reference the column \"{}.{}\" or the column \"{}.{}\".",
                first.relation.as_str(),
                first.column.as_str(),
                second.relation.as_str(),
                second.column.as_str()
            ),
        };
        Some(hint)
    }
}

/// `error` with the suggestion a single relation's columns give, for a
/// query whose only FROM item is `relation`.
pub(crate) fn from_relation(
    error: crate::sql::eval::SqlError,
    qualifier: Option<&str>,
    name: &str,
    relation: &str,
    columns: &[crate::storage::ColumnMeta],
) -> crate::sql::eval::SqlError {
    let mut suggestion = ColumnSuggestion::new(qualifier, name);
    suggestion.relation(relation, columns.iter().map(|column| column.name.as_str()));
    crate::sql::error_position::with_column_suggestion(error, &suggestion)
}

/// PostgreSQL's error for `qualifier.name` (or `name`) missing from the one
/// relation an expression can read, such as a CHECK constraint's table or
/// COPY's target: positioned at the reference during analysis, with the
/// relation's closest column as the hint.
pub(crate) fn missing_relation_column(
    qualifier: Option<&str>,
    name: &str,
    relation: &str,
    columns: &[crate::storage::ColumnMeta],
) -> crate::sql::eval::SqlError {
    use crate::sql::eval::sqlstate;
    let error = match qualifier {
        Some(qualifier) => crate::sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column {}.{} does not exist",
            qualifier,
            name
        ),
        None => crate::sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" does not exist",
            name
        ),
    };
    from_relation(
        crate::sql::error_position::at_reference(error, qualifier.unwrap_or(name)),
        qualifier,
        name,
        relation,
        columns,
    )
}

/// Levenshtein distance in characters with unit costs, as PostgreSQL's
/// `varstr_levenshtein`. Names beyond the identifier limit never match.
fn levenshtein(source: &str, target: &str) -> usize {
    let target_len = target.chars().count();
    if source.chars().count() > MAX_NAME_CHARS || target_len > MAX_NAME_CHARS {
        return usize::MAX / 2;
    }
    let mut previous = [0usize; MAX_NAME_CHARS + 1];
    let mut current = [0usize; MAX_NAME_CHARS + 1];
    for (index, cell) in previous.iter_mut().enumerate().take(target_len + 1) {
        *cell = index;
    }
    for (row, source_char) in source.chars().enumerate() {
        current[0] = row + 1;
        for (column, target_char) in target.chars().enumerate() {
            let substitution = previous[column] + usize::from(source_char != target_char);
            current[column + 1] = substitution
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        previous[..=target_len].copy_from_slice(&current[..=target_len]);
    }
    previous[target_len]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_counts_character_edits() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("ä", "a"), 1);
    }

    #[test]
    fn suggests_the_closest_or_two_tied_columns() {
        let mut one = ColumnSuggestion::new(None, "abd");
        one.relation("t", ["abc", "xyz"]);
        assert_eq!(
            one.hint().unwrap().as_str(),
            "Perhaps you meant to reference the column \"t.abc\"."
        );
        let mut two = ColumnSuggestion::new(None, "abd");
        two.relation("t", ["abc", "abe"]);
        assert!(
            two.hint()
                .unwrap()
                .as_str()
                .contains("or the column \"t.abe\"")
        );
        let mut three = ColumnSuggestion::new(None, "abd");
        three.relation("t", ["abc", "abe", "abf"]);
        assert!(three.hint().is_none());
        let mut penalised = ColumnSuggestion::new(Some("t1"), "b");
        penalised.relation("t1", ["a"]);
        penalised.relation("t2", ["b"]);
        assert_eq!(
            penalised.hint().unwrap().as_str(),
            "Perhaps you meant to reference the column \"t2.b\"."
        );
    }
}
