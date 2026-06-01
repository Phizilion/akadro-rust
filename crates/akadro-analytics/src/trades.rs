// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Trade-level statistics reconstructed from the fill log.
//!
//! A *trade* here is a flat → flat episode for one instrument (it may include a
//! position flip). Its `PnL` is the realized `PnL` over the episode **minus the
//! fees** of the fills that made it up (so every per-trade figure below — wins,
//! losses, `gross_profit`, `expectancy` — is *net of fees*, unlike
//! `RunReport::realized_pnl`, which is gross of fees). Open positions still running
//! at the end of the data are not counted. `PnL` figures are in `Money` raw
//! (fixed-point) units.
//!
//! Round trips are matched by **average-cost** accounting (the running average
//! entry price), not FIFO. For single-lot entries this is identical to FIFO; for a
//! position built from multiple entries at different prices it differs from the
//! FIFO convention used by pyfolio / `QuantConnect` LEAN.

use std::collections::HashMap;

use akadro_engine::FillRecord;

/// Aggregate statistics over completed (flat-to-flat) trades.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TradeStats {
    /// Number of completed trades.
    pub num_trades: usize,
    /// Trades with positive `PnL` (net of fees).
    pub wins: usize,
    /// Trades with negative `PnL` (net of fees).
    pub losses: usize,
    /// Trades with exactly zero `PnL` (net of fees). `wins + losses + break_even
    /// == num_trades`; `win_rate`'s denominator includes break-even trades.
    pub break_even: usize,
    /// Fraction of completed trades that were winners (`wins / num_trades`).
    pub win_rate: f64,
    /// Sum of winning-trade `PnL`, net of fees (Money raw units). Named "gross" in
    /// the trading sense (wins not yet netted against losses), not "before fees".
    pub gross_profit: f64,
    /// Magnitude of losing-trade `PnL`, net of fees (positive, Money raw units).
    pub gross_loss: f64,
    /// Mean winning-trade `PnL` (`gross_profit / wins`, `0` if no wins).
    pub avg_win: f64,
    /// Mean losing-trade magnitude (`gross_loss / losses`, `0` if no losses).
    pub avg_loss: f64,
    /// Reward/risk payoff `avg_win / avg_loss` (`+∞` if there are wins but no
    /// losses, `0` if no wins).
    pub payoff_ratio: f64,
    /// `gross_profit / gross_loss` (`+∞` if there is profit but no loss, `0` if
    /// there is no profit / no completed trades).
    pub profit_factor: f64,
    /// Mean `PnL` per completed trade (Money raw units). Algebraically equal to the
    /// canonical weighted expectancy `win_rate·avg_win − loss_rate·avg_loss`.
    pub expectancy: f64,
    /// Total fees across all fills (Money raw units).
    pub total_fees: f64,
}

#[derive(Default, Clone, Copy)]
struct Position {
    net: i64,
    avg: i64,
    realized: i128,
    fees: i128,
}

impl Position {
    /// Apply one fill; returns `Some(trade_pnl)` if this fill returned the
    /// position to flat (closing a trade).
    fn apply(&mut self, side_sign: i64, price: i64, qty: i64, fee: i128) -> Option<i128> {
        // A zero-qty fill cannot move the position; ignore it to avoid a
        // divide-by-zero in the average-entry update (m26).
        if qty == 0 {
            return None;
        }
        // Saturating mul + widen-before-`abs` so an extreme qty can never panic at
        // `i64::MIN` or wrap, matching the engine `Portfolio` (m27).
        let signed = side_sign.saturating_mul(qty);
        let pos = self.net;
        self.fees += fee;

        if pos == 0 || (pos > 0) == (signed > 0) {
            // Opening or increasing: update the average entry.
            let pos_abs = i128::from(pos).abs();
            let add_abs = i128::from(signed).abs();
            let total_cost = pos_abs * i128::from(self.avg) + add_abs * i128::from(price);
            let total_qty = pos_abs + add_abs; // > 0 (add_abs > 0 since signed != 0)
            // Round half away from zero — symmetric (matches the engine average).
            let half = total_qty / 2;
            let rounded = if total_cost >= 0 {
                (total_cost + half) / total_qty
            } else {
                (total_cost - half) / total_qty
            };
            self.avg = rounded as i64;
            self.net = pos.saturating_add(signed);
        } else {
            // Reducing / closing / flipping: realize `PnL` on the closed portion.
            let closing = i128::from(signed).abs().min(i128::from(pos).abs());
            let dir = i128::from(pos.signum());
            self.realized += closing * (i128::from(price) - i128::from(self.avg)) * dir;
            let new_net = pos.saturating_add(signed);
            self.net = new_net;
            if new_net == 0 {
                self.avg = 0;
            } else if (new_net > 0) != (pos > 0) {
                self.avg = price;
            }
        }

        if self.net == 0 {
            let pnl = self.realized - self.fees;
            self.realized = 0;
            self.fees = 0;
            Some(pnl)
        } else {
            None
        }
    }
}

impl TradeStats {
    /// Reconstruct trade statistics from a fill log.
    #[must_use]
    pub fn from_fills(fills: &[FillRecord]) -> Self {
        let mut positions: HashMap<u32, Position> = HashMap::new();
        let mut pnls: Vec<i128> = Vec::new();
        let mut total_fees: i128 = 0;

        for f in fills {
            total_fees += f.fee.raw();
            let pos = positions.entry(f.instrument.index()).or_default();
            if let Some(pnl) = pos.apply(f.side.sign(), f.price.raw(), f.qty.raw(), f.fee.raw()) {
                pnls.push(pnl);
            }
        }

        let num_trades = pnls.len();
        let wins = pnls.iter().filter(|&&p| p > 0).count();
        let losses = pnls.iter().filter(|&&p| p < 0).count();
        let gross_profit: f64 = pnls.iter().filter(|&&p| p > 0).map(|&p| p as f64).sum();
        let gross_loss: f64 = pnls.iter().filter(|&&p| p < 0).map(|&p| -p as f64).sum();
        let sum: f64 = pnls.iter().map(|&p| p as f64).sum();

        let avg_win = if wins > 0 {
            gross_profit / wins as f64
        } else {
            0.0
        };
        let avg_loss = if losses > 0 {
            gross_loss / losses as f64
        } else {
            0.0
        };
        // `+∞` for an all-wins record (best-possible), `0` only when there is no
        // profit at all — never conflate "perfect" with "no edge".
        let ratio_or_inf = |num: f64, den: f64| {
            if den > 0.0 {
                num / den
            } else if num > 0.0 {
                f64::INFINITY
            } else {
                0.0
            }
        };

        TradeStats {
            num_trades,
            wins,
            losses,
            break_even: num_trades - wins - losses,
            win_rate: if num_trades > 0 {
                wins as f64 / num_trades as f64
            } else {
                0.0
            },
            gross_profit,
            gross_loss,
            avg_win,
            avg_loss,
            payoff_ratio: ratio_or_inf(avg_win, avg_loss),
            profit_factor: ratio_or_inf(gross_profit, gross_loss),
            expectancy: if num_trades > 0 {
                sum / num_trades as f64
            } else {
                0.0
            },
            total_fees: total_fees as f64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{ClientOrderId, Costs, InstrumentId, Money, Price, Qty, Side, Timestamp};

    fn fill(side: Side, price: i64, qty: i64, fee: i128) -> FillRecord {
        let _ = Costs::new(); // keep the import meaningful across versions
        FillRecord {
            id: ClientOrderId::new(0),
            instrument: InstrumentId::new(0),
            side,
            price: Price::from_raw(price),
            qty: Qty::from_raw(qty),
            fee: Money::from_raw(fee),
            ts: Timestamp::from_nanos(1),
        }
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn zero_qty_fill_does_not_panic() {
        // m26: a zero-qty fill must not divide-by-zero in the average update; it is
        // ignored, leaving the position flat.
        let s = TradeStats::from_fills(&[fill(Side::Buy, 100, 0, 1)]);
        assert_eq!(s.num_trades, 0);
    }

    #[test]
    fn no_fills_is_empty() {
        let s = TradeStats::from_fills(&[]);
        assert_eq!(s.num_trades, 0);
        assert!(approx(s.win_rate, 0.0));
        assert!(approx(s.profit_factor, 0.0));
    }

    #[test]
    fn one_winning_round_trip() {
        // buy 1 @ 100, sell 1 @ 110 -> +10, no fees.
        let fills = [fill(Side::Buy, 100, 1, 0), fill(Side::Sell, 110, 1, 0)];
        let s = TradeStats::from_fills(&fills);
        assert_eq!(s.num_trades, 1);
        assert_eq!(s.wins, 1);
        assert_eq!(s.losses, 0);
        assert_eq!(s.break_even, 0);
        assert!(approx(s.win_rate, 1.0));
        assert!(approx(s.gross_profit, 10.0));
        assert!(approx(s.avg_win, 10.0));
        assert!(approx(s.avg_loss, 0.0));
        assert!(approx(s.expectancy, 10.0));
        // All wins, no losses: profit factor & payoff are +∞ (best-possible), NOT 0.
        assert!(s.profit_factor.is_infinite() && s.profit_factor > 0.0);
        assert!(s.payoff_ratio.is_infinite() && s.payoff_ratio > 0.0);
    }

    #[test]
    fn payoff_and_break_even() {
        // win +20, loss -10, break-even 0: payoff = avg_win/avg_loss = 20/10 = 2.
        let fills = [
            fill(Side::Buy, 100, 1, 0),
            fill(Side::Sell, 120, 1, 0), // +20
            fill(Side::Buy, 100, 1, 0),
            fill(Side::Sell, 90, 1, 0), // -10
            fill(Side::Buy, 100, 1, 0),
            fill(Side::Sell, 100, 1, 0), // 0 (break-even)
        ];
        let s = TradeStats::from_fills(&fills);
        assert_eq!(s.num_trades, 3);
        assert_eq!((s.wins, s.losses, s.break_even), (1, 1, 1));
        assert!(approx(s.avg_win, 20.0));
        assert!(approx(s.avg_loss, 10.0));
        assert!(approx(s.payoff_ratio, 2.0));
    }

    #[test]
    fn losing_trade_and_fees() {
        // buy 1 @ 100 (fee 1), sell 1 @ 90 (fee 1) -> -10 - 2 fees = -12.
        let fills = [fill(Side::Buy, 100, 1, 1), fill(Side::Sell, 90, 1, 1)];
        let s = TradeStats::from_fills(&fills);
        assert_eq!(s.num_trades, 1);
        assert_eq!(s.losses, 1);
        assert!(approx(s.gross_loss, 12.0));
        assert!(approx(s.total_fees, 2.0));
        assert!(approx(s.expectancy, -12.0));
    }

    #[test]
    fn mixed_trades_profit_factor() {
        let fills = [
            fill(Side::Buy, 100, 1, 0),
            fill(Side::Sell, 120, 1, 0), // +20 win
            fill(Side::Buy, 100, 1, 0),
            fill(Side::Sell, 90, 1, 0), // -10 loss
        ];
        let s = TradeStats::from_fills(&fills);
        assert_eq!(s.num_trades, 2);
        assert_eq!(s.wins, 1);
        assert_eq!(s.losses, 1);
        assert!(approx(s.win_rate, 0.5));
        assert!(approx(s.profit_factor, 2.0)); // 20 / 10
        assert!(approx(s.expectancy, 5.0)); // (20-10)/2
    }

    #[test]
    fn open_position_not_counted() {
        // buy then never close -> no completed trade.
        let s = TradeStats::from_fills(&[fill(Side::Buy, 100, 1, 0)]);
        assert_eq!(s.num_trades, 0);
    }
}

#[cfg(test)]
mod cov_tests {
    use super::*;
    use akadro_core::{ClientOrderId, InstrumentId, Money, Price, Qty, Side, Timestamp};
    fn f(side: Side, price: i64, qty: i64) -> FillRecord {
        FillRecord {
            id: ClientOrderId::new(0),
            instrument: InstrumentId::new(0),
            side,
            price: Price::from_raw(price),
            qty: Qty::from_raw(qty),
            fee: Money::ZERO,
            ts: Timestamp::from_nanos(1),
        }
    }
    #[test]
    fn flip_through_zero_is_one_trade() {
        // long 5 -> sell 8 (flip to short 3, sets new avg) -> buy 3 (flat): one trade.
        let s = TradeStats::from_fills(&[
            f(Side::Buy, 100, 5),
            f(Side::Sell, 120, 8),
            f(Side::Buy, 110, 3),
        ]);
        assert_eq!(s.num_trades, 1);
    }
}
