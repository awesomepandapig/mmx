//! Exact conversion between decimal strings and the engine's `i64` mantissas (exponent -8).

const DECIMALS: usize = 8;
const SCALE: i64 = 100_000_000;

/// Parses a plain non-negative decimal such as `12` or `0.25`. Anything `f64` would round is
/// refused rather than approximated.
pub fn parse(text: &str) -> Result<i64, String> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    let dangling_point = text.contains('.') && fraction.is_empty();
    if whole.is_empty() || !digits(whole) || !digits(fraction) || dangling_point {
        return Err(format!("'{text}' is not a plain decimal number"));
    }
    if fraction.len() > DECIMALS {
        return Err(format!("'{text}' has more than {DECIMALS} decimal places"));
    }

    let too_large = || format!("'{text}' is too large");
    let whole: i64 = whole.parse().map_err(|_| too_large())?;
    let fraction: i64 = format!("{fraction:0<DECIMALS$}").parse().map_err(|_| too_large())?;
    whole
        .checked_mul(SCALE)
        .and_then(|value| value.checked_add(fraction))
        .ok_or_else(too_large)
}

/// Formats a mantissa with no trailing zeros, such as `12.5`.
pub fn format(mantissa: i128) -> String {
    let sign = if mantissa < 0 { "-" } else { "" };
    let value = mantissa.unsigned_abs();
    let scale = SCALE as u128;
    let (whole, fraction) = (value / scale, value % scale);
    if fraction == 0 {
        return format!("{sign}{whole}");
    }
    let fraction = format!("{fraction:0>DECIMALS$}");
    format!("{sign}{whole}.{}", fraction.trim_end_matches('0'))
}

/// The value of `quantity` at `price`, both as mantissas, as a mantissa.
pub fn notional(quantity: i64, price: i64) -> i128 {
    i128::from(quantity) * i128::from(price) / i128::from(SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exactly() {
        assert_eq!(parse("1"), Ok(100_000_000));
        assert_eq!(parse("0.1"), Ok(10_000_000));
        assert_eq!(parse("0.00000001"), Ok(1));
        assert_eq!(parse("92233720368.54775807"), Ok(i64::MAX));
    }

    #[test]
    fn refuses_what_it_cannot_hold_exactly() {
        for text in ["", ".", "1.", ".5", "-1", "+1", "1e3", "NaN", "1,5", " 1", "0.000000001"] {
            assert!(parse(text).is_err(), "{text:?} should be refused");
        }
        assert!(parse("92233720368.54775808").is_err());
        assert!(parse("99999999999999999999").is_err());
    }

    #[test]
    fn formats_without_trailing_zeros() {
        assert_eq!(format(1_250_000_000), "12.5");
        assert_eq!(format(100_000_000), "1");
        assert_eq!(format(1), "0.00000001");
        assert_eq!(format(0), "0");
        assert_eq!(format(-150_000_000), "-1.5");
    }

    #[test]
    fn round_trips() {
        for text in ["0.1", "0.3", "1234.56789012", "92233720368.54775807"] {
            assert_eq!(format(i128::from(parse(text).unwrap())), text);
        }
    }

    #[test]
    fn notional_is_scaled() {
        assert_eq!(format(notional(parse("3").unwrap(), parse("0.5").unwrap())), "1.5");
    }
}
