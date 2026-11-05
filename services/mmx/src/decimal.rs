//! Exact conversion between decimal strings and the engine's `i64` mantissas (exponent -8).
//!
//! This repeats the gateway's rules, because linking the gateway would pull in Aeron. The tests in
//! `markets.rs` hold it to the gateway's own file.

const DECIMALS: usize = 8;
const SCALE: i64 = 100_000_000;

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
    let fraction: i64 = format!("{fraction:0<DECIMALS$}")
        .parse()
        .map_err(|_| too_large())?;
    whole
        .checked_mul(SCALE)
        .and_then(|value| value.checked_add(fraction))
        .ok_or_else(too_large)
}

/// Formats a mantissa with no trailing zeros, such as `12.5`.
pub fn format(mantissa: i64) -> String {
    let sign = if mantissa < 0 { "-" } else { "" };
    let value = mantissa.unsigned_abs();
    let scale = SCALE as u64;
    let (whole, fraction) = (value / scale, value % scale);
    if fraction == 0 {
        return format!("{sign}{whole}");
    }
    let fraction = format!("{fraction:0>DECIMALS$}");
    format!("{sign}{whole}.{}", fraction.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exactly_and_refuses_what_it_cannot_hold() {
        assert_eq!(parse("0.00000001"), Ok(1));
        assert_eq!(parse("92233720368.54775807"), Ok(i64::MAX));
        for text in [
            "",
            ".",
            "1.",
            ".5",
            "-1",
            "+1",
            "1e3",
            "NaN",
            " 1",
            "0.000000001",
            "92233720368.54775808",
        ] {
            assert!(parse(text).is_err(), "{text:?} should be refused");
        }
    }

    #[test]
    fn formats_without_trailing_zeros() {
        assert_eq!(format(1_250_000_000), "12.5");
        assert_eq!(format(1), "0.00000001");
        assert_eq!(format(0), "0");
        assert_eq!(format(-150_000_000), "-1.5");
        assert_eq!(format(i64::MIN), "-92233720368.54775808");
    }
}
