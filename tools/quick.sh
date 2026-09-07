#!/usr/bin/env bash
#
# Rebuild only the v0.2 fixture, then run only the suites you name.
#
# `tools/test.sh` is the gate: it rebuilds both fixtures, runs 300+ tests
# including a six-minute fuzz suite, and checks seven other things. That is the
# right thing to run before a commit and the wrong thing to run in a loop.
#
# The trap this exists to avoid: `cargo test` does NOT rebuild `aera_v0_2.so`,
# and the integration suites load that fixture rather than the freshly compiled
# library. Change the program, run `cargo test`, and the tests execute the *old*
# binary -- which shows up as `InstructionFallbackNotFound` for a new
# instruction, or as an account-order mismatch blamed on the wrong account.
# Both of those cost real time here before the cause was obvious.
#
#   ./tools/quick.sh                          rebuild the fixture only
#   ./tools/quick.sh test_wallet_borrow_cap   rebuild, then run that suite
#   ./tools/quick.sh test_a test_b            several suites
#
set -euo pipefail
cd "$(dirname "$0")/.."

# Solana's platform-tools rustc, found the same way build-artifacts.sh finds it.
# `cargo build-sbf` passes -Zremap-cwd-prefix, which stable rejects, and setting
# RUSTUP_TOOLCHAIN is not enough because build-sbf re-invokes rustc from PATH.
TOOLS="${AERA_PLATFORM_TOOLS:-}"
if [ -z "$TOOLS" ]; then
  for candidate in "$HOME"/.cache/solana/*/platform-tools/rust/bin; do
    [ -d "$candidate" ] && TOOLS="$candidate"
  done
fi
RUSTC_BIN=""
if [ -n "$TOOLS" ]; then
  for name in rustc rustc.exe; do
    [ -x "$TOOLS/$name" ] && RUSTC_BIN="$TOOLS/$name"
  done
fi

echo "building programs/aera -> fixtures/aera_v0_2.so"
if [ -n "$RUSTC_BIN" ]; then
  RUSTC="$RUSTC_BIN" PATH="$TOOLS:$PATH" cargo build-sbf >/dev/null
else
  cargo build-sbf >/dev/null
fi
cp target/deploy/aera.so fixtures/aera_v0_2.so
echo "  $(stat -c%s fixtures/aera_v0_2.so) bytes"

# `aera_v0_1.so` is built from a git worktree and never changes with local edits,
# so it is deliberately not rebuilt here.

if [ $# -eq 0 ]; then
  echo "fixture current. Pass suite names to run them."
  exit 0
fi

# `--features no-entrypoint` for the same reason test.sh passes it: without it
# `libaera` and `spl-token` both define `entrypoint` and the test binary does
# not link on Linux. Kept identical here so the fast loop and the real run build
# the same thing.
for suite in "$@"; do
  echo
  echo "--- $suite ---"
  cargo test -p aera --features no-entrypoint --test "$suite" 2>&1 | grep -E "^test |test result|panicked at|Error Code:" || true
done
