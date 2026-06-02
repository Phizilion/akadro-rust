#!/usr/bin/env bash
#
# scaffold-lab.sh — create an akadro "strategy lab": a consumer crate set up for an
# agent (or human) to write strategies against akadro's PUBLIC API, with the API
# docs bundled, the source locked away, and the `unsafe`-verification gate wired in.
#
# Produces a directory structured like the reference `akadro-lab`:
#
#   <lab>/
#   ├── Cargo.toml              path dep on akadro (+ venue-mexc), unsafe forbidden
#   ├── rust-toolchain.toml     pinned 1.95
#   ├── AGENTS.md               agent rules (copied from akadro-rust/agentic/)
#   ├── CLAUDE.md -> AGENTS.md   symlink
#   ├── GUIDE.md                strategy guide (copied from akadro-rust/)
#   ├── src/main.rs             LOCKED: #![forbid(unsafe_code)] + mod strategies
#   ├── src/strategies/mod.rs   the ONLY place the agent edits
#   ├── api/{doc,json,examples} bundled public API (no akadro source)
#   ├── .claude/settings.json   deny-list: no reading akadro source, locked files
#   └── scripts/{refresh-akadro-api,verify-no-unsafe,verify-no-custom-datasource}.sh
#
# Usage:  agentic/scaffold-lab.sh <target-dir> [--force]
#
set -euo pipefail

AKADRO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # .../akadro-rust
AGENTIC_DIR="$AKADRO_DIR/agentic"
CARGO="${CARGO:-cargo}"

# --- args ---------------------------------------------------------------------
TARGET="${1:-}"
FORCE="${2:-}"
if [ -z "$TARGET" ] || [ "$TARGET" = "--help" ] || [ "$TARGET" = "-h" ]; then
  echo "usage: $0 <target-dir> [--force]"; exit 1
fi
mkdir -p "$TARGET"
LAB_DIR="$(cd "$TARGET" && pwd)"
LAB_NAME="$(basename "$LAB_DIR")"
# Cargo package/bin name: sanitise the dir name (fallback to akadro-lab).
LAB_PKG="$(printf '%s' "$LAB_NAME" | tr -c 'a-zA-Z0-9_-' '-' | sed 's/^-*//; s/-*$//')"
[ -z "$LAB_PKG" ] && LAB_PKG="akadro-lab"

if [ -e "$LAB_DIR/Cargo.toml" ] && [ "$FORCE" != "--force" ]; then
  echo "refusing: $LAB_DIR already looks like a crate (use --force to overwrite scaffolding)"; exit 1
fi

echo "akadro    : $AKADRO_DIR"
echo "new lab   : $LAB_DIR   (package '$LAB_PKG')"

mkdir -p "$LAB_DIR/src/strategies" "$LAB_DIR/.claude" "$LAB_DIR/scripts" "$LAB_DIR/api"

# --- Cargo.toml (path dep, unsafe forbidden) ----------------------------------
cat > "$LAB_DIR/Cargo.toml" <<EOF
[package]
name = "$LAB_PKG"
version = "0.1.0"
edition = "2024"
rust-version = "1.95"
publish = false

[dependencies]
# akadro by PATH. Cargo (a subprocess) reads its source to compile; the agent's
# file tools are denied that path by .claude/settings.json — builds, can't read.
akadro = { path = "$AKADRO_DIR/crates/akadro", features = ["analytics", "indicators", "data"] }
akadro-venue-mexc = { path = "$AKADRO_DIR/crates/akadro-venue-mexc", features = ["net"] }

[lints.rust]
# Mirrored by #![forbid(unsafe_code)] in src/main.rs and enforced by
# scripts/verify-no-unsafe.sh. The look-ahead guarantee only holds for safe code.
unsafe_code = "forbid"
EOF

cat > "$LAB_DIR/rust-toolchain.toml" <<'EOF'
[toolchain]
# akadro is MSRV 1.95 / edition 2024.
channel = "1.95.0"
EOF

cat > "$LAB_DIR/.gitignore" <<'EOF'
/target
# api/doc is bulky + regenerable (scripts/refresh-akadro-api.sh). The agent is
# denied akadro source so it cannot regenerate; commit json + examples.
/api/doc
EOF

# --- src/main.rs (LOCKED) -----------------------------------------------------
cat > "$LAB_DIR/src/main.rs" <<'EOF'
#![forbid(unsafe_code)]
//! Lab entry point — **LOCKED FILE, do not edit.**
//!
//! `#![forbid(unsafe_code)]` binds the ENTIRE crate, including `strategies/`. It
//! cannot be locally overridden (an inner `#[allow]`/`unsafe` is a hard error) and
//! is re-checked by `scripts/verify-no-unsafe.sh` even if this attribute were
//! deleted. You write in `src/strategies/` and wire into `strategies::run()`.

mod strategies;

fn main() {
    strategies::run();
}
EOF

# --- src/strategies/mod.rs (agent-editable) -----------------------------------
cat > "$LAB_DIR/src/strategies/mod.rs" <<'EOF'
//! # Your strategies live here — the ONLY directory you edit
//!
//! Add files (`mod my_strategy;`) and implement [`run`]. The crate-root
//! `#![forbid(unsafe_code)]` binds everything here; `unsafe` will not compile.
//! API: read `../../GUIDE.md`, `../../api/doc/akadro/index.html`, `../../api/examples/`.

use akadro::prelude::*;

/// Entry point, called by `main()`. Replace the body with your research.
pub fn run() {
    // Starter smoke test: construct the pieces that need no data, proving the
    // toolchain + akadro resolve. (No `HistoricalFeed::from_bars` — raw Vec injection
    // is gated off here — and no custom `DataSource`, which is forbidden in this lab.)
    let spec = InstrumentSpec::new(
        InstrumentId::new(0),
        AssetId::new(0),
        AssetId::new(1),
        InstrumentKind::Spot,
        Price::from_raw(1),
        Qty::from_raw(1),
        Money::ZERO,
        CapSet::empty(),
    );
    let _exchange = SimulatedExchange::new(vec![spec], 0);
    let _strategy = Flat;

    // Your real backtest gets its feed ONLY from the akadro data API — never your own
    // data. Uncomment and adapt (see GUIDE.md + api/examples/full_pipeline.rs):
    //
    //   use std::path::Path;
    //   let feed = akadro::data::load_or_cache_feed(
    //       Path::new("cache/BTCUSDT-1m.feather"), spec.id, /*price_scale*/ 2, /*qty_scale*/ 6,
    //       || akadro_venue_mexc::MexcKlineFeed::new(/* base_url, symbol, ... */),
    //   ).expect("fetch/cache");
    //   let report = Engine::new(&[spec], Money::from_raw(1_000_000), feed,
    //       SimulatedExchange::new(vec![spec], 0), Flat).unwrap().run();
    //   println!("bars processed: {}", report.bars_processed);

    println!("lab OK — akadro resolves; drive the Engine with akadro::data::load_or_cache_feed(..)");
}

/// Starter strategy: flat (does nothing). Replace with your logic.
struct Flat;

impl Strategy for Flat {
    fn on_bar(&mut self, _bar: Bar, _ctx: &mut Ctx<'_>) {}
}
EOF

# --- canonical docs: README.md + GUIDE.md + AGENTS.md (+ CLAUDE.md symlink) ----
cp "$AKADRO_DIR/README.md"      "$LAB_DIR/README.md"
cp "$AKADRO_DIR/GUIDE.md"       "$LAB_DIR/GUIDE.md"
cp "$AGENTIC_DIR/AGENTS.md"     "$LAB_DIR/AGENTS.md"
ln -sf AGENTS.md "$LAB_DIR/CLAUDE.md"

# --- .claude/settings.json (deny-list, absolute paths) ------------------------
A="//${AKADRO_DIR#/}"
L="//${LAB_DIR#/}"
cat > "$LAB_DIR/.claude/settings.json" <<EOF
{
  "permissions": {
    "deny": [
      "Read($A/**)",
      "Edit($A/**)",

      "Edit($L/src/main.rs)",
      "Edit($L/Cargo.toml)",
      "Edit($L/rust-toolchain.toml)",
      "Edit($L/build.rs)",
      "Edit($L/.cargo/**)",
      "Edit($L/api/**)",
      "Edit($L/README.md)",
      "Edit($L/AGENTS.md)",
      "Edit($L/CLAUDE.md)",
      "Edit($L/GUIDE.md)",

      "Edit($L/.claude/**)",
      "Edit($L/scripts/**)"
    ]
  }
}
EOF

# --- scripts/refresh-akadro-api.sh (AKADRO_DIR baked in) -----------------------
{
  printf '#!/usr/bin/env bash\n'
  printf '# Regenerate the API bundle the agent reads. Run as MAINTAINER (the agent is\n'
  printf '# denied akadro source, so it cannot run this).\n'
  printf 'set -euo pipefail\n'
  printf 'AKADRO_DIR=%q\n' "$AKADRO_DIR"
  cat <<'REFRESH_BODY'
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
API_DIR="$LAB_DIR/api"
CARGO="${CARGO:-cargo}"
echo "akadro: $AKADRO_DIR  ->  bundle: $API_DIR"

# 1. HTML docs. The umbrella is built with the LAB'S feature set (not --all-features)
#    so the off-by-default `escape-hatch` footgun API (the IS/OOS-leaking generic
#    walk-forward runners) is NOT advertised — the docs match what the lab can call.
( cd "$AKADRO_DIR" && "$CARGO" doc --no-deps -p akadro --features "analytics,indicators,data" >/dev/null )
( cd "$AKADRO_DIR" && "$CARGO" doc --no-deps -p akadro-venue-mexc --all-features >/dev/null 2>&1 ) || true
rm -rf "$API_DIR/doc"; mkdir -p "$API_DIR"
cp -r "$AKADRO_DIR/target/doc" "$API_DIR/doc"
rm -rf "$API_DIR/doc/src"
echo "  doc/  : HTML (source browser stripped)"

# 2. Machine-readable rustdoc JSON, per crate (nightly; skipped if absent).
if PATH="$HOME/.cargo/bin:$PATH" rustup run nightly rustdoc --version >/dev/null 2>&1; then
  rm -rf "$API_DIR/json"; mkdir -p "$API_DIR/json"
  for c in akadro-core akadro-engine akadro-backtest akadro-analytics \
           akadro-indicators akadro-data akadro-live akadro-testkit \
           akadro-venue-mexc akadro; do
    # Match the lab's feature set: never enable the off-by-default `escape-hatch`
    # footgun feature (only `akadro` + `akadro-analytics` define it).
    case "$c" in
      akadro)           feats="--features analytics,indicators,data" ;;
      akadro-analytics) feats="" ;;
      *)                feats="--all-features" ;;
    esac
    # shellcheck disable=SC2086 # $feats is an intentional word-split flag list
    if ( cd "$AKADRO_DIR" && "$CARGO" +nightly rustdoc -p "$c" $feats \
           -- -Z unstable-options --output-format json >/dev/null 2>&1 ); then
      cp "$AKADRO_DIR/target/doc/${c//-/_}.json" "$API_DIR/json/"
    fi
  done
  echo "  json/ : $(ls "$API_DIR/json"/*.json 2>/dev/null | wc -l) per-crate files"
else
  echo "  json/ : skipped (no nightly; install: rustup toolchain install nightly --profile minimal)"
fi

# 3. Curated PUBLIC-API usage examples + behaviour tests (no internals).
rm -rf "$API_DIR/examples"; mkdir -p "$API_DIR/examples"
copy() { [ -f "$1" ] && command cp "$1" "$2"; }
copy "$AKADRO_DIR/crates/sma-crossover/src/lib.rs" "$API_DIR/examples/sma_crossover.rs"
copy "$AKADRO_DIR/crates/playground/src/main.rs"  "$API_DIR/examples/full_pipeline.rs"
for t in backtest_e2e analytics_pipeline parity_golden_master determinism two_week_15m_backtest; do
  copy "$AKADRO_DIR/crates/akadro/tests/$t.rs" "$API_DIR/examples/test_$t.rs"
done
copy "$AKADRO_DIR/crates/akadro/tests/common/mod.rs" "$API_DIR/examples/test_common.rs"
for p in collect_past_copies scalar_copyout; do
  copy "$AKADRO_DIR/crates/akadro-compile-tests/tests/ui-pass/$p.rs" "$API_DIR/examples/legal_$p.rs"
done
echo "  examples/ : $(ls "$API_DIR/examples"/*.rs 2>/dev/null | wc -l) files"
echo "done."
REFRESH_BODY
} > "$LAB_DIR/scripts/refresh-akadro-api.sh"
chmod +x "$LAB_DIR/scripts/refresh-akadro-api.sh"

# --- scripts/verify-no-unsafe.sh (LAB_PKG baked in) ---------------------------
{
  printf '#!/usr/bin/env bash\n'
  printf '# Trusted gate: prove the lab has no `unsafe`, independent of in-tree config.\n'
  printf '# Run as MAINTAINER / in CI (the agent cannot edit this script).\n'
  printf 'set -uo pipefail\n'
  printf 'LAB_PKG=%q\n' "$LAB_PKG"
  cat <<'VERIFY_BODY'
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CARGO="${CARGO:-cargo}"
fail=0
echo "== verify-no-unsafe: $LAB_DIR =="

grep -q '#!\[forbid(unsafe_code)\]' "$LAB_DIR/src/main.rs" \
  && echo "  ok   : #![forbid(unsafe_code)] present in src/main.rs" \
  || { echo "  FAIL : #![forbid(unsafe_code)] removed from src/main.rs"; fail=1; }

grep -qE 'unsafe_code[[:space:]]*=[[:space:]]*"forbid"' "$LAB_DIR/Cargo.toml" \
  && echo "  ok   : unsafe_code = \"forbid\" present in Cargo.toml" \
  || { echo "  FAIL : unsafe_code = \"forbid\" removed from Cargo.toml"; fail=1; }

echo "  ...  : compiling with forced -F unsafe_code on the crate ..."
if ( cd "$LAB_DIR" && "$CARGO" rustc --quiet --bin "$LAB_PKG" -- -F unsafe_code ) 2>/tmp/lab_forbid.err; then
  echo "  ok   : crate compiles under forced forbid — no unsafe present"
else
  echo "  FAIL : crate does NOT compile under forced forbid — unsafe present:"
  grep -iE 'unsafe|error' /tmp/lab_forbid.err | head -6 | sed 's/^/         /'
  fail=1
fi

if command -v cargo-geiger >/dev/null 2>&1; then
  ( cd "$LAB_DIR" && "$CARGO" geiger --quiet 2>/dev/null | tail -3 ) || true
fi

echo
[ "$fail" -eq 0 ] && echo "RESULT: PASS — lab is unsafe-free." || echo "RESULT: FAIL — see above."
exit "$fail"
VERIFY_BODY
} > "$LAB_DIR/scripts/verify-no-unsafe.sh"
chmod +x "$LAB_DIR/scripts/verify-no-unsafe.sh"

# --- scripts/verify-no-custom-datasource.sh -----------------------------------
# Enforces the hard rule "No custom DataSource": hand-rolling a feed/exchange trait
# is the one remaining way to import your own data, so it is disallowed in the lab.
{
  printf '#!/usr/bin/env bash\n'
  printf '# Trusted gate: the lab forbids hand-rolling a feed/exchange to import data.\n'
  printf '# Data must come only from akadro::data::load_or_cache_feed / connector feeds.\n'
  printf '# Run as MAINTAINER / in CI (the agent cannot edit this script).\n'
  printf 'set -uo pipefail\n'
  cat <<'DS_BODY'
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
echo "== verify-no-custom-datasource: $LAB_DIR/src/strategies =="
# Reject any `impl <...> DataSource|ExecutionClient|InstrumentCatalog for ...` in the
# agent-editable code: those are the feed/exchange seams that would let a strategy
# inject its own data (the D18 leakage point). The blessed path is the data API.
hits="$(grep -rnE 'impl([[:space:]]|<).*\b(DataSource|ExecutionClient|InstrumentCatalog)\b[[:space:]]+for' "$LAB_DIR/src/strategies" 2>/dev/null || true)"
if [ -n "$hits" ]; then
  echo "  FAIL : custom data/venue trait impl found (disallowed — use load_or_cache_feed):"
  echo "$hits" | sed 's/^/         /'
  echo "RESULT: FAIL — remove the custom DataSource; get data via akadro::data::load_or_cache_feed."
  exit 1
fi
echo "  ok   : no custom DataSource / ExecutionClient / InstrumentCatalog impl"
echo "RESULT: PASS — no custom data source."
exit 0
DS_BODY
} > "$LAB_DIR/scripts/verify-no-custom-datasource.sh"
chmod +x "$LAB_DIR/scripts/verify-no-custom-datasource.sh"

# --- build the API bundle -----------------------------------------------------
echo "generating API bundle ..."
"$LAB_DIR/scripts/refresh-akadro-api.sh" | sed 's/^/  /'

echo
echo "lab ready: $LAB_DIR"
echo "  agent reads : GUIDE.md, api/doc/akadro/index.html, api/json/, api/examples/"
echo "  agent writes: src/strategies/"
echo "  build/run   : ( cd '$LAB_DIR' && cargo run )"
echo "  verify safe : '$LAB_DIR/scripts/verify-no-unsafe.sh'   (run after each session)"
echo "  verify data : '$LAB_DIR/scripts/verify-no-custom-datasource.sh'   (no self-imported data)"
