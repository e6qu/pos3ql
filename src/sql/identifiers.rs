//! Decode identifier spellings without changing Unicode bytes or truncating names.

use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::storage::SqlName;

#[derive(Clone, Copy)]
pub(crate) struct IdentifierSpelling<'a> {
    text: &'a str,
    quoted: bool,
}

impl IdentifierSpelling<'_> {
    pub(crate) fn decoded_len(self) -> usize {
        self.text.len()
            - if self.quoted {
                self.text.bytes().filter(|byte| *byte == b'"').count() / 2
            } else {
                0
            }
    }

    pub(crate) fn decode_into(self, output: &mut [u8]) -> Result<&str, SqlError> {
        let len = self.decoded_len();
        if len > output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "identifier output capacity is exhausted"
            ));
        }
        let bytes = self.text.as_bytes();
        let mut source = 0;
        let mut target = 0;
        while source < bytes.len() {
            let byte = bytes[source];
            output[target] = if self.quoted {
                byte
            } else {
                byte.to_ascii_lowercase()
            };
            source += if self.quoted && byte == b'"' { 2 } else { 1 };
            target += 1;
        }
        Ok(core::str::from_utf8(&output[..len]).expect("identifier decoding preserves UTF-8"))
    }

    pub(crate) fn sql_name(self) -> Result<SqlName, SqlError> {
        if self.decoded_len() > 63 {
            return Err(sql_err!(
                sqlstate::NAME_TOO_LONG,
                "identifier is longer than 63 bytes"
            ));
        }
        let mut output = [0_u8; 63];
        SqlName::parse(self.decode_into(&mut output)?)
    }
}

pub(crate) struct QualifiedObjectName {
    pub(crate) database: Option<SqlName>,
    pub(crate) schema: Option<SqlName>,
    pub(crate) name: SqlName,
}

pub(crate) enum ObjectReference {
    Oid(u32),
    Name(QualifiedObjectName),
}

impl ObjectReference {
    pub(crate) fn parse(input: &str) -> Result<Self, SqlError> {
        if input == "-" {
            return Ok(Self::Oid(0));
        }
        if !input.is_empty() && input.bytes().all(|byte| byte.is_ascii_digit()) {
            input.parse::<u32>().map(Self::Oid).map_err(|_| {
                sql_err!(
                    sqlstate::NUMERIC_OUT_OF_RANGE,
                    "object identifier is out of range"
                )
            })
        } else {
            QualifiedObjectName::parse(input).map(Self::Name)
        }
    }
}

impl QualifiedObjectName {
    pub(crate) fn parse(input: &str) -> Result<Self, SqlError> {
        let mut names = [SqlName::EMPTY; 3];
        let mut count = 0;
        for_each_qualified_identifier(input, IdentifierGrammar::ObjectReference, true, |part| {
            if count == names.len() {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "object name has too many parts"
                ));
            }
            names[count] = part.sql_name()?;
            count += 1;
            Ok(())
        })
        .map_err(|mut error| {
            if error.sqlstate == sqlstate::INVALID_PARAMETER_VALUE {
                error.sqlstate = crate::sql::eval::SqlState::known(sqlstate::INVALID_NAME);
            }
            error
        })?;
        Ok(match count {
            1 => Self {
                database: None,
                schema: None,
                name: names[0],
            },
            2 => Self {
                database: None,
                schema: Some(names[0]),
                name: names[1],
            },
            3 => Self {
                database: Some(names[0]),
                schema: Some(names[1]),
                name: names[2],
            },
            _ => unreachable!("the identifier parser admits one through three object name parts"),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentifierGrammar {
    Sql,
    ObjectReference,
}

pub(crate) fn for_each_qualified_identifier(
    input: &str,
    grammar: IdentifierGrammar,
    strict: bool,
    mut visit: impl FnMut(IdentifierSpelling<'_>) -> Result<(), SqlError>,
) -> Result<usize, SqlError> {
    let bad = || {
        sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "string is not a valid identifier: \"{}\"",
            input
        )
    };
    let bytes = input.as_bytes();
    let mut at = 0;
    let mut count = 0;
    loop {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        let start = at;
        let spelling = if bytes.get(at) == Some(&b'"') {
            at += 1;
            let inner = at;
            let end = loop {
                match bytes.get(at) {
                    Some(b'"') if bytes.get(at + 1) == Some(&b'"') => at += 2,
                    Some(b'"') => {
                        let end = at;
                        at += 1;
                        break end;
                    }
                    Some(_) => at += 1,
                    None => return Err(bad()),
                }
            };
            if end == inner && grammar == IdentifierGrammar::Sql {
                return Err(bad());
            }
            IdentifierSpelling {
                text: &input[inner..end],
                quoted: true,
            }
        } else {
            if grammar == IdentifierGrammar::Sql
                && !matches!(
                    bytes.get(at),
                    Some(b'_' | b'A'..=b'Z' | b'a'..=b'z' | 0x80..=0xff)
                )
            {
                return Err(bad());
            }
            while bytes.get(at).is_some_and(|byte| match grammar {
                IdentifierGrammar::Sql => {
                    byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'$' | 0x80..=0xff)
                }
                IdentifierGrammar::ObjectReference => {
                    *byte != b'.' && !byte.is_ascii_whitespace()
                }
            }) {
                at += 1;
            }
            if start == at {
                return Err(bad());
            }
            IdentifierSpelling {
                text: &input[start..at],
                quoted: false,
            }
        };
        visit(spelling)?;
        count += 1;
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        match bytes.get(at) {
            Some(b'.') => at += 1,
            None => return Ok(count),
            _ if !strict => return Ok(count),
            _ => return Err(bad()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_name_identifier_decoding_preserves_unicode_quotes_and_capacity() {
        let quoted = format!("\"{}\"", "é".repeat(600));
        let mut output = [0_u8; 1200];
        crate::mem::guard::forbid_alloc(|| {
            let mut names = [SqlName::EMPTY; 2];
            let mut count = 0;
            for_each_qualified_identifier(" \"élève\" . \"a\"\"b\" ", IdentifierGrammar::Sql, true, |part| {
                names[count] = part.sql_name()?;
                count += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(names[0].as_str(), "élève");
            assert_eq!(names[1].as_str(), "a\"b");
            for_each_qualified_identifier(&quoted, IdentifierGrammar::Sql, true, |part| {
                assert_eq!(part.decoded_len(), 1200);
                assert_eq!(
                    part.sql_name().unwrap_err().sqlstate,
                    sqlstate::NAME_TOO_LONG
                );
                assert_eq!(part.decode_into(&mut output)?.len(), 1200);
                assert_eq!(
                    part.decode_into(&mut output[..1199]).unwrap_err().sqlstate,
                    sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
                Ok(())
            })
            .unwrap();
            for invalid in ["", "\"\"", "x.", "x..y", "\"unterminated", "x y"] {
                assert!(
                    for_each_qualified_identifier(invalid, IdentifierGrammar::Sql, true, |_| Ok(())).is_err(),
                    "{invalid}"
                );
            }
            assert_eq!(
                for_each_qualified_identifier("UPPER ignored", IdentifierGrammar::Sql, false, |part| {
                    assert_eq!(part.sql_name()?.as_str(), "upper");
                    Ok(())
                })
                .unwrap(),
                1
            );
        });
    }
}
