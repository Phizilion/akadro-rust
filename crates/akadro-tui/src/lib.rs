// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A live terminal dashboard for an akadro run — price & equity charts, a recent-
//! fills table, and a stats line — driven by the engine's
//! [`akadro_engine::Observer`] hook, so it works for both backtest and
//! (paper/live) runs with no change to the strategy.
//!
//! ```no_run
//! # use akadro_engine::Engine;
//! # use akadro_tui::{TuiObserver, TuiConfig};
//! # fn demo<S: akadro_engine::Strategy, D: akadro_core::DataSource, X: akadro_core::ExecutionClient>(engine: Engine<S, D, X>) {
//! let mut tui = TuiObserver::new("BTCUSDT 1m", TuiConfig::default());
//! let report = engine.run_observed(&mut tui); // renders each bar; restores the terminal on drop
//! # let _ = report;
//! # }
//! ```
//!
//! Strategy / client code selects the view via [`TuiConfig`] (which panels, the
//! rolling window, the render throttle, and the fixed-point scales). The render
//! state ([`TuiState`]) is decoupled from terminal I/O so it is unit-testable.

use akadro_core::{Money, Side};
use akadro_engine::{FillRecord, Observer, RunReport};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table},
};

/// Which panels the dashboard shows and how — the knob strategy/client code uses
/// to pick its view.
#[derive(Debug, Clone, Copy)]
pub struct TuiConfig {
    /// Show the rolling price chart.
    pub show_price: bool,
    /// Show the equity-curve chart.
    pub show_equity: bool,
    /// Show the recent-fills table.
    pub show_fills: bool,
    /// Rolling window (points kept) for the charts.
    pub max_points: usize,
    /// Render once per this many bars (throttle for high-frequency runs; `1` =
    /// every bar).
    pub render_every: u64,
    /// Price fixed-point scale (decimals) for display.
    pub price_scale: u32,
    /// Money fixed-point scale (decimals) for display.
    pub money_scale: u32,
    /// Quantity fixed-point scale (decimals) for display — used for fill sizes,
    /// which are quantities, not money (i14).
    pub qty_scale: u32,
}

impl Default for TuiConfig {
    fn default() -> Self {
        TuiConfig {
            show_price: true,
            show_equity: true,
            show_fills: true,
            max_points: 240,
            render_every: 1,
            price_scale: 2,
            money_scale: 2,
            qty_scale: 2,
        }
    }
}

/// Accumulated, render-ready display state — **no terminal I/O**, so it is
/// unit-testable. The [`Observer`] wrapper ([`TuiObserver`]) feeds it.
#[derive(Debug)]
pub struct TuiState {
    title: String,
    cfg: TuiConfig,
    price: Vec<(f64, f64)>,
    equity: Vec<(f64, f64)>,
    fills: Vec<FillRow>,
    fills_seen: usize,
    bars: u64,
    last_price: f64,
    last_equity: f64,
    final_pnl: Option<f64>,
}

#[derive(Debug, Clone)]
struct FillRow {
    ts: i64,
    side: Side,
    price: f64,
    qty: f64,
}

impl TuiState {
    /// Create empty state for `title` with `cfg`.
    #[must_use]
    pub fn new(title: impl Into<String>, cfg: TuiConfig) -> Self {
        TuiState {
            title: title.into(),
            cfg,
            price: Vec::new(),
            equity: Vec::new(),
            fills: Vec::new(),
            fills_seen: 0,
            bars: 0,
            last_price: 0.0,
            last_equity: 0.0,
            final_pnl: None,
        }
    }

    fn scale(raw: i64, decimals: u32) -> f64 {
        raw as f64 / 10f64.powi(decimals as i32)
    }

    /// Record a bar: append the close to the price chart, equity to the equity
    /// chart, and any new fills to the recent-fills table (trimmed to the window).
    pub fn push_bar(&mut self, close_raw: i64, equity: Money, fills: &[FillRecord]) {
        let x = self.bars as f64;
        self.last_price = Self::scale(close_raw, self.cfg.price_scale);
        self.last_equity = Self::scale(
            equity
                .raw()
                .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
            self.cfg.money_scale,
        );
        self.price.push((x, self.last_price));
        self.equity.push((x, self.last_equity));
        // `remove(0)` shifts the bounded `max_points`-long window (O(n)). Kept a `Vec`
        // deliberately: ratatui's `Dataset::data` needs a *contiguous* `&[(f64, f64)]`, so
        // a `VecDeque` would force `make_contiguous` (a `&mut` at render) or a per-frame
        // clone — and at a dashboard's human-cadence updates over a small capped window
        // the shift is free. Not a hot path.
        let cap = self.cfg.max_points.max(1);
        if self.price.len() > cap {
            self.price.remove(0);
        }
        if self.equity.len() > cap {
            self.equity.remove(0);
        }
        // New fills since the last bar.
        for f in &fills[self.fills_seen.min(fills.len())..] {
            self.fills.push(FillRow {
                ts: f.ts.as_nanos(),
                side: f.side,
                price: Self::scale(f.price.raw(), self.cfg.price_scale),
                qty: Self::scale(f.qty.raw(), self.cfg.qty_scale),
            });
        }
        self.fills_seen = fills.len();
        // Keep only the most recent fills for the table.
        let keep = 12;
        if self.fills.len() > keep {
            let drop = self.fills.len() - keep;
            self.fills.drain(0..drop);
        }
        self.bars += 1;
    }

    fn finish(&mut self, report: &RunReport) {
        let first = report.equity_curve.first().map(|p| p.equity.raw());
        let last = report.equity_curve.last().map(|p| p.equity.raw());
        if let (Some(f), Some(l)) = (first, last) {
            self.final_pnl = Some(Self::scale(
                (l - f).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
                self.cfg.money_scale,
            ));
        }
    }

    /// Number of bars recorded so far (for tests / status).
    #[must_use]
    pub fn bars(&self) -> u64 {
        self.bars
    }
}

/// Bounds `[min, max]` of the y-values in `data`, padded slightly; falls back to
/// `[0, 1]` for empty/degenerate data.
fn y_bounds(data: &[(f64, f64)]) -> [f64; 2] {
    if data.is_empty() {
        return [0.0, 1.0];
    }
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for &(_, y) in data {
        lo = lo.min(y);
        hi = hi.max(y);
    }
    if (hi - lo).abs() < f64::EPSILON {
        return [lo - 1.0, hi + 1.0];
    }
    let pad = (hi - lo) * 0.05;
    [lo - pad, hi + pad]
}

fn x_bounds(data: &[(f64, f64)]) -> [f64; 2] {
    match (data.first(), data.last()) {
        (Some(a), Some(b)) if b.0 > a.0 => [a.0, b.0],
        _ => [0.0, 1.0],
    }
}

/// Render the full dashboard for `state` into `frame`.
pub fn render(frame: &mut Frame, state: &TuiState) {
    let mut rows: Vec<Constraint> = Vec::new();
    if state.cfg.show_price {
        rows.push(Constraint::Min(6));
    }
    if state.cfg.show_equity {
        rows.push(Constraint::Min(6));
    }
    if state.cfg.show_fills {
        rows.push(Constraint::Length(14));
    }
    rows.push(Constraint::Length(3)); // status line
    let areas = Layout::vertical(rows).split(frame.area());
    let mut i = 0;

    if state.cfg.show_price {
        line_chart(
            frame,
            areas[i],
            &format!("{} — price ({:.2})", state.title, state.last_price),
            &state.price,
            Color::Cyan,
        );
        i += 1;
    }
    if state.cfg.show_equity {
        line_chart(
            frame,
            areas[i],
            &format!("equity ({:.2})", state.last_equity),
            &state.equity,
            Color::Green,
        );
        i += 1;
    }
    if state.cfg.show_fills {
        fills_table(frame, areas[i], state);
        i += 1;
    }
    status_line(frame, areas[i], state);
}

fn line_chart(frame: &mut Frame, area: Rect, title: &str, data: &[(f64, f64)], color: Color) {
    let xb = x_bounds(data);
    let yb = y_bounds(data);
    let datasets = vec![
        Dataset::default()
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(color))
            .data(data),
    ];
    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(title.to_string())
                .borders(Borders::ALL),
        )
        .x_axis(Axis::default().bounds(xb))
        .y_axis(Axis::default().bounds(yb).labels(vec![
            Span::raw(format!("{:.2}", yb[0])),
            Span::raw(format!("{:.2}", yb[1])),
        ]));
    frame.render_widget(chart, area);
}

fn fills_table(frame: &mut Frame, area: Rect, state: &TuiState) {
    let header = Row::new(vec![
        Cell::from("time(ns)"),
        Cell::from("side"),
        Cell::from("price"),
        Cell::from("qty"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = state.fills.iter().map(|f| {
        let (side, color) = match f.side {
            Side::Buy => ("BUY", Color::Green),
            Side::Sell => ("SELL", Color::Red),
        };
        Row::new(vec![
            Cell::from(f.ts.to_string()),
            Cell::from(side).style(Style::default().fg(color)),
            Cell::from(format!("{:.2}", f.price)),
            Cell::from(format!("{:.4}", f.qty)),
        ])
    });
    let widths = [
        Constraint::Length(22),
        Constraint::Length(6),
        Constraint::Length(14),
        Constraint::Length(14),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().title("recent fills").borders(Borders::ALL));
    frame.render_widget(table, area);
}

fn status_line(frame: &mut Frame, area: Rect, state: &TuiState) {
    let pnl = state
        .final_pnl
        .map_or_else(|| "running".to_string(), |p| format!("PnL {p:+.2}"));
    let text = Line::from(vec![
        Span::styled(
            format!(" bars {} ", state.bars),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw(format!(
            "| last {:.2} | equity {:.2} | {pnl}",
            state.last_price, state.last_equity
        )),
    ]);
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

/// An [`Observer`] that renders the live dashboard to the terminal each bar.
/// Construct with [`TuiObserver::new`], pass `&mut` it to
/// [`Engine::run_observed`](akadro_engine::Engine::run_observed); the terminal is
/// restored automatically on drop (even on panic).
pub struct TuiObserver {
    terminal: DefaultTerminal,
    state: TuiState,
}

impl core::fmt::Debug for TuiObserver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TuiObserver")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl TuiObserver {
    /// Enter the alternate screen / raw mode and start a dashboard titled `title`.
    #[must_use]
    pub fn new(title: impl Into<String>, cfg: TuiConfig) -> Self {
        let terminal = ratatui::init();
        TuiObserver {
            terminal,
            state: TuiState::new(title, cfg),
        }
    }

    fn draw(&mut self) {
        let state = &self.state;
        let _ = self.terminal.draw(|f| render(f, state));
    }
}

impl Observer for TuiObserver {
    fn on_bar(&mut self, bar: akadro_core::Bar, equity: Money, fills: &[FillRecord]) {
        self.state.push_bar(bar.close.raw(), equity, fills);
        if self
            .state
            .bars
            .is_multiple_of(self.state.cfg.render_every.max(1))
        {
            self.draw();
        }
    }

    fn on_finish(&mut self, report: &RunReport) {
        self.state.finish(report);
        self.draw();
    }
}

impl Drop for TuiObserver {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akadro_core::{ClientOrderId, InstrumentId, Price, Qty, Timestamp};

    fn fill(ts: i64, side: Side, price: i64, qty: i64) -> FillRecord {
        FillRecord::new(
            ClientOrderId::new(0),
            InstrumentId::new(0),
            side,
            Price::from_raw(price),
            Qty::from_raw(qty),
            Money::ZERO,
            Timestamp::from_nanos(ts),
        )
    }

    #[test]
    fn state_accumulates_price_equity_and_new_fills() {
        let cfg = TuiConfig {
            max_points: 2,
            price_scale: 2,
            money_scale: 2,
            ..TuiConfig::default()
        };
        let mut s = TuiState::new("t", cfg);
        // Bar 1: no fills.
        s.push_bar(10_000, Money::from_raw(100_000), &[]);
        // Bar 2: one new fill.
        let fills = vec![fill(1, Side::Buy, 10_000, 50)];
        s.push_bar(10_100, Money::from_raw(100_500), &fills);
        // Bar 3: same fills slice (no NEW fill) — table unchanged.
        s.push_bar(10_050, Money::from_raw(100_200), &fills);

        assert_eq!(s.bars(), 3);
        // max_points = 2 → only the last two price/equity points retained.
        assert_eq!(s.price.len(), 2);
        assert_eq!(s.equity.len(), 2);
        assert!((s.last_price - 100.50).abs() < 1e-9);
        // Exactly one fill recorded (the dedup by fills_seen worked).
        assert_eq!(s.fills.len(), 1);
        assert!((s.fills[0].price - 100.0).abs() < 1e-9);
    }

    fn arr_eq(a: [f64; 2], b: [f64; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9
    }

    fn buffer_text(t: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        t.backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn render_draws_all_panels_headless() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // Default config has all panels on; populate price/equity/fills so every
        // render branch (line_chart × 2, fills_table with BUY+SELL rows, status_line
        // with a finished PnL) executes.
        let mut s = TuiState::new("dash", TuiConfig::default());
        s.push_bar(10_000, Money::from_raw(100_000), &[]);
        s.push_bar(
            10_100,
            Money::from_raw(100_500),
            &[
                fill(1, Side::Buy, 10_000, 50),
                fill(2, Side::Sell, 10_200, 30),
            ],
        );
        let mut report = RunReport::default();
        report.equity_curve = vec![akadro_engine::EquityPoint {
            ts: Timestamp::from_nanos(1),
            equity: Money::from_raw(110_000),
        }];
        s.finish(&report); // sets final_pnl -> status line's Some(p) branch

        let mut terminal = Terminal::new(TestBackend::new(140, 44)).unwrap();
        terminal.draw(|f| render(f, &s)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("price"), "price chart title: {text:.0}");
        assert!(text.contains("equity"));
        assert!(text.contains("recent fills"));
        assert!(text.contains("BUY") && text.contains("SELL"));
        assert!(text.contains("bars"));
        assert!(text.contains("PnL"));
    }

    #[test]
    fn render_with_all_panels_disabled_draws_only_status() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // Exercise the show_price/show_equity/show_fills == false branches.
        let cfg = TuiConfig {
            show_price: false,
            show_equity: false,
            show_fills: false,
            ..TuiConfig::default()
        };
        let s = TuiState::new("min", cfg);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|f| render(f, &s)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("bars")); // only the status line rendered
        assert!(!text.contains("recent fills"));
        assert!(text.contains("running")); // no final PnL -> "running" branch
    }

    #[test]
    fn bounds_helpers() {
        assert!(arr_eq(y_bounds(&[]), [0.0, 1.0]));
        let b = y_bounds(&[(0.0, 10.0), (1.0, 20.0)]);
        assert!(b[0] < 10.0 && b[1] > 20.0); // padded
        // Flat series gets a ±1 band.
        assert!(arr_eq(y_bounds(&[(0.0, 5.0), (1.0, 5.0)]), [4.0, 6.0]));
        assert!(arr_eq(x_bounds(&[]), [0.0, 1.0]));
        assert!(arr_eq(x_bounds(&[(3.0, 0.0), (7.0, 0.0)]), [3.0, 7.0]));
    }

    #[test]
    fn finish_computes_pnl_from_equity_curve() {
        let cfg = TuiConfig::default();
        let mut s = TuiState::new("t", cfg);
        let mut report = RunReport::default();
        report.equity_curve = vec![
            akadro_engine::EquityPoint {
                ts: Timestamp::from_nanos(1),
                equity: Money::from_raw(100_000),
            },
            akadro_engine::EquityPoint {
                ts: Timestamp::from_nanos(2),
                equity: Money::from_raw(110_000),
            },
        ];
        s.finish(&report);
        assert!((s.final_pnl.unwrap() - 100.0).abs() < 1e-9); // (110000-100000)/100
    }
}
