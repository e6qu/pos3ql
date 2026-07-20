//! `to_char(numeric, text)` number formatting, following PostgreSQL's
//! `NUM_processor`. Supports digit positions (`9`, `0`), decimal point
//! (`.`, `D`), group separators (`,`, `G`), the floating sign (`S` and the
//! implicit sign slot), positional signs (`MI`, `PL`, `SG`), angle brackets
//! (`PR`), ordinal suffixes (`TH`/`th`), Roman numerals (`RN`/`rn`),
//! scientific notation (`EEEE`), the implied-decimal multiplier (`V`), a
//! currency marker (`L`, `$`), fill mode (`FM`), and the overflow `#` fill.
//! Format-combination validation mirrors `NUMDesc_prepare` (PostgreSQL 18,
//! `src/backend/utils/adt/formatting.c`), and every rendering rule here was
//! pinned empirically against PostgreSQL 18.4.

use super::eval::SqlError;
use super::numeric::{Numeric, RoundMode};
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
    /// `MI`: `-` for negatives, a space otherwise (nothing under FM).
    SignMinus,
    /// `PL`: `+` for non-negatives, a space otherwise (nothing under FM).
    SignPlus,
    /// `SG`: `-` or `+`, always.
    SignSg,
    /// `PR`: the closing `>` position (`<value>` for negatives; the opening
    /// bracket floats like a sign).
    BracketClose,
    /// `TH` / `th`: English ordinal suffix (skipped for negatives and for
    /// formats with a decimal point).
    Ordinal { upper: bool },
    /// `V`: renders nothing itself; the digits after it extend the integer
    /// field and each `9` multiplies the value by ten.
    VMark,
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
    /// `MI`/`SG` present: the positional token carries the sign; the implicit
    /// slot disappears entirely.
    None,
    /// `PR`: `<` floats like a sign for negatives (space otherwise); the
    /// matching `>` sits at the `PR` position.
    Bracket,
}

/// Formats `value` per `fmt`, returning an arena-allocated string.
/// `float_source` carries the original float8 when the input was one:
/// PostgreSQL formats a float8 from its binary value with C's `%.*f`
/// (round-half-even on the true binary expansion), while a numeric input
/// rounds half-away-from-zero on its decimal value.
pub fn number<'a>(
    value: &Numeric,
    fmt: &str,
    negative_sign_override: bool,
    float_source: Option<f64>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let mut toks = [Tok::Nine; MAX_TOKS];
    let mut ntok = 0usize;
    let mut fm = false;
    let mut sign_kind = SignKind::Default;
    let mut sign_seen = false; // `S`
    let mut sign_trailing = false;
    let mut has_point = false;
    let mut int_digits = 0usize;
    let mut frac_digits = 0usize;
    let mut seen_digit = false;
    let mut plus = false; // PL
    let mut minus = false; // MI
    let mut bracket = false; // PR
    let mut roman = false; // RN
    let mut roman_upper = true;
    let mut multi = 0usize; // `9` positions after V: multiply by 10^multi
    let mut in_multi = false;
    let mut eeee = false;

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
        // Anything but a plain literal after EEEE is an error, as in
        // PostgreSQL (literal characters may still follow).
        let is_action = matches!(up, b'9' | b'0' | b'.' | b'D' | b',' | b'G' | b'L' | b'$' | b'S' | b'V')
            || matches!(&two, b"MI" | b"PL" | b"SG" | b"PR" | b"TH" | b"RN" | b"FM" | b"EE");
        if eeee && is_action {
            return Err(sql_err!("42601", "\"EEEE\" must be the last pattern used"));
        }
        match &two {
            b"FM" => {
                fm = true;
                i += 2;
                continue;
            }
            b"MI" => {
                if sign_seen {
                    return Err(sql_err!("42601", "cannot use \"S\" and \"MI\" together"));
                }
                minus = true;
                push(&mut toks, &mut ntok, Tok::SignMinus)?;
                i += 2;
                continue;
            }
            b"PL" => {
                if sign_seen {
                    return Err(sql_err!("42601", "cannot use \"S\" and \"PL\" together"));
                }
                plus = true;
                push(&mut toks, &mut ntok, Tok::SignPlus)?;
                i += 2;
                continue;
            }
            b"SG" => {
                if sign_seen {
                    return Err(sql_err!("42601", "cannot use \"S\" and \"SG\" together"));
                }
                minus = true;
                plus = true;
                push(&mut toks, &mut ntok, Tok::SignSg)?;
                i += 2;
                continue;
            }
            b"PR" => {
                if sign_seen || plus || minus {
                    return Err(sql_err!(
                        "42601",
                        "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together"
                    ));
                }
                bracket = true;
                push(&mut toks, &mut ntok, Tok::BracketClose)?;
                i += 2;
                continue;
            }
            b"TH" => {
                push(&mut toks, &mut ntok, Tok::Ordinal { upper: bytes[i] == b'T' })?;
                i += 2;
                continue;
            }
            b"RN" => {
                if roman {
                    return Err(sql_err!("42601", "cannot use \"RN\" twice"));
                }
                roman = true;
                roman_upper = bytes[i] == b'R';
                i += 2;
                continue;
            }
            b"EE" => {
                let four = i + 3 < bytes.len()
                    && bytes[i + 2].eq_ignore_ascii_case(&b'E')
                    && bytes[i + 3].eq_ignore_ascii_case(&b'E');
                if !four {
                    return Err(sql_err!(
                        "0A000",
                        "to_char format code not supported: \"E\""
                    ));
                }
                if eeee {
                    return Err(sql_err!("42601", "cannot use \"EEEE\" twice"));
                }
                if fm || sign_seen || bracket || minus || plus || roman || multi > 0 || in_multi {
                    return Err(sql_err!("42601", "\"EEEE\" is incompatible with other formats"));
                }
                eeee = true;
                i += 4;
                continue;
            }
            _ => {}
        }
        match up {
            b'9' => {
                if bracket {
                    return Err(sql_err!("42601", "\"9\" must be ahead of \"PR\""));
                }
                push(&mut toks, &mut ntok, Tok::Nine)?;
                if in_multi {
                    multi += 1;
                    int_digits += 1;
                } else if has_point {
                    frac_digits += 1;
                } else {
                    int_digits += 1;
                }
                seen_digit = true;
            }
            b'0' => {
                if bracket {
                    return Err(sql_err!("42601", "\"0\" must be ahead of \"PR\""));
                }
                push(&mut toks, &mut ntok, Tok::Zero)?;
                if has_point && !in_multi {
                    frac_digits += 1;
                } else {
                    // A `0` after `V` extends the integer field without
                    // multiplying (PostgreSQL's NUMDesc_prepare quirk).
                    int_digits += 1;
                }
                seen_digit = true;
            }
            b'.' | b'D' => {
                if has_point {
                    return Err(sql_err!("42601", "multiple decimal points"));
                }
                if in_multi {
                    return Err(sql_err!(
                        "42601",
                        "cannot use \"V\" and decimal point together"
                    ));
                }
                has_point = true;
                push(&mut toks, &mut ntok, Tok::Point)?;
            }
            b',' | b'G' => push(&mut toks, &mut ntok, Tok::Group)?,
            b'L' | b'$' => push(&mut toks, &mut ntok, Tok::Currency)?,
            b'S' => {
                if sign_seen {
                    return Err(sql_err!("42601", "cannot use \"S\" twice"));
                }
                if plus || minus || bracket {
                    return Err(sql_err!(
                        "42601",
                        "cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together"
                    ));
                }
                sign_seen = true;
                sign_kind = SignKind::S;
                sign_trailing = seen_digit;
            }
            b'V' => {
                if has_point {
                    return Err(sql_err!(
                        "42601",
                        "cannot use \"V\" and decimal point together"
                    ));
                }
                in_multi = true;
                push(&mut toks, &mut ntok, Tok::VMark)?;
            }
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

    // RN combines only with FM (plain digit positions carry no flag and are
    // ignored); anything else is an error, as in PostgreSQL.
    if roman && (sign_seen || plus || minus || bracket || multi > 0 || in_multi || eeee) {
        return Err(sql_err!("42601", "\"RN\" is incompatible with other formats"));
    }
    if roman {
        return render_roman(value, float_source, roman_upper, fm, arena);
    }
    if eeee {
        return render_eeee(
            value,
            float_source,
            int_digits,
            frac_digits,
            negative_sign_override,
            arena,
        );
    }

    if minus || bracket {
        sign_kind = match sign_kind {
            SignKind::Default if bracket => SignKind::Bracket,
            SignKind::Default => SignKind::None,
            other => other,
        };
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
        multi,
        negative_sign_override,
        float_source,
        arena,
    )
}

/// The English ordinal suffix for the integer whose decimal digits end as
/// given (`1st`, `2nd`, `3rd`, `11th`–`13th`, else `th`).
fn ordinal_suffix(int_digits_text: &[u8]) -> &'static str {
    let last = int_digits_text.last().copied().unwrap_or(b'0');
    let prev = if int_digits_text.len() >= 2 {
        int_digits_text[int_digits_text.len() - 2]
    } else {
        b'0'
    };
    if prev == b'1' {
        return "TH";
    }
    match last {
        b'1' => "ST",
        b'2' => "ND",
        b'3' => "RD",
        _ => "TH",
    }
}

/// `RN`/`rn`: the value rounded to an integer as a Roman numeral,
/// right-justified to 15 characters (FM trims). Out of 1..=3999 fills with
/// `#`, as PostgreSQL.
fn render_roman<'a>(
    value: &Numeric,
    float_source: Option<f64>,
    upper: bool,
    fm: bool,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let text = match float_source {
        Some(x) if x.is_finite() => stack_format!(512, "{:.0}", x),
        _ => {
            let rounded = value.round_scale(0, RoundMode::HalfAwayZero, arena)?;
            stack_format!(512, "{}", rounded)
        }
    };
    let n: i64 = text.as_str().parse().unwrap_or(-1);
    let mut out = [0u8; 16];
    let mut olen = 0usize;
    if !(1..=3999).contains(&n) {
        let filled = "###############";
        return arena.alloc_str(filled).map_err(|_| sql_err!("53200", "out of memory"));
    }
    const ONES: [&str; 10] = ["", "I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX"];
    const TENS: [&str; 10] = ["", "X", "XX", "XXX", "XL", "L", "LX", "LXX", "LXXX", "XC"];
    const HUNDREDS: [&str; 10] = ["", "C", "CC", "CCC", "CD", "D", "DC", "DCC", "DCCC", "CM"];
    let emit = |s: &str, out: &mut [u8; 16], olen: &mut usize| {
        for &b in s.as_bytes() {
            out[*olen] = b;
            *olen += 1;
        }
    };
    for _ in 0..(n / 1000) {
        emit("M", &mut out, &mut olen);
    }
    emit(HUNDREDS[(n / 100 % 10) as usize], &mut out, &mut olen);
    emit(TENS[(n / 10 % 10) as usize], &mut out, &mut olen);
    emit(ONES[(n % 10) as usize], &mut out, &mut olen);
    let mut body = [0u8; 16];
    for k in 0..olen {
        body[k] = if upper { out[k] } else { out[k].to_ascii_lowercase() };
    }
    let roman = core::str::from_utf8(&body[..olen]).expect("ascii");
    if fm {
        return arena.alloc_str(roman).map_err(|_| sql_err!("53200", "out of memory"));
    }
    let padded = stack_format!(24, "{:>15}", roman);
    arena.alloc_str(padded.as_str()).map_err(|_| sql_err!("53200", "out of memory"))
}

/// `EEEE`: scientific notation `[sign]d.<frac>e±XX`. A float8 source rounds
/// half-even on its binary value; numeric rounds half-away on its decimal
/// digits.
fn render_eeee<'a>(
    value: &Numeric,
    float_source: Option<f64>,
    int_digits: usize,
    frac_digits: usize,
    negative_sign_override: bool,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    // NaN/Infinity: a space, then `#` fill with the point after the integer
    // positions — `pre + post + 6` characters total, as PostgreSQL.
    let nonfinite = matches!(float_source, Some(x) if !x.is_finite())
        || (float_source.is_none() && value.is_nan());
    if nonfinite {
        let total = int_digits.max(1) + frac_digits + 6;
        let mut out = [b'#'; 64];
        let n = total.min(out.len());
        out[0] = b' ';
        let dot = int_digits.max(1) + 1;
        if dot < n {
            out[dot] = b'.';
        }
        let text = core::str::from_utf8(&out[..n]).expect("ascii");
        return arena.alloc_str(text).map_err(|_| sql_err!("53200", "out of memory"));
    }
    // Mantissa digits (1 + frac) and a base-10 exponent.
    let (neg, mantissa, exponent) = match float_source {
        Some(x) if x.is_finite() => {
            let t = stack_format!(64, "{:.*e}", frac_digits, x.abs());
            let s = t.as_str();
            let (m, e) = s.split_once('e').expect("float scientific form");
            let mut digits = [b'0'; 512];
            let mut nd = 0usize;
            for b in m.bytes() {
                if b.is_ascii_digit() {
                    digits[nd] = b;
                    nd += 1;
                }
            }
            let exponent: i32 = e.parse().expect("float exponent");
            ((x < 0.0 && x != 0.0) || negative_sign_override, (digits, nd), exponent)
        }
        _ => {
            let t = stack_format!(512, "{}", value);
            let s = t.as_str();
            let neg = s.starts_with('-') || negative_sign_override;
            let body = s.strip_prefix('-').unwrap_or(s);
            let (ip, fp) = body.split_once('.').unwrap_or((body, ""));
            // Significant digits and the exponent of the first one.
            let mut digits = [b'0'; 512];
            let mut nd = 0usize;
            let mut exponent = 0i32;
            let mut seen = false;
            for (k, b) in ip.bytes().enumerate() {
                if !seen && b != b'0' {
                    seen = true;
                    exponent = (ip.len() - 1 - k) as i32;
                }
                if seen {
                    digits[nd] = b;
                    nd += 1;
                }
            }
            for (k, b) in fp.bytes().enumerate() {
                if !seen && b != b'0' {
                    seen = true;
                    exponent = -(k as i32 + 1);
                }
                if seen && nd < digits.len() {
                    digits[nd] = b;
                    nd += 1;
                }
            }
            if !seen {
                // Zero.
                (false, ([b'0'; 512], 1), 0)
            } else {
                // Round the significant digits to 1 + frac places
                // (half-away-from-zero), carrying into the exponent.
                let keep = 1 + frac_digits;
                if nd > keep {
                    let round_up = digits[keep] >= b'5';
                    nd = keep;
                    if round_up {
                        let mut k = keep;
                        loop {
                            if k == 0 {
                                // 9.99… rolled over: mantissa becomes 1, the
                                // exponent grows.
                                digits[0] = b'1';
                                for slot in digits[1..keep].iter_mut() {
                                    *slot = b'0';
                                }
                                exponent += 1;
                                break;
                            }
                            k -= 1;
                            if digits[k] == b'9' {
                                digits[k] = b'0';
                            } else {
                                digits[k] += 1;
                                break;
                            }
                        }
                    }
                }
                (neg, (digits, nd), exponent)
            }
        }
    };
    let (digits, nd) = mantissa;
    let mut out = StackStr::<128>::default();
    let _ = write!(out, "{}", if neg { '-' } else { ' ' });
    let _ = write!(out, "{}", digits[0] as char);
    if frac_digits > 0 {
        let _ = write!(out, ".");
        for k in 1..=frac_digits {
            let d = *digits.get(k).filter(|_| k < nd).unwrap_or(&b'0');
            let _ = write!(out, "{}", d as char);
        }
    }
    let _ = write!(out, "e{}", if exponent < 0 { '-' } else { '+' });
    let e = exponent.unsigned_abs();
    if e < 10 {
        let _ = write!(out, "0");
    }
    let _ = write!(out, "{}", e);
    arena.alloc_str(out.as_str()).map_err(|_| sql_err!("53200", "out of memory"))
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
    multi: usize,
    negative_sign_override: bool,
    float_source: Option<f64>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    // Round to the number of fractional positions the format provides. A
    // finite float8 input formats from its binary value with round-half-even
    // (C's `%.*f`, so `-120.975` — really `-120.97499…` — gives `-120.97`);
    // a numeric input rounds half-away-from-zero on its decimal value. With
    // `V`, the value is scaled by 10^multi instead (rounded at that many
    // decimals, then the point dropped — `V` and `.` never combine).
    // Non-finite values (pinned against PostgreSQL 18.4): NaN lays the text
    // "NaN" into the digit positions with no fractional part; Infinity
    // overflows every position (keeping its sign).
    let nan = matches!(float_source, Some(x) if x.is_nan())
        || (float_source.is_none() && value.is_nan());
    let infinite = matches!(float_source, Some(x) if x.is_infinite());
    if nan || infinite {
        return render_nonfinite(
            toks,
            fm,
            sign_kind,
            sign_trailing,
            int_digits,
            nan,
            negative_sign_override,
            arena,
        );
    }
    let scale = if multi > 0 { multi } else { frac_digits };
    let text = match float_source {
        // A float8 with `V` multiplies the binary value by 10^multi first,
        // then rounds to an integer (PostgreSQL's `float8_to_char`).
        Some(x) if x.is_finite() && multi > 0 => {
            stack_format!(512, "{:.0}", x * 10f64.powi(multi as i32))
        }
        Some(x) if x.is_finite() => stack_format!(512, "{:.*}", scale, x),
        _ => {
            let rounded = value.round_scale(scale, RoundMode::HalfAwayZero, arena)?;
            stack_format!(512, "{}", rounded)
        }
    };
    let mut scaled = StackStr::<512>::default();
    let s: &str = if multi > 0 {
        for ch in text.as_str().chars() {
            if ch != '.' {
                let _ = write!(scaled, "{}", ch);
            }
        }
        scaled.as_str()
    } else {
        text.as_str()
    };
    let body = s.strip_prefix('-').unwrap_or(s);
    let (intpart, fracpart) = body.split_once('.').unwrap_or((body, ""));
    let body_zero = body.bytes().all(|b| !b.is_ascii_digit() || b == b'0');
    // A numeric that rounds to zero loses its sign, but a float8 input keeps
    // its own sign bit even at zero (`to_char(-0.001::float8, 'FM999.99')` →
    // `-0.` while the numeric form gives `0.`) — verified against PostgreSQL.
    let neg = (s.starts_with('-') && !body_zero) || negative_sign_override;
    let whole_zero = body_zero;

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

    // The ordinal suffix reads the integer digits (blank for negatives and
    // decimal formats).
    let ordinal = ordinal_suffix(if intstr.is_empty() { b"0" } else { intstr });

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
        // MI / SG carry the sign at their own token position.
        SignKind::None => None,
        SignKind::Bracket => {
            if neg {
                Some(b'<')
            } else if fm {
                None
            } else {
                Some(b' ')
            }
        }
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
    let emit_str =
        |out: &mut [u8; MAX_OUT], olen: &mut usize, s: &str| -> Result<(), SqlError> {
            for &b in s.as_bytes() {
                if *olen >= MAX_OUT {
                    return Err(sql_err!("22023", "to_char output too long"));
                }
                out[*olen] = b;
                *olen += 1;
            }
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
            Tok::SignMinus => {
                if neg {
                    emit(&mut out, &mut olen, b'-')?;
                } else if !fm {
                    emit(&mut out, &mut olen, b' ')?;
                }
            }
            Tok::SignPlus => {
                if !neg {
                    emit(&mut out, &mut olen, b'+')?;
                } else if !fm {
                    emit(&mut out, &mut olen, b' ')?;
                }
            }
            Tok::SignSg => emit(&mut out, &mut olen, if neg { b'-' } else { b'+' })?,
            Tok::BracketClose => {
                if neg {
                    emit(&mut out, &mut olen, b'>')?;
                } else if !fm {
                    emit(&mut out, &mut olen, b' ')?;
                }
            }
            Tok::Ordinal { upper } => {
                // Skipped for negatives and for formats carrying a decimal
                // point, as PostgreSQL.
                if !neg && !has_point {
                    if upper {
                        emit_str(&mut out, &mut olen, ordinal)?;
                    } else {
                        let mut low = [0u8; 2];
                        low[0] = ordinal.as_bytes()[0].to_ascii_lowercase();
                        low[1] = ordinal.as_bytes()[1].to_ascii_lowercase();
                        emit_str(
                            &mut out,
                            &mut olen,
                            core::str::from_utf8(&low).expect("ascii"),
                        )?;
                    }
                }
            }
            Tok::VMark => {}
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

/// NaN / Infinity through a plain digit format: NaN lays "NaN" into the
/// integer positions (`#` on overflow); Infinity overflows every position.
/// The decimal point and fractional positions disappear; the sign slot keeps
/// its normal behavior (so `-Infinity` shows `-###`).
#[allow(clippy::too_many_arguments)]
fn render_nonfinite<'a>(
    toks: &[Tok],
    fm: bool,
    sign_kind: SignKind,
    sign_trailing: bool,
    int_digits: usize,
    nan: bool,
    neg: bool,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let image: &[u8] = if nan { b"NaN" } else { b"" };
    let overflow = !nan || image.len() > int_digits;
    let fill_start = int_digits.saturating_sub(image.len());
    let sig_start = if overflow { 0 } else { fill_start };
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
        SignKind::None => None,
        SignKind::Bracket => {
            if neg {
                Some(b'<')
            } else if fm {
                None
            } else {
                Some(b' ')
            }
        }
    };
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
    let mut int_idx = 0usize;
    let mut sign_emitted = false;
    for &t in toks {
        match t {
            Tok::Nine | Tok::Zero => {
                if int_idx >= int_digits {
                    // Fractional position: suppressed for non-finite values.
                    continue;
                }
                if !sign_trailing && !sign_emitted && int_idx == sig_start {
                    if let Some(sc) = sign_char {
                        emit(&mut out, &mut olen, sc)?;
                    }
                    sign_emitted = true;
                }
                let ch = if overflow {
                    b'#'
                } else if int_idx >= fill_start {
                    image[int_idx - fill_start]
                } else if fm {
                    int_idx += 1;
                    continue;
                } else {
                    b' '
                };
                emit(&mut out, &mut olen, ch)?;
                int_idx += 1;
            }
            Tok::Point => {}
            Tok::Group => {
                if !fm {
                    emit(&mut out, &mut olen, b' ')?;
                }
            }
            Tok::Currency => emit(&mut out, &mut olen, b'$')?,
            Tok::SignMinus => {
                if neg {
                    emit(&mut out, &mut olen, b'-')?;
                } else if !fm {
                    emit(&mut out, &mut olen, b' ')?;
                }
            }
            Tok::SignPlus => {
                if !neg {
                    emit(&mut out, &mut olen, b'+')?;
                } else if !fm {
                    emit(&mut out, &mut olen, b' ')?;
                }
            }
            Tok::SignSg => emit(&mut out, &mut olen, if neg { b'-' } else { b'+' })?,
            Tok::BracketClose => {
                if neg {
                    emit(&mut out, &mut olen, b'>')?;
                } else if !fm {
                    emit(&mut out, &mut olen, b' ')?;
                }
            }
            Tok::Ordinal { .. } | Tok::VMark => {}
            Tok::Literal(c) => emit(&mut out, &mut olen, c)?,
        }
    }
    if !sign_emitted && let Some(sc) = sign_char {
        emit(&mut out, &mut olen, sc)?;
    }
    let text = core::str::from_utf8(&out[..olen]).expect("ascii output");
    arena.alloc_str(text).map_err(|_| sql_err!("53200", "out of memory"))
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
        number(&Numeric::parse(v, a).unwrap(), f, false, None, a).unwrap().to_string()
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
        // Formerly-rejected codes now format (verified against PostgreSQL
        // 18.4); invalid combinations still error loudly.
        let a = arena();
        assert_eq!(number(&Numeric::parse("5", &a).unwrap(), "999MI", false, None, &a).unwrap(), "  5 ");
        assert_eq!(number(&Numeric::parse("5", &a).unwrap(), "RN", false, None, &a).unwrap(), "              V");
        assert_eq!(number(&Numeric::parse("5", &a).unwrap(), "9EEEE", false, None, &a).unwrap(), " 5e+00");
        assert!(number(&Numeric::parse("5", &a).unwrap(), "S999MI", false, None, &a).is_err());
        assert!(number(&Numeric::parse("5", &a).unwrap(), "9.9V9", false, None, &a).is_err());
        assert!(number(&Numeric::parse("5", &a).unwrap(), "EEEE9", false, None, &a).is_err());
    }
}
