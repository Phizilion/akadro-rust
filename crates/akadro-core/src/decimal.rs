// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Decimal-string ↔ fixed-point `i64` conversion, shared by every venue connector.
//!
//! Venues quote prices/quantities as decimal strings (`"123.45"`, `"0.0001"`); the
//! engine works in fixed-point `i64` raw units at a per-field `scale` (decimal
//! places). Every `akadro-venue-*` crate needs the same parse/format/scale-inference
//! logic, so it lives here once (DRY) instead of being copied — and copied with
//! subtly different edge-case handling — into each connector. Connectors wrap these
//! with a thin adapter that maps `None` to their own `…Error::Parse` variant.
//!
//! The conversion is **truncating** (drops fractional digits beyond `scale`, never
//! rounds), `i128`-intermediate (so a large value can't overflow mid-scale before
//! the final `i64` check), and rejects empty / non-numeric / overflowing input by
//! returning `None`. No floats are involved at any point.

/// Parse a decimal string `s` to a raw fixed-point `i64` at `scale` decimal places,
/// truncating any excess fractional digits. Accepts an optional leading `+`/`-` and
/// surrounding whitespace.
///
/// Returns `None` if `s` is empty/whitespace-only, contains a non-digit (other than
/// one leading sign and a single `.`), or the scaled value does not fit `i64`.
///
/// ```
/// use akadro_core::decimal_to_raw;
/// assert_eq!(decimal_to_raw("123.45", 2), Some(12_345));
/// assert_eq!(decimal_to_raw("123.4", 2), Some(12_340)); // right-padded
/// assert_eq!(decimal_to_raw("123.456", 2), Some(12_345)); // truncated, not rounded
/// assert_eq!(decimal_to_raw("-0.5", 4), Some(-5_000));
/// assert_eq!(decimal_to_raw("", 2), None); // empty rejected
/// assert_eq!(decimal_to_raw("1.2x", 2), None); // non-digit rejected
/// ```
#[must_use]
pub fn decimal_to_raw(s: &str, scale: u32) -> Option<i64> {
    let s = s.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None; // empty / sign-only / "."
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }

    let scale = scale as usize;
    // Keep at most `scale` fractional digits (truncate), then left-pad so the value
    // occupies exactly `scale` places.
    let frac_kept = if frac_part.len() >= scale {
        &frac_part[..scale]
    } else {
        frac_part
    };
    let pad = scale - frac_kept.len();

    let parse = |t: &str| -> Option<i128> {
        if t.is_empty() {
            Some(0)
        } else {
            t.parse::<i128>().ok()
        }
    };
    let int_v = parse(int_part)?;
    let frac_v = parse(frac_kept)?;

    let pow = |p: usize| 10i128.checked_pow(u32::try_from(p).ok()?);
    let scale_pow = pow(scale)?;
    let pad_pow = pow(pad)?;
    let raw = int_v
        .checked_mul(scale_pow)?
        .checked_add(frac_v.checked_mul(pad_pow)?)?;
    let raw = if neg { -raw } else { raw };
    i64::try_from(raw).ok()
}

/// Number of significant fractional digits in a decimal string (trailing zeros
/// stripped): `"0.10"` → 1, `"5"` → 0, `"1.2300"` → 2. Used to infer a field's
/// `scale` from a venue's tick/lot-size string.
#[must_use]
pub fn scale_of(decimal: &str) -> u32 {
    match decimal.split_once('.') {
        Some((_, frac)) => frac.trim_end_matches('0').len() as u32,
        None => 0,
    }
}

/// Format a raw fixed-point `i64` at `scale` decimal places back into a decimal
/// string, trimming trailing fractional zeros (`12_340` @ scale 2 → `"123.4"`).
/// Inverse of [`decimal_to_raw`] for representable values.
#[must_use]
pub fn raw_to_decimal(raw: i64, scale: u32) -> String {
    // Clamp to the largest base-10 exponent a u128 holds (`10u128.pow(39)` overflows).
    // No real venue uses a scale this large, but the guard mirrors `decimal_to_raw`'s
    // `checked_pow` so neither direction can panic.
    let scale = scale.min(38);
    if scale == 0 {
        return raw.to_string();
    }
    let neg = raw < 0;
    let mag = i128::from(raw).unsigned_abs();
    let div = 10u128.pow(scale);
    let int = (mag / div).to_string();
    let frac = format!("{:0width$}", mag % div, width = scale as usize);
    let frac = frac.trim_end_matches('0');
    let s = if frac.is_empty() {
        int
    } else {
        format!("{int}.{frac}")
    };
    if neg { format!("-{s}") } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_truncates() {
        assert_eq!(decimal_to_raw("123.45", 2), Some(12_345));
        assert_eq!(decimal_to_raw("123.4", 2), Some(12_340)); // pad
        assert_eq!(decimal_to_raw("123.456", 2), Some(12_345)); // truncate, no round
        assert_eq!(decimal_to_raw("123.999", 2), Some(12_399)); // truncate, NOT 12400
        assert_eq!(decimal_to_raw("100", 0), Some(100));
        assert_eq!(decimal_to_raw("0.0001", 4), Some(1));
        assert_eq!(decimal_to_raw("-0.5", 4), Some(-5_000));
        assert_eq!(decimal_to_raw("+1.5", 2), Some(150)); // leading +
        assert_eq!(decimal_to_raw("  2.5  ", 1), Some(25)); // trimmed
        assert_eq!(decimal_to_raw(".5", 2), Some(50)); // empty int part
        assert_eq!(decimal_to_raw("5.", 2), Some(500)); // empty frac part
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(decimal_to_raw("", 2), None);
        assert_eq!(decimal_to_raw("   ", 2), None);
        assert_eq!(decimal_to_raw("-", 2), None);
        assert_eq!(decimal_to_raw(".", 2), None);
        assert_eq!(decimal_to_raw("1.2x", 2), None);
        assert_eq!(decimal_to_raw("1.2.3", 2), None);
        assert_eq!(decimal_to_raw("abc", 2), None);
        // Overflows i64 at scale.
        assert_eq!(decimal_to_raw("99999999999999999999", 2), None);
    }

    #[test]
    fn scale_of_counts_significant_frac_digits() {
        assert_eq!(scale_of("0.1"), 1);
        assert_eq!(scale_of("0.10"), 1); // trailing zero stripped
        assert_eq!(scale_of("5"), 0);
        assert_eq!(scale_of("1.2300"), 2);
        assert_eq!(scale_of("100"), 0);
    }

    #[test]
    fn raw_to_decimal_round_trips() {
        assert_eq!(raw_to_decimal(12_345, 2), "123.45");
        assert_eq!(raw_to_decimal(12_340, 2), "123.4"); // trailing zero trimmed
        assert_eq!(raw_to_decimal(100, 0), "100");
        assert_eq!(raw_to_decimal(-5_000, 4), "-0.5");
        // Round-trip a sample of values.
        for &(raw, scale) in &[(12_345i64, 2u32), (1, 4), (-9_999, 3), (1_000_000, 6)] {
            let s = raw_to_decimal(raw, scale);
            assert_eq!(decimal_to_raw(&s, scale), Some(raw), "round-trip {s}");
        }
    }
}
