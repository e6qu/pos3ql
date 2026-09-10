//! PostgreSQL 18's Unicode 16 normalization, case-folding, and escape rules.

use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, arena_full, sqlstate};
use crate::sql_err;

use super::unicode_data::{
    ALPHANUMERIC_RANGES, ASSIGNED_RANGES, CASE_FOLDS, CASE_IGNORABLE_RANGES, CASE_LOWER,
    CASE_TITLE, CASE_UPPER, CASED_RANGES, COMPOSITIONS, DECOMPOSITION_CODEPOINTS, DECOMPOSITIONS,
    Decomposition,
};

const DECOMP_SIZE: u8 = 0x1f;
const DECOMP_COMPAT: u8 = 0x20;
const DECOMP_INLINE: u8 = 0x40;

const SBASE: u32 = 0xac00;
const LBASE: u32 = 0x1100;
const VBASE: u32 = 0x1161;
const TBASE: u32 = 0x11a7;
const LCOUNT: u32 = 19;
const VCOUNT: u32 = 21;
const TCOUNT: u32 = 28;
const SCOUNT: u32 = LCOUNT * VCOUNT * TCOUNT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NormalizationForm {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

#[derive(Clone, Copy)]
pub(crate) enum CaseKind {
    Lower,
    Title,
    Upper,
}

type CaseTable = &'static [(u32, [u32; 3], u8)];

fn mapped_codepoints(codepoint: u32, table: CaseTable) -> ([u32; 3], u8) {
    table
        .binary_search_by_key(&codepoint, |entry| entry.0)
        .ok()
        .map_or(([codepoint, 0, 0], 1), |index| {
            (table[index].1, table[index].2)
        })
}

pub(crate) fn case_mapping(character: char, kind: CaseKind, final_sigma: bool) -> ([u32; 3], u8) {
    if character == 'Σ' && matches!(kind, CaseKind::Lower) && final_sigma {
        return (['ς' as u32, 0, 0], 1);
    }
    let table = match kind {
        CaseKind::Lower => &CASE_LOWER[..],
        CaseKind::Title => &CASE_TITLE[..],
        CaseKind::Upper => &CASE_UPPER[..],
    };
    mapped_codepoints(character as u32, table)
}

impl NormalizationForm {
    pub(crate) fn parse(value: &str) -> Result<Self, SqlError> {
        if value.eq_ignore_ascii_case("NFC") {
            Ok(Self::Nfc)
        } else if value.eq_ignore_ascii_case("NFD") {
            Ok(Self::Nfd)
        } else if value.eq_ignore_ascii_case("NFKC") {
            Ok(Self::Nfkc)
        } else if value.eq_ignore_ascii_case("NFKD") {
            Ok(Self::Nfkd)
        } else {
            Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid normalization form: {}",
                value
            ))
        }
    }

    const fn compatibility(self) -> bool {
        matches!(self, Self::Nfkc | Self::Nfkd)
    }

    const fn compose(self) -> bool {
        matches!(self, Self::Nfc | Self::Nfkc)
    }
}

fn decomposition(codepoint: u32) -> Option<&'static Decomposition> {
    DECOMPOSITIONS
        .binary_search_by_key(&codepoint, |entry| entry.codepoint)
        .ok()
        .map(|index| &DECOMPOSITIONS[index])
}

fn decomposition_values(entry: &Decomposition) -> &[u32] {
    let size = usize::from(entry.flags & DECOMP_SIZE);
    if entry.flags & DECOMP_INLINE != 0 {
        // Inline entries are consumed separately so their u16 payload need not
        // masquerade as a static slice.
        &[]
    } else {
        let start = usize::from(entry.index);
        &DECOMPOSITION_CODEPOINTS[start..start + size]
    }
}

fn decomposed_size(codepoint: u32, compatibility: bool) -> Result<usize, SqlError> {
    if (SBASE..SBASE + SCOUNT).contains(&codepoint) {
        return Ok(if (codepoint - SBASE).is_multiple_of(TCOUNT) {
            2
        } else {
            3
        });
    }
    let Some(entry) = decomposition(codepoint) else {
        return Ok(1);
    };
    let size = entry.flags & DECOMP_SIZE;
    if size == 0 || (!compatibility && entry.flags & DECOMP_COMPAT != 0) {
        return Ok(1);
    }
    if entry.flags & DECOMP_INLINE != 0 {
        return decomposed_size(u32::from(entry.index), compatibility);
    }
    decomposition_values(entry)
        .iter()
        .try_fold(0usize, |sum, value| {
            sum.checked_add(decomposed_size(*value, compatibility)?)
                .ok_or_else(|| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "string is too long"))
        })
}

fn decompose_into(codepoint: u32, compatibility: bool, output: &mut [u32], at: &mut usize) {
    if (SBASE..SBASE + SCOUNT).contains(&codepoint) {
        let index = codepoint - SBASE;
        output[*at] = LBASE + index / (VCOUNT * TCOUNT);
        *at += 1;
        output[*at] = VBASE + (index % (VCOUNT * TCOUNT)) / TCOUNT;
        *at += 1;
        let trailing = index % TCOUNT;
        if trailing != 0 {
            output[*at] = TBASE + trailing;
            *at += 1;
        }
        return;
    }
    let Some(entry) = decomposition(codepoint) else {
        output[*at] = codepoint;
        *at += 1;
        return;
    };
    let size = entry.flags & DECOMP_SIZE;
    if size == 0 || (!compatibility && entry.flags & DECOMP_COMPAT != 0) {
        output[*at] = codepoint;
        *at += 1;
    } else if entry.flags & DECOMP_INLINE != 0 {
        decompose_into(u32::from(entry.index), compatibility, output, at);
    } else {
        for value in decomposition_values(entry) {
            decompose_into(*value, compatibility, output, at);
        }
    }
}

fn combining_class(codepoint: u32) -> u8 {
    decomposition(codepoint).map_or(0, |entry| entry.combining_class)
}

fn compose_pair(starter: u32, codepoint: u32) -> Option<u32> {
    if (LBASE..LBASE + LCOUNT).contains(&starter) && (VBASE..VBASE + VCOUNT).contains(&codepoint) {
        return Some(SBASE + ((starter - LBASE) * VCOUNT + codepoint - VBASE) * TCOUNT);
    }
    if (SBASE..SBASE + SCOUNT).contains(&starter)
        && (starter - SBASE).is_multiple_of(TCOUNT)
        && (TBASE + 1..TBASE + TCOUNT).contains(&codepoint)
    {
        return Some(starter + codepoint - TBASE);
    }
    let key = (u64::from(starter) << 32) | u64::from(codepoint);
    COMPOSITIONS
        .binary_search_by_key(&key, |entry| entry.0)
        .ok()
        .map(|index| COMPOSITIONS[index].1)
}

fn encode_codepoints<'a>(codepoints: &[u32], arena: &'a Arena) -> Result<&'a str, SqlError> {
    let size = codepoints.iter().try_fold(0usize, |sum, codepoint| {
        let character = char::from_u32(*codepoint).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid generated Unicode code point"
            )
        })?;
        sum.checked_add(character.len_utf8())
            .ok_or_else(|| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "string is too long"))
    })?;
    let output = arena
        .alloc_slice_with(size, |_| 0u8)
        .map_err(|_| arena_full())?;
    let mut at = 0;
    for codepoint in codepoints {
        let character = char::from_u32(*codepoint).expect("validated generated Unicode code point");
        at += character.encode_utf8(&mut output[at..]).len();
    }
    Ok(unsafe { core::str::from_utf8_unchecked(output) })
}

pub(crate) fn normalize<'a>(
    input: &str,
    form: NormalizationForm,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let compatibility = form.compatibility();
    let count = input.chars().try_fold(0usize, |sum, character| {
        sum.checked_add(decomposed_size(character as u32, compatibility)?)
            .ok_or_else(|| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "string is too long"))
    })?;
    let decomposed = arena
        .alloc_slice_with(count, |_| 0u32)
        .map_err(|_| arena_full())?;
    let mut at = 0;
    for character in input.chars() {
        decompose_into(character as u32, compatibility, decomposed, &mut at);
    }

    // Canonical ordering is an insertion sort within each starter segment.
    for index in 1..decomposed.len() {
        let mut current = index;
        while current > 0 {
            let previous_class = combining_class(decomposed[current - 1]);
            let current_class = combining_class(decomposed[current]);
            if previous_class == 0 || current_class == 0 || previous_class <= current_class {
                break;
            }
            decomposed.swap(current - 1, current);
            current -= 1;
        }
    }

    let output_len = if form.compose() && !decomposed.is_empty() {
        let mut last_class = -1i16;
        let mut starter_position = 0usize;
        let mut target_position = 1usize;
        let mut starter = decomposed[0];
        for index in 1..decomposed.len() {
            let codepoint = decomposed[index];
            let class = i16::from(combining_class(codepoint));
            if last_class < class
                && let Some(composite) = compose_pair(starter, codepoint)
            {
                decomposed[starter_position] = composite;
                starter = composite;
            } else if class == 0 {
                starter_position = target_position;
                starter = codepoint;
                last_class = -1;
                decomposed[target_position] = codepoint;
                target_position += 1;
            } else {
                last_class = class;
                decomposed[target_position] = codepoint;
                target_position += 1;
            }
        }
        target_position
    } else {
        decomposed.len()
    };
    encode_codepoints(&decomposed[..output_len], arena)
}

pub(crate) fn is_normalized(
    input: &str,
    form: NormalizationForm,
    arena: &Arena,
) -> Result<bool, SqlError> {
    Ok(normalize(input, form, arena)? == input)
}

pub(crate) fn casefold<'a>(input: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let output_size = input.chars().try_fold(0usize, |sum, character| {
        let (mapped, count) = mapped_codepoints(character as u32, &CASE_FOLDS);
        let size = mapped[..usize::from(count)]
            .iter()
            .map(|value| char::from_u32(*value).expect("valid case map").len_utf8())
            .sum::<usize>();
        sum.checked_add(size)
            .ok_or_else(|| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "string is too long"))
    })?;
    let output = arena
        .alloc_slice_with(output_size, |_| 0u8)
        .map_err(|_| arena_full())?;
    let mut at = 0;
    for character in input.chars() {
        let (mapped, count) = mapped_codepoints(character as u32, &CASE_FOLDS);
        for codepoint in &mapped[..usize::from(count)] {
            let mapped = char::from_u32(*codepoint).expect("valid case map");
            at += mapped.encode_utf8(&mut output[at..]).len();
        }
    }
    Ok(unsafe { core::str::from_utf8_unchecked(output) })
}

fn in_ranges(character: char, ranges: &[(u32, u32)]) -> bool {
    let codepoint = character as u32;
    ranges
        .binary_search_by(|(first, last)| {
            if codepoint < *first {
                core::cmp::Ordering::Greater
            } else if codepoint > *last {
                core::cmp::Ordering::Less
            } else {
                core::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

pub(crate) fn assigned(character: char) -> bool {
    in_ranges(character, &ASSIGNED_RANGES)
}

pub(crate) fn cased(character: char) -> bool {
    in_ranges(character, &CASED_RANGES)
}

pub(crate) fn alphanumeric(character: char) -> bool {
    in_ranges(character, &ALPHANUMERIC_RANGES)
}

pub(crate) fn case_ignorable(character: char) -> bool {
    in_ranges(character, &CASE_IGNORABLE_RANGES)
}

fn parse_hex(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |value, byte| {
        value * 16
            + u32::from(match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => unreachable!("hex input was checked"),
            })
    })
}

fn hex_digits(bytes: &[u8]) -> bool {
    bytes.iter().all(u8::is_ascii_hexdigit)
}

pub(crate) fn unistr<'a>(input: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let bytes = input.as_bytes();
    let output = arena
        .alloc_slice_with(bytes.len(), |_| 0u8)
        .map_err(|_| arena_full())?;
    let mut source = 0usize;
    let mut target = 0usize;
    let mut high_surrogate: Option<u32> = None;
    while source < bytes.len() {
        if bytes[source] != b'\\' {
            if high_surrogate.is_some() {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "invalid Unicode surrogate pair"
                ));
            }
            output[target] = bytes[source];
            source += 1;
            target += 1;
            continue;
        }
        if bytes.get(source + 1) == Some(&b'\\') {
            if high_surrogate.is_some() {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "invalid Unicode surrogate pair"
                ));
            }
            output[target] = b'\\';
            source += 2;
            target += 1;
            continue;
        }
        let (digits_at, digits) = if bytes.get(source + 1) == Some(&b'u') {
            (source + 2, 4)
        } else if bytes.get(source + 1) == Some(&b'+') {
            (source + 2, 6)
        } else if bytes.get(source + 1) == Some(&b'U') {
            (source + 2, 8)
        } else {
            (source + 1, 4)
        };
        let Some(hex) = bytes.get(digits_at..digits_at + digits) else {
            return Err(sql_err!(sqlstate::SYNTAX_ERROR, "invalid Unicode escape"));
        };
        if !hex_digits(hex) {
            return Err(sql_err!(sqlstate::SYNTAX_ERROR, "invalid Unicode escape"));
        }
        let mut codepoint = parse_hex(hex);
        source = digits_at + digits;
        if codepoint > 0x10ffff {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid Unicode code point: {:04X}",
                codepoint
            ));
        }
        if let Some(high) = high_surrogate.take() {
            if !(0xdc00..=0xdfff).contains(&codepoint) {
                return Err(sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "invalid Unicode surrogate pair"
                ));
            }
            codepoint = 0x10000 + ((high - 0xd800) << 10) + codepoint - 0xdc00;
        } else if (0xdc00..=0xdfff).contains(&codepoint) {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "invalid Unicode surrogate pair"
            ));
        }
        if (0xd800..=0xdbff).contains(&codepoint) {
            high_surrogate = Some(codepoint);
            continue;
        }
        if codepoint == 0 {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid Unicode code point: 0000"
            ));
        }
        let character = char::from_u32(codepoint).ok_or_else(|| {
            sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "invalid Unicode code point: {:04X}",
                codepoint
            )
        })?;
        target += character.encode_utf8(&mut output[target..]).len();
    }
    if high_surrogate.is_some() {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "invalid Unicode surrogate pair"
        ));
    }
    Ok(unsafe { core::str::from_utf8_unchecked(&output[..target]) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    fn arena() -> Arena {
        Arena::new(&mut Budget::new(1 << 20), "unicode test", 1 << 20).unwrap()
    }

    #[test]
    fn normalization_forms_and_hangul_match_unicode() {
        let arena = arena();
        assert_eq!(
            normalize("a\u{308}", NormalizationForm::Nfc, &arena).unwrap(),
            "ä"
        );
        assert_eq!(
            normalize("ä", NormalizationForm::Nfd, &arena).unwrap(),
            "a\u{308}"
        );
        assert_eq!(
            normalize("①", NormalizationForm::Nfkc, &arena).unwrap(),
            "1"
        );
        assert_eq!(
            normalize("각", NormalizationForm::Nfd, &arena).unwrap(),
            "각"
        );
        assert_eq!(
            normalize("각", NormalizationForm::Nfc, &arena).unwrap(),
            "각"
        );
    }

    #[test]
    fn casefold_expands_and_unistr_pairs_surrogates() {
        let arena = arena();
        assert_eq!(
            casefold("Straße İ Σς", &arena).unwrap(),
            "strasse i\u{307} σσ"
        );
        assert_eq!(unistr(r"d\0061t\+000061", &arena).unwrap(), "data");
        assert_eq!(unistr(r"\D83D\DE00", &arena).unwrap(), "😀");
    }
}
