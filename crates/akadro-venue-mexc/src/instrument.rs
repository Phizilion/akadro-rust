// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `exchangeInfo` → [`InstrumentSpec`] + a symbol↔id registry.
//!
//! MEXC spot `exchangeInfo` has no Binance-style tick/lot/min-notional filters;
//! precision comes from `quoteAssetPrecision` (price decimals) and
//! `baseAssetPrecision` (qty decimals), with `baseSizePrecision` /
//! `quoteAmountPrecision` giving minimum size/notional as decimal *values*. We
//! derive the akadro `InstrumentSpec` from these and assign each tradable symbol
//! a dense [`InstrumentId`] (the engine's hot-path key).

use std::collections::HashMap;

use akadro_core::{
    AssetId, CapSet, Capability, InstrumentCatalog, InstrumentId, InstrumentKind, InstrumentSpec,
    Money, Price, Qty,
};
use serde::Deserialize;

use crate::convert::decimal_to_raw;
use crate::error::MexcError;

#[derive(Deserialize)]
struct ExchangeInfo {
    #[serde(default)]
    symbols: Vec<SymbolInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SymbolInfo {
    symbol: String,
    base_asset: String,
    quote_asset: String,
    #[serde(default)]
    base_asset_precision: u32,
    #[serde(default)]
    quote_asset_precision: u32,
    #[serde(default)]
    base_size_precision: String,
    #[serde(default)]
    quote_amount_precision: String,
    #[serde(default)]
    status: serde_json::Value,
}

/// A symbol is tradable when its status is `"1"` (online) or textual `ENABLED`.
/// (Do NOT gate on `isSpotTradingAllowed` — it is `false` even for active majors.)
fn is_tradable(status: &serde_json::Value) -> bool {
    match status {
        serde_json::Value::String(s) => s == "1" || s.eq_ignore_ascii_case("ENABLED"),
        serde_json::Value::Number(n) => n.as_i64() == Some(1),
        _ => false,
    }
}

fn intern(assets: &mut Vec<String>, ids: &mut HashMap<String, AssetId>, name: &str) -> AssetId {
    if let Some(id) = ids.get(name) {
        return *id;
    }
    let id = AssetId::new(assets.len() as u32);
    assets.push(name.to_owned());
    ids.insert(name.to_owned(), id);
    id
}

/// A MEXC instrument catalogue built from `exchangeInfo`.
#[derive(Debug, Clone)]
pub struct MexcCatalog {
    specs: Vec<InstrumentSpec>,
    symbols: Vec<String>,
    by_symbol: HashMap<String, InstrumentId>,
    assets: Vec<String>,
    scales: Vec<(u32, u32)>, // (price_scale, qty_scale) per instrument id
}

impl MexcCatalog {
    /// Parse a spot `exchangeInfo` JSON body into a catalogue. Only tradable
    /// symbols are included; they are numbered densely `0..n` in document order.
    pub fn from_exchange_info(json: &str) -> Result<Self, MexcError> {
        let info: ExchangeInfo =
            serde_json::from_str(json).map_err(|e| MexcError::Parse(e.to_string()))?;

        let mut specs = Vec::new();
        let mut symbols = Vec::new();
        let mut by_symbol = HashMap::new();
        let mut scales = Vec::new();
        let mut assets = Vec::new();
        let mut asset_ids = HashMap::new();

        for s in info.symbols {
            if !is_tradable(&s.status) {
                continue;
            }
            let id = InstrumentId::new(specs.len() as u32);
            let base = intern(&mut assets, &mut asset_ids, &s.base_asset);
            let quote = intern(&mut assets, &mut asset_ids, &s.quote_asset);
            let price_scale = s.quote_asset_precision;
            let qty_scale = s.base_asset_precision;

            // Minimum order size / notional come as decimal value strings. A
            // non-empty but UNPARSEABLE field must not silently fall back to a
            // guard-disabling default (m19) — surface it so a malformed
            // `exchangeInfo` is caught rather than producing a spec whose lot/min
            // checks never fire.
            let lot = if s.base_size_precision.is_empty() {
                1
            } else {
                decimal_to_raw(&s.base_size_precision, qty_scale)
                    .map_err(|_| {
                        MexcError::Parse("invalid baseSizePrecision in exchangeInfo".into())
                    })?
                    .max(1)
            };
            let min_notional = if s.quote_amount_precision.is_empty() {
                0
            } else {
                // Store at the COMBINED scale (price_scale + qty_scale): that is the
                // scale of `price.notional(qty)` that `meets_min_notional` compares
                // against, so the guard actually fires (at `price_scale` alone it
                // never would for a symbol with nonzero base precision).
                decimal_to_raw(&s.quote_amount_precision, price_scale + qty_scale).map_err(
                    |_| MexcError::Parse("invalid quoteAmountPrecision in exchangeInfo".into()),
                )?
            };

            let spec = InstrumentSpec::new(
                id,
                base,
                quote,
                InstrumentKind::Spot,
                Price::from_raw(1), // tick = one raw unit at price_scale
                Qty::from_raw(lot),
                Money::from_raw(i128::from(min_notional)),
                CapSet::empty()
                    .with(Capability::LimitOrders)
                    .with(Capability::PostOnly),
            );
            by_symbol.insert(s.symbol.clone(), id);
            symbols.push(s.symbol);
            specs.push(spec);
            scales.push((price_scale, qty_scale));
        }

        Ok(MexcCatalog {
            specs,
            symbols,
            by_symbol,
            assets,
            scales,
        })
    }

    /// Number of tradable instruments.
    #[must_use]
    pub fn len(&self) -> usize {
        self.specs.len()
    }

    /// `true` if no instruments were registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// The dense id for a venue symbol (e.g. `"BTCUSDT"`).
    #[must_use]
    pub fn id_of(&self, symbol: &str) -> Option<InstrumentId> {
        self.by_symbol.get(symbol).copied()
    }

    /// The venue symbol for a dense id.
    #[must_use]
    pub fn symbol_of(&self, id: InstrumentId) -> Option<&str> {
        self.symbols.get(id.index() as usize).map(String::as_str)
    }

    /// The `(price_scale, qty_scale)` for an instrument.
    #[must_use]
    pub fn scales(&self, id: InstrumentId) -> Option<(u32, u32)> {
        self.scales.get(id.index() as usize).copied()
    }

    /// The asset name for an interned [`AssetId`].
    #[must_use]
    pub fn asset_name(&self, asset: AssetId) -> Option<&str> {
        self.assets.get(asset.index() as usize).map(String::as_str)
    }

    /// The specs, densely numbered — ready for `Engine::new`.
    #[must_use]
    pub fn specs(&self) -> &[InstrumentSpec] {
        &self.specs
    }
}

impl InstrumentCatalog for MexcCatalog {
    fn spec(&self, id: InstrumentId) -> Option<&InstrumentSpec> {
        self.specs.get(id.index() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "symbols": [
        {"symbol":"BTCUSDT","baseAsset":"BTC","quoteAsset":"USDT",
         "baseAssetPrecision":6,"quoteAssetPrecision":2,
         "baseSizePrecision":"0.000001","quoteAmountPrecision":"5","status":"1"},
        {"symbol":"ETHUSDT","baseAsset":"ETH","quoteAsset":"USDT",
         "baseAssetPrecision":5,"quoteAssetPrecision":2,
         "baseSizePrecision":"0.00001","quoteAmountPrecision":"5","status":"ENABLED"},
        {"symbol":"DEADUSDT","baseAsset":"DEAD","quoteAsset":"USDT",
         "baseAssetPrecision":2,"quoteAssetPrecision":2,
         "baseSizePrecision":"0.01","quoteAmountPrecision":"1","status":"3"}
      ]
    }"#;

    #[test]
    fn parses_only_tradable_symbols_densely() {
        let cat = MexcCatalog::from_exchange_info(SAMPLE).unwrap();
        assert_eq!(cat.len(), 2); // DEADUSDT (status 3) excluded
        assert!(!cat.is_empty());
        assert_eq!(cat.id_of("BTCUSDT"), Some(InstrumentId::new(0)));
        assert_eq!(cat.id_of("ETHUSDT"), Some(InstrumentId::new(1)));
        assert_eq!(cat.id_of("DEADUSDT"), None);
        assert_eq!(cat.symbol_of(InstrumentId::new(0)), Some("BTCUSDT"));
        assert_eq!(cat.symbol_of(InstrumentId::new(9)), None);
    }

    #[test]
    fn derives_spec_fields() {
        let cat = MexcCatalog::from_exchange_info(SAMPLE).unwrap();
        let btc = cat.spec(InstrumentId::new(0)).unwrap();
        assert_eq!(btc.kind, InstrumentKind::Spot);
        assert_eq!(btc.tick_size, Price::from_raw(1));
        // baseSizePrecision "0.000001" at qty_scale 6 -> 1 raw lot.
        assert_eq!(btc.lot_size, Qty::from_raw(1));
        // quoteAmountPrecision "5" stored at the COMBINED scale price_scale(2) +
        // qty_scale(6) = 8 -> 5 * 10^8 raw, so it matches `price.notional(qty)`.
        assert_eq!(btc.min_notional, Money::from_raw(500_000_000));
        // The guard now actually fires: $5.00 notional passes, $0.50 fails
        // (price $50,000 = raw 5_000_000 @ scale 2).
        let px = Price::from_raw(5_000_000);
        assert!(btc.meets_min_notional(px, Qty::from_raw(100))); // 0.0001 BTC = $5
        assert!(!btc.meets_min_notional(px, Qty::from_raw(10))); // 0.00001 BTC = $0.50
        assert!(btc.caps.contains(Capability::LimitOrders));
        assert!(btc.caps.contains(Capability::PostOnly));
        assert_eq!(cat.scales(InstrumentId::new(0)), Some((2, 6)));
    }

    #[test]
    fn interns_assets_shared_quote() {
        let cat = MexcCatalog::from_exchange_info(SAMPLE).unwrap();
        let btc = cat.spec(InstrumentId::new(0)).unwrap();
        let eth = cat.spec(InstrumentId::new(1)).unwrap();
        // BTC and ETH differ in base but share the same interned USDT quote.
        assert_ne!(btc.base, eth.base);
        assert_eq!(btc.quote, eth.quote);
        assert_eq!(cat.asset_name(btc.base), Some("BTC"));
        assert_eq!(cat.asset_name(btc.quote), Some("USDT"));
    }

    #[test]
    fn rejects_garbage_json() {
        assert!(MexcCatalog::from_exchange_info("not json").is_err());
        // Missing symbols array -> empty catalogue (not an error).
        let empty = MexcCatalog::from_exchange_info("{}").unwrap();
        assert!(empty.is_empty());
    }
}

#[cfg(test)]
mod cov_tests {
    use super::*;

    // status as a JSON *number* 1 (tradable), a number 0 (not), and a non-string
    // non-number value (the `_` arm). The numeric-1 symbol also omits the
    // precision strings, exercising the empty-precision defaults (lot 1, min 0).
    const NUMERIC_STATUS: &str = r#"{
      "symbols": [
        {"symbol":"AAAUSDT","baseAsset":"AAA","quoteAsset":"USDT",
         "baseAssetPrecision":4,"quoteAssetPrecision":2,"status":1},
        {"symbol":"BBBUSDT","baseAsset":"BBB","quoteAsset":"USDT",
         "baseAssetPrecision":4,"quoteAssetPrecision":2,"status":0},
        {"symbol":"CCCUSDT","baseAsset":"CCC","quoteAsset":"USDT",
         "baseAssetPrecision":4,"quoteAssetPrecision":2,"status":true}
      ]
    }"#;

    #[test]
    fn numeric_and_non_scalar_status() {
        let cat = MexcCatalog::from_exchange_info(NUMERIC_STATUS).unwrap();
        // Only the numeric-1 symbol is tradable.
        assert_eq!(cat.len(), 1);
        assert_eq!(cat.id_of("AAAUSDT"), Some(InstrumentId::new(0)));
        assert_eq!(cat.id_of("BBBUSDT"), None); // status 0
        assert_eq!(cat.id_of("CCCUSDT"), None); // status true (the `_` arm)
    }

    #[test]
    fn empty_precision_strings_default_lot_and_notional() {
        let cat = MexcCatalog::from_exchange_info(NUMERIC_STATUS).unwrap();
        let spec = cat.spec(InstrumentId::new(0)).unwrap();
        // Absent baseSizePrecision -> lot defaults to 1 raw.
        assert_eq!(spec.lot_size, Qty::from_raw(1));
        // Absent quoteAmountPrecision -> min_notional defaults to 0.
        assert_eq!(spec.min_notional, Money::from_raw(0));
    }
}
