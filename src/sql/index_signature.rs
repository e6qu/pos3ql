//! Lossy token summaries used to skip immutable inverted-index subtrees.

use core::hash::Hasher as _;

use crate::mem::fixed_map::Fnv1aHasher;
use crate::store::{
    INTERVAL_KEY_BYTES, IntervalSummary, NavigationSummary, POSTING_KEY_BYTES, TokenSignature,
};

use super::ast::BinaryOp;
use super::types::Datum;

const ARRAY_ELEMENT: u8 = 1;
const TEXT_LEXEME: u8 = 2;
const JSON_KEY: u8 = 3;
const JSON_STRING: u8 = 4;
const JSON_LITERAL: u8 = 5;
const JSON_EXISTS: u8 = 6;

/// Exact lossy-token identity used by immutable GIN posting generations.
/// Hash collisions only add candidates because SQL and MVCC rechecks remain
/// authoritative.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PostingToken {
    namespace: u8,
    hash: u64,
}

impl PostingToken {
    pub(crate) const KEY_BYTES: usize = POSTING_KEY_BYTES;

    const fn new(namespace: u8, hash: u64) -> Self {
        Self { namespace, hash }
    }

    pub(crate) fn encode(self) -> [u8; Self::KEY_BYTES] {
        let mut encoded = [0; Self::KEY_BYTES];
        encoded[0] = self.namespace;
        encoded[1..].copy_from_slice(&self.hash.to_be_bytes());
        encoded
    }

    pub(crate) fn decode(encoded: &[u8]) -> Option<Self> {
        (encoded.len() == Self::KEY_BYTES).then(|| Self {
            namespace: encoded[0],
            hash: u64::from_be_bytes(encoded[1..].try_into().expect("posting hash width")),
        })
    }

    pub(crate) const fn hash(self) -> u64 {
        self.hash
    }

    pub(crate) fn summary(self) -> NavigationSummary {
        let encoded = self.encode();
        let mut key = [0; INTERVAL_KEY_BYTES];
        key[..Self::KEY_BYTES].copy_from_slice(&encoded);
        NavigationSummary::Interval(IntervalSummary::bounded(key, key))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PostingProbe {
    Never,
    Candidates,
    Unusable,
}

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

pub(crate) fn for_each_value_token(datum: Datum<'_>, mut emit: impl FnMut(PostingToken)) {
    match datum {
        Datum::Null => {}
        Datum::Array { element, raw } => {
            for index in 0..super::array::len(raw) {
                let value = super::array::get(raw, element, index).unwrap_or(Datum::Null);
                if !value.is_null() {
                    emit(PostingToken::new(
                        ARRAY_ELEMENT,
                        super::eval::hash_key(&[value], &[0]),
                    ));
                }
            }
        }
        Datum::TsVector(vector) => {
            super::full_text::for_each_vector_lexeme_hash(vector.as_str(), |hash| {
                emit(PostingToken::new(TEXT_LEXEME, hash));
            });
        }
        Datum::Json { text, jsonb: true } => json_tokens(text, emit),
        _ => {}
    }
}

fn datum_signature(datum: Datum<'_>) -> NavigationSummary {
    if datum.is_null() {
        return NavigationSummary::Empty;
    }
    if !matches!(
        datum,
        Datum::Array { .. } | Datum::TsVector(_) | Datum::Json { jsonb: true, .. }
    ) {
        return NavigationSummary::Unbounded;
    }
    let mut signature = TokenSignature::default();
    for_each_value_token(datum, |token| signature.insert(token.namespace, token.hash));
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

/// Emits posting tokens whose union is a conservative candidate set for a
/// GIN predicate. A conjunction needs only one positive exact token, while a
/// disjunction needs a token restriction for every branch. Prefix and negated
/// branches therefore participate only when another conjunct restricts them.
pub(crate) fn posting_probe(
    search: Datum<'_>,
    operator: BinaryOp,
    mut emit: impl FnMut(PostingToken),
) -> PostingProbe {
    let mut emitted = 0usize;
    match search {
        Datum::Array { element, raw }
            if matches!(operator, BinaryOp::JsonExistsAny | BinaryOp::JsonExistsAll) =>
        {
            for index in 0..super::array::len(raw) {
                let Some(Datum::Text(text) | Datum::Bpchar(text)) =
                    super::array::get(raw, element, index)
                else {
                    continue;
                };
                let token = PostingToken::new(JSON_EXISTS, hash_bytes(text.as_bytes()));
                if operator == BinaryOp::JsonExistsAny || emitted == 0 {
                    emit(token);
                    emitted += 1;
                }
            }
            match (operator, emitted) {
                (BinaryOp::JsonExistsAny, 0) => PostingProbe::Never,
                (BinaryOp::JsonExistsAll, 0) => PostingProbe::Unusable,
                _ => PostingProbe::Candidates,
            }
        }
        Datum::Array { element, raw } => {
            if operator == BinaryOp::ContainedBy {
                return PostingProbe::Unusable;
            }
            for index in 0..super::array::len(raw) {
                let value = super::array::get(raw, element, index).unwrap_or(Datum::Null);
                if value.is_null() {
                    continue;
                }
                let token = PostingToken::new(ARRAY_ELEMENT, super::eval::hash_key(&[value], &[0]));
                if operator == BinaryOp::Overlaps || emitted == 0 {
                    emit(token);
                    emitted += 1;
                }
            }
            match (operator, emitted) {
                (BinaryOp::Overlaps, 0) => PostingProbe::Never,
                (BinaryOp::Contains | BinaryOp::Eq, 0) => PostingProbe::Unusable,
                (BinaryOp::Overlaps | BinaryOp::Contains | BinaryOp::Eq, _) => {
                    PostingProbe::Candidates
                }
                _ => PostingProbe::Unusable,
            }
        }
        Datum::TsQuery(query) if operator == BinaryOp::TextSearchMatch => {
            let Some(indexable) = super::full_text::fold_query_lexemes(
                query.as_str(),
                |_, prefix| !prefix,
                false,
                |left, right| left || right,
                |left, right| left && right,
            ) else {
                return PostingProbe::Never;
            };
            if !indexable {
                return PostingProbe::Unusable;
            }
            let _ = super::full_text::fold_query_lexemes(
                query.as_str(),
                |hash, prefix| {
                    if !prefix {
                        emit(PostingToken::new(TEXT_LEXEME, hash));
                        emitted += 1;
                    }
                },
                (),
                |_, _| (),
                |_, _| (),
            );
            if emitted == 0 {
                PostingProbe::Unusable
            } else {
                PostingProbe::Candidates
            }
        }
        Datum::Json { text, jsonb: true } if operator == BinaryOp::Contains => {
            // Every emitted JSON token is required by containment. Prefer the
            // final token because canonical objects emit a key before its
            // value, and values are commonly more selective than shared keys.
            let mut selected = None;
            json_tokens(text, |token| selected = Some(token));
            if let Some(token) = selected {
                emit(token);
                PostingProbe::Candidates
            } else {
                PostingProbe::Unusable
            }
        }
        Datum::Text(text) if operator == BinaryOp::JsonExists => {
            emit(PostingToken::new(JSON_EXISTS, hash_bytes(text.as_bytes())));
            PostingProbe::Candidates
        }
        _ => PostingProbe::Unusable,
    }
}

#[derive(Clone, Copy)]
struct QueryRequirement {
    all: TokenSignature,
    any: TokenSignature,
    has_any: bool,
}

fn json_signature(source: &str, signature: &mut TokenSignature) {
    json_tokens(source, |token| {
        signature.insert(token.namespace, token.hash)
    });
}

fn json_tokens(source: &str, mut emit: impl FnMut(PostingToken)) {
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
                    emit(PostingToken::new(JSON_KEY, hash));
                    if depth == 1 {
                        emit(PostingToken::new(JSON_EXISTS, hash));
                    }
                } else {
                    emit(PostingToken::new(JSON_STRING, hash));
                    if depth == 0 || top_array && depth == 1 {
                        emit(PostingToken::new(JSON_EXISTS, hash));
                    }
                }
            }
            b't' if bytes[at..].starts_with(b"true") => {
                emit(PostingToken::new(JSON_LITERAL, hash_bytes(b"true")));
                at += 4;
            }
            b'f' if bytes[at..].starts_with(b"false") => {
                emit(PostingToken::new(JSON_LITERAL, hash_bytes(b"false")));
                at += 5;
            }
            b'n' if bytes[at..].starts_with(b"null") => {
                emit(PostingToken::new(JSON_LITERAL, hash_bytes(b"null")));
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

    #[test]
    fn posting_probes_cover_positive_boolean_branches_without_false_negatives() {
        let mut tokens = [PostingToken::new(0, 0); 8];
        let mut count = 0usize;
        let probe = posting_probe(
            Datum::TsQuery(restore_query("'alpha' | 'beta'")),
            BinaryOp::TextSearchMatch,
            |token| {
                tokens[count] = token;
                count += 1;
            },
        );
        assert_eq!(probe, PostingProbe::Candidates);
        assert_eq!(count, 2);

        count = 0;
        assert_eq!(
            posting_probe(
                Datum::TsQuery(restore_query("'alpha' & !'beta'")),
                BinaryOp::TextSearchMatch,
                |token| {
                    tokens[count] = token;
                    count += 1;
                },
            ),
            PostingProbe::Candidates
        );
        assert_eq!(count, 1);
        assert_eq!(
            posting_probe(
                Datum::TsQuery(restore_query("!'alpha'")),
                BinaryOp::TextSearchMatch,
                |_| panic!("negation has no safe posting token"),
            ),
            PostingProbe::Unusable
        );
        assert_eq!(
            posting_probe(
                Datum::TsQuery(restore_query("'alpha':*")),
                BinaryOp::TextSearchMatch,
                |_| panic!("prefixes have no exact posting token"),
            ),
            PostingProbe::Unusable
        );
    }

    #[test]
    fn value_and_probe_tokens_share_exact_stable_encodings() {
        let document = Datum::Json {
            text: r#"{"escaped\nkey":"needle"}"#,
            jsonb: true,
        };
        let mut indexed = [PostingToken::new(0, 0); 8];
        let mut count = 0usize;
        for_each_value_token(document, |token| {
            indexed[count] = token;
            count += 1;
        });
        let mut probe = None;
        assert_eq!(
            posting_probe(Datum::Text("escaped\nkey"), BinaryOp::JsonExists, |token| {
                probe = Some(token)
            },),
            PostingProbe::Candidates
        );
        let probe = probe.expect("exists probe emits one token");
        assert!(indexed[..count].contains(&probe));
        assert_eq!(PostingToken::decode(&probe.encode()), Some(probe));
        assert!(matches!(probe.summary(), NavigationSummary::Interval(_)));
    }
}
