// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Connector errors and the MEXC-code → venue-neutral reason mapping.

use akadro_core::RejectReason;

/// An error from the MEXC connector's pure logic or transport.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MexcError {
    /// A response could not be parsed into the expected shape.
    #[error("mexc parse error: {0}")]
    Parse(String),
    /// The transport (HTTP/WS) failed.
    #[error("mexc transport error: {0}")]
    Transport(String),
    /// The venue returned a structured error.
    #[error("mexc api error {code}: {msg}")]
    Api {
        /// MEXC numeric error code.
        code: i64,
        /// Human-readable message.
        msg: String,
    },
}

/// Map a MEXC error code to a venue-neutral [`RejectReason`].
///
/// Venue-specific causes that are not strategy-actionable (signature/clock/
/// market-disabled) bucket into [`RejectReason::VenueRejected`]; balance issues
/// into [`RejectReason::InsufficientFunds`]; filter/precision issues into
/// [`RejectReason::InvalidOrder`]. The exact code set is not fully documented by
/// MEXC, so unknown codes default conservatively to `VenueRejected`.
#[must_use]
pub fn map_reject_code(code: i64) -> RejectReason {
    match code {
        // Insufficient balance / position (observed 30004/30005/2005-class).
        30004 | 30005 | 2005 => RejectReason::InsufficientFunds,
        // Parameter / precision / min-notional / oversold filter violations.
        10007 | 30002 | 30003 | 700_001 | 700_004 => RejectReason::InvalidOrder,
        // Everything else — signature/clock/auth/market-disabled/unknown-symbol
        // (e.g. 602, 700_002, 700_003, 1100, 1121) and any unmapped code — is a
        // non-strategy-actionable venue rejection.
        _ => RejectReason::VenueRejected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        assert!(format!("{}", MexcError::Parse("x".into())).contains("parse"));
        assert!(format!("{}", MexcError::Transport("x".into())).contains("transport"));
        assert!(
            format!(
                "{}",
                MexcError::Api {
                    code: 700_002,
                    msg: "bad sig".into()
                }
            )
            .contains("700002")
        );
    }

    #[test]
    fn reject_code_buckets() {
        assert_eq!(map_reject_code(30005), RejectReason::InsufficientFunds);
        assert_eq!(map_reject_code(10007), RejectReason::InvalidOrder);
        assert_eq!(map_reject_code(700_001), RejectReason::InvalidOrder);
        assert_eq!(map_reject_code(700_002), RejectReason::VenueRejected);
        assert_eq!(map_reject_code(700_003), RejectReason::VenueRejected);
        // Unknown codes default to VenueRejected.
        assert_eq!(map_reject_code(999_999), RejectReason::VenueRejected);
    }
}
