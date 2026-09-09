//! PostgreSQL `money`: signed cents with locale-shaped text I/O.

use core::fmt;

use crate::mem::arena::Arena;
use crate::sql_err;

use super::eval::{SqlError, sqlstate};
use super::numeric::{Numeric, RoundMode};

fn invalid(input: &str) -> SqlError {
    sql_err!(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type money: \"{}\"",
        input
    )
}

fn out_of_range() -> SqlError {
    sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "money out of range")
}

/// Parses the C.UTF-8/en_US monetary spelling exposed at SQL and COPY
/// boundaries. Currency marks and grouping separators are presentation; the
/// stored value is always an exact signed count of cents.
pub fn parse(input: &str) -> Result<i64, SqlError> {
    let original = input;
    let mut text = input.trim();
    if text.is_empty() {
        return Ok(0);
    }
    let mut negative = false;
    if let Some(rest) = text.strip_suffix('$') {
        text = rest.trim_end();
    }
    if let Some(rest) = text.strip_suffix('+') {
        text = rest.trim_end();
    } else if let Some(rest) = text.strip_suffix('-') {
        negative = true;
        text = rest.trim_end();
    }
    if let Some(rest) = text.strip_suffix('$') {
        text = rest.trim_end();
    }
    if let Some(rest) = text.strip_prefix('$') {
        text = rest.trim_start();
    }
    if let Some(rest) = text.strip_prefix('+') {
        text = rest.trim_start();
    } else if let Some(rest) = text.strip_prefix('-') {
        negative = true;
        text = rest.trim_start();
    } else if let Some(rest) = text.strip_prefix('(') {
        negative = true;
        text = rest.trim();
        text = text
            .strip_suffix(')')
            .ok_or_else(|| invalid(original))?
            .trim_end();
    }
    if let Some(rest) = text.strip_prefix('$') {
        text = rest.trim_start();
    }
    if text.is_empty() {
        return Err(invalid(original));
    }

    let mut whole = 0i128;
    let mut fraction = 0i128;
    let mut fractional_digits = 0usize;
    let mut decimal = false;
    let mut saw_digit = false;
    for byte in text.bytes() {
        match byte {
            b',' => {}
            b'.' if !decimal => decimal = true,
            b'0'..=b'9' => {
                saw_digit = true;
                let digit = i128::from(byte - b'0');
                if decimal {
                    fractional_digits += 1;
                    if fractional_digits <= 3 {
                        fraction = fraction * 10 + digit;
                    }
                } else {
                    whole = whole
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(digit))
                        .ok_or_else(out_of_range)?;
                }
            }
            _ => return Err(invalid(original)),
        }
    }
    if !saw_digit {
        return Err(invalid(original));
    }
    let cents_fraction = match fractional_digits {
        0 => 0,
        1 => fraction * 10,
        2 => fraction,
        _ => fraction / 10 + i128::from(fraction % 10 >= 5),
    };
    let magnitude = whole
        .checked_mul(100)
        .and_then(|value| value.checked_add(cents_fraction))
        .ok_or_else(out_of_range)?;
    let signed = if negative { -magnitude } else { magnitude };
    i64::try_from(signed).map_err(|_| out_of_range())
}

pub struct Display(pub i64);

impl fmt::Display for Display {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let magnitude = i128::from(self.0).abs();
        if self.0 < 0 {
            formatter.write_str("-")?;
        }
        formatter.write_str("$")?;
        write_grouped(formatter, magnitude / 100)?;
        write!(formatter, ".{:02}", magnitude % 100)
    }
}

fn write_grouped(formatter: &mut fmt::Formatter<'_>, value: i128) -> fmt::Result {
    if value >= 1000 {
        write_grouped(formatter, value / 1000)?;
        write!(formatter, ",{:03}", value % 1000)
    } else {
        write!(formatter, "{value}")
    }
}

pub fn from_integer(value: i64) -> Result<i64, SqlError> {
    value.checked_mul(100).ok_or_else(out_of_range)
}

pub fn from_numeric(value: &Numeric<'_>, arena: &Arena) -> Result<i64, SqlError> {
    if value.is_nan() {
        return Err(out_of_range());
    }
    let rounded = value.round_scale(2, RoundMode::HalfAwayZero, arena)?;
    let text = crate::stack_format!(2100, "{rounded}");
    parse(text.as_str())
}

pub fn to_numeric<'a>(value: i64, arena: &'a Arena) -> Result<Numeric<'a>, SqlError> {
    let magnitude = i128::from(value).abs();
    let text = crate::stack_format!(
        32,
        "{}{}.{:02}",
        if value < 0 { "-" } else { "" },
        magnitude / 100,
        magnitude % 100
    );
    Numeric::parse(text.as_str(), arena)
}

pub fn add(left: i64, right: i64) -> Result<i64, SqlError> {
    left.checked_add(right).ok_or_else(out_of_range)
}

pub fn sub(left: i64, right: i64) -> Result<i64, SqlError> {
    left.checked_sub(right).ok_or_else(out_of_range)
}

pub fn mul_integer(value: i64, factor: i64) -> Result<i64, SqlError> {
    value.checked_mul(factor).ok_or_else(out_of_range)
}

pub fn div_integer(value: i64, divisor: i64) -> Result<i64, SqlError> {
    if divisor == 0 {
        return Err(sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero"));
    }
    value.checked_div(divisor).ok_or_else(out_of_range)
}

pub fn scale_float(value: i64, factor: f64, divide: bool) -> Result<i64, SqlError> {
    if divide && factor == 0.0 {
        return Err(sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero"));
    }
    let result = if divide {
        value as f64 / factor
    } else {
        value as f64 * factor
    };
    if !result.is_finite() || result < i64::MIN as f64 || result >= -(i64::MIN as f64) {
        return Err(out_of_range());
    }
    Ok(result.round_ties_even() as i64)
}

pub fn ratio(left: i64, right: i64) -> Result<f64, SqlError> {
    if right == 0 {
        return Err(sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero"));
    }
    Ok(left as f64 / right as f64)
}

/// English wording used by PostgreSQL's `cash_words` compatibility function.
pub struct Words(pub i64);

impl fmt::Display for Words {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let magnitude = i128::from(self.0).abs();
        if self.0 < 0 {
            formatter.write_str("Minus ")?;
        }
        let dollars = magnitude / 100;
        write_cardinal(formatter, dollars, self.0 >= 0)?;
        formatter.write_str(if dollars == 1 {
            " dollar and "
        } else {
            " dollars and "
        })?;
        let cents = magnitude % 100;
        write_cardinal(formatter, cents, false)?;
        formatter.write_str(if cents == 1 { " cent" } else { " cents" })
    }
}

fn write_cardinal(
    formatter: &mut fmt::Formatter<'_>,
    value: i128,
    initial_capital: bool,
) -> fmt::Result {
    if value == 0 {
        return formatter.write_str(if initial_capital { "Zero" } else { "zero" });
    }
    const SCALES: [(i128, &str); 5] = [
        (1_000_000_000_000_000, "quadrillion"),
        (1_000_000_000_000, "trillion"),
        (1_000_000_000, "billion"),
        (1_000_000, "million"),
        (1_000, "thousand"),
    ];
    let mut remaining = value;
    let mut capitalize = initial_capital;
    for (scale, name) in SCALES {
        if remaining >= scale {
            write_under_thousand(formatter, remaining / scale, &mut capitalize)?;
            formatter.write_str(" ")?;
            write_word(formatter, name, &mut capitalize)?;
            formatter.write_str(" ")?;
            remaining %= scale;
        }
    }
    if remaining != 0 {
        write_under_thousand(formatter, remaining, &mut capitalize)?;
    }
    Ok(())
}

fn write_under_thousand(
    formatter: &mut fmt::Formatter<'_>,
    value: i128,
    capitalize: &mut bool,
) -> fmt::Result {
    const SMALL: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 10] = [
        "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    if value < 20 {
        return write_word(formatter, SMALL[value as usize], capitalize);
    }
    if value == 20 {
        return write_word(formatter, "twenty", capitalize);
    }
    let hundreds = value / 100;
    let rest = value % 100;
    if rest == 0 {
        write_word(formatter, SMALL[hundreds as usize], capitalize)?;
        return formatter.write_str(" hundred");
    }
    if value > 99 {
        write_word(formatter, SMALL[hundreds as usize], capitalize)?;
        formatter.write_str(" hundred ")?;
        if value % 10 == 0 && rest > 10 {
            write_word(formatter, TENS[(rest / 10) as usize], capitalize)?;
        } else if rest < 20 {
            formatter.write_str("and ")?;
            write_word(formatter, SMALL[rest as usize], capitalize)?;
        } else {
            write_word(formatter, TENS[(rest / 10) as usize], capitalize)?;
            formatter.write_str(" ")?;
            write_word(formatter, SMALL[(rest % 10) as usize], capitalize)?;
        }
    } else if value % 10 == 0 {
        write_word(formatter, TENS[(rest / 10) as usize], capitalize)?;
    } else if rest < 20 {
        write_word(formatter, SMALL[rest as usize], capitalize)?;
    } else {
        write_word(formatter, TENS[(rest / 10) as usize], capitalize)?;
        formatter.write_str(" ")?;
        write_word(formatter, SMALL[(rest % 10) as usize], capitalize)?;
    }
    Ok(())
}

fn write_word(
    formatter: &mut fmt::Formatter<'_>,
    word: &str,
    capitalize: &mut bool,
) -> fmt::Result {
    if *capitalize {
        let (first, rest) = word.split_at(1);
        formatter.write_str(match first {
            "a" => "A",
            "b" => "B",
            "c" => "C",
            "d" => "D",
            "e" => "E",
            "f" => "F",
            "g" => "G",
            "h" => "H",
            "i" => "I",
            "j" => "J",
            "k" => "K",
            "l" => "L",
            "m" => "M",
            "n" => "N",
            "o" => "O",
            "p" => "P",
            "q" => "Q",
            "r" => "R",
            "s" => "S",
            "t" => "T",
            "u" => "U",
            "v" => "V",
            "w" => "W",
            "x" => "X",
            "y" => "Y",
            "z" => "Z",
            _ => first,
        })?;
        formatter.write_str(rest)?;
        *capitalize = false;
        Ok(())
    } else {
        formatter.write_str(word)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_rounding_and_boundaries() {
        assert_eq!(parse("($1,234.565)").unwrap(), -123_457);
        assert_eq!(parse("1.005").unwrap(), 101);
        assert_eq!(parse("1,234.56-$").unwrap(), -123_456);
        assert_eq!(parse("( $1,234.56 )$").unwrap(), -123_456);
        assert_eq!(parse("-92233720368547758.08").unwrap(), i64::MIN);
        assert!(parse("92233720368547758.08").is_err());
        assert_eq!(
            crate::stack_format!(64, "{}", Display(-123_456)).as_str(),
            "-$1,234.56"
        );
    }

    #[test]
    fn words_cover_scale_and_pluralization() {
        assert_eq!(
            crate::stack_format!(128, "{}", Words(101)).as_str(),
            "One dollar and one cent"
        );
        assert_eq!(
            crate::stack_format!(128, "{}", Words(-1_234)).as_str(),
            "Minus twelve dollars and thirty four cents"
        );
    }
}
