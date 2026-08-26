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
use super::numeric::{self, Numeric, RoundMode};
use super::types::Interval;
use crate::mem::arena::Arena;
use crate::sql::eval::sqlstate;
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
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "to_char format too long"
            ));
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
        let is_action = matches!(
            up,
            b'9' | b'0' | b'.' | b'D' | b',' | b'G' | b'L' | b'$' | b'S' | b'V'
        ) || matches!(
            &two,
            b"MI" | b"PL" | b"SG" | b"PR" | b"TH" | b"RN" | b"FM" | b"EE"
        );
        if eeee && is_action {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "\"EEEE\" must be the last pattern used"
            ));
        }
        match &two {
            b"FM" => {
                fm = true;
                i += 2;
                continue;
            }
            b"MI" => {
                if sign_seen {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "cannot use \"S\" and \"MI\" together"
                    ));
                }
                minus = true;
                push(&mut toks, &mut ntok, Tok::SignMinus)?;
                i += 2;
                continue;
            }
            b"PL" => {
                if sign_seen {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "cannot use \"S\" and \"PL\" together"
                    ));
                }
                plus = true;
                push(&mut toks, &mut ntok, Tok::SignPlus)?;
                i += 2;
                continue;
            }
            b"SG" => {
                if sign_seen {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "cannot use \"S\" and \"SG\" together"
                    ));
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
                        sqlstate::SYNTAX_ERROR,
                        "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together"
                    ));
                }
                bracket = true;
                push(&mut toks, &mut ntok, Tok::BracketClose)?;
                i += 2;
                continue;
            }
            b"TH" => {
                push(
                    &mut toks,
                    &mut ntok,
                    Tok::Ordinal {
                        upper: bytes[i] == b'T',
                    },
                )?;
                i += 2;
                continue;
            }
            b"RN" => {
                if roman {
                    return Err(sql_err!(sqlstate::SYNTAX_ERROR, "cannot use \"RN\" twice"));
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
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "to_char format code not supported: \"E\""
                    ));
                }
                if eeee {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "cannot use \"EEEE\" twice"
                    ));
                }
                if fm || sign_seen || bracket || minus || plus || roman || multi > 0 || in_multi {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "\"EEEE\" is incompatible with other formats"
                    ));
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
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "\"9\" must be ahead of \"PR\""
                    ));
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
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "\"0\" must be ahead of \"PR\""
                    ));
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
                    return Err(sql_err!(sqlstate::SYNTAX_ERROR, "multiple decimal points"));
                }
                if in_multi {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
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
                    return Err(sql_err!(sqlstate::SYNTAX_ERROR, "cannot use \"S\" twice"));
                }
                if plus || minus || bracket {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
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
                        sqlstate::SYNTAX_ERROR,
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
                        sqlstate::FEATURE_NOT_SUPPORTED,
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
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "\"RN\" is incompatible with other formats"
        ));
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
        return arena
            .alloc_str(filled)
            .map_err(|_| sql_err!(sqlstate::OUT_OF_MEMORY, "out of memory"));
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
        body[k] = if upper {
            out[k]
        } else {
            out[k].to_ascii_lowercase()
        };
    }
    let roman = core::str::from_utf8(&body[..olen]).expect("ascii");
    if fm {
        return arena
            .alloc_str(roman)
            .map_err(|_| sql_err!(sqlstate::OUT_OF_MEMORY, "out of memory"));
    }
    let padded = stack_format!(24, "{:>15}", roman);
    arena
        .alloc_str(padded.as_str())
        .map_err(|_| sql_err!(sqlstate::OUT_OF_MEMORY, "out of memory"))
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
        return arena
            .alloc_str(text)
            .map_err(|_| sql_err!(sqlstate::OUT_OF_MEMORY, "out of memory"));
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
            (
                (x < 0.0 && x != 0.0) || negative_sign_override,
                (digits, nd),
                exponent,
            )
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
    arena
        .alloc_str(out.as_str())
        .map_err(|_| sql_err!(sqlstate::OUT_OF_MEMORY, "out of memory"))
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
    let nan =
        matches!(float_source, Some(x) if x.is_nan()) || (float_source.is_none() && value.is_nan());
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
    // On overflow a float8 source only carries as many fractional positions as
    // its ~15 significant digits leave past the integer part; a large enough
    // magnitude leaves none, so PostgreSQL prints the point but no `#` fill
    // there (a numeric always keeps every fractional position).
    if overflow && let Some(x) = float_source {
        let digits_before = if x.abs() >= 1.0 {
            x.abs().log10().floor() as i32 + 1
        } else {
            1
        };
        let available = (15 - digits_before).max(0);
        frac_emit = frac_emit.min(available as usize);
    }
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
    let sig_start = if overflow {
        0
    } else {
        fill_start.min(zero_start)
    };

    let mut out = [0u8; MAX_OUT];
    let mut olen = 0usize;
    let emit = |out: &mut [u8; MAX_OUT], olen: &mut usize, ch: u8| -> Result<(), SqlError> {
        if *olen >= MAX_OUT {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "to_char output too long"
            ));
        }
        out[*olen] = ch;
        *olen += 1;
        Ok(())
    };
    let emit_str = |out: &mut [u8; MAX_OUT], olen: &mut usize, s: &str| -> Result<(), SqlError> {
        for &b in s.as_bytes() {
            if *olen >= MAX_OUT {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "to_char output too long"
                ));
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
                    if sign_leading
                        && !sign_emitted
                        && int_idx == sig_start
                        && sig_start < int_digits
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
    arena
        .alloc_str(text)
        .map_err(|_| sql_err!(sqlstate::OUT_OF_MEMORY, "out of memory"))
}

/// Position (token index) of the decimal point, or `usize::MAX` if none.
fn point_index(toks: &[Tok]) -> usize {
    toks.iter()
        .position(|t| *t == Tok::Point)
        .unwrap_or(usize::MAX)
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
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "to_char output too long"
            ));
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
    arena
        .alloc_str(text)
        .map_err(|_| sql_err!(sqlstate::OUT_OF_MEMORY, "out of memory"))
}

/// `to_number(text, fmt)`: parse a formatted number. The format determines the
/// result scale (its fractional digit positions); the value's digits, sign, and
/// decimal point are read from the input, ignoring group separators, currency,
/// and spaces — matching PostgreSQL.
pub fn to_number<'a>(input: &str, fmt: &str, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    let model = NumberInputModel::parse(fmt)?;
    if model.roman {
        let value = roman_to_integer(input).ok_or_else(|| {
            sql_err!(
                sqlstate::INVALID_TEXT_REPRESENTATION,
                "invalid Roman numeral"
            )
        })?;
        return Numeric::parse(stack_format!(16, "{value}").as_str(), arena);
    }

    let bytes = input.as_bytes();
    let mut at = 0usize;
    let mut out = [0u8; MAX_OUT];
    let mut olen = 1usize;
    let mut digits = false;
    let mut decimal = false;
    let mut read_post = 0usize;
    let mut negative = false;

    for token in &model.tokens[..model.len] {
        if at >= bytes.len() {
            break;
        }
        match *token {
            NumberInputToken::Digit | NumberInputToken::Decimal => {
                if bytes[at] == b' ' {
                    at += 1;
                }
                if at >= bytes.len() {
                    break;
                }
                if !digits && matches!(bytes[at], b'+' | b'-' | b'<') {
                    negative = matches!(bytes[at], b'-' | b'<');
                    at += 1;
                }
                if at >= bytes.len() {
                    break;
                }
                if bytes[at].is_ascii_digit() {
                    if !decimal || read_post < model.post {
                        if olen >= out.len() {
                            return Err(sql_err!(
                                sqlstate::INVALID_TEXT_REPRESENTATION,
                                "value too long for to_number"
                            ));
                        }
                        out[olen] = bytes[at];
                        olen += 1;
                        digits = true;
                        if decimal {
                            read_post += 1;
                        }
                    }
                    at += 1;
                } else if model.decimal && !decimal && bytes[at] == b'.' {
                    out[olen] = b'.';
                    olen += 1;
                    decimal = true;
                    at += 1;
                }
                if at < bytes.len() && matches!(bytes[at], b'+' | b'-') && model.positional_sign {
                    negative = bytes[at] == b'-';
                }
            }
            NumberInputToken::Comma => {
                if bytes[at] == b',' {
                    at += 1;
                }
            }
            NumberInputToken::Group => {
                if bytes[at] == b',' {
                    at += 1;
                }
            }
            NumberInputToken::Skip(count) => {
                for _ in 0..count {
                    if at >= bytes.len() || is_number_data(bytes[at]) {
                        break;
                    }
                    at += 1;
                }
            }
            NumberInputToken::Sign(kind) => match (kind, bytes[at]) {
                (InputSign::Minus, b'-') | (InputSign::Either, b'-') => {
                    negative = true;
                    at += 1;
                }
                (InputSign::Plus, b'+') | (InputSign::Either, b'+') => at += 1,
                _ if !is_number_data(bytes[at]) => at += 1,
                _ => {}
            },
            NumberInputToken::Bracket => {
                if bytes[at] == b'>' {
                    at += 1;
                }
            }
        }
    }
    if !digits {
        return Err(sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "invalid input syntax for type numeric: \"{}\"",
            input
        ));
    }
    if olen > 1 && out[olen - 1] == b'.' {
        olen -= 1;
    }
    out[0] = if negative { b'-' } else { b'+' };
    let parsed = Numeric::parse(core::str::from_utf8(&out[..olen]).expect("ascii"), arena)?;
    if model.multi == 0 {
        parsed.round_scale(read_post, RoundMode::HalfAwayZero, arena)
    } else {
        let factor = stack_format!(280, "1{:0<width$}", "", width = model.multi);
        let divisor = Numeric::parse(factor.as_str(), arena)?;
        let mut divided = numeric::div(&parsed, &divisor, arena)?;
        divided.dscale = divided.dscale.max((16 + model.multi) as u16);
        Ok(divided)
    }
}

#[derive(Clone, Copy)]
enum InputSign {
    Minus,
    Plus,
    Either,
}

#[derive(Clone, Copy)]
enum NumberInputToken {
    Digit,
    Decimal,
    Comma,
    Group,
    Skip(u8),
    Sign(InputSign),
    Bracket,
}

struct NumberInputModel {
    tokens: [NumberInputToken; MAX_TOKS],
    len: usize,
    post: usize,
    multi: usize,
    decimal: bool,
    positional_sign: bool,
    roman: bool,
}

impl NumberInputModel {
    fn parse(fmt: &str) -> Result<Self, SqlError> {
        let mut model = Self {
            tokens: [NumberInputToken::Skip(0); MAX_TOKS],
            len: 0,
            post: 0,
            multi: 0,
            decimal: false,
            positional_sign: false,
            roman: false,
        };
        let bytes = fmt.as_bytes();
        let mut at = 0usize;
        let mut after_decimal = false;
        let mut after_multi = false;
        while at < bytes.len() {
            let up = bytes[at].to_ascii_uppercase();
            let two = if at + 1 < bytes.len() {
                [up, bytes[at + 1].to_ascii_uppercase()]
            } else {
                [up, 0]
            };
            if &two == b"EE" {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "\"EEEE\" not supported for input"
                ));
            }
            if &two == b"FM" {
                at += 2;
                continue;
            }
            if &two == b"RN" {
                model.roman = true;
                at += 2;
                continue;
            }
            let (token, consumed) = match &two {
                b"MI" => {
                    model.positional_sign = true;
                    (NumberInputToken::Sign(InputSign::Minus), 2)
                }
                b"PL" => {
                    model.positional_sign = true;
                    (NumberInputToken::Sign(InputSign::Plus), 2)
                }
                b"SG" => {
                    model.positional_sign = true;
                    (NumberInputToken::Sign(InputSign::Either), 2)
                }
                b"PR" => (NumberInputToken::Bracket, 2),
                b"TH" => (NumberInputToken::Skip(2), 2),
                _ => match up {
                    b'9' | b'0' => {
                        if after_decimal {
                            model.post += 1;
                        }
                        if after_multi {
                            model.multi += 1;
                        }
                        (NumberInputToken::Digit, 1)
                    }
                    b'.' | b'D' => {
                        if after_multi {
                            return Err(sql_err!(
                                sqlstate::SYNTAX_ERROR,
                                "cannot use \"V\" and decimal point together"
                            ));
                        }
                        model.decimal = true;
                        after_decimal = true;
                        (NumberInputToken::Decimal, 1)
                    }
                    b'V' => {
                        if after_decimal {
                            return Err(sql_err!(
                                sqlstate::SYNTAX_ERROR,
                                "cannot use \"V\" and decimal point together"
                            ));
                        }
                        after_multi = true;
                        at += 1;
                        continue;
                    }
                    b',' => (NumberInputToken::Comma, 1),
                    b'G' => (NumberInputToken::Group, 1),
                    b'L' | b'$' => (NumberInputToken::Skip(1), 1),
                    b'S' => (NumberInputToken::Sign(InputSign::Either), 1),
                    _ => (NumberInputToken::Skip(1), 1),
                },
            };
            if model.len >= model.tokens.len() {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "to_number format too long"
                ));
            }
            model.tokens[model.len] = token;
            model.len += 1;
            at += consumed;
        }
        if model.roman
            && model.tokens[..model.len]
                .iter()
                .any(|token| !matches!(token, NumberInputToken::Skip(0)))
        {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "\"RN\" is incompatible with other formats"
            ));
        }
        Ok(model)
    }
}

fn is_number_data(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'.' | b',' | b'+' | b'-')
}

fn roman_to_integer(input: &str) -> Option<i32> {
    const ONES: [&str; 10] = ["", "I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX"];
    const TENS: [&str; 10] = ["", "X", "XX", "XXX", "XL", "L", "LX", "LXX", "LXXX", "XC"];
    const HUNDREDS: [&str; 10] = ["", "C", "CC", "CCC", "CD", "D", "DC", "DCC", "DCCC", "CM"];
    let roman = input.trim();
    if roman.is_empty() || roman.len() > 15 {
        return None;
    }
    let value = |byte: u8| match byte.to_ascii_uppercase() {
        b'I' => 1,
        b'V' => 5,
        b'X' => 10,
        b'L' => 50,
        b'C' => 100,
        b'D' => 500,
        b'M' => 1000,
        _ => 0,
    };
    let bytes = roman.as_bytes();
    let mut total = 0i32;
    let mut at = 0usize;
    while at < bytes.len() {
        let current = value(bytes[at]);
        if current == 0 {
            return None;
        }
        if at + 1 < bytes.len() && current < value(bytes[at + 1]) {
            total -= current;
        } else {
            total += current;
        }
        at += 1;
    }
    if !(1..=3999).contains(&total) {
        return None;
    }
    let mut canonical = [0u8; 16];
    let mut len = 0usize;
    let emit = |text: &str, out: &mut [u8; 16], len: &mut usize| {
        for byte in text.bytes() {
            out[*len] = byte;
            *len += 1;
        }
    };
    for _ in 0..total / 1000 {
        emit("M", &mut canonical, &mut len);
    }
    emit(
        HUNDREDS[(total / 100 % 10) as usize],
        &mut canonical,
        &mut len,
    );
    emit(TENS[(total / 10 % 10) as usize], &mut canonical, &mut len);
    emit(ONES[(total % 10) as usize], &mut canonical, &mut len);
    (roman.len() == len
        && roman
            .bytes()
            .zip(canonical[..len].iter().copied())
            .all(|(left, right)| left.eq_ignore_ascii_case(&right)))
    .then_some(total)
}

const MONTH_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const DAY_FULL: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const DAY_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_ROMAN: [&str; 12] = [
    "I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X", "XI", "XII",
];

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

/// ISO week-year fields share one calculation so `IYYY`, `IW`, and `ID`
/// cannot disagree at a calendar-year boundary.
fn iso_week_date(days: i64) -> (i64, i64, i64) {
    use crate::sql::datetime::{PG_EPOCH_DAYS, civil_from_days, day_of_week, days_from_civil};

    let iso_day = ((day_of_week(days) + 6) % 7 + 1) as i64;
    // Thursday always belongs to the ISO year of its week.
    let (iso_year, _, _) = civil_from_days(days + (4 - iso_day) + PG_EPOCH_DAYS);
    let jan4 = days_from_civil(iso_year, 1, 4) - PG_EPOCH_DAYS;
    let jan4_iso_day = ((day_of_week(jan4) + 6) % 7 + 1) as i64;
    let first_monday = jan4 - (jan4_iso_day - 1);
    (iso_year, (days - first_monday) / 7 + 1, iso_day)
}

fn era_year(year: i64) -> (i64, bool) {
    if year <= 0 {
        (1 - year, true)
    } else {
        (year, false)
    }
}

/// Formats a timestamp without time zone.
pub fn timestamp<'a>(micros: i64, fmt: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    temporal(TemporalFields::timestamp(micros, None), fmt, arena)
}

/// Formats a timestamp with time zone after projecting it into the session
/// zone selected by PostgreSQL's overload.
pub fn timestamptz<'a>(
    utc_micros: i64,
    offset_seconds: i32,
    abbreviation: &str,
    fmt: &str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    temporal(
        TemporalFields::timestamp(
            utc_micros + i64::from(offset_seconds) * 1_000_000,
            Some(ZoneFields {
                offset_seconds,
                abbreviation,
            }),
        ),
        fmt,
        arena,
    )
}

/// Formats an interval using duration fields rather than inventing a calendar
/// date. Calendar-only format tokens are rejected with PostgreSQL's error.
pub fn interval<'a>(value: Interval, fmt: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    temporal(TemporalFields::interval(value), fmt, arena)
}

/// PostgreSQL implicitly casts `time` to interval for `to_char`; date-only
/// fields must therefore remain unavailable.
pub fn time<'a>(micros: i64, fmt: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    interval(
        Interval {
            months: 0,
            days: 0,
            micros,
        },
        fmt,
        arena,
    )
}

#[derive(Clone, Copy)]
struct ZoneFields<'a> {
    offset_seconds: i32,
    abbreviation: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TemporalKind {
    Timestamp,
    Interval,
}

#[derive(Clone, Copy)]
struct TemporalFields<'a> {
    kind: TemporalKind,
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    microsecond: i64,
    days: i64,
    zone: Option<ZoneFields<'a>>,
}

impl<'a> TemporalFields<'a> {
    fn timestamp(micros: i64, zone: Option<ZoneFields<'a>>) -> Self {
        use crate::sql::datetime::{PG_EPOCH_DAYS, civil_from_days};
        let days = micros.div_euclid(86_400_000_000);
        let time = micros.rem_euclid(86_400_000_000);
        let (year, month, day) = civil_from_days(days + PG_EPOCH_DAYS);
        Self {
            kind: TemporalKind::Timestamp,
            year,
            month: i64::from(month),
            day: i64::from(day),
            hour: time / 3_600_000_000,
            minute: time / 60_000_000 % 60,
            second: time / 1_000_000 % 60,
            microsecond: time % 1_000_000,
            days,
            zone,
        }
    }

    fn interval(value: Interval) -> Self {
        let hour = value.micros / 3_600_000_000;
        let remainder = value.micros % 3_600_000_000;
        Self {
            kind: TemporalKind::Interval,
            year: i64::from(value.months / 12),
            month: i64::from(value.months % 12),
            day: i64::from(value.days),
            hour,
            minute: remainder / 60_000_000,
            second: remainder % 60_000_000 / 1_000_000,
            microsecond: remainder % 1_000_000,
            days: 0,
            zone: None,
        }
    }

    fn calendar(self) -> Result<(), SqlError> {
        if self.kind == TemporalKind::Timestamp {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::INVALID_DATETIME_FORMAT,
                "invalid format specification for an interval value"
            ))
        }
    }
}

fn temporal<'a>(
    fields: TemporalFields<'_>,
    fmt: &str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    use crate::sql::datetime::{PG_EPOCH_DAYS, day_of_week, days_from_civil};
    if fmt.len() > MAX_TOKS {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "to_char format too long"
        ));
    }
    let calendar = fields.kind == TemporalKind::Timestamp;
    let adays = fields.days + PG_EPOCH_DAYS;
    let dow = if calendar {
        day_of_week(fields.days)
    } else {
        0
    };
    let doy = if calendar {
        adays - days_from_civil(fields.year, 1, 1) + 1
    } else {
        fields.year * 360 + fields.month * 30 + fields.day
    };
    let hour_sign = fields.hour.signum();
    let hour_abs = fields.hour.unsigned_abs() as i64;
    let hh12_abs = if hour_abs % 12 == 0 {
        12
    } else {
        hour_abs % 12
    };
    let hh12 = if calendar {
        hh12_abs
    } else {
        hour_sign * hh12_abs
    };
    let (display_year, bc) = if calendar {
        era_year(fields.year)
    } else {
        (fields.year, false)
    };
    let (iso_year, iso_week, iso_day) = if calendar {
        iso_week_date(fields.days)
    } else {
        (0, 0, 0)
    };
    let (display_iso_year, _) = era_year(iso_year);

    let mut out = StackStr::<2048>::new();
    let append = |out: &mut StackStr<2048>, text: &str| -> Result<(), SqlError> {
        out.write_str(text)
            .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "to_char output too long"))
    };
    let name = |out: &mut StackStr<2048>,
                s: &str,
                case: Case,
                pad: usize,
                fm: bool|
     -> Result<(), SqlError> {
        let mut buffer = [0u8; 16];
        let n = s.len().min(buffer.len());
        for (i, b) in s.bytes().take(n).enumerate() {
            buffer[i] = match case {
                Case::Upper => b.to_ascii_uppercase(),
                Case::Lower => b.to_ascii_lowercase(),
                Case::Title => b,
            };
        }
        append(out, core::str::from_utf8(&buffer[..n]).unwrap_or(""))?;
        if !fm {
            for _ in n..pad {
                append(out, " ")?;
            }
        }
        Ok(())
    };
    let num = |out: &mut StackStr<2048>, v: i64, width: usize, fm: bool| -> Result<(), SqlError> {
        let negative = v < 0;
        let s = crate::stack_format!(24, "{}", v.unsigned_abs());
        if negative {
            append(out, "-")?;
        }
        if !fm {
            for _ in s.as_str().len()..width {
                append(out, "0")?;
            }
        }
        append(out, s.as_str())
    };
    let comma_year = |out: &mut StackStr<2048>, value: i64, fm: bool| -> Result<(), SqlError> {
        let text = stack_format!(32, "{}", value.unsigned_abs());
        if value < 0 {
            append(out, "-")?;
        }
        let width = text.as_str().len().max(4);
        let mut padded = [b'0'; 32];
        let start = width - text.as_str().len();
        padded[start..width].copy_from_slice(text.as_str().as_bytes());
        let bytes = &padded[..width];
        for (index, byte) in bytes.iter().enumerate() {
            if index > 0 && (width - index).is_multiple_of(3) {
                append(out, ",")?;
            }
            append(
                out,
                core::str::from_utf8(core::slice::from_ref(byte)).expect("ascii"),
            )?;
        }
        let _ = fm;
        Ok(())
    };

    let fb = fmt.as_bytes();
    let mut i = 0usize;
    while i < fb.len() {
        let mut fm = false;
        loop {
            if i + 1 < fb.len()
                && (fb[i..i + 2].eq_ignore_ascii_case(b"FM")
                    || fb[i..i + 2].eq_ignore_ascii_case(b"TM"))
            {
                fm = true;
                i += 2;
            } else if i + 1 < fb.len() && fb[i..i + 2].eq_ignore_ascii_case(b"FX") {
                i += 2;
            } else {
                break;
            }
        }
        if i >= fb.len() {
            break;
        }
        if fb[i] == b'"' {
            i += 1;
            while i < fb.len() && fb[i] != b'"' {
                if fb[i] == b'\\' && i + 1 < fb.len() {
                    i += 1;
                }
                let width = fmt[i..]
                    .chars()
                    .next()
                    .expect("format byte index is a character boundary")
                    .len_utf8();
                append(&mut out, &fmt[i..i + width])?;
                i += width;
            }
            if i < fb.len() {
                i += 1;
            }
            continue;
        }
        let rest = &fb[i..];
        let m = |w: &[u8]| rest.len() >= w.len() && rest[..w.len()].eq_ignore_ascii_case(w);
        let mut ordinal = None;
        if m(b"IYYY") {
            fields.calendar()?;
            num(&mut out, display_iso_year, 4, fm)?;
            ordinal = Some(display_iso_year);
            i += 4;
        } else if m(b"IDDD") {
            fields.calendar()?;
            let jan4 = days_from_civil(iso_year, 1, 4) - PG_EPOCH_DAYS;
            let first_monday = jan4 - (iso_week_date(jan4).2 - 1);
            let value = fields.days - first_monday + 1;
            num(&mut out, value, 3, fm)?;
            ordinal = Some(value);
            i += 4;
        } else if m(b"Y,YYY") {
            comma_year(&mut out, display_year, fm)?;
            ordinal = Some(display_year);
            i += 5;
        } else if m(b"YYYY") {
            num(&mut out, display_year, 4, fm)?;
            ordinal = Some(display_year);
            i += 4;
        } else if m(b"HH24") {
            num(&mut out, fields.hour, 2, fm)?;
            ordinal = Some(fields.hour);
            i += 4;
        } else if m(b"HH12") {
            num(&mut out, hh12, 2, fm)?;
            ordinal = Some(hh12);
            i += 4;
        } else if m(b"SSSSS") {
            let value = fields.hour * 3600 + fields.minute * 60 + fields.second;
            num(&mut out, value, 1, fm)?;
            ordinal = Some(value);
            i += 5;
        } else if m(b"SSSS") {
            let value = fields.hour * 3600 + fields.minute * 60 + fields.second;
            num(&mut out, value, 1, fm)?;
            ordinal = Some(value);
            i += 4;
        } else if m(b"MONTH") {
            fields.calendar()?;
            name(
                &mut out,
                MONTH_FULL[(fields.month - 1) as usize],
                name_case(&rest[..5]),
                9,
                fm,
            )?;
            i += 5;
        } else if m(b"MON") {
            fields.calendar()?;
            name(
                &mut out,
                MONTH_ABBR[(fields.month - 1) as usize],
                name_case(&rest[..3]),
                3,
                fm,
            )?;
            i += 3;
        } else if m(b"DAY") {
            fields.calendar()?;
            name(&mut out, DAY_FULL[dow], name_case(&rest[..3]), 9, fm)?;
            i += 3;
        } else if m(b"DDD") {
            num(&mut out, doy, 3, fm)?;
            ordinal = Some(doy);
            i += 3;
        } else if m(b"IYY") {
            fields.calendar()?;
            num(&mut out, display_iso_year % 1000, 3, fm)?;
            ordinal = Some(display_iso_year % 1000);
            i += 3;
        } else if m(b"DY") {
            fields.calendar()?;
            name(&mut out, DAY_ABBR[dow], name_case(&rest[..2]), 3, fm)?;
            i += 2;
        } else if m(b"YYY") {
            let value = display_year % 1000;
            num(&mut out, value, 3, fm)?;
            ordinal = Some(value);
            i += 3;
        } else if m(b"IY") {
            fields.calendar()?;
            let value = display_iso_year % 100;
            num(&mut out, value, 2, fm)?;
            ordinal = Some(value);
            i += 2;
        } else if m(b"IW") {
            fields.calendar()?;
            num(&mut out, iso_week, 2, fm)?;
            ordinal = Some(iso_week);
            i += 2;
        } else if m(b"HH") {
            num(&mut out, hh12, 2, fm)?;
            ordinal = Some(hh12);
            i += 2;
        } else if m(b"YY") {
            let value = display_year % 100;
            num(&mut out, value, 2, fm)?;
            ordinal = Some(value);
            i += 2;
        } else if m(b"MI") {
            num(&mut out, fields.minute, 2, fm)?;
            ordinal = Some(fields.minute);
            i += 2;
        } else if m(b"MM") {
            num(&mut out, fields.month, 2, fm)?;
            ordinal = Some(fields.month);
            i += 2;
        } else if m(b"MS") {
            let value = fields.microsecond / 1000;
            num(&mut out, value, 3, fm)?;
            ordinal = Some(value);
            i += 2;
        } else if m(b"US") {
            num(&mut out, fields.microsecond, 6, fm)?;
            ordinal = Some(fields.microsecond);
            i += 2;
        } else if m(b"SS") {
            num(&mut out, fields.second, 2, fm)?;
            ordinal = Some(fields.second);
            i += 2;
        } else if m(b"DD") {
            num(&mut out, fields.day, 2, fm)?;
            ordinal = Some(fields.day);
            i += 2;
        } else if m(b"WW") {
            let value = (doy - doy.signum()).div_euclid(7) + doy.signum();
            num(&mut out, value, 2, fm)?;
            ordinal = Some(value);
            i += 2;
        } else if m(b"RM") {
            let month = fields.month.unsigned_abs() as usize;
            if !(1..=12).contains(&month) {
                return Err(sql_err!(
                    sqlstate::DATETIME_FIELD_OVERFLOW,
                    "date/time field value out of range"
                ));
            }
            name(
                &mut out,
                MONTH_ROMAN[month - 1],
                name_case(&rest[..2]),
                4,
                fm,
            )?;
            i += 2;
        } else if m(b"A.M.") || m(b"P.M.") {
            let mer = if hour_abs % 24 < 12 { "A.M." } else { "P.M." };
            name(&mut out, mer, name_case(&rest[..4]), 0, true)?;
            i += 4;
        } else if m(b"AM") || m(b"PM") {
            let mer = if hour_abs % 24 < 12 { "AM" } else { "PM" };
            name(&mut out, mer, name_case(&rest[..2]), 0, true)?;
            i += 2;
        } else if m(b"A.D.") || m(b"B.C.") {
            fields.calendar()?;
            let era = if bc { "B.C." } else { "A.D." };
            name(&mut out, era, name_case(&rest[..4]), 0, true)?;
            i += 4;
        } else if m(b"AD") || m(b"BC") {
            fields.calendar()?;
            let era = if bc { "BC" } else { "AD" };
            name(&mut out, era, name_case(&rest[..2]), 0, true)?;
            i += 2;
        } else if m(b"CC") {
            let century = if calendar {
                let magnitude = (display_year - 1) / 100 + 1;
                if bc { -magnitude } else { magnitude }
            } else {
                display_year / 100
            };
            num(&mut out, century, 2, fm)?;
            ordinal = Some(century);
            i += 2;
        } else if m(b"Q") {
            let value =
                (fields.month - fields.month.signum()).div_euclid(3) + fields.month.signum();
            num(&mut out, value, 1, fm)?;
            ordinal = Some(value);
            i += 1;
        } else if m(b"ID") {
            fields.calendar()?;
            num(&mut out, iso_day, 1, fm)?;
            ordinal = Some(iso_day);
            i += 2;
        } else if m(b"D") {
            fields.calendar()?;
            let value = (dow + 1) as i64;
            num(&mut out, value, 1, fm)?;
            ordinal = Some(value);
            i += 1;
        } else if m(b"W") {
            let value = (fields.day - fields.day.signum()).div_euclid(7) + fields.day.signum();
            num(&mut out, value, 1, fm)?;
            ordinal = Some(value);
            i += 1;
        } else if m(b"I") {
            fields.calendar()?;
            let value = display_iso_year % 10;
            num(&mut out, value, 1, fm)?;
            ordinal = Some(value);
            i += 1;
        } else if m(b"Y") {
            let value = display_year % 10;
            num(&mut out, value, 1, fm)?;
            ordinal = Some(value);
            i += 1;
        } else if rest.len() >= 3
            && rest[0].eq_ignore_ascii_case(&b'F')
            && rest[1].eq_ignore_ascii_case(&b'F')
            && matches!(rest[2], b'1'..=b'6')
        {
            let width = usize::from(rest[2] - b'0');
            let value = fields.microsecond / 10i64.pow((6 - width) as u32);
            num(&mut out, value, width, fm)?;
            ordinal = Some(value);
            i += 3;
        } else if m(b"TZH") {
            let offset = fields.zone.map_or(0, |zone| zone.offset_seconds);
            append(&mut out, if offset < 0 { "-" } else { "+" })?;
            num(
                &mut out,
                i64::from(offset).unsigned_abs() as i64 / 3600,
                2,
                false,
            )?;
            i += 3;
        } else if m(b"TZM") {
            let offset = fields.zone.map_or(0, |zone| zone.offset_seconds);
            num(
                &mut out,
                i64::from(offset).unsigned_abs() as i64 / 60 % 60,
                2,
                false,
            )?;
            i += 3;
        } else if m(b"TZ") {
            fields.calendar()?;
            if let Some(zone) = fields.zone {
                name(&mut out, zone.abbreviation, name_case(&rest[..2]), 0, true)?;
            }
            i += 2;
        } else if m(b"OF") {
            fields.calendar()?;
            let offset = fields.zone.map_or(0, |zone| zone.offset_seconds);
            append(&mut out, if offset < 0 { "-" } else { "+" })?;
            num(
                &mut out,
                i64::from(offset).unsigned_abs() as i64 / 3600,
                2,
                false,
            )?;
            let minute = i64::from(offset).unsigned_abs() as i64 / 60 % 60;
            if minute != 0 {
                append(&mut out, ":")?;
                num(&mut out, minute, 2, false)?;
            }
            i += 2;
        } else if m(b"J") {
            fields.calendar()?;
            let value = adays + 2_440_588;
            num(&mut out, value, 1, fm)?;
            ordinal = Some(value);
            i += 1;
        } else {
            let width = fmt[i..]
                .chars()
                .next()
                .expect("format byte index is a character boundary")
                .len_utf8();
            append(&mut out, &fmt[i..i + width])?;
            i += width;
        }
        if let Some(value) = ordinal
            && i + 1 < fb.len()
            && fb[i..i + 2].eq_ignore_ascii_case(b"TH")
        {
            let digits = stack_format!(32, "{}", value.unsigned_abs());
            let suffix = ordinal_suffix(digits.as_str().as_bytes());
            if fb[i] == b't' {
                let lower = stack_format!(
                    2,
                    "{}{}",
                    suffix.as_bytes()[0].to_ascii_lowercase() as char,
                    suffix.as_bytes()[1].to_ascii_lowercase() as char
                );
                append(&mut out, lower.as_str())?;
            } else {
                append(&mut out, suffix)?;
            }
            i += 2;
        }
        if i + 1 < fb.len() && fb[i..i + 2].eq_ignore_ascii_case(b"SP") {
            i += 2;
        }
    }
    arena
        .alloc_str(out.as_str())
        .map_err(|_| sql_err!(sqlstate::OUT_OF_MEMORY, "out of memory"))
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
        number(&Numeric::parse(v, a).unwrap(), f, false, None, a)
            .unwrap()
            .to_string()
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
        assert_eq!(tn("12abc34", "99L99"), "12");
        assert_eq!(tn("<123>", "999PR"), "-123");
        assert_eq!(tn("123-", "999MI"), "-123");
        assert_eq!(tn("XII", "RN"), "12");
        assert_eq!(tn("xiv", "rn"), "14");
        assert_eq!(tn("12", "9V9"), "1.20000000000000000");
        assert!(to_number("IIII", "RN", &a).is_err());
        assert!(to_number("abc", "999", &a).is_err());
    }

    #[test]
    fn timestamp_formats_match_postgres() {
        let a = arena();
        // 2024-06-15 14:07:09.123456 (a Saturday) in micros since 2000-01-01.
        let micros =
            crate::sql::datetime::parse_timestamp("2024-06-15 14:07:09.123456", false).unwrap();
        let tc = |f: &str| timestamp(micros, f, &a).unwrap().to_string();
        assert_eq!(tc("YYYY-MM-DD HH24:MI:SS"), "2024-06-15 14:07:09");
        assert_eq!(tc("HH12:MI:SS AM"), "02:07:09 PM");
        assert_eq!(tc("Mon DD, YYYY"), "Jun 15, 2024");
        assert_eq!(tc("Month"), "June     ");
        assert_eq!(tc("FMMonth FMDD"), "June 15");
        assert_eq!(tc("Day DY D"), "Saturday  SAT 7");
        assert_eq!(tc("Q WW DDD"), "2 24 167");
        assert_eq!(tc("US"), "123456");
        assert_eq!(tc("ZZZ"), "ZZZ");
        assert_eq!(
            tc("FF1|FF2|FF3|FF4|FF5|FF6|SSSSS|Y,YYY|IDDD|J|DDTH|\"X\"YYYY"),
            "1|12|123|1234|12345|123456|50829|2,024|167|2460477|15TH|X2024"
        );
        assert_eq!(tc("TZ|TZH|TZM|OF"), "|+00|00|+00");
        assert_eq!(tc("YYYY年\"月\"MM"), "2024年月06");
    }

    #[test]
    fn interval_formats_match_postgres() {
        let a = arena();
        let value = Interval {
            months: 27,
            days: 15,
            micros: 36 * 3_600_000_000 + 7 * 60_000_000 + 5_123_456,
        };
        assert_eq!(
            interval(
                value,
                "YYYY|MM|DDD|DD|HH|HH24|MI|SS|MS|US|FF4|SSSSS|W|WW|CC|Q|RM|DDTH",
                &a,
            )
            .unwrap(),
            "0002|03|825|15|12|36|07|05|123|123456|1234|130025|3|118|00|1|III |15TH"
        );
        assert!(interval(value, "DAY", &a).is_err());
    }

    #[test]
    fn timestamp_calendar_models_match_postgres() {
        let a = arena();
        let boundary =
            crate::sql::datetime::parse_timestamp("2021-01-01 13:02:03.456789", false).unwrap();
        let tc = |micros, f: &str| timestamp(micros, f, &a).unwrap().to_string();
        assert_eq!(
            tc(boundary, "IYYY-IW-ID|YYYY-CC-W-WW-D-DDD|RM|A.M.|AD|SSSS"),
            "2020-53-5|2021-21-1-01-6-001|I   |P.M.|AD|46923"
        );
        let december = crate::sql::datetime::parse_timestamp("2020-12-31 00:00:00", false).unwrap();
        assert_eq!(
            tc(
                december,
                "IYYY-IW-ID|YYYY-MM-DD|RM|Month|FMMonth|A.M.|AM|A.D.|AD|CC|SSSS"
            ),
            "2020-53-4|2020-12-31|XII |December |December|A.M.|AM|A.D.|AD|21|0"
        );
        let bc = crate::sql::datetime::make_timestamp(0, 1, 1, 0, 0, 0.0).unwrap();
        assert_eq!(
            tc(bc, "YYYY|YY|Y|CC|AD|A.D.|IYYY-IW-ID"),
            "0001|01|1|-01|BC|B.C.|0002-52-6"
        );
    }

    #[test]
    fn unsupported_codes_are_loud() {
        // Formerly-rejected codes now format (verified against PostgreSQL
        // 18.4); invalid combinations still error loudly.
        let a = arena();
        assert_eq!(
            number(&Numeric::parse("5", &a).unwrap(), "999MI", false, None, &a).unwrap(),
            "  5 "
        );
        assert_eq!(
            number(&Numeric::parse("5", &a).unwrap(), "RN", false, None, &a).unwrap(),
            "              V"
        );
        assert_eq!(
            number(&Numeric::parse("5", &a).unwrap(), "9EEEE", false, None, &a).unwrap(),
            " 5e+00"
        );
        assert!(number(&Numeric::parse("5", &a).unwrap(), "S999MI", false, None, &a).is_err());
        assert!(number(&Numeric::parse("5", &a).unwrap(), "9.9V9", false, None, &a).is_err());
        assert!(number(&Numeric::parse("5", &a).unwrap(), "EEEE9", false, None, &a).is_err());
    }

    #[test]
    fn format_models_do_not_allocate() {
        let a = arena();
        let numeric = Numeric::parse("1234.50", &a).unwrap();
        let fourteen = Numeric::parse("14", &a).unwrap();
        let timestamp =
            crate::sql::datetime::parse_timestamp("2024-06-15 14:07:09.123456", false).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                number(&numeric, "FM9,999.99", false, None, &a).unwrap(),
                "1,234.5"
            );
            assert_eq!(to_number("XIV", "RN", &a).unwrap(), fourteen);
            assert_eq!(
                self::timestamp(timestamp, "YYYY-MM-DD HH24:MI:SS.US", &a).unwrap(),
                "2024-06-15 14:07:09.123456"
            );
            assert!(
                crate::sql::datetime::parse_formatted("22-24-2nd-Monday", "CC-YY-Wth-DAY").is_ok()
            );
        });
    }
}
