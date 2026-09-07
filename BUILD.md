# Building Aera

## The trap

`programs/aera/tests/common/mod.rs` loads the program with:

```rust
include_bytes!("../../../../target/deploy/aera.so")
```

**The integration tests run whatever SBF binary is on disk, not the source you
just edited.** `cargo test` rebuilds the *test* crates and the library, but it
does not rebuild `aera.so`. So editing an instruction handler and running
`cargo test` tests the previous binary, silently, and the results look real.

This cost real time during the v0.2 work: a whole test run was interpreted
against a binary five days stale, and the failures it produced were failures of
the *old* program.

**Rebuild the SBF binary before trusting any integration-test result.**

```
cargo build-sbf && cargo test
```

## Why `cargo build-sbf` needs help on this machine

Out of the box it fails:

```
error: the option `Z` is only accepted on the nightly compiler
```

`cargo-build-sbf` passes `-Zremap-cwd-prefix`, which needs Solana's
platform-tools rustc rather than the default stable toolchain. On this machine
that toolchain is installed but not on `PATH`, so:

```bash
RUSTC=~/.cache/solana/v1.53/platform-tools/rust/bin/rustc.exe \
PATH="$HOME/.cache/solana/v1.53/platform-tools/rust/bin:$PATH" \
cargo build-sbf
```

Setting `RUSTUP_TOOLCHAIN` alone does **not** work — the version selected is
`1.89.0-sbpf-solana-v1.53`, but `cargo build-sbf` re-invokes `rustc` from
`PATH`, so the directory has to be ahead of the default toolchain's.

Adjust `v1.53` if the installed platform-tools version differs; check with
`ls ~/.cache/solana/`.

## The full sequence

```bash
cargo fmt --all
cargo clippy --all-targets
cargo build-sbf          # with the environment above
cargo test --no-fail-fast
```

`--no-fail-fast` matters: without it, `cargo test` stops at the first failing
test *binary*, so one broken suite hides the state of the other fourteen.
