# Contributing

This repository is the deployed lending program. Changes to it change what
every open position is worth, so the bar is different from most open source.

## What is most useful

**A finding.** If you have read the code and think something is wrong, that is
the contribution this repository most wants. If it has security impact, do not
open an issue — see [SECURITY.md](SECURITY.md).

**A failing test.** A reproduction in `programs/aera/tests/` is worth more than
a description of a bug, and far more than a fix without one. The harness runs a
real SBF build in a local validator, so a test there is a statement about the
program rather than about a mock.

**A question about an invariant.** If the reasoning in a comment does not hold,
saying so is a contribution. Several of the comments in this repository exist
because somebody was wrong first.

## What will be declined

- **Economic parameter changes.** LTV, thresholds, the curve, the fees and the
  caps are set in `launch.rs`, deployed, and changed through an operator
  decision — not a pull request.
- **Refactors without a behavioural reason.** This code is read far more often
  than it is written. A change that makes it shorter but harder to audit is a
  net loss.
- **Dependency bumps without cause.** Every dependency in a program that moves
  money is a trust decision. "Newer" is not one.
- **Removing a check to make a test pass.** The scripts in `tools/` refuse to
  run under conditions that have previously produced a false green. Each refusal
  is load-bearing and documented where it lives.

## Before you open a pull request

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
./tools/test.sh
```

`./tools/test.sh`, not `cargo test` — `cargo test` does not rebuild the SBF
artifact and will report a stale program as green. The script refuses to run if
a source file is newer than the binary, if any test is ignored or filtered out,
or if a suite has fewer tests than its recorded floor.

## Style

Match the file you are in. The one rule worth stating: **comments explain why,
not what.** A comment that restates the line under it is noise; a comment
recording the failure a check exists to prevent is the most valuable line in
the file, and this repository has a lot of them for that reason.

## Licence

Contributions are accepted under the [Business Source License 1.1](LICENSE)
this repository is published under, converting to Apache 2.0 on the change
date. By opening a pull request you confirm you have the right to submit the
work under those terms.
