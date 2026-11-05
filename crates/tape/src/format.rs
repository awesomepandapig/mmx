//! Text forms of the values on the wire.

use chrono::{DateTime, SecondsFormat, Utc};

const SCALE: u64 = 100_000_000;
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// Prints a mantissa with exponent -8, without trailing zeros.
pub fn format_decimal(mantissa: i64) -> String {
    let sign = if mantissa < 0 { "-" } else { "" };
    let magnitude = mantissa.unsigned_abs();
    let integral = magnitude / SCALE;
    let fractional = magnitude % SCALE;
    if fractional == 0 {
        return format!("{sign}{integral}");
    }
    let digits = format!("{fractional:08}");
    format!("{sign}{integral}.{}", digits.trim_end_matches('0'))
}

pub fn format_timestamp(timestamp: u64) -> String {
    let seconds = i64::try_from(timestamp / NANOS_PER_SECOND).unwrap_or(0);
    let nanos = (timestamp % NANOS_PER_SECOND) as u32;
    let datetime: DateTime<Utc> = DateTime::from_timestamp(seconds, nanos).unwrap_or_default();
    datetime.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_drop_trailing_zeros() {
        assert_eq!(format_decimal(0), "0");
        assert_eq!(format_decimal(1_200_000_000), "12");
        assert_eq!(format_decimal(1_250_000_000), "12.5");
        assert_eq!(format_decimal(1), "0.00000001");
    }

    #[test]
    fn negative_fractions_keep_their_sign() {
        assert_eq!(format_decimal(-50_000_000), "-0.5");
        assert_eq!(format_decimal(i64::MIN), "-92233720368.54775808");
    }

    #[test]
    fn timestamps_keep_nanoseconds() {
        assert_eq!(format_timestamp(1_500_000_000), "1970-01-01T00:00:01.500000000Z");
    }

    #[test]
    fn the_largest_timestamp_formats_without_panicking() {
        assert_eq!(format_timestamp(u64::MAX), "2554-07-21T23:34:33.709551615Z");
    }
}
