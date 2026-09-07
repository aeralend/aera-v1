#!/usr/bin/env bash
#
# Build the artifacts, prove which ones are under test, then test.
#
#   ./tools/test.sh                 everything, with every integrity gate
#   ./tools/test.sh test_oracle     one suite (gates 4-6 are skipped, and say so)
#   ./tools/test.sh --update-floors rewrite tools/suite-floors.txt from this run
#
# `cargo test` alone does not rebuild the SBF artifact, so it will happily test
# a stale program and report results that look entirely real -- which is how a
# full v0.2 test run was once read against a five-day-old v0.1 binary. Every
# gate below exists because of a way a green run has been, or could be, a lie.
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$HERE"

FLOORS="tools/suite-floors.txt"
UPDATE_FLOORS=0
if [ "${1:-}" = "--update-floors" ]; then
  UPDATE_FLOORS=1
  shift
fi
FULL_RUN=0
[ $# -eq 0 ] && FULL_RUN=1

fail() {
  echo >&2
  echo "FAIL: $*" >&2
  exit 1
}

# ---------------------------------------------------------------------------
echo "=== 1. building artifacts ==="
./tools/build-artifacts.sh >/dev/null
echo "    done"

# ---------------------------------------------------------------------------
echo
echo "=== 2. the fixtures are current ==="
#
# A fixture older than any source file under programs/ means the build above did
# not pick up an edit, which is the failure this script exists to make
# impossible.
STALE="$(find programs -name '*.rs' -newer fixtures/aera_v0_2.so -print -quit 2>/dev/null || true)"
[ -n "$STALE" ] && fail "$STALE is newer than fixtures/aera_v0_2.so.
      The artifact did not rebuild. Do not trust any test result."
echo "    aera_v0_2.so is newer than every source file"

# ---------------------------------------------------------------------------
echo
echo "=== 3. the test environment is anchored to the chain ==="
#
# Nothing else in the suite is. Every other oracle test drives a synthetic pool
# the harness wrote, so the whole suite could agree with itself and disagree
# with Cookie Chain.
for f in fixtures/bcook_stake_pool.bin fixtures/bcook_stake_pool.json; do
  [ -f "$f" ] || fail "$f is missing. The oracle suite would have nothing real
      to check its layout against."
done
POOL_LEN="$(stat -c%s fixtures/bcook_stake_pool.bin)"
[ "$POOL_LEN" -eq 611 ] || fail "the captured stake-pool account is $POOL_LEN bytes, not 611."
POOL_HASH="$(sha256sum fixtures/bcook_stake_pool.bin | cut -d' ' -f1)"
POOL_SLOT="$(grep -o '"slot":[0-9]*' fixtures/bcook_stake_pool.json | cut -d: -f2)"
echo "    captured bCOOK stake pool   611 bytes, slot $POOL_SLOT"
echo "    sha256                      $POOL_HASH"
echo "    parsed by                   test_real_pool.rs, against measured figures"

# ---------------------------------------------------------------------------
echo
echo "=== 4. running tests ==="
LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT
set +e
if [ $# -gt 0 ]; then
  cargo test --no-fail-fast "$@" 2>&1 | tee "$LOG"
else
  cargo test --no-fail-fast 2>&1 | tee "$LOG"
fi
TEST_STATUS="${PIPESTATUS[0]}"
set -e

# ---------------------------------------------------------------------------
echo
echo "=== 5. nothing was skipped ==="
#
# A green run with tests quietly disabled is worse than a red one, because it
# is believed. `#[ignore]` and a `--` filter both produce a pass.
IGNORED="$(awk '/^test result/ { for (i = 1; i <= NF; i++) if ($i == "ignored;") s += $(i-1) } END { print s + 0 }' "$LOG")"
[ "$IGNORED" -eq 0 ] || fail "$IGNORED test(s) are #[ignore]d. A disabled test is not a passing test."
echo "    0 ignored"

if [ "$FULL_RUN" -eq 1 ]; then
  FILTERED="$(awk '/^test result/ { for (i = 1; i <= NF; i++) if ($i == "filtered") s += $(i-1) } END { print s + 0 }' "$LOG")"
  [ "$FILTERED" -eq 0 ] || fail "$FILTERED test(s) were filtered out of a full run."
  echo "    0 filtered out"
else
  echo "    (filter check skipped: this was not a full run)"
fi

# ---------------------------------------------------------------------------
echo
echo "=== 6. no suite shrank ==="
#
# The brief this was written under says it plainly: no deleting difficult tests
# to make CI pass. Deleting a test produces a green run, so the count is the
# only thing that notices.
#
# Suite name and count are on different lines of cargo's output -- "Running
# tests/x.rs" then, later, "test result: ..." -- so they are paired by order.
COUNTS="$(awk '
  # "Running unittests src/lib.rs (...)" or "Running tests/x.rs (...)".
  # Match the source path specifically -- the executable path follows on the
  # same line, and stripping to the last "/" picks that up instead.
  match($0, /(tests|src)[\/\\][A-Za-z0-9_]+\.rs/) {
    path = substr($0, RSTART, RLENGTH)
    sub(/.*[\/\\]/, "", path)
    sub(/\.rs$/, "", path)
    suite = path
    next
  }
  /^test result/ && suite != "" {
    for (i = 1; i <= NF; i++) if ($i == "passed;") print suite, $(i-1)
    suite = ""
  }
' "$LOG" | sort)"

if [ "$UPDATE_FLOORS" -eq 1 ]; then
  {
    sed -n '1,/^# suite/p' "$FLOORS"
    echo "$COUNTS" | awk '{ printf "%-24s %6d\n", $1, $2 }'
  } > "$FLOORS.new"
  mv "$FLOORS.new" "$FLOORS"
  echo "    floors rewritten from this run"
elif [ "$FULL_RUN" -eq 1 ]; then
  SHRANK=0
  while read -r suite floor; do
    case "$suite" in ''|'#'*) continue ;; esac
    actual="$(echo "$COUNTS" | awk -v s="$suite" '$1 == s { print $2 }')"
    if [ -z "$actual" ]; then
      echo "    MISSING  $suite (floor $floor) did not run at all" >&2
      SHRANK=1
    elif [ "$actual" -lt "$floor" ]; then
      echo "    SHRANK   $suite: $actual tests, floor $floor" >&2
      SHRANK=1
    fi
  done < <(grep -v '^#' "$FLOORS" | grep -v '^[[:space:]]*$')
  [ "$SHRANK" -eq 0 ] || fail "a suite lost tests. Raise the floor deliberately, or
      restore what was deleted. See $FLOORS."
  TOTAL="$(echo "$COUNTS" | awk '{ s += $2 } END { print s }')"
  echo "    every suite at or above its floor -- $TOTAL tests across $(echo "$COUNTS" | wc -l) suites"
else
  echo "    (floor check skipped: this was not a full run)"
fi

# ---------------------------------------------------------------------------
echo
echo "=== 7. what these results are about ==="
awk '/^v0\.[12] (commit|artifact|size|sha256)/ { printf "    %s\n", $0 }' fixtures/MANIFEST.txt
echo
echo "    Integration suites load these fixtures by name. Nothing loads"
echo "    target/deploy/aera.so, which is whatever was built last."
echo
echo "    Randomised suites, and the seeds they ran:"
grep -hoE '0x[0-9a-f_]{8,}' programs/aera/tests/test_invariants.rs programs/aera/tests/audit_fuzz.rs 2>/dev/null \
  | sort -u | sed 's/^/      /'
echo
echo "    Every seed is fixed in source, so a failure is replayable and a"
echo "    passing run means the same thing on every machine."

exit "$TEST_STATUS"
