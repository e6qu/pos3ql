//! `to_char(numeric, text)` number formatting, following PostgreSQL's
//! `NUM_processor`. Supports the common format codes — digit positions (`9`,
//! `0`), decimal point (`.`, `D`), group separators (`,`, `G`), the floating
//! sign (`S` and the implicit sign slot), a currency marker (`L`, `$`), fill
//! mode (`FM`), and the overflow `#` fill. Exotic codes (`MI`, `PL`, `SG`,
//! `PR`, `V`, `EEEE`, `RN`, `TH`) are rejected loudly rather than misformatted.

use super::eval::SqlError;
use super::numeric::{Numeric, RoundMode, Sign};
use crate::mem::arena::Arena;
use crate::util::StackStr;
use crate::{sql_err, stack_format};
use core::fmt::Write as _;

const MAX_TOKS: usize = 256;
const MAX_OUT: usize = 512;

#[derive(Clone, Copy, PartialEq)]
enum Tok {
    /// `9`: digit, leading zeros shown as blank.
    Nine,
    /// `0`: digit, zero-filled.
    Zero,
    /// `.` / `D`: decimal point.
    Point,
    /// `,` / `G`: group separator.
    Group,
    /// `L` / `$`: currency marker (`$` in the C locale).
    Currency,
    /// Any literal character emitted verbatim.
    Literal(u8),
}

/// Which sign convention the format selects.
#[derive(Clone, Copy, PartialEq)]
enum SignKind {
    /// Implicit slot: `-` for negatives, blank (or nothing under FM) otherwise.
    Default,
    /// `S`: `-`/`+` glued to the number.
    S,
}

/// Formats `value` per `fmt`, returning an arena-allocated string.
pub fn number<'a>(value: &Numeric, fmt: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let mut toks = [Tok::Nine; MAX_TOKS];
    let mut ntok = 0usize;
    let mut fm = false;
    let mut sign_kind = SignKind::Default;
    let mut sign_seen = false;
    let mut sign_trailing = false;
    let mut has_point = false;
    let mut int_digits = 0usize;
    let mut frac_digits = 0usize;
    let mut seen_digit = false;

    let bytes = fmt.as_bytes();
    let mut i = 0usize;
    let push = |toks: &mut [Tok; MAX_TOKS], ntok: &mut usize, t: Tok| -> Result<(), SqlError> {
        if *ntok >= MAX_TOKS {
            return Err(sql_err!("22023", "to_char format too long"));
        }
        toks[*ntok] = t;
        *ntok += 1;
        Ok(())
    };
    while i < bytes.len() {
        let c = bytes[i];
        let up = c.to_ascii_uppercase();
        // Two-character codes first.
        let two = if i + 1 < bytes.len() {
            [up, bytes[i + 1].to_ascii_uppercase()]
        } else {
            [up, 0]
        };
        match &two {
            b"FM" => {
                fm = true;
                i += 2;
                continue;
            }
            b"MI" | b"PL" | b"SG" | b"PR" | b"TH" | b"RN" => {
                return Err(sql_err!(
                    "0A000",
                    "to_char format code not supported: \"{}{}\"",
                    two[0] as char,
                    (two[1] as char).to_ascii_lowercase()
                ));
            }
            b"EE" => {
                return Err(sql_err!("0A000", "to_char format code not supported: \"EEEE\""));
            }
            _ => {}
        }
        match up {
            b'9' => {
                push(&mut toks, &mut ntok, Tok::Nine)?;
                if has_point {
                    frac_digits += 1;
                } else {
                    int_digits += 1;
                }
                seen_digit = true;
            }
            b'0' => {
                push(&mut toks, &mut ntok, Tok::Zero)?;
                if has_point {
                    frac_digits += 1;
                } else {
                    int_digits += 1;
                }
                seen_digit = true;
            }
            b'.' | b'D' => {
                if has_point {
                    return Err(sql_err!("22023", "cannot use \"{}\" twice", c as char));
                }
                has_point = true;
                push(&mut toks, &mut ntok, Tok::Point)?;
            }
            b',' | b'G' => push(&mut toks, &mut ntok, Tok::Group)?,
            b'L' | b'$' => push(&mut toks, &mut ntok, Tok::Currency)?,
            b'S' => {
                if sign_seen {
                    return Err(sql_err!("22023", "cannot use \"S\" twice"));
                }
                sign_seen = true;
                sign_kind = SignKind::S;
                sign_trailing = seen_digit;
            }
            b'V' => return Err(sql_err!("0A000", "to_char format code not supported: \"V\"")),
            // Punctuation and spaces are literal; unrecognized letters are a
            // loud gap rather than a silent mis-format.
            _ => {
                if c.is_ascii_alphabetic() {
                    return Err(sql_err!(
                        "0A000",
                        "to_char format code not supported: \"{}\"",
                        c as char
                    ));
                }
                push(&mut toks, &mut ntok, Tok::Literal(c))?;
            }
        }
        i += 1;
    }

    render(
        value,
        &toks[..ntok],
        fm,
        sign_kind,
        sign_trailing,
        has_point,
        int_digits,
        frac_digits,
        arena,
    )
}

#[allow(clippy::too_many_arguments)]
fn render<'a>(
    value: &Numeric,
    toks: &[Tok],
    fm: bool,
    sign_kind: SignKind,
    sign_trailing: bool,
    has_point: bool,
    int_digits: usize,
    frac_digits: usize,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    // Round to the number of fractional positions the format provides.
    let rounded = value.round_scale(frac_digits, RoundMode::HalfAwayZero, arena)?;
    let text = stack_format!(512, "{}", rounded);
    let s = text.as_str();
    let body = s.strip_prefix('-').unwrap_or(s);
    let (intpart, fracpart) = body.split_once('.').unwrap_or((body, ""));
    let neg = rounded.sign == Sign::Neg && !rounded.is_zero();
    let whole_zero = rounded.is_zero();

    let mut fracbuf = [b'0'; MAX_OUT];
    let fb = fracpart.as_bytes();
    for (k, slot) in fracbuf[..frac_digits].iter_mut().enumerate() {
        *slot = *fb.get(k).unwrap_or(&b'0');
    }
    let fracstr = &fracbuf[..frac_digits];

    // An all-zero integer part never overflows, so the trimmed integer digits
    // decide it independently of the whole-zero display rule below.
    let int_trimmed = intpart.trim_start_matches('0');
    let overflow = int_trimmed.len() > int_digits;

    // How many trailing fractional positions to emit (fill mode trims trailing
    // zeros that sit on `9` positions, keeping `0` positions and the point);
    // overflow keeps every position as `#`, so no trimming there.
    let mut frac_emit = frac_digits;
    if fm && !overflow {
        let mut p = toks.len();
        let mut fi = frac_digits;
        while p > 0 && fi > 0 {
            p -= 1;
            match toks[p] {
                Tok::Nine => {
                    fi -= 1;
                    if fracstr[fi] == b'0' {
                        frac_emit -= 1;
                    } else {
                        break;
                    }
                }
                Tok::Zero => break,
                _ => {}
            }
        }
    }

    // Integer digits without leading zeros; an all-zero integer part shows
    // nothing (blank positions) except the whole-zero case with no fractional
    // digit emitted, which shows a single "0".
    let intpart_zero = int_trimmed.is_empty();
    let intstr: &[u8] = if intpart_zero {
        if whole_zero && frac_emit == 0 {
            b"0"
        } else {
            b""
        }
    } else {
        int_trimmed.as_bytes()
    };

    // A `0` code forces zero-fill from its integer position rightward; leading
    // `9` positions to its left stay blank.
    let dp = point_index(toks);
    let mut zero_start = int_digits;
    {
        let mut index = 0usize;
        for (tp, &t) in toks.iter().enumerate() {
            if tp == dp {
                break;
            }
            match t {
                Tok::Zero => {
                    zero_start = index;
                    break;
                }
                Tok::Nine => index += 1,
                _ => {}
            }
        }
    }

    let sign_char: Option<u8> = match sign_kind {
        SignKind::Default => {
            if neg {
                Some(b'-')
            } else if fm {
                None
            } else {
                Some(b' ')
            }
        }
        SignKind::S => Some(if neg { b'-' } else { b'+' }),
    };
    let sign_leading = !sign_trailing;

    // First integer position that carries a real digit, and the first that is
    // non-blank at all (a zero-fill position counts); the floating sign sits
    // just before the first non-blank position.
    let fill_start = int_digits.saturating_sub(intstr.len());
    // On overflow every integer position is `#` (non-blank), so the sign sits
    // at the very front.
    let sig_start = if overflow { 0 } else { fill_start.min(zero_start) };

    let mut out = [0u8; MAX_OUT];
    let mut olen = 0usize;
    let emit = |out: &mut [u8; MAX_OUT], olen: &mut usize, ch: u8| -> Result<(), SqlError> {
        if *olen >= MAX_OUT {
            return Err(sql_err!("22023", "to_char output too long"));
        }
        out[*olen] = ch;
        *olen += 1;
        Ok(())
    };

    let mut int_idx = 0usize; // integer digit tokens seen
    let mut frac_idx = 0usize; // fractional digit tokens seen
    let mut seen_nonblank_int = false; // for group-separator blanking
    let mut sign_emitted = false;

    for (t_pos, &t) in toks.iter().enumerate() {
        match t {
            Tok::Nine | Tok::Zero => {
                let after_point = has_point && t_pos > point_index(toks);
                if after_point {
                    if frac_idx < frac_emit {
                        let ch = if overflow { b'#' } else { fracstr[frac_idx] };
                        emit(&mut out, &mut olen, ch)?;
                    }
                    frac_idx += 1;
                } else {
                    // Integer digit position. The floating sign lands just
                    // before the first non-blank position.
                    if sign_leading && !sign_emitted && int_idx == sig_start && sig_start < int_digits
                    {
                        if let Some(sc) = sign_char {
                            emit(&mut out, &mut olen, sc)?;
                        }
                        sign_emitted = true;
                    }
                    let ch = if overflow {
                        b'#'
                    } else if int_idx >= fill_start {
                        seen_nonblank_int = true;
                        intstr[int_idx - fill_start]
                    } else if int_idx >= zero_start {
                        seen_nonblank_int = true;
                        b'0'
                    } else if fm {
                        // Leading blank suppressed entirely.
                        int_idx += 1;
                        continue;
                    } else {
                        b' '
                    };
                    emit(&mut out, &mut olen, ch)?;
                    int_idx += 1;
                }
            }
            Tok::Group => {
                // The separator stays literal on overflow; otherwise it shows
                // only once a non-blank integer digit precedes it.
                let ch = if overflow || seen_nonblank_int {
                    b','
                } else if fm {
                    continue;
                } else {
                    b' '
                };
                emit(&mut out, &mut olen, ch)?;
            }
            Tok::Point => {
                // The sign floats to just before the point when no integer
                // digit was filled.
                if sign_leading && !sign_emitted {
                    if let Some(sc) = sign_char {
                        emit(&mut out, &mut olen, sc)?;
                    }
                    sign_emitted = true;
                }
                // The point stays literal even on overflow; without overflow it
                // appears only when the format has fractional positions.
                if has_point && (overflow || frac_digits > 0) {
                    emit(&mut out, &mut olen, b'.')?;
                }
            }
            Tok::Currency => emit(&mut out, &mut olen, b'$')?,
            Tok::Literal(c) => emit(&mut out, &mut olen, c)?,
        }
    }

    // A leading sign with no integer digits and no point still needs emitting
    // (e.g. a bare `S` with only literals); a trailing sign appends at the end.
    if !sign_emitted && let Some(sc) = sign_char {
        emit(&mut out, &mut olen, sc)?;
    }

    let text = core::str::from_utf8(&out[..olen]).expect("ascii output");
    arena.alloc_str(text).map_err(|_| sql_err!("53200", "out of memory"))
}

/// Position (token index) of the decimal point, or `usize::MAX` if none.
fn point_index(toks: &[Tok]) -> usize {
    toks.iter().position(|t| *t == Tok::Point).unwrap_or(usize::MAX)
}

/// `to_number(text, fmt)`: parse a formatted number. The format determines the
/// result scale (its fractional digit positions); the value's digits, sign, and
/// decimal point are read from the input, ignoring group separators, currency,
/// and spaces — matching PostgreSQL.
pub fn to_number<'a>(input: &str, fmt: &str, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    // Count fractional digit positions in the format; reject input-unsupported
    // codes loudly.
    let mut frac = 0usize;
    let mut seen_point = false;
    let fb = fmt.as_bytes();
    let mut i = 0usize;
    while i < fb.len() {
        let up = fb[i].to_ascii_uppercase();
        if i + 1 < fb.len() {
            let two = [up, fb[i + 1].to_ascii_uppercase()];
            if &two == b"EE" {
                return Err(sql_err!("0A000", "\"EEEE\" not supported for input"));
            }
            if matches!(&two, b"FM" | b"MI" | b"PL" | b"SG" | b"PR" | b"TH") {
                i += 2;
                continue;
            }
        }
        match up {
            b'9' | b'0' => {
                if seen_point {
                    frac += 1;
                }
            }
            b'.' | b'D' => seen_point = true,
            b'V' => return Err(sql_err!("0A000", "\"V\" not supported for input")),
            b'R' => return Err(sql_err!("0A000", "\"RN\" not supported for input")),
            _ => {}
        }
        i += 1;
    }

    // Extract sign, digits, and a single decimal point from the input.
    let mut buffer = [0u8; 512];
    let mut k = 0usize;
    let mut neg = false;
    let mut dot = false;
    let mut digits = false;
    buffer[k] = b' '; // placeholder for a sign slot
    k += 1;
    for &c in input.as_bytes() {
        match c {
            b'0'..=b'9' => {
                if k >= buffer.len() {
                    return Err(sql_err!("22P02", "value too long for to_number"));
                }
                buffer[k] = c;
                k += 1;
                digits = true;
            }
            b'.' if !dot => {
                dot = true;
                buffer[k] = b'.';
                k += 1;
            }
            b'-' => neg = true,
            _ => {}
        }
    }
    if !digits {
        return Err(sql_err!("22P02", "invalid input syntax for type numeric: \"{}\"", input));
    }
    if neg {
        buffer[0] = b'-';
    }
    let start = if neg { 0 } else { 1 };
    let cleaned = core::str::from_utf8(&buffer[start..k]).expect("ascii");
    Numeric::parse(cleaned, arena)?.round_scale(frac, RoundMode::HalfAwayZero, arena)
}

const MONTH_FULL: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const DAY_FULL: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];
const DAY_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// The output casing a name-producing code selects: `MONTH`→upper, `month`→
/// lower, `Month`→title (matching the code's own casing).
#[derive(Clone, Copy)]
enum Case {
    Upper,
    Lower,
    Title,
}

fn name_case(code: &[u8]) -> Case {
    let mut any_upper = false;
    let mut any_lower = false;
    for &c in code {
        if c.is_ascii_uppercase() {
            any_upper = true;
        } else if c.is_ascii_lowercase() {
            any_lower = true;
        }
    }
    match (any_upper, any_lower) {
        (true, false) => Case::Upper,
        (false, true) => Case::Lower,
        _ => Case::Title,
    }
}

/// `to_char(timestamp/date, text)` — formats the temporal value `micros`
/// (microseconds since 2000-01-01) per the format string. Supports the common
/// field codes; unrecognized letter codes are rejected loudly.
pub fn timestamp<'a>(micros: i64, fmt: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    use crate::sql::datetime::{civil_from_days, day_of_week, days_from_civil, PG_EPOCH_DAYS};
    let days = micros.div_euclid(86_400_000_000);
    let time_of_day = micros.rem_euclid(86_400_000_000);
    let adays = days + PG_EPOCH_DAYS;
    let (y, month, d) = civil_from_days(adays);
    let hh24 = (time_of_day / 3_600_000_000) as u32;
    let minute = ((time_of_day / 60_000_000) % 60) as u32;
    let ss = ((time_of_day / 1_000_000) % 60) as u32;
    let us = (time_of_day % 1_000_000) as u32;
    let dow = day_of_week(days); // 0=Sun..6=Sat (PG-epoch day count)
    let doy = (adays - days_from_civil(y, 1, 1) + 1) as u32;
    let hh12 = if hh24.is_multiple_of(12) { 12 } else { hh24 % 12 };

    let mut out = StackStr::<512>::new();
    let name = |out: &mut StackStr<512>, s: &str, case: Case, pad: usize, fm: bool| {
        let mut buffer = [0u8; 16];
        let n = s.len().min(buffer.len());
        for (i, b) in s.bytes().take(n).enumerate() {
            buffer[i] = match case {
                Case::Upper => b.to_ascii_uppercase(),
                Case::Lower => b.to_ascii_lowercase(),
                Case::Title => b,
            };
        }
        let _ = out.write_str(core::str::from_utf8(&buffer[..n]).unwrap_or(""));
        if !fm {
            for _ in n..pad {
                let _ = out.write_char(' ');
            }
        }
    };
    let num = |out: &mut StackStr<512>, v: i64, width: usize, fm: bool| {
        let s = crate::stack_format!(24, "{}", v);
        if !fm {
            for _ in s.as_str().len()..width {
                let _ = out.write_char('0');
            }
        }
        let _ = out.write_str(s.as_str());
    };

    let fb = fmt.as_bytes();
    let mut i = 0usize;
    while i < fb.len() {
        let mut fm = false;
        if i + 1 < fb.len() && fb[i].eq_ignore_ascii_case(&b'F') && fb[i + 1].eq_ignore_ascii_case(&b'M') {
            fm = true;
            i += 2;
        }
        let rest = &fb[i..];
        let m = |w: &[u8]| rest.len() >= w.len() && rest[..w.len()].eq_ignore_ascii_case(w);
        if m(b"HH24") {
            num(&mut out, hh24 as i64, 2, fm);
            i += 4;
        } else if m(b"HH12") {
            num(&mut out, hh12 as i64, 2, fm);
            i += 4;
        } else if m(b"YYYY") {
            num(&mut out, y, 4, fm);
            i += 4;
        } else if m(b"MONTH") {
            name(&mut out, MONTH_FULL[(month - 1) as usize], name_case(&rest[..5]), 9, fm);
            i += 5;
        } else if m(b"MON") {
            name(&mut out, MONTH_ABBR[(month - 1) as usize], name_case(&rest[..3]), 3, fm);
            i += 3;
        } else if m(b"DAY") {
            name(&mut out, DAY_FULL[dow], name_case(&rest[..3]), 9, fm);
            i += 3;
        } else if m(b"DDD") {
            num(&mut out, doy as i64, 3, fm);
            i += 3;
        } else if m(b"DY") {
            name(&mut out, DAY_ABBR[dow], name_case(&rest[..2]), 3, fm);
            i += 2;
        } else if m(b"YYY") {
            num(&mut out, y % 1000, 3, fm);
            i += 3;
        } else if m(b"HH") {
            num(&mut out, hh12 as i64, 2, fm);
            i += 2;
        } else if m(b"YY") {
            num(&mut out, y % 100, 2, fm);
            i += 2;
        } else if m(b"MI") {
            num(&mut out, minute as i64, 2, fm);
            i += 2;
        } else if m(b"MM") {
            num(&mut out, month as i64, 2, fm);
            i += 2;
        } else if m(b"MS") {
            num(&mut out, (us / 1000) as i64, 3, fm);
            i += 2;
        } else if m(b"US") {
            num(&mut out, us as i64, 6, fm);
            i += 2;
        } else if m(b"SS") {
            num(&mut out, ss as i64, 2, fm);
            i += 2;
        } else if m(b"DD") {
            num(&mut out, d as i64, 2, fm);
            i += 2;
        } else if m(b"WW") {
            num(&mut out, ((doy - 1) / 7 + 1) as i64, 2, fm);
            i += 2;
        } else if m(b"AM") || m(b"PM") {
            let mer = if hh24 < 12 { "AM" } else { "PM" };
            name(&mut out, mer, name_case(&rest[..2]), 0, true);
            i += 2;
        } else if m(b"Q") {
            num(&mut out, ((month - 1) / 3 + 1) as i64, 1, fm);
            i += 1;
        } else if m(b"D") {
            num(&mut out, (dow + 1) as i64, 1, fm);
            i += 1;
        } else if m(b"Y") {
            num(&mut out, y % 10, 1, fm);
            i += 1;
        } else if rest[0].is_ascii_alphabetic() {
            return Err(sql_err!("0A000", "to_char timestamp code not supported: \"{}\"", rest[0] as char));
        } else {
            let _ = out.write_char(rest[0] as char);
            i += 1;
        }
    }
    arena.alloc_str(out.as_str()).map_err(|_| sql_err!("53200", "out of memory"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;

    fn arena() -> Arena {
        let budget = Box::leak(Box::new(Budget::new(1 << 20)));
        Arena::new(budget, "t", 1 << 19).unwrap()
    }

    fn tc(v: &str, f: &str, a: &Arena) -> String {
        number(&Numeric::parse(v, a).unwrap(), f, a).unwrap().to_string()
    }

    #[test]
    fn matches_postgres_number_formats() {
        let a = arena();
        // Digit positions, blank leading, floating sign, rounding.
        assert_eq!(tc("3.14", "999.99", &a), "   3.14");
        assert_eq!(tc("-3.14", "999.99", &a), "  -3.14");
        assert_eq!(tc("3.146", "999.99", &a), "   3.15");
        assert_eq!(tc("0", "999.99", &a), "    .00");
        assert_eq!(tc("0", "999", &a), "   0");
        // Zero-fill, group separators.
        assert_eq!(tc("7", "000", &a), " 007");
        assert_eq!(tc("1234.5", "9,999.99", &a), " 1,234.50");
        // Fill mode trims leading blanks and trailing 9-zeros.
        assert_eq!(tc("3.14", "FM999.99", &a), "3.14");
        assert_eq!(tc("1234.5", "FM9,999.99", &a), "1,234.5");
        assert_eq!(tc("1.0", "FM9.99", &a), "1.");
        // Sign codes and currency.
        assert_eq!(tc("1234.5", "S9,999.99", &a), "+1,234.50");
        assert_eq!(tc("1234.5", "L9999.99", &a), "$ 1234.50");
        // Overflow fills the number field with '#', keeping the point.
        assert_eq!(tc("12345", "999", &a), " ###");
        assert_eq!(tc("12345", "9,999.99", &a), " #,###.##");
    }

    #[test]
    fn to_number_matches_postgres() {
        let a = arena();
        let tn = |v: &str, f: &str| to_number(v, f, &a).unwrap().to_string();
        assert_eq!(tn("1234.5", "9999.9"), "1234.5");
        assert_eq!(tn("1,234.56", "9,999.99"), "1234.56");
        assert_eq!(tn("-12.5", "99.9"), "-12.5");
        assert_eq!(tn("12.30", "99.99"), "12.30");
        assert_eq!(tn("42", "99"), "42");
        assert_eq!(tn("12,345", "99G999"), "12345");
        assert!(to_number("abc", "999", &a).is_err());
    }

    #[test]
    fn timestamp_formats_match_postgres() {
        let a = arena();
        // 2024-06-15 14:07:09.123456 (a Saturday) in micros since 2000-01-01.
        let micros = crate::sql::datetime::parse_timestamp("2024-06-15 14:07:09.123456", false)
            .unwrap();
        let tc = |f: &str| timestamp(micros, f, &a).unwrap().to_string();
        assert_eq!(tc("YYYY-MM-DD HH24:MI:SS"), "2024-06-15 14:07:09");
        assert_eq!(tc("HH12:MI:SS AM"), "02:07:09 PM");
        assert_eq!(tc("Mon DD, YYYY"), "Jun 15, 2024");
        assert_eq!(tc("Month"), "June     ");
        assert_eq!(tc("FMMonth FMDD"), "June 15");
        assert_eq!(tc("Day DY D"), "Saturday  SAT 7");
        assert_eq!(tc("Q WW DDD"), "2 24 167");
        assert_eq!(tc("US"), "123456");
        assert!(timestamp(micros, "ZZZ", &a).is_err());
    }

    #[test]
    fn unsupported_codes_are_loud() {
        let a = arena();
        assert!(number(&Numeric::parse("5", &a).unwrap(), "999MI", &a).is_err());
        assert!(number(&Numeric::parse("5", &a).unwrap(), "RN", &a).is_err());
        assert!(number(&Numeric::parse("5", &a).unwrap(), "9EEEE", &a).is_err());
    }
}
