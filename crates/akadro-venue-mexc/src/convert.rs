// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decimal-string ↔ fixed-point conversion.
//!
//! MEXC reports every price/quantity as a decimal string (e.g. `"50000.25"`).
//! akadro uses scaled integers. These helpers convert between the two at a given
//! per-instrument scale (number of decimal places), truncating any excess
//! fractional digits toward zero (deterministic, matching the venue's own
//! precision handling).

use crate::error::MexcError;

/// Parse a decimal string into a fixed-point raw integer at `scale` decimal
/// places. Excess fractional digits are truncated toward zero. Thin adapter over
/// the venue-neutral [`akadro_core::decimal_to_raw`] (which this crate's parser was
/// the model for — DRY), mapping a rejected value to [`MexcError`].
///
/// ```
/// use akadro_venue_mexc::decimal_to_raw;
/// assert_eq!(decimal_to_raw("123.45", 2).unwrap(), 12_345);
/// assert_eq!(decimal_to_raw("123.4",   2).unwrap(), 12_340); // padded
/// assert_eq!(decimal_to_raw("123.456", 2).unwrap(), 12_345); // truncated
/// assert_eq!(decimal_to_raw("0.000001", 6).unwrap(), 1);
/// assert_eq!(decimal_to_raw("-5", 0).unwrap(), -5);
/// ```
///
/// # Errors
/// [`MexcError::Parse`] if `s` is empty, non-numeric, or overflows `i64` at `scale`.
pub fn decimal_to_raw(s: &str, scale: u32) -> Result<i64, MexcError> {
    akadro_core::decimal_to_raw(s, scale)
        .ok_or_else(|| MexcError::Parse(format!("bad decimal {s:?} at scale {scale}")))
}

/// Render a fixed-point raw integer at `scale` decimal places as a decimal
/// string (for request bodies and logs). Unlike [`akadro_core::raw_to_decimal`],
/// this **pads to exactly `scale` places** (no trailing-zero trim), preserving the
/// precise byte form MEXC order params are tested/signed against.
///
/// ```
/// use akadro_venue_mexc::raw_to_decimal;
/// assert_eq!(raw_to_decimal(12_345, 2), "123.45");
/// assert_eq!(raw_to_decimal(5, 0), "5");
/// assert_eq!(raw_to_decimal(-1, 6), "-0.000001");
/// ```
#[must_use]
pub fn raw_to_decimal(raw: i64, scale: u32) -> String {
    // Clamp to the max base-10 exponent a u128 can hold; `10u128.pow(39)` overflows
    // (m31/m13). No real venue uses a scale this large, but the guard mirrors
    // `decimal_to_raw`'s `checked_pow` so neither direction can panic.
    let scale = scale.min(38);
    if scale == 0 {
        return raw.to_string();
    }
    let neg = raw < 0;
    let mag = i128::from(raw).unsigned_abs();
    let div = 10u128.pow(scale);
    let int = mag / div;
    let frac = mag % div;
    let sign = if neg { "-" } else { "" };
    format!("{sign}{int}.{frac:0width$}", width = scale as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        assert_eq!(decimal_to_raw("123.45", 2).unwrap(), 12_345);
        assert_eq!(decimal_to_raw("123", 2).unwrap(), 12_300);
        assert_eq!(decimal_to_raw("0.5", 2).unwrap(), 50);
        assert_eq!(decimal_to_raw(".5", 2).unwrap(), 50);
        assert_eq!(decimal_to_raw("123.4", 2).unwrap(), 12_340);
        assert_eq!(decimal_to_raw("123.456", 2).unwrap(), 12_345);
        assert_eq!(decimal_to_raw("0.000001", 6).unwrap(), 1);
        assert_eq!(decimal_to_raw("1000000", 0).unwrap(), 1_000_000);
        assert_eq!(decimal_to_raw("+7", 0).unwrap(), 7);
        assert_eq!(decimal_to_raw("-12.5", 1).unwrap(), -125);
        assert_eq!(decimal_to_raw("  42.0  ", 0).unwrap(), 42);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(decimal_to_raw("abc", 2).is_err());
        assert!(decimal_to_raw("1.2.3", 2).is_err());
        assert!(decimal_to_raw("", 2).is_err());
        assert!(decimal_to_raw("1e5", 2).is_err());
        // Overflows i64.
        assert!(decimal_to_raw("99999999999999999999", 2).is_err());
    }

    #[test]
    fn render_basic() {
        assert_eq!(raw_to_decimal(12_345, 2), "123.45");
        assert_eq!(raw_to_decimal(5, 0), "5");
        assert_eq!(raw_to_decimal(1, 6), "0.000001");
        assert_eq!(raw_to_decimal(-1, 6), "-0.000001");
        assert_eq!(raw_to_decimal(0, 4), "0.0000");
    }

    #[test]
    fn round_trip_truncating() {
        for (raw, scale) in [
            (12_345i64, 2u32),
            (1, 6),
            (0, 0),
            (-9999, 3),
            (1_000_000, 0),
        ] {
            let s = raw_to_decimal(raw, scale);
            assert_eq!(
                decimal_to_raw(&s, scale).unwrap(),
                raw,
                "round-trip {s} @ {scale}"
            );
        }
    }
}
