//! Lossy token summaries used to skip immutable inverted-index subtrees.

use core::hash::Hasher as _;

use crate::mem::fixed_map::Fnv1aHasher;
use crate::store::{NavigationSummary, TokenSignature};

use super::ast::BinaryOp;
use super::types::Datum;

const ARRAY_ELEMENT: u8 = 1;
const TEXT_LEXEME: u8 = 2;
const JSON_KEY: u8 = 3;
const JSON_STRING: u8 = 4;
const JSON_LITERAL: u8 = 5;
const JSON_EXISTS: u8 = 6;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SignaturePredicate {
    Always,
    Never,
    Required {
        all: TokenSignature,
        any: TokenSignature,
        has_any: bool,
    },
}

impl SignaturePredicate {
    pub(crate) fn may_match(self, summary: NavigationSummary) -> bool {
        match (self, summary) {
            (Self::Never, _) | (_, NavigationSummary::Empty) => false,
            (Self::Always, _) | (_, NavigationSummary::Unbounded) => true,
            (Self::Required { all, any, has_any }, NavigationSummary::Signature(available)) => {
                available.contains(all) && (!has_any || available.intersects(any))
            }
            // A mismatched summary kind is corruption at construction time,
            // but retaining it here keeps this lossy filter one-sided.
            _ => true,
        }
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = Fnv1aHasher::default();
    hasher.write(bytes);
    hasher.finish()
}

fn datum_signature(datum: Datum<'_>) -> NavigationSummary {
    let mut signature = TokenSignature::default();
    match datum {
        Datum::Null => return NavigationSummary::Empty,
        Datum::Array { element, raw } => {
            for index in 0..super::array::len(raw) {
                let value = super::array::get(raw, element, index).unwrap_or(Datum::Null);
                if !value.is_null() {
                    signature.insert(ARRAY_ELEMENT, super::eval::hash_key(&[value], &[0]));
                }
            }
        }
        Datum::TsVector(vector) => {
            super::full_text::for_each_vector_lexeme_hash(vector.as_str(), |hash| {
                signature.insert(TEXT_LEXEME, hash);
            });
        }
        Datum::Json { text, jsonb: true } => json_signature(text, &mut signature),
        _ => return NavigationSummary::Unbounded,
    }
    NavigationSummary::Signature(signature)
}

pub(crate) fn summary(datum: Datum<'_>) -> NavigationSummary {
    datum_signature(datum)
}

fn required(all: TokenSignature, any: TokenSignature, has_any: bool) -> SignaturePredicate {
    if all.is_empty() && !has_any {
        SignaturePredicate::Always
    } else {
        SignaturePredicate::Required { all, any, has_any }
    }
}

pub(crate) fn predicate(search: Datum<'_>, operator: BinaryOp) -> SignaturePredicate {
    match search {
        Datum::Array { element, raw }
            if matches!(operator, BinaryOp::JsonExistsAny | BinaryOp::JsonExistsAll) =>
        {
            let mut tokens = TokenSignature::default();
            let mut count = 0usize;
            for index in 0..super::array::len(raw) {
                if let Some(Datum::Text(text) | Datum::Bpchar(text)) =
                    super::array::get(raw, element, index)
                {
                    tokens.insert(JSON_EXISTS, hash_bytes(text.as_bytes()));
                    count += 1;
                }
            }
            match operator {
                BinaryOp::JsonExistsAny if count == 0 => SignaturePredicate::Never,
                BinaryOp::JsonExistsAny => required(TokenSignature::default(), tokens, true),
                BinaryOp::JsonExistsAll if count != 0 => {
                    required(tokens, TokenSignature::default(), false)
                }
                _ => SignaturePredicate::Always,
            }
        }
        Datum::Array { element, raw } => {
            if operator == BinaryOp::ContainedBy {
                SignaturePredicate::Always
            } else {
                let mut tokens = TokenSignature::default();
                let mut count = 0usize;
                for index in 0..super::array::len(raw) {
                    let value = super::array::get(raw, element, index).unwrap_or(Datum::Null);
                    if !value.is_null() {
                        tokens.insert(ARRAY_ELEMENT, super::eval::hash_key(&[value], &[0]));
                        count += 1;
                    }
                }
                match operator {
                    BinaryOp::Overlaps if count == 0 => SignaturePredicate::Never,
                    BinaryOp::Overlaps => required(TokenSignature::default(), tokens, true),
                    BinaryOp::Contains | BinaryOp::Eq if count != 0 => {
                        required(tokens, TokenSignature::default(), false)
                    }
                    _ => SignaturePredicate::Always,
                }
            }
        }
        Datum::TsQuery(query) if operator == BinaryOp::TextSearchMatch => {
            let none = QueryRequirement {
                all: TokenSignature::default(),
                any: TokenSignature::default(),
                has_any: false,
            };
            let Some(requirement) = super::full_text::fold_query_lexemes(
                query.as_str(),
                |hash, prefix| {
                    if prefix {
                        none
                    } else {
                        let mut token = TokenSignature::default();
                        token.insert(TEXT_LEXEME, hash);
                        QueryRequirement {
                            all: token,
                            any: token,
                            has_any: true,
                        }
                    }
                },
                none,
                |left, right| QueryRequirement {
                    all: left.all.union(right.all),
                    any: left.any.union(right.any),
                    has_any: left.has_any || right.has_any,
                },
                |left, right| QueryRequirement {
                    all: left.all.intersection(right.all),
                    any: left.any.union(right.any),
                    has_any: left.has_any && right.has_any,
                },
            ) else {
                return SignaturePredicate::Never;
            };
            required(requirement.all, requirement.any, requirement.has_any)
        }
        Datum::Json { text, jsonb: true } if operator == BinaryOp::Contains => {
            let mut tokens = TokenSignature::default();
            json_signature(text, &mut tokens);
            required(tokens, TokenSignature::default(), false)
        }
        Datum::Text(text) if operator == BinaryOp::JsonExists => {
            let mut token = TokenSignature::default();
            token.insert(JSON_EXISTS, hash_bytes(text.as_bytes()));
            required(token, TokenSignature::default(), false)
        }
        _ => SignaturePredicate::Always,
    }
}

#[derive(Clone, Copy)]
struct QueryRequirement {
    all: TokenSignature,
    any: TokenSignature,
    has_any: bool,
}

fn json_signature(source: &str, signature: &mut TokenSignature) {
    let bytes = source.as_bytes();
    let top_array = bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'[');
    let mut depth = 0usize;
    let mut at = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'{' | b'[' => {
                depth += 1;
                at += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                at += 1;
            }
            b'"' => {
                let start = at + 1;
                at = start;
                while at < bytes.len() {
                    if bytes[at] == b'\\' {
                        at += 2;
                    } else if bytes[at] == b'"' {
                        break;
                    } else {
                        at += 1;
                    }
                }
                let raw = &bytes[start..at];
                let hash = hash_json_string(raw);
                at += 1;
                let is_key =
                    bytes[at..].iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b':');
                if is_key {
                    signature.insert(JSON_KEY, hash);
                    if depth == 1 {
                        signature.insert(JSON_EXISTS, hash);
                    }
                } else {
                    signature.insert(JSON_STRING, hash);
                    if depth == 0 || top_array && depth == 1 {
                        signature.insert(JSON_EXISTS, hash);
                    }
                }
            }
            b't' if bytes[at..].starts_with(b"true") => {
                signature.insert(JSON_LITERAL, hash_bytes(b"true"));
                at += 4;
            }
            b'f' if bytes[at..].starts_with(b"false") => {
                signature.insert(JSON_LITERAL, hash_bytes(b"false"));
                at += 5;
            }
            b'n' if bytes[at..].starts_with(b"null") => {
                signature.insert(JSON_LITERAL, hash_bytes(b"null"));
                at += 4;
            }
            _ => at += 1,
        }
    }
}

fn hash_json_string(raw: &[u8]) -> u64 {
    let mut hasher = Fnv1aHasher::default();
    let mut at = 0usize;
    while at < raw.len() {
        if raw[at] != b'\\' {
            hasher.write_u8(raw[at]);
            at += 1;
            continue;
        }
        at += 1;
        let escaped = raw[at];
        at += 1;
        match escaped {
            b'"' | b'\\' | b'/' => hasher.write_u8(escaped),
            b'b' => hasher.write_u8(8),
            b'f' => hasher.write_u8(12),
            b'n' => hasher.write_u8(b'\n'),
            b'r' => hasher.write_u8(b'\r'),
            b't' => hasher.write_u8(b'\t'),
            b'u' => {
                let mut scalar = json_hex(raw, at);
                at += 4;
                if (0xd800..=0xdbff).contains(&scalar) {
                    at += 2; // Canonical validated JSON has the following `\\u`.
                    let low = json_hex(raw, at);
                    at += 4;
                    scalar = 0x1_0000 + ((scalar - 0xd800) << 10) + (low - 0xdc00);
                }
                let mut encoded = [0u8; 4];
                hasher.write(
                    char::from_u32(scalar)
                        .expect("validated JSON scalar")
                        .encode_utf8(&mut encoded)
                        .as_bytes(),
                );
            }
            _ => unreachable!("canonical JSON escape"),
        }
    }
    hasher.finish()
}

fn json_hex(raw: &[u8], at: usize) -> u32 {
    raw[at..at + 4].iter().fold(0, |value, byte| {
        value * 16
            + u32::from(match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => unreachable!("canonical JSON hex escape"),
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::full_text::{restore_query, restore_vector};

    #[test]
    fn full_text_boolean_requirements_are_conservative() {
        let indexed = summary(Datum::TsVector(restore_vector("'alpha' 'beta'")));
        for query in ["'alpha'", "'alpha' & 'beta'", "'alpha' | 'missing'"] {
            assert!(
                predicate(
                    Datum::TsQuery(restore_query(query)),
                    BinaryOp::TextSearchMatch,
                )
                .may_match(indexed)
            );
        }
        assert!(
            !predicate(
                Datum::TsQuery(restore_query("'missing'")),
                BinaryOp::TextSearchMatch,
            )
            .may_match(indexed)
        );
    }

    #[test]
    fn json_strings_keys_and_escapes_share_tokens() {
        let indexed = summary(Datum::Json {
            text: r#"{"a\nb": "value", "nested": {"key": true}}"#,
            jsonb: true,
        });
        assert!(predicate(Datum::Text("a\nb"), BinaryOp::JsonExists).may_match(indexed));
        assert!(!predicate(Datum::Text("absent"), BinaryOp::JsonExists).may_match(indexed));
        let scalar = summary(Datum::Json {
            text: r#""scalar""#,
            jsonb: true,
        });
        assert!(predicate(Datum::Text("scalar"), BinaryOp::JsonExists).may_match(scalar));
        assert!(
            predicate(
                Datum::Json {
                    text: r#"{"nested": {"key": true}}"#,
                    jsonb: true,
                },
                BinaryOp::Contains,
            )
            .may_match(indexed)
        );
    }
}
