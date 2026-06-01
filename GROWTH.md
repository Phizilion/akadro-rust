# Where akadro can grow next (in plain language)

This is a non-technical map of what the project could become. Today it has a
solid, tested foundation: you can write a trading strategy once, run it on
historical data (a "backtest"), and the design **stops you from accidentally
peeking at the future** while testing — and the first real exchange, **MEXC**,
is connected. Here is what would make it more powerful, roughly easiest-and-most-
valuable first.

## 1. Connect more exchanges
The whole point of the design is that adding an exchange is a small, self-
contained job that touches nothing else. MEXC is done as the template; Binance,
Bybit, Coinbase, Kraven, Hyperliquid, etc. would each be a similar small add-on.
**Why it matters:** trade more markets, compare venues, avoid being locked to one.

## 2. Make the MEXC connection faster and fuller
Right now MEXC talks over simple "polling" (asking for new prices every bar) on
the **spot** market. Two upgrades:
- **Live streaming (WebSockets):** the exchange pushes prices/updates the instant
  they happen, instead of us asking repeatedly — lower latency, important for
  faster strategies.
- **Futures (leverage) trading:** support borrowing/leverage, funding payments,
  and shorting on MEXC's futures market.
- **Exact fees:** today we assume MEXC's standard fee; reading the real fee from
  each trade would make results penny-accurate.

## 3. Harden real live trading
The backtest and a simulated "live" feed already behave identically. To trade
real money safely we'd add: automatic reconnection (and catching up cleanly after
a dropped connection), a **paper-trading** mode (live prices, fake money), and
**safety rails** (maximum position size, a "kill switch", sanity checks before
sending orders).

## 4. More order types
Add stop-loss, take-profit, trailing stops, and bracket/OCO orders ("if it hits
this price, do X; otherwise Y"). **Why it matters:** most real strategies need
these to manage risk automatically.

## 5. Make backtests even more realistic
A backtest is only useful if it resembles reality. Add models for: **slippage**
(you don't always get the exact price), **partial fills** (big orders fill in
pieces), **latency** (orders take time to arrive), funding/borrow costs, and
liquidations. **Why it matters:** prevents strategies that look great on paper but
lose money live. (The honest limit: we can only fully prove "behaves the same
live" by also running against a *real* exchange with API keys — see §10.)

## 6. Speed for large datasets
For testing over years of data or thousands of parameter combinations: faster data
storage (compressed columnar files), a faster way to merge many data streams, and
running many backtests in parallel across CPU cores. The groundwork (and a
benchmark proving where the time goes) is already in place.

## 7. A library of indicators
Ready-made building blocks (moving averages, RSI, MACD, Bollinger Bands, …) so
strategy authors don't reinvent them. **Why it matters:** writing a strategy
becomes faster and less error-prone.

## 8. Performance and risk reporting
After a backtest, automatically produce the numbers traders care about: total
profit, Sharpe ratio (return vs. risk), maximum drawdown (worst dip), win rate,
turnover — plus charts. Also **walk-forward / out-of-sample testing**, which
guards against "fooling yourself" by tuning a strategy to past data.

## 9. Make strategies easier to write
A small amount of "magic" (a code macro) and friendlier error messages would hide
the most advanced part of the safety system from beginners, plus a written guide /
tutorial site. **Why it matters:** lowers the barrier so more people can use it.

## 10. Data and DEX support
Tools to download and cache historical data and detect bad/missing data. Longer
term: support decentralized exchanges (Uniswap-style), which need bigger-number
math and gas-fee modelling — deliberately left out of v1.

## 11. Multi-market strategies
Trade several instruments at once, or combine multiple timeframes (e.g. a 1-hour
trend filter on a 1-minute strategy).

---

## Known gaps we're being honest about (worth closing)
- **Prove it against a *real* exchange.** We've proven the backtest and a stand-in
  "live" feed behave identically; proving it against real MEXC needs API keys and
  network access (there's a ready, switched-off test for exactly this).
- **Confirm a few MEXC streaming details.** Some WebSocket message specifics
  couldn't be verified from documentation alone and should be checked live before
  relying on them (the REST path we built does not depend on them).
- **Show a true 100% test-coverage number.** Every function is tested, but the
  coverage *tool* available here can't "see" a handful of one-line helpers; a
  different tool (needs a component we couldn't install here) would report 100%.
