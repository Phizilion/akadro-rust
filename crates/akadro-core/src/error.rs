// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Library error type.
//!
//! One `#[non_exhaustive]` enum per the workspace error policy; foreign errors
//! are wrapped (their types never leak into our public signatures).

use crate::ids::InstrumentId;

/// Errors produced by akadro engine construction and run setup.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AkadroError {
    /// An instrument id was referenced that the catalogue does not know.
    #[error("unknown instrument: {0:?}")]
    UnknownInstrument(InstrumentId),

    /// The engine was configured incorrectly (e.g. no instruments, no strategy).
    #[error("invalid configuration: {0}")]
    Config(String),

    /// The live async→sync bridge overflowed its bounded queue. Per decision D7
    /// this aborts the session rather than dropping events (which would break
    /// parity).
    #[error("live bridge overflow: bounded event queue exceeded capacity {capacity}")]
    LiveBackpressure {
        /// The configured queue capacity that was exceeded.
        capacity: usize,
    },
}

/// Convenience alias for results in this crate.
pub type Result<T> = core::result::Result<T, AkadroError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages() {
        let e = AkadroError::UnknownInstrument(InstrumentId::new(3));
        assert!(format!("{e}").contains("unknown instrument"));

        let e = AkadroError::Config("no strategy".to_owned());
        assert_eq!(format!("{e}"), "invalid configuration: no strategy");

        let e = AkadroError::LiveBackpressure { capacity: 1024 };
        assert!(format!("{e}").contains("1024"));
    }

    #[test]
    fn result_alias_works() {
        // A function that really can fail, exercising both arms of the alias.
        fn checked(id: u32) -> Result<u8> {
            if id == 0 {
                Ok(7)
            } else {
                Err(AkadroError::UnknownInstrument(InstrumentId::new(id)))
            }
        }
        assert_eq!(checked(0).unwrap(), 7);
        assert!(matches!(checked(1), Err(AkadroError::UnknownInstrument(_))));
    }
}
