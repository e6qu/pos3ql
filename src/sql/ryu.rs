//! Shortest float-to-decimal via the Ryū algorithm, matching PostgreSQL's
//! output byte for byte.
//!
//! PostgreSQL formats `real`/`double precision` (with the default
//! `extra_float_digits >= 1`) through its port of Ulf Adams' Ryū. Rust's own
//! float formatting is also shortest-round-trip but breaks the boundary case
//! differently — PostgreSQL builds Ryū **without** `STRICTLY_SHORTEST`, so its
//! `acceptBounds` is always `false`, which yields a different (often longer)
//! representation for values whose rounding interval touches a boundary. To
//! match PostgreSQL exactly we reproduce its choice here.
//!
//! Provenance: ported verbatim from PostgreSQL 18.4, `src/common/f2s.c`
//! (tag `REL_18_4`), itself a port of <https://github.com/ulfjack/ryu>
//! (Boost Software License 1.0 / Apache-2.0). Reference: Ulf Adams, "Ryū:
//! fast float-to-string conversion", PLDI 2018. Only the `float`/32-bit path is
//! ported here; the `double` path follows in its own change.

const FLOAT_MANTISSA_BITS: u32 = 23;
const FLOAT_EXPONENT_BITS: u32 = 8;
const FLOAT_BIAS: i32 = 127;
const FLOAT_POW5_INV_BITCOUNT: i32 = 59;
const FLOAT_POW5_BITCOUNT: i32 = 61;

/// The shortest decimal of an f32: `mantissa * 10^exponent`, PostgreSQL's
/// spelling. Verbatim from `f2s.c`'s `FLOAT_POW5_INV_SPLIT`.
static FLOAT_POW5_INV_SPLIT: [u64; 31] = [
    576460752303423489, 461168601842738791, 368934881474191033, 295147905179352826,
    472236648286964522, 377789318629571618, 302231454903657294, 483570327845851670,
    386856262276681336, 309485009821345069, 495176015714152110, 396140812571321688,
    316912650057057351, 507060240091291761, 405648192073033409, 324518553658426727,
    519229685853482763, 415383748682786211, 332306998946228969, 531691198313966350,
    425352958651173080, 340282366920938464, 544451787073501542, 435561429658801234,
    348449143727040987, 557518629963265579, 446014903970612463, 356811923176489971,
    570899077082383953, 456719261665907162, 365375409332725730,
];

/// Verbatim from `f2s.c`'s `FLOAT_POW5_SPLIT`.
static FLOAT_POW5_SPLIT: [u64; 47] = [
    1152921504606846976, 1441151880758558720, 1801439850948198400, 2251799813685248000,
    1407374883553280000, 1759218604441600000, 2199023255552000000, 1374389534720000000,
    1717986918400000000, 2147483648000000000, 1342177280000000000, 1677721600000000000,
    2097152000000000000, 1310720000000000000, 1638400000000000000, 2048000000000000000,
    1280000000000000000, 1600000000000000000, 2000000000000000000, 1250000000000000000,
    1562500000000000000, 1953125000000000000, 1220703125000000000, 1525878906250000000,
    1907348632812500000, 1192092895507812500, 1490116119384765625, 1862645149230957031,
    1164153218269348144, 1455191522836685180, 1818989403545856475, 2273736754432320594,
    1421085471520200371, 1776356839400250464, 2220446049250313080, 1387778780781445675,
    1734723475976807094, 2168404344971008868, 1355252715606880542, 1694065894508600678,
    2117582368135750847, 1323488980084844279, 1654361225106055349, 2067951531382569187,
    1292469707114105741, 1615587133892632177, 2019483917365790221,
];

fn pow5bits(e: i32) -> i32 {
    (((e as u32).wrapping_mul(1217359) >> 19) + 1) as i32
}

fn log10_pow2(e: i32) -> u32 {
    ((e as u32 as u64 * 78913) >> 18) as u32
}

fn log10_pow5(e: i32) -> u32 {
    ((e as u32 as u64 * 732923) >> 20) as u32
}

/// The high bits of `m * factor`, right-shifted by `shift` (> 32). PostgreSQL's
/// `mulShift`, done in u128 (the full 96-bit product fits).
fn mul_shift(m: u32, factor: u64, shift: i32) -> u32 {
    ((m as u128 * factor as u128) >> shift) as u32
}

fn mul_pow5_inv_div_pow2(m: u32, q: u32, j: i32) -> u32 {
    mul_shift(m, FLOAT_POW5_INV_SPLIT[q as usize], j)
}

fn mul_pow5_div_pow2(m: u32, i: u32, j: i32) -> u32 {
    mul_shift(m, FLOAT_POW5_SPLIT[i as usize], j)
}

fn pow5_factor(mut value: u32) -> u32 {
    let mut count = 0;
    loop {
        let q = value / 5;
        let r = value % 5;
        if r != 0 {
            break;
        }
        value = q;
        count += 1;
    }
    count
}

fn multiple_of_power_of_5(value: u32, p: u32) -> bool {
    pow5_factor(value) >= p
}

fn multiple_of_power_of_2(value: u32, p: u32) -> bool {
    value & ((1u32 << p) - 1) == 0
}

/// PostgreSQL's `f2d`: the shortest `(mantissa, exponent)` for a decoded f32,
/// with `acceptBounds = false` (the non-STRICTLY_SHORTEST build). Ported arm
/// for arm from `src/common/f2s.c`.
fn f2d(ieee_mantissa: u32, ieee_exponent: u32) -> (u32, i32) {
    let (e2, m2) = if ieee_exponent == 0 {
        (1 - FLOAT_BIAS - FLOAT_MANTISSA_BITS as i32 - 2, ieee_mantissa)
    } else {
        (
            ieee_exponent as i32 - FLOAT_BIAS - FLOAT_MANTISSA_BITS as i32 - 2,
            (1u32 << FLOAT_MANTISSA_BITS) | ieee_mantissa,
        )
    };
    // PostgreSQL builds Ryū without STRICTLY_SHORTEST.
    let accept_bounds = false;

    let mv = 4 * m2;
    let mp = 4 * m2 + 2;
    let mm_shift = u32::from(ieee_mantissa != 0 || ieee_exponent <= 1);
    let mm = 4 * m2 - 1 - mm_shift;

    let (mut vr, mut vp, mut vm);
    let e10;
    let mut vm_is_trailing_zeros = false;
    let mut vr_is_trailing_zeros = false;
    let mut last_removed_digit: u8 = 0;

    if e2 >= 0 {
        let q = log10_pow2(e2);
        e10 = q as i32;
        let k = FLOAT_POW5_INV_BITCOUNT + pow5bits(q as i32) - 1;
        let i = -e2 + q as i32 + k;
        vr = mul_pow5_inv_div_pow2(mv, q, i);
        vp = mul_pow5_inv_div_pow2(mp, q, i);
        vm = mul_pow5_inv_div_pow2(mm, q, i);
        if q != 0 && (vp - 1) / 10 <= vm / 10 {
            let l = FLOAT_POW5_INV_BITCOUNT + pow5bits(q as i32 - 1) - 1;
            last_removed_digit =
                (mul_pow5_inv_div_pow2(mv, q - 1, -e2 + q as i32 - 1 + l) % 10) as u8;
        }
        if q <= 9 {
            if mv % 5 == 0 {
                vr_is_trailing_zeros = multiple_of_power_of_5(mv, q);
            } else if accept_bounds {
                vm_is_trailing_zeros = multiple_of_power_of_5(mm, q);
            } else {
                vp -= u32::from(multiple_of_power_of_5(mp, q));
            }
        }
    } else {
        let q = log10_pow5(-e2);
        e10 = q as i32 + e2;
        let i = -e2 - q as i32;
        let k = pow5bits(i) - FLOAT_POW5_BITCOUNT;
        let j = q as i32 - k;
        vr = mul_pow5_div_pow2(mv, i as u32, j);
        vp = mul_pow5_div_pow2(mp, i as u32, j);
        vm = mul_pow5_div_pow2(mm, i as u32, j);
        if q != 0 && (vp - 1) / 10 <= vm / 10 {
            let j = q as i32 - 1 - (pow5bits(i + 1) - FLOAT_POW5_BITCOUNT);
            last_removed_digit = (mul_pow5_div_pow2(mv, (i + 1) as u32, j) % 10) as u8;
        }
        if q <= 1 {
            vr_is_trailing_zeros = true;
            if accept_bounds {
                vm_is_trailing_zeros = mm_shift == 1;
            } else {
                vp -= 1;
            }
        } else if q < 31 {
            vr_is_trailing_zeros = multiple_of_power_of_2(mv, q - 1);
        }
    }

    let mut removed: i32 = 0;
    let output;
    if vm_is_trailing_zeros || vr_is_trailing_zeros {
        while vp / 10 > vm / 10 {
            vm_is_trailing_zeros &= vm % 10 == 0;
            vr_is_trailing_zeros &= last_removed_digit == 0;
            last_removed_digit = (vr % 10) as u8;
            vr /= 10;
            vp /= 10;
            vm /= 10;
            removed += 1;
        }
        if vm_is_trailing_zeros {
            while vm % 10 == 0 {
                vr_is_trailing_zeros &= last_removed_digit == 0;
                last_removed_digit = (vr % 10) as u8;
                vr /= 10;
                // vp is not read past this loop (the first loop's condition was
                // its last use), so it is not divided here as in the C source.
                vm /= 10;
                removed += 1;
            }
        }
        if vr_is_trailing_zeros && last_removed_digit == 5 && vr % 2 == 0 {
            last_removed_digit = 4;
        }
        output = vr
            + u32::from(
                (vr == vm && (!accept_bounds || !vm_is_trailing_zeros)) || last_removed_digit >= 5,
            );
    } else {
        while vp / 10 > vm / 10 {
            last_removed_digit = (vr % 10) as u8;
            vr /= 10;
            vp /= 10;
            vm /= 10;
            removed += 1;
        }
        output = vr + u32::from(vr == vm || last_removed_digit >= 5);
    }

    (output, e10 + removed)
}

/// Shortest decimal for a finite, non-zero f32: `(significant digits as an
/// integer, base-10 exponent)` such that the value is `digits * 10^exponent`.
/// The caller handles sign, zero, and the infinities/NaN.
pub(crate) fn f32_shortest(v: f32) -> (u32, i32) {
    debug_assert!(v.is_finite() && v != 0.0);
    let bits = v.abs().to_bits();
    let ieee_mantissa = bits & ((1u32 << FLOAT_MANTISSA_BITS) - 1);
    let ieee_exponent = (bits >> FLOAT_MANTISSA_BITS) & ((1u32 << FLOAT_EXPONENT_BITS) - 1);
    f2d(ieee_mantissa, ieee_exponent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::excessive_precision)] // the boundary literals are deliberate
    fn matches_postgres_boundary_cases() {
        // acceptBounds=false yields these (the STRICTLY_SHORTEST build would
        // give one fewer digit for the integer cases).
        assert_eq!(f32_shortest(87535936.0), (87535936, 0));
        assert_eq!(f32_shortest(59326392.0), (59326392, 0));
        assert_eq!(f32_shortest(3188318.25), (31883182, -1));
        assert_eq!(f32_shortest(0.1), (1, -1));
        assert_eq!(f32_shortest(1000000.0), (1, 6));
        assert_eq!(f32_shortest(12345678.0), (12345678, 0));
        assert_eq!(f32_shortest(1.0), (1, 0));
    }
}
