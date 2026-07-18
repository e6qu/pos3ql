//! Arbitrary-precision decimal (`NUMERIC`), matching PostgreSQL's semantics
//! and representation.
//!
//! The value model is PostgreSQL's own (see `src/backend/utils/adt/numeric.c`,
//! PostgreSQL License): a sign, a base-10000 digit array most-significant
//! first, a `weight` giving the power of 10000 of the first digit, and a
//! display scale `dscale` (fractional decimal digits shown). Storing it this
//! way makes the binary wire format a direct copy and keeps arithmetic in
//! base 10000.
//!
//! Digit storage is arena-allocated (the per-statement bump arena), like
//! `Text`/`Bytea`; intermediates use fixed stack buffers, so nothing touches
//! the heap after startup. Precision is bounded by [`MAX_NDIGITS`]; exceeding
//! it is a loud `22003`, never a silent truncation.

use core::cmp::Ordering;
use core::fmt;

use crate::mem::arena::Arena;
use crate::sql_err;

use super::eval::{sqlstate, SqlError};

/// Base-10000: each digit holds four decimal places.
pub const NBASE: i32 = 10_000;
pub const DEC_DIGITS: usize = 4;

/// Maximum base-10000 digits in any value (≈ 4× this many decimal digits).
/// PostgreSQL caps precision near 16383 decimal digits; this is comfortably
/// within that and bounds every intermediate buffer.
pub const MAX_NDIGITS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sign {
    Pos,
    Neg,
    NaN,
}

/// Rounding mode for [`Numeric::round_scale`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundMode {
    /// Round half away from zero (PostgreSQL `round`).
    HalfAwayZero,
    /// Toward negative infinity (`floor`).
    Floor,
    /// Toward positive infinity (`ceil`).
    Ceil,
    /// Toward zero (`trunc`).
    Trunc,
}

/// A NUMERIC value. `digits` holds base-10000 digits MSD-first as
/// little-endian `i16` pairs (2 bytes each, values 0..9999), canonical (no
/// leading/trailing all-zero digit). Byte-backed so a value can borrow
/// directly from a stored row, a wire buffer, or the arena — like Text and
/// Bytea. The empty slice is zero. Access digits via [`Self::digit`] /
/// [`Self::ndigits`].
#[derive(Debug, Clone, Copy)]
pub struct Numeric<'a> {
    pub sign: Sign,
    pub weight: i16,
    pub dscale: u16,
    pub digits: &'a [u8],
}

impl PartialEq for Numeric<'_> {
    fn eq(&self, other: &Self) -> bool {
        compare(self, other) == Ordering::Equal
    }
}

impl<'a> Numeric<'a> {
    pub const ZERO: Numeric<'static> = Numeric {
        sign: Sign::Pos,
        weight: 0,
        dscale: 0,
        digits: &[],
    };

    pub const NAN: Numeric<'static> = Numeric {
        sign: Sign::NaN,
        weight: 0,
        dscale: 0,
        digits: &[],
    };

    pub fn is_zero(&self) -> bool {
        self.sign != Sign::NaN && self.digits.is_empty()
    }

    pub fn is_nan(&self) -> bool {
        self.sign == Sign::NaN
    }

    /// Number of base-10000 digits.
    pub fn ndigits(&self) -> usize {
        self.digits.len() / 2
    }

    /// Rounds to `scale` fractional digits under `mode`, returning a new value
    /// in `arena`. Works on the decimal text so all carry logic reuses `parse`.
    pub fn round_scale<'b>(
        &self,
        scale: usize,
        mode: RoundMode,
        arena: &'b Arena,
    ) -> Result<Numeric<'b>, SqlError> {
        if self.is_nan() {
            return Ok(Numeric::NAN);
        }
        const DIG: usize = 2100;
        let text = crate::stack_format!(2100, "{}", self);
        let s = text.as_str();
        let (neg, body) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s),
        };
        let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
        let (int_b, frac_b) = (int_part.as_bytes(), frac_part.as_bytes());
        let int_len = int_b.len();
        if int_len + scale + 2 >= DIG {
            // Beyond what we round here; return as-is (already exact enough).
            return Numeric::parse(s, arena);
        }
        let mut digits = [b'0'; DIG];
        digits[..int_len].copy_from_slice(int_b);
        for i in 0..scale {
            digits[int_len + i] = *frac_b.get(i).unwrap_or(&b'0');
        }
        let first_dropped = frac_b.get(scale).copied().unwrap_or(b'0');
        let has_dropped_nonzero = frac_b.get(scale..).is_some_and(|d| d.iter().any(|&c| c != b'0'));
        let round_up = match mode {
            RoundMode::HalfAwayZero => first_dropped >= b'5',
            RoundMode::Trunc => false,
            RoundMode::Floor => neg && has_dropped_nonzero,
            RoundMode::Ceil => !neg && has_dropped_nonzero,
        };
        let mut carry = round_up;
        let mut i = int_len + scale;
        while carry && i > 0 {
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                carry = false;
            }
        }
        let mut out = [0u8; DIG + 8];
        let mut k = 0;
        if neg {
            out[k] = b'-';
            k += 1;
        }
        if carry {
            out[k] = b'1';
            k += 1;
        }
        out[k..k + int_len].copy_from_slice(&digits[..int_len]);
        k += int_len;
        if scale > 0 {
            out[k] = b'.';
            k += 1;
            out[k..k + scale].copy_from_slice(&digits[int_len..int_len + scale]);
            k += scale;
        }
        let rounded = core::str::from_utf8(&out[..k]).expect("ascii digits");
        Numeric::parse(rounded, arena)
    }

    /// The `i`-th base-10000 digit (0..9999), MSD-first.
    pub fn digit(&self, i: usize) -> i16 {
        i16::from_le_bytes([self.digits[i * 2], self.digits[i * 2 + 1]])
    }

    /// Parses a decimal string as PostgreSQL's numeric_in does: optional
    /// sign, digits with an optional '.', optional exponent, or `NaN`.
    pub fn parse(s: &str, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("nan") {
            return Ok(Numeric::NAN);
        }
        let bad = || {
            sql_err!(
                sqlstate::INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type numeric: \"{}\"",
                s
            )
        };
        let bytes = t.as_bytes();
        let mut i = 0;
        let mut neg = false;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            neg = bytes[i] == b'-';
            i += 1;
        }
        // Collect the decimal digit string and the position of the point.
        let mut dec_digits: [u8; MAX_NDIGITS * DEC_DIGITS] = [0; MAX_NDIGITS * DEC_DIGITS];
        let mut ndec = 0usize;
        let mut point_at: Option<usize> = None; // index into dec_digits
        let mut saw_digit = false;
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_ascii_digit() {
                if ndec >= dec_digits.len() {
                    return Err(overflow());
                }
                dec_digits[ndec] = c - b'0';
                ndec += 1;
                saw_digit = true;
                i += 1;
            } else if c == b'.' && point_at.is_none() {
                point_at = Some(ndec);
                i += 1;
            } else {
                break;
            }
        }
        if !saw_digit {
            return Err(bad());
        }
        // Optional exponent.
        let mut exp: i64 = 0;
        if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
            i += 1;
            let mut esign = 1i64;
            if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                if bytes[i] == b'-' {
                    esign = -1;
                }
                i += 1;
            }
            let estart = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                exp = exp
                    .checked_mul(10)
                    .and_then(|v| v.checked_add((bytes[i] - b'0') as i64))
                    .ok_or_else(overflow)?;
                i += 1;
            }
            if i == estart {
                return Err(bad());
            }
            exp *= esign;
        }
        if i != bytes.len() {
            return Err(bad());
        }

        // Decimal exponent of the least-significant collected digit.
        let point = point_at.unwrap_or(ndec);
        let frac_digits = ndec - point; // decimal places present
        // dscale before applying exponent.
        let dscale_dec = frac_digits as i64 - exp;
        let dscale = dscale_dec.max(0) as u16;

        Self::from_decimal_digits(&dec_digits[..ndec], point as i64 + exp, dscale, neg, arena)
    }

    /// Builds a Numeric from an array of decimal digits (0..9) with an
    /// implied decimal point after `int_len` leading digits (which may be
    /// negative or exceed the array), a target display scale, and a sign.
    fn from_decimal_digits(
        dec: &[u8],
        int_len: i64,
        dscale: u16,
        neg: bool,
        arena: &'a Arena,
    ) -> Result<Numeric<'a>, SqlError> {
        // Strip leading zeros; track how many we dropped.
        let mut start = 0;
        while start < dec.len() && dec[start] == 0 {
            start += 1;
        }
        let mut end = dec.len();
        while end > start && dec[end - 1] == 0 {
            end -= 1;
        }
        if start >= end {
            // All zero.
            return Ok(Numeric {
                sign: Sign::Pos,
                weight: 0,
                dscale,
                digits: &[],
            });
        }
        // Decimal exponent of dec[start]: number of decimal places between it
        // and the point. int_len counts digits before the point in the
        // original array (index of the point). So dec[k] has decimal weight
        // (int_len - 1 - k).
        let msd_decimal_weight = int_len - 1 - start as i64;

        // Align to base-10000 boundaries. A base-10000 digit at weight w
        // covers decimal weights [4w, 4w+3]. Pad the significant decimal run
        // with leading zeros so its first decimal weight is ≡ 3 mod 4.
        let first_dw = msd_decimal_weight;
        let lead_pad = ((first_dw % 4) + 4) % 4; // decimals to prepend
        let lead_pad = (3 - lead_pad + 4) % 4;
        let sig = &dec[start..end];
        let total = lead_pad as usize + sig.len();
        let n_base = total.div_ceil(DEC_DIGITS);
        if n_base > MAX_NDIGITS {
            return Err(overflow());
        }
        let mut buf: [i16; MAX_NDIGITS] = [0; MAX_NDIGITS];
        // Fill a scratch of decimal digits: lead_pad zeros, sig, trailing pad
        // to a multiple of 4.
        let mut scratch: [u8; MAX_NDIGITS * DEC_DIGITS + 8] = [0; MAX_NDIGITS * DEC_DIGITS + 8];
        let pad = lead_pad as usize;
        for (k, &d) in sig.iter().enumerate() {
            scratch[pad + k] = d;
        }
        let filled = lead_pad as usize + sig.len();
        let padded = n_base * DEC_DIGITS;
        // trailing zeros already present (scratch is zeroed)
        let _ = filled;
        for (bi, chunk) in scratch[..padded].chunks(DEC_DIGITS).enumerate() {
            let mut v = 0i16;
            for &d in chunk {
                v = v * 10 + d as i16;
            }
            buf[bi] = v;
        }
        // weight of buf[0] in base-10000: msd decimal weight of buf[0] is
        // (first_dw + lead_pad) ... buf[0] covers decimals [first_dw+lead_pad
        // down]. Its base-10000 weight = (first_dw + lead_pad) / 4.
        let base_weight = (first_dw + lead_pad).div_euclid(4);

        // Trim trailing zero base-digits (they don't affect value; dscale
        // keeps display width).
        let mut nb = n_base;
        while nb > 0 && buf[nb - 1] == 0 {
            nb -= 1;
        }
        // Trim leading zero base-digits.
        let mut lead = 0;
        while lead < nb && buf[lead] == 0 {
            lead += 1;
        }
        let ndigits = nb - lead;
        if ndigits == 0 {
            return Ok(Numeric {
                sign: Sign::Pos,
                weight: 0,
                dscale,
                digits: &[],
            });
        }
        let weight = (base_weight - lead as i64) as i16;
        Ok(Numeric {
            sign: if neg { Sign::Neg } else { Sign::Pos },
            weight,
            dscale,
            digits: pack(&buf[lead..nb], arena)?,
        })
    }

    /// Exact conversion from a 128-bit integer.
    pub fn from_i128(mut v: i128, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
        if v == 0 {
            return Ok(Numeric::ZERO);
        }
        let neg = v < 0;
        // Work in magnitude; i128::MIN handled via unsigned.
        let mut mag: u128 = if neg {
            v.unsigned_abs()
        } else {
            v as u128
        };
        let _ = &mut v;
        let mut rev: [i16; MAX_NDIGITS] = [0; MAX_NDIGITS];
        let mut n = 0;
        while mag > 0 {
            rev[n] = (mag % NBASE as u128) as i16;
            mag /= NBASE as u128;
            n += 1;
        }
        // rev is least-significant first; reverse into canonical MSD-first.
        let mut buf: [i16; MAX_NDIGITS] = [0; MAX_NDIGITS];
        for k in 0..n {
            buf[k] = rev[n - 1 - k];
        }
        let weight = (n - 1) as i16;
        // Trim trailing zero digits.
        let mut nb = n;
        while nb > 0 && buf[nb - 1] == 0 {
            nb -= 1;
        }
        Ok(Numeric {
            sign: if neg { Sign::Neg } else { Sign::Pos },
            weight,
            dscale: 0,
            digits: pack(&buf[..nb], arena)?,
        })
    }

    pub fn from_i64(v: i64, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
        Self::from_i128(v as i128, arena)
    }

    /// Builds a Numeric for `v` borrowing digit bytes from a caller stack
    /// buffer (>= 20 bytes), for allocation-free comparison. i64 needs at
    /// most 5 base-10000 digits (10 bytes).
    pub fn from_i64_stack(v: i64, buf: &'a mut [u8; 20]) -> Numeric<'a> {
        if v == 0 {
            return Numeric { sign: Sign::Pos, weight: 0, dscale: 0, digits: &[] };
        }
        let neg = v < 0;
        let mut mag = (v as i128).unsigned_abs();
        let mut rev = [0i16; 10];
        let mut n = 0;
        while mag > 0 {
            rev[n] = (mag % NBASE as u128) as i16;
            mag /= NBASE as u128;
            n += 1;
        }
        // MSD-first into buf; trim trailing zero digits.
        let mut nb = n;
        while nb > 0 && rev[0] == 0 {
            // trailing (least-significant) zero: drop from the low end
            for k in 0..nb - 1 {
                rev[k] = rev[k + 1];
            }
            nb -= 1;
        }
        for k in 0..nb {
            let d = rev[n - 1 - k];
            buf[k * 2..k * 2 + 2].copy_from_slice(&d.to_le_bytes());
        }
        Numeric {
            sign: if neg { Sign::Neg } else { Sign::Pos },
            weight: (n - 1) as i16,
            dscale: 0,
            digits: &buf[..nb * 2],
        }
    }

    /// Approximate conversion to f64 (for float casts / mixed arithmetic).
    pub fn to_f64(&self) -> f64 {
        if self.is_nan() {
            return f64::NAN;
        }
        let mut val = 0.0f64;
        for k in 0..self.ndigits() {
            val = val * NBASE as f64 + self.digit(k) as f64;
        }
        // digits[0] has weight `weight`; we multiplied as if weight 0 for the
        // last, so scale by NBASE^(weight - (ndigits-1)).
        let exp = self.weight as i32 - (self.ndigits() as i32 - 1);
        val *= (NBASE as f64).powi(exp);
        if self.sign == Sign::Neg {
            -val
        } else {
            val
        }
    }

    /// Rounds to an i64, erroring on overflow (for int casts).
    pub fn to_i64(&self) -> Result<i64, SqlError> {
        if self.is_nan() {
            return Err(sql_err!(
                sqlstate::NUMERIC_OUT_OF_RANGE,
                "cannot convert NaN to integer"
            ));
        }
        // Build integer magnitude from digits above/at weight 0, rounding the
        // fractional part.
        let mut acc: i128 = 0;
        for k in 0..self.ndigits() {
            let d = self.digit(k);
            let w = self.weight as i32 - k as i32;
            if w < 0 {
                // Fractional: round half-up on the first fractional base-digit.
                if w == -1 && d >= (NBASE as i16) / 2 {
                    acc += 1;
                }
                break;
            }
            acc = acc
                .checked_mul(NBASE as i128)
                .and_then(|a| a.checked_add(d as i128))
                .ok_or_else(overflow_int)?;
        }
        // Account for weight gaps (trailing implicit zero base-digits above 0).
        let lowest = self.weight as i32 - (self.digits.len() as i32 - 1);
        if lowest > 0 {
            for _ in 0..lowest {
                acc = acc.checked_mul(NBASE as i128).ok_or_else(overflow_int)?;
            }
        }
        if self.sign == Sign::Neg {
            acc = -acc;
        }
        i64::try_from(acc).map_err(|_| overflow_int())
    }
}

fn overflow() -> SqlError {
    sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "value overflows numeric format")
}

/// Serializes base-10000 digits (as i16) into LE byte pairs in the arena.
fn pack<'a>(digits: &[i16], arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    let mut bytes: [u8; MAX_NDIGITS * 2] = [0; MAX_NDIGITS * 2];
    if digits.len() > MAX_NDIGITS {
        return Err(overflow());
    }
    for (k, &d) in digits.iter().enumerate() {
        bytes[k * 2..k * 2 + 2].copy_from_slice(&d.to_le_bytes());
    }
    let out = arena
        .alloc_slice_copy(&bytes[..digits.len() * 2])
        .map_err(|_| overflow())?;
    Ok(&*out)
}

fn overflow_int() -> SqlError {
    sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "integer out of range")
}

impl fmt::Display for Numeric<'_> {
    /// Formats exactly as PostgreSQL's numeric_out: a plain decimal string
    /// with `dscale` fractional digits (no exponent for ordinary values).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_nan() {
            return f.write_str("NaN");
        }
        if self.sign == Sign::Neg {
            f.write_str("-")?;
        }
        // Expand base-10000 digits into decimal digits with the point placed
        // per `weight`. Integer part covers decimal weights >= 0.
        // Leftmost decimal weight = 4*weight + 3 (top digit of digits[0]),
        // but leading zeros of digits[0] are suppressed.
        let dscale = self.dscale as i32;
        // Build the full decimal digit sequence.
        // The most significant base-digit has weight self.weight; its decimal
        // positions are [4*weight+3 .. 4*weight]. The least significant shown
        // decimal position is -dscale.
        let hi_dweight = self.weight as i32 * DEC_DIGITS as i32 + (DEC_DIGITS as i32 - 1);
        let lo_dweight = -dscale;

        // Emit integer part.
        if hi_dweight < 0 {
            // Value magnitude < 1: integer part is exactly "0".
            f.write_str("0")?;
        } else {
            let mut started = false;
            let mut dw = hi_dweight;
            while dw >= 0 {
                let d = self.decimal_digit_at(dw);
                if !started && d == 0 && dw > 0 {
                    dw -= 1;
                    continue;
                }
                started = true;
                write_digit(f, d)?;
                dw -= 1;
            }
            if !started {
                f.write_str("0")?;
            }
        }
        // Fractional part.
        if dscale > 0 {
            f.write_str(".")?;
            let mut dw = -1;
            while dw >= lo_dweight {
                write_digit(f, self.decimal_digit_at(dw))?;
                dw -= 1;
            }
        }
        Ok(())
    }
}

impl Numeric<'_> {
    /// The decimal digit (0..9) at decimal weight `dw` (10^dw place).
    fn decimal_digit_at(&self, dw: i32) -> u8 {
        // Which base-10000 digit and which of its 4 decimal positions?
        let base_w = dw.div_euclid(DEC_DIGITS as i32);
        let within = dw.rem_euclid(DEC_DIGITS as i32); // 0..3, 0 = least sig
        let idx = self.weight as i32 - base_w;
        if idx < 0 || idx as usize >= self.ndigits() {
            return 0;
        }
        let mut v = self.digit(idx as usize) as i32;
        for _ in 0..within {
            v /= 10;
        }
        (v % 10) as u8
    }
}

fn write_digit(f: &mut fmt::Formatter<'_>, d: u8) -> fmt::Result {
    f.write_str(match d {
        0 => "0", 1 => "1", 2 => "2", 3 => "3", 4 => "4",
        5 => "5", 6 => "6", 7 => "7", 8 => "8", _ => "9",
    })
}

/// Sign-and-magnitude comparison (ignores dscale; NaN sorts highest, as in
/// PostgreSQL).
pub fn compare(a: &Numeric, b: &Numeric) -> Ordering {
    match (a.sign, b.sign) {
        (Sign::NaN, Sign::NaN) => return Ordering::Equal,
        (Sign::NaN, _) => return Ordering::Greater,
        (_, Sign::NaN) => return Ordering::Less,
        _ => {}
    }
    if a.is_zero() && b.is_zero() {
        return Ordering::Equal;
    }
    if a.is_zero() {
        return if b.sign == Sign::Neg { Ordering::Greater } else { Ordering::Less };
    }
    if b.is_zero() {
        return if a.sign == Sign::Neg { Ordering::Less } else { Ordering::Greater };
    }
    match (a.sign, b.sign) {
        (Sign::Pos, Sign::Neg) => return Ordering::Greater,
        (Sign::Neg, Sign::Pos) => return Ordering::Less,
        _ => {}
    }
    let mag = compare_magnitude(a, b);
    if a.sign == Sign::Neg {
        mag.reverse()
    } else {
        mag
    }
}

fn compare_magnitude(a: &Numeric, b: &Numeric) -> Ordering {
    if a.weight != b.weight {
        return a.weight.cmp(&b.weight);
    }
    let n = a.ndigits().max(b.ndigits());
    for i in 0..n {
        let da = if i < a.ndigits() { a.digit(i) } else { 0 };
        let db = if i < b.ndigits() { b.digit(i) } else { 0 };
        if da != db {
            return da.cmp(&db);
        }
    }
    Ordering::Equal
}

// ---- arithmetic ----

/// Digit buffer used for intermediates; large enough for any bounded op.
type DigitBuf = [i32; MAX_NDIGITS * 2 + 4];

pub fn add<'a>(a: &Numeric, b: &Numeric, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    if a.is_nan() || b.is_nan() {
        return Ok(Numeric::NAN);
    }
    let dscale = a.dscale.max(b.dscale);
    if a.sign == b.sign || a.is_zero() || b.is_zero() {
        // Same sign (or one zero): add magnitudes, keep the nonzero sign.
        if a.is_zero() {
            return finish(b.sign, b.weight, dscale, b.digits, arena);
        }
        if b.is_zero() {
            return finish(a.sign, a.weight, dscale, a.digits, arena);
        }
        add_magnitudes(a, b, a.sign, dscale, arena)
    } else {
        // Opposite signs: subtract smaller magnitude from larger.
        match compare_magnitude(a, b) {
            Ordering::Equal => Ok(Numeric {
                sign: Sign::Pos,
                weight: 0,
                dscale,
                digits: &[],
            }),
            Ordering::Greater => sub_magnitudes(a, b, a.sign, dscale, arena),
            Ordering::Less => sub_magnitudes(b, a, b.sign, dscale, arena),
        }
    }
}

pub fn sub<'a>(a: &Numeric, b: &Numeric, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    if b.is_nan() {
        return Ok(Numeric::NAN);
    }
    let negb = Numeric {
        sign: match b.sign {
            Sign::Pos => Sign::Neg,
            Sign::Neg => Sign::Pos,
            Sign::NaN => Sign::NaN,
        },
        ..*b
    };
    add(a, &negb, arena)
}

fn add_magnitudes<'a>(
    a: &Numeric,
    b: &Numeric,
    sign: Sign,
    dscale: u16,
    arena: &'a Arena,
) -> Result<Numeric<'a>, SqlError> {
    // Align by weight. Result weight = max(weight)+1 possibly (carry).
    let hi = a.weight.max(b.weight);
    let lo_a = a.weight as i32 - a.ndigits() as i32;
    let lo_b = b.weight as i32 - b.ndigits() as i32;
    let lo = lo_a.min(lo_b);
    let n = (hi as i32 - lo) as usize; // number of base-digits
    // +2: one for the top digit at weight `hi`, one for a carry-out that
    // creates a new most-significant digit (9999 + 1 = 10000).
    if n + 2 > MAX_NDIGITS * 2 {
        return Err(overflow());
    }
    let mut buf: DigitBuf = [0; MAX_NDIGITS * 2 + 4];
    // buf[i] corresponds to base-weight lo + i.
    accumulate(&mut buf, a, lo, 1);
    accumulate(&mut buf, b, lo, 1);
    let mut carry = 0;
    for slot in buf.iter_mut().take(n + 2) {
        *slot += carry;
        carry = *slot / NBASE;
        *slot %= NBASE;
    }
    finish_from_lsf(sign, lo, &buf[..n + 2], dscale, arena)
}

fn sub_magnitudes<'a>(
    a: &Numeric, // larger magnitude
    b: &Numeric, // smaller
    sign: Sign,
    dscale: u16,
    arena: &'a Arena,
) -> Result<Numeric<'a>, SqlError> {
    let hi = a.weight;
    let lo_a = a.weight as i32 - a.ndigits() as i32;
    let lo_b = b.weight as i32 - b.ndigits() as i32;
    let lo = lo_a.min(lo_b);
    let n = (hi as i32 - lo) as usize;
    let mut buf: DigitBuf = [0; MAX_NDIGITS * 2 + 4];
    accumulate(&mut buf, a, lo, 1);
    accumulate(&mut buf, b, lo, -1);
    // Borrow.
    let mut borrow = 0;
    for slot in buf.iter_mut().take(n + 1) {
        *slot += borrow;
        if *slot < 0 {
            *slot += NBASE;
            borrow = -1;
        } else {
            borrow = 0;
        }
    }
    finish_from_lsf(sign, lo, &buf[..n + 1], dscale, arena)
}

/// Adds `sign * digits` of `x` into `buf`, where buf position i has base
/// weight `lo + i`.
fn accumulate(buf: &mut DigitBuf, x: &Numeric, lo: i32, mul: i32) {
    for k in 0..x.ndigits() {
        let w = x.weight as i32 - k as i32;
        let idx = (w - lo) as usize;
        buf[idx] += mul * x.digit(k) as i32;
    }
}

pub fn mul<'a>(a: &Numeric, b: &Numeric, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    if a.is_nan() || b.is_nan() {
        return Ok(Numeric::NAN);
    }
    if a.is_zero() || b.is_zero() {
        return Ok(Numeric {
            sign: Sign::Pos,
            weight: 0,
            dscale: (a.dscale + b.dscale),
            digits: &[],
        });
    }
    let na = a.ndigits();
    let nb = b.ndigits();
    if na + nb + 1 > MAX_NDIGITS * 2 {
        return Err(overflow());
    }
    // Schoolbook multiply into an LSF accumulator.
    let mut buf: DigitBuf = [0; MAX_NDIGITS * 2 + 4];
    // a.digits[i] has base-weight a.weight-i; product term weight = sum.
    // Use LSF indexing with lo = (a.weight - (na-1)) + (b.weight - (nb-1)).
    let lo = (a.weight as i32 - (na as i32 - 1)) + (b.weight as i32 - (nb as i32 - 1));
    for i in 0..na {
        let da = a.digit(i) as i32;
        for j in 0..nb {
            // weight of this term = (a.weight-i)+(b.weight-j)
            let w = (a.weight as i32 - i as i32) + (b.weight as i32 - j as i32);
            let idx = (w - lo) as usize;
            buf[idx] += da * b.digit(j) as i32;
        }
    }
    let n = na + nb;
    let mut carry = 0;
    for slot in buf.iter_mut().take(n + 1) {
        *slot += carry;
        carry = *slot / NBASE;
        *slot %= NBASE;
    }
    let sign = if a.sign == b.sign { Sign::Pos } else { Sign::Neg };
    let dscale = a.dscale + b.dscale;
    finish_from_lsf(sign, lo, &buf[..n + 1], dscale, arena)
}

/// PostgreSQL's div result display scale (select_div_scale, simplified):
/// enough fractional digits to preserve significance, at least
/// NUMERIC_MIN_SIG_DIGITS (4) beyond the point, capped for our bounds.
fn div_result_scale(a: &Numeric, b: &Numeric) -> u16 {
    // PostgreSQL select_div_scale (numeric.c): the quotient carries at least
    // NUMERIC_MIN_SIG_DIGITS (16) significant digits, and at least the scale
    // of either operand. qweight is the base-NBASE weight of the quotient's
    // leading digit, refined down by one when the dividend's lead digit is
    // smaller than the divisor's (the quotient starts below that weight).
    const MIN_SIG_DIGITS: i32 = 16;
    let mut qweight = a.weight as i32 - b.weight as i32;
    // PostgreSQL decrements when the dividend's leading digit <= the
    // divisor's (numeric.c uses <=, not <). A zero dividend has a leading
    // digit of 0, which is <= any divisor digit, so it decrements too.
    if b.ndigits() > 0 {
        let fa = if a.ndigits() > 0 { a.digit(0) } else { 0 };
        if fa <= b.digit(0) {
            qweight -= 1;
        }
    }
    let mut rscale = MIN_SIG_DIGITS - qweight * DEC_DIGITS as i32;
    rscale = rscale.max(a.dscale as i32).max(b.dscale as i32).max(0);
    rscale.min((MAX_NDIGITS * DEC_DIGITS) as i32 - 1) as u16
}

pub fn div<'a>(a: &Numeric, b: &Numeric, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    if a.is_nan() || b.is_nan() {
        return Ok(Numeric::NAN);
    }
    if b.is_zero() {
        return Err(sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero"));
    }
    let rscale = div_result_scale(a, b);
    div_with_scale(a, b, rscale, true, arena)
}

/// PostgreSQL numeric modulo: `a - trunc(a / b) * b`, with the quotient
/// truncated toward zero (scale 0). Result scale is max(a, b) as in PG.
pub fn rem<'a>(a: &Numeric, b: &Numeric, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    if a.is_nan() || b.is_nan() {
        return Ok(Numeric::NAN);
    }
    if b.is_zero() {
        return Err(sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero"));
    }
    let q = div_with_scale(a, b, 0, false, arena)?; // truncated integer quotient
    let qb = mul(&q, b, arena)?;
    let mut r = sub(a, &qb, arena)?;
    r.dscale = a.dscale.max(b.dscale);
    Ok(r)
}

/// Long division producing a quotient with `rscale` fractional decimal
/// digits, rounded half-up. Works in decimal digits for simplicity and
/// exactness.
fn div_with_scale<'a>(
    a: &Numeric,
    b: &Numeric,
    rscale: u16,
    round: bool,
    arena: &'a Arena,
) -> Result<Numeric<'a>, SqlError> {
    if a.is_zero() {
        return Ok(Numeric { sign: Sign::Pos, weight: 0, dscale: rscale, digits: &[] });
    }
    // Significant decimal digits (no leading/trailing zeros) and the weight
    // of the least-significant digit, so each operand is `int * 10^lsw`.
    let mut na = [0i8; MAX_NDIGITS * DEC_DIGITS + 8];
    let mut nb = [0i8; MAX_NDIGITS * DEC_DIGITS + 8];
    let (na_len, na_lsw) = sig_decimal(a, &mut na);
    let (nb_len, nb_lsw) = sig_decimal(b, &mut nb);

    // result = (na_int / nb_int) * 10^(na_lsw - nb_lsw). To get `rscale`
    // fractional digits plus one guard, scale the dividend so the integer
    // quotient already carries them: Q = (na_int * 10^shift) / nb_int, and
    // result = Q * 10^-(rscale+1).
    let p = rscale as i32 + 1;
    let shift = na_lsw - nb_lsw + p;
    if shift < 0 {
        // The exact result needs fewer than `rscale` fractional digits; a
        // zero-padded numerator still divides correctly with shift 0.
        return Ok(Numeric { sign: Sign::Pos, weight: 0, dscale: rscale, digits: &[] });
    }
    let dlen = na_len + shift as usize;
    if dlen > MAX_NDIGITS * DEC_DIGITS + 4 {
        return Err(overflow());
    }
    let mut dividend = [0i8; MAX_NDIGITS * DEC_DIGITS + 8];
    dividend[..na_len].copy_from_slice(&na[..na_len]);
    // trailing zeros already present (array zeroed)

    // Q = dividend / nb_int (integer long division), MSD-first, no leading
    // zeros. Q's least-significant digit has weight -(rscale+1).
    let mut q = [0i8; MAX_NDIGITS * DEC_DIGITS + 8];
    let qlen = long_divide(&dividend[..dlen], &nb[..nb_len], &mut q);
    if qlen == 0 {
        return Ok(Numeric { sign: Sign::Pos, weight: 0, dscale: rscale, digits: &[] });
    }

    // Round: the guard digit (q[qlen-1], weight -(rscale+1)) rounds the digit
    // at weight -rscale (q[qlen-2]).
    let mut out_len = qlen - 1; // drop the guard
    if round && q[qlen - 1] >= 5 {
        let mut k = out_len as i32 - 1;
        let mut carry = 1i8;
        while k >= 0 && carry > 0 {
            q[k as usize] += carry;
            if q[k as usize] >= 10 {
                q[k as usize] -= 10;
                carry = 1;
            } else {
                carry = 0;
            }
            k -= 1;
        }
        if carry > 0 {
            // A new most-significant digit; shift right and prepend 1.
            for m in (0..out_len).rev() {
                q[m + 1] = q[m];
            }
            q[0] = 1;
            out_len += 1;
            // The MSD weight rose by one; int_len accounts for it below.
            let neg = a.sign != b.sign;
            // q[0] now has weight ((out_len-1) - 1) - rscale  (one higher).
            let msd_w = (out_len as i32 - 1) - rscale as i32;
            let mut dec = [0u8; MAX_NDIGITS * DEC_DIGITS + 8];
            for (k, &d) in q[..out_len].iter().enumerate() {
                dec[k] = d as u8;
            }
            let mut r = Numeric::from_decimal_digits(&dec[..out_len], msd_w as i64 + 1, rscale, neg, arena)?;
            r.dscale = rscale;
            return Ok(r);
        }
    }
    if out_len == 0 {
        return Ok(Numeric { sign: Sign::Pos, weight: 0, dscale: rscale, digits: &[] });
    }
    // q[0..out_len]: MSD at weight (out_len-1) - rscale.
    let neg = a.sign != b.sign;
    let msd_w = (out_len as i32 - 1) - rscale as i32;
    let mut dec = [0u8; MAX_NDIGITS * DEC_DIGITS + 8];
    for (k, &d) in q[..out_len].iter().enumerate() {
        dec[k] = d as u8;
    }
    let mut r = Numeric::from_decimal_digits(&dec[..out_len], msd_w as i64 + 1, rscale, neg, arena)?;
    r.dscale = rscale;
    Ok(r)
}

/// Significant decimal digits of `|x|`, MSD-first with no leading or
/// trailing zeros, written to `out`. Returns (len, weight-of-LSD). x != 0.
fn sig_decimal(x: &Numeric, out: &mut [i8]) -> (usize, i32) {
    let hi_dw = x.weight as i32 * DEC_DIGITS as i32 + (DEC_DIGITS as i32 - 1);
    let lo_dw = (x.weight as i32 - (x.ndigits() as i32 - 1)) * DEC_DIGITS as i32;
    // First significant (nonzero) decimal weight from the top.
    let mut first = hi_dw;
    while first >= lo_dw && x.decimal_digit_at(first) == 0 {
        first -= 1;
    }
    // Last significant (nonzero) decimal weight from the bottom.
    let mut last = lo_dw;
    while last <= first && x.decimal_digit_at(last) == 0 {
        last += 1;
    }
    let mut idx = 0;
    let mut dw = first;
    while dw >= last {
        out[idx] = x.decimal_digit_at(dw) as i8;
        idx += 1;
        dw -= 1;
    }
    (idx, last)
}

/// Schoolbook integer long division of decimal-digit arrays (MSD-first, no
/// leading zeros). Writes the quotient MSD-first (no leading zeros) to `q`,
/// returns its length. `den` must be nonzero.
fn long_divide(num: &[i8], den: &[i8], q: &mut [i8]) -> usize {
    let mut rem = [0i32; MAX_NDIGITS * DEC_DIGITS + 8];
    let mut rem_len = 0usize;
    let mut raw = [0i8; MAX_NDIGITS * DEC_DIGITS + 8];
    for (i, &d) in num.iter().enumerate() {
        // rem = rem * 10 + d
        rem[rem_len] = d as i32;
        rem_len += 1;
        trim_leading(&mut rem, &mut rem_len);
        // count = how many times den fits into rem (0..9)
        let mut count = 0i8;
        while cmp_arr(&rem[..rem_len], den) >= 0 {
            sub_arr(&mut rem, &mut rem_len, den);
            count += 1;
        }
        raw[i] = count;
    }
    // raw has num.len() digits (leading zeros for the integer alignment).
    let mut lead = 0;
    while lead < num.len() && raw[lead] == 0 {
        lead += 1;
    }
    let out_len = num.len() - lead;
    q[..out_len].copy_from_slice(&raw[lead..num.len()]);
    out_len
}

fn trim_leading(a: &mut [i32], len: &mut usize) {
    let mut lead = 0;
    while lead < *len && a[lead] == 0 {
        lead += 1;
    }
    if lead > 0 && lead < *len {
        for k in lead..*len {
            a[k - lead] = a[k];
        }
        *len -= lead;
    } else if lead == *len {
        *len = 0;
    }
}

/// Compares magnitudes of an i32 digit array and an i8 digit array (both
/// MSD-first, no leading zeros).
fn cmp_arr(a: &[i32], b: &[i8]) -> i32 {
    if a.len() != b.len() {
        return if a.len() > b.len() { 1 } else { -1 };
    }
    for i in 0..a.len() {
        let bv = b[i] as i32;
        if a[i] != bv {
            return if a[i] > bv { 1 } else { -1 };
        }
    }
    0
}

/// a -= b (a >= b), both MSD-first; renormalizes a's length.
fn sub_arr(a: &mut [i32], alen: &mut usize, b: &[i8]) {
    let mut borrow = 0i32;
    let mut ai = *alen as i32 - 1;
    let mut bi = b.len() as i32 - 1;
    while ai >= 0 {
        let mut v = a[ai as usize] - borrow;
        if bi >= 0 {
            v -= b[bi as usize] as i32;
        }
        if v < 0 {
            v += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        a[ai as usize] = v;
        ai -= 1;
        bi -= 1;
    }
    trim_leading(a, alen);
}

// -- shared finishers --

fn finish<'a>(
    sign: Sign,
    weight: i16,
    dscale: u16,
    digits: &[u8],
    arena: &'a Arena,
) -> Result<Numeric<'a>, SqlError> {
    // Zero is always positive in PostgreSQL (no negative zero).
    let sign = if digits.is_empty() { Sign::Pos } else { sign };
    let d = arena.alloc_slice_copy(digits).map_err(|_| overflow())?;
    Ok(Numeric { sign, weight, dscale, digits: &*d })
}

/// Builds a canonical Numeric from an LSF base-10000 buffer where `buf[i]`
/// has base-weight `lo + i`.
fn finish_from_lsf<'a>(
    sign: Sign,
    lo: i32,
    buf: &[i32],
    dscale: u16,
    arena: &'a Arena,
) -> Result<Numeric<'a>, SqlError> {
    // Find most significant nonzero.
    let mut hi = buf.len();
    while hi > 0 && buf[hi - 1] == 0 {
        hi -= 1;
    }
    let mut lead = 0;
    while lead < hi && buf[lead] == 0 {
        lead += 1;
    }
    if lead >= hi {
        return Ok(Numeric {
            sign: Sign::Pos,
            weight: 0,
            dscale,
            digits: &[],
        });
    }
    let ndigits = hi - lead;
    if ndigits > MAX_NDIGITS {
        return Err(overflow());
    }
    // MSD-first output.
    let mut out: [i16; MAX_NDIGITS] = [0; MAX_NDIGITS];
    for k in 0..ndigits {
        out[k] = buf[hi - 1 - k] as i16;
    }
    let weight = (lo + hi as i32 - 1) as i16;
    Ok(Numeric { sign, weight, dscale, digits: pack(&out[..ndigits], arena)? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;

    fn arena() -> Arena {
        let budget = Box::leak(Box::new(Budget::new(1 << 20)));
        Arena::new(budget, "t", 1 << 19).unwrap()
    }

    fn p<'a>(s: &str, a: &'a Arena) -> Numeric<'a> {
        Numeric::parse(s, a).unwrap()
    }

    fn disp(n: &Numeric) -> String {
        format!("{n}")
    }

    #[test]
    fn parse_and_display_roundtrip() {
        let a = arena();
        for s in ["0", "1", "10", "100", "1000", "10000", "12345", "-7",
                  "0.1", "0.30", "10.0", "2.5", "-2.25", "3.14159",
                  "0.001", "1234567890123456789", "0.30000000000000000"] {
            assert_eq!(disp(&p(s, &a)), s, "roundtrip {s}");
        }
    }

    #[test]
    fn exponent_and_sign() {
        let a = arena();
        assert_eq!(disp(&p("1e3", &a)), "1000");
        assert_eq!(disp(&p("1.5e2", &a)), "150");
        assert_eq!(disp(&p("15e-1", &a)), "1.5");
    }

    #[test]
    fn addition_matches_decimal() {
        let a = arena();
        assert_eq!(disp(&add(&p("0.1", &a), &p("0.2", &a), &a).unwrap()), "0.3");
        assert_eq!(disp(&add(&p("10.0", &a), &p("0.5", &a), &a).unwrap()), "10.5");
        assert_eq!(disp(&add(&p("-7", &a), &p("7", &a), &a).unwrap()), "0");
        assert_eq!(disp(&add(&p("999", &a), &p("1", &a), &a).unwrap()), "1000");
        assert_eq!(disp(&add(&p("9999", &a), &p("1", &a), &a).unwrap()), "10000");
    }

    #[test]
    fn subtraction() {
        let a = arena();
        assert_eq!(disp(&sub(&p("10", &a), &p("3", &a), &a).unwrap()), "7");
        assert_eq!(disp(&sub(&p("0.3", &a), &p("0.1", &a), &a).unwrap()), "0.2");
        assert_eq!(disp(&sub(&p("1", &a), &p("0.001", &a), &a).unwrap()), "0.999");
        assert_eq!(disp(&sub(&p("3", &a), &p("10", &a), &a).unwrap()), "-7");
    }

    #[test]
    fn multiplication() {
        let a = arena();
        assert_eq!(disp(&mul(&p("2.5", &a), &p("4", &a), &a).unwrap()), "10.0");
        assert_eq!(disp(&mul(&p("1.1", &a), &p("1.1", &a), &a).unwrap()), "1.21");
        assert_eq!(disp(&mul(&p("-3", &a), &p("7", &a), &a).unwrap()), "-21");
        assert_eq!(disp(&mul(&p("12345", &a), &p("67890", &a), &a).unwrap()), "838102050");
    }

    #[test]
    fn division_matches_postgres_scale() {
        let a = arena();
        // PostgreSQL: 1/3 -> 0.33333333333333333333 (20 digits)
        assert_eq!(disp(&div(&p("1", &a), &p("3", &a), &a).unwrap()),
                   "0.33333333333333333333");
        assert_eq!(disp(&div(&p("10", &a), &p("2", &a), &a).unwrap()),
                   "5.0000000000000000");
        assert_eq!(disp(&div(&p("7", &a), &p("2", &a), &a).unwrap()),
                   "3.5000000000000000");
    }

    #[test]
    fn comparison() {
        let a = arena();
        assert_eq!(compare(&p("1", &a), &p("2", &a)), Ordering::Less);
        assert_eq!(compare(&p("2.0", &a), &p("2", &a)), Ordering::Equal);
        assert_eq!(compare(&p("-1", &a), &p("1", &a)), Ordering::Less);
        assert_eq!(compare(&p("0.1", &a), &p("0.09", &a)), Ordering::Greater);
    }

    #[test]
    fn integer_conversions() {
        let a = arena();
        assert_eq!(disp(&Numeric::from_i64(0, &a).unwrap()), "0");
        assert_eq!(disp(&Numeric::from_i64(-12345, &a).unwrap()), "-12345");
        assert_eq!(disp(&Numeric::from_i64(1000000, &a).unwrap()), "1000000");
        assert_eq!(p("42.7", &a).to_i64().unwrap(), 43);
        assert_eq!(p("42.4", &a).to_i64().unwrap(), 42);
        assert_eq!(p("-42.5", &a).to_i64().unwrap(), -43);
    }

}
