#!/usr/bin/env bash
#
# Build the named SBF artifacts the test suite loads, and record what they are.
#
#   ./tools/build-artifacts.sh          build both, write the manifest
#   ./tools/build-artifacts.sh --check  fail if the manifest is stale
#
# ## Why this exists
#
# `programs/aera/tests/common/mod.rs` used to load the program with
# `include_bytes!("../../../../target/deploy/aera.so")`. `cargo test` rebuilds
# the test crates and the library but NOT that artifact, so editing a handler
# and running the tests exercised the previous binary -- silently, with results
# that looked entirely real. A whole v0.2 test run was interpreted against a
# five-day-old v0.1 binary before anyone noticed.
#
# So: no test loads `target/deploy/aera.so` any more. They load explicitly
# named, explicitly hashed fixtures, and the migration suite loads two of them
# at once because it has to run real v0.1 state through a real v0.2 program.
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURES="$HERE/fixtures"
MANIFEST="$FIXTURES/MANIFEST.txt"

# The last commit before the v0.2 oracle work began. Anything at or before this
# is the guardian protocol.
#
# This was 734a58f, and that commit no longer exists. Rewriting the repository's
# authorship gave every commit a new hash -- same trees, same content, new SHAs --
# and this pin was left naming one from before the rewrite. It kept passing
# locally, because the fixture it guards is gitignored and was already on the
# machine; CI, cloning fresh, had neither the commit nor the fixture and stopped
# with "no v0.1 artifact and no way to build one".
#
# c5bf24c is the same commit: identical tree 2b88e938, and the only one of the
# two reachable from main. A pin into history has to be re-checked whenever
# history is rewritten, which is a good reason not to rewrite it.
V0_1_COMMIT="c5bf24c"

# The hashes v0.1 is known to build to. Every migration result is a statement
# about *this* binary, so an unrecognised hash means the migration suite is
# testing something other than what its tests were written against.
#
# A list rather than a single value because an SBF build is reproducible for a
# given platform-tools release and not across releases: the same commit hashes
# differently under a different rustc. Each line therefore records the
# toolchain that produced it.
#
# A new hash appearing is not automatically a problem -- it is a question with
# exactly two answers, and the fix is to establish which and add a line, never
# to remove the check:
#
#   the toolchain changed   add the hash, with the version that produced it
#   the commit moved        stop, because the pinned commit is supposed to be
#                           immutable
#
V0_1_EXPECTED_HASHES="
  2bee2d5a8cff9375525c34b0c725b70dbbb15223dcc10ebbc32fb3fba46e57d6  # rustc 1.89.0-dev (platform-tools v1.53, windows)
  08f5d22b86ebfe4e9395317f1332ba90d5f8b3ac83d3325cf820c5b7a73fa3ce  # cargo-build-sbf 4.1.0 (ubuntu, GitHub Actions)
"
#
# Outside the repository, because a worktree inside it would be picked up by
# `cargo` and by every `git status`. `/c/Temp` was the default and it is a
# Windows path: on CI it became `/c/Temp/...` at the filesystem root and failed
# with "could not create leading directories ... Permission denied". TMPDIR is
# what the platform says it is.
WORKTREE="${AERA_V0_1_WORKTREE:-${TMPDIR:-/tmp}/aera-v0_1}"

# Solana's platform-tools rustc. `cargo build-sbf` passes -Zremap-cwd-prefix,
# which the default stable toolchain rejects, and setting RUSTUP_TOOLCHAIN is
# not enough because build-sbf re-invokes rustc from PATH.
#
# Found rather than hardcoded: the cached version moves, and the binary is
# `rustc` on Linux and `rustc.exe` under Git Bash on Windows. Where the shim
# already works on its own -- a plain `solana-install` setup, which is what CI
# has -- none of this is needed and the override stays empty.
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

# Half the reason an artifact hash moves, so it goes in the manifest.
toolchain_version() {
  if [ -n "$RUSTC_BIN" ]; then
    "$RUSTC_BIN" --version 2>/dev/null || echo unknown
  else
    cargo build-sbf --version 2>/dev/null | head -1 || echo unknown
  fi
}

sbf_build() {
  local dir="$1"
  if [ -n "$RUSTC_BIN" ]; then
    ( cd "$dir" \
      && RUSTC="$RUSTC_BIN" PATH="$TOOLS:$PATH" cargo build-sbf >/dev/null 2>&1 )
  else
    ( cd "$dir" && cargo build-sbf >/dev/null 2>&1 )
  fi
}

hash_of() { sha256sum "$1" | cut -d' ' -f1; }

mkdir -p "$FIXTURES"

# --- v0.2: the working tree ------------------------------------------------
echo "building v0.2 from the working tree..."
sbf_build "$HERE"
cp "$HERE/target/deploy/aera.so" "$FIXTURES/aera_v0_2.so"

# --- v0.1: a worktree pinned to the pre-oracle commit ----------------------
#
# Built from git rather than kept as a committed binary, so it is reproducible
# from source and cannot drift into being "whatever was lying around".
#
# ## When the commit is not reachable
#
# `aera-v1`, the public repository, contains the program and nothing else — it
# has no history from before v0.2, so `734a58f` is not in it. The guarantee the
# migration suite actually depends on is the HASH: every migration result is a
# statement about a specific v0.1 binary, and rebuilding from a pinned commit is
# one way of obtaining a binary with that hash, not the guarantee itself.
#
# So when the commit is absent, the committed fixture is used and checked
# against the same list below. It is not a weaker check — an unrecognised or
# missing fixture still stops the run. What it is not able to do is prove the
# binary came from that source, which is why the full build is preferred
# wherever the history exists.
if git -C "$HERE/.." cat-file -e "$V0_1_COMMIT^{commit}" 2>/dev/null; then
  if [ ! -d "$WORKTREE/protocol" ]; then
    echo "creating v0.1 worktree at $WORKTREE ($V0_1_COMMIT)..."
    git -C "$HERE/.." worktree add --detach "$WORKTREE" "$V0_1_COMMIT" >/dev/null
  fi

  echo "building v0.1 from $V0_1_COMMIT..."
  sbf_build "$WORKTREE/protocol"
  cp "$WORKTREE/protocol/target/deploy/aera.so" "$FIXTURES/aera_v0_1.so"
elif [ -f "$FIXTURES/aera_v0_1.so" ]; then
  echo "v0.1 commit $V0_1_COMMIT is not in this repository; using the committed fixture."
else
  echo "FAIL: no v0.1 artifact and no way to build one." >&2
  echo >&2
  echo "      Commit $V0_1_COMMIT is not in this repository and" >&2
  echo "      fixtures/aera_v0_1.so is missing, so the migration suite has" >&2
  echo "      nothing to run real v0.1 state through." >&2
  exit 1
fi

# --- prove they are actually different ------------------------------------
V1_HASH="$(hash_of "$FIXTURES/aera_v0_1.so")"
V2_HASH="$(hash_of "$FIXTURES/aera_v0_2.so")"

if ! echo "$V0_1_EXPECTED_HASHES" | grep -q "^[[:space:]]*$V1_HASH"; then
  echo "FAIL: aera_v0_1.so hashed a value this script does not recognise." >&2
  echo >&2
  echo "      got       $V1_HASH" >&2
  echo "      toolchain $(toolchain_version)" >&2
  echo >&2
  echo "      known good:" >&2
  echo "$V0_1_EXPECTED_HASHES" | sed '/^[[:space:]]*$/d;s/^/      /' >&2
  echo >&2
  echo "      v0.1 is built from commit $V0_1_COMMIT, which is supposed to be" >&2
  echo "      immutable, so exactly one of two things happened:" >&2
  echo >&2
  echo "        the toolchain changed   add the hash above to" >&2
  echo "                                V0_1_EXPECTED_HASHES, with this version" >&2
  echo "        the commit moved        stop -- every migration result is now" >&2
  echo "                                about a different program" >&2
  echo >&2
  echo "      Establish which before doing either. Deleting the check is not" >&2
  echo "      one of the options." >&2
  exit 1
fi

if [ "$V1_HASH" = "$V2_HASH" ]; then
  echo "FAIL: the two artifacts are identical. The migration suite would be" >&2
  echo "      migrating v0.2 to itself and proving nothing." >&2
  exit 1
fi

# v0.1 must contain the guardian instruction; v0.2 must not. A cheap, direct
# check that the fixtures are what their names claim.
if ! grep -qa "PublishPrice" "$FIXTURES/aera_v0_1.so"; then
  echo "FAIL: aera_v0_1.so has no PublishPrice symbol -- it is not v0.1." >&2
  exit 1
fi
if grep -qa "PublishPrice" "$FIXTURES/aera_v0_2.so"; then
  echo "FAIL: aera_v0_2.so still carries PublishPrice -- guardian code survived." >&2
  exit 1
fi

cat > "$MANIFEST" <<EOF
# Aera SBF artifacts under test.
#
# Regenerate with tools/build-artifacts.sh. The hashes below are what the
# migration suite actually ran against; if they do not match your fixtures,
# your test results are about a different program than you think.

generated          $(date -u +"%Y-%m-%dT%H:%M:%SZ")

toolchain          $(toolchain_version)

v0.1 commit        $V0_1_COMMIT
v0.1 artifact      fixtures/aera_v0_1.so
v0.1 size          $(stat -c%s "$FIXTURES/aera_v0_1.so") bytes
v0.1 sha256        $V1_HASH

v0.2 artifact      fixtures/aera_v0_2.so
v0.2 size          $(stat -c%s "$FIXTURES/aera_v0_2.so") bytes
v0.2 sha256        $V2_HASH
EOF

echo
cat "$MANIFEST"
echo
echo "verified: v0.1 carries PublishPrice, v0.2 does not, hashes differ."
