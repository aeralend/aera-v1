# Aera Protocol v1

An overcollateralized lending market for COOK on [Cookie Chain](https://cookiescan.io).
Lend COOK and earn the interest borrowers pay, or lock bCOOK as collateral and
borrow COOK against it.

[![License: BUSL-1.1](https://img.shields.io/badge/License-BUSL--1.1-blue.svg)](./LICENSE)
[![CI](https://github.com/aeralend/aera-v1/actions/workflows/ci.yml/badge.svg)](https://github.com/aeralend/aera-v1/actions/workflows/ci.yml)

This repository is the on-chain program and nothing else. The interface, the
SDK, the keepers and the operational tooling are not open source.

---

## What it is

One market, two assets, and no more surface than that.

| | |
| --- | --- |
| **COOK** | Lent and borrowed. Cookie Chain's native token. |
| **bCOOK** | Collateral only, never lent. Staked COOK from [BakeYourStake](https://stake.cookiescan.io). |

Locking bCOOK does not unstake it. Aera holds the token and never redeems it,
so a borrower keeps their staking position and has COOK to use on top of it.
That is the only structural reason to prefer this to selling, and it is the
reason the protocol exists in this shape.

There are no vaults, no strategies, no leverage loops and no governance token.

## Parameters, as deployed

Every value below is generated from [`programs/aera/src/launch.rs`](programs/aera/src/launch.rs)
and asserted against the program by `test_launch_config`. It is not a
description of the code — it is read out of it.

| | |
| --- | --- |
| Max loan-to-value | 55% |
| Liquidation threshold | 65% |
| Collateral haircut | 5% |
| Liquidation bonus | 8% |
| Close factor | 50%, and 100% below a health factor of 0.95 |
| Borrow curve | 2% → 10% at the 60% kink → 90% |
| Reserve factor | 15% of interest |
| Origination fee | 0.15%, once, taken from the payout |
| Caps | 1,000,000 supply · 600,000 borrow · 250,000 per wallet |

| | |
| --- | --- |
| Program | `AerafpFHsY4N16i4KufPJQyURryxKtYgCr6oZMwbp76q` |
| COOK | `So11111111111111111111111111111111111111112` |
| bCOOK | `EkPafx58mgwkEnGwo62jXhXDAdJ37Z8G8MFBRPsr9uhz` |
| Chain genesis | `9wDaBRDgArEUpvhHxGguNkwozsZh4UpGZB9o2EoEcBB2` |

## The oracle is not a price feed

bCOOK is staked COOK, so its value is what the stake pool will redeem it for —
a number the pool itself publishes and that trading cannot move. Aera reads the
pool directly. There is no price feed, no oracle committee and no aggregator.

A circuit breaker bounds how far that rate may move in one epoch (200 bps up,
100 bps down, 1000 bps to declare an emergency). When it is exceeded the
protocol holds the last rate it accepted and pauses the actions that add risk,
rather than repricing every position on a number it does not trust.

**Repaying, lending and locking collateral stay open in every restricted
state.** All three reduce the protocol's exposure, and a protocol that stops
you reducing your own risk during an incident is a trap.

## Layout

```
programs/aera/src/
  instructions/     the eighteen entry points, one per file
    admin/          init, params, pause, risk config, fee collection
  oracle/           the stake-pool reader, the breaker, deployment pinning
  state/            account layouts
  launch.rs         every deployed parameter, in one place
  math.rs           rates, indexes, health, the liquidation split
  risk.rs           gate evaluation

programs/aera/tests/
  audit_*.rs        adversarial suites: accounts, raw instructions, rounding,
                    first-depositor, fuzz, gates, liquidation, oracle economics
  test_*.rs         behaviour, migration, invariants, launch scenario
  common/           the LiteSVM harness and the invariant assertions

fixtures/           aera_v0_1.so, the previously deployed program, kept so the
                    migration suite has real v0.1 state to replay; aera_v0_2.so
                    is a build output, not a release
tools/              build, test and artifact scripts
```

## Building

Requires Rust, the Solana platform tools, and Anchor 1.1.2.

```sh
sh -c "$(curl -sSfL https://release.anza.xyz/stable/install)"
cargo build-sbf
```

See [BUILD.md](BUILD.md) for the toolchain versions this is pinned to and why.

## Testing

```sh
./tools/test.sh
```

**Not `cargo test`.** `cargo test` does not rebuild the SBF artifact, so it
will happily test a stale program and report results that look entirely real —
which is how a full v0.2 run was once read against a five-day-old v0.1 binary.
`tools/test.sh` builds the artifacts, hashes them, and refuses to run if a
source file is newer than the binary, if any test is ignored or filtered out,
or if any suite has fewer tests than its recorded floor.

`./tools/quick.sh` is the fast loop while iterating. It skips the artifact
rebuild, so it is for feedback and never for a result you intend to rely on.

The migration suite runs real v0.1 account state through the current program.
It needs a v0.1 binary, and this repository ships one as a hashed fixture —
`tools/build-artifacts.sh` verifies its hash against a known-good list before
any test loads it.

`fixtures/aera_v0_2.so` sitting beside it is **not** a release. It is the
current program, rebuilt from source before every run, committed only because
the test harness pulls it in with `include_bytes!` and the crate would not
compile on a fresh clone without it. Whatever is committed there was built on
somebody's machine and is overwritten before the first test executes. For the
hash of the program a run actually tested, run it — step 7 prints it.

## Security

Report vulnerabilities privately through a
[GitHub security advisory](https://github.com/aeralend/aera-v1/security/advisories/new),
never a public issue. Scope, safe harbour and what to expect are in
[SECURITY.md](SECURITY.md).

The security posture of the deployed protocol — what has been reviewed, what
has not, and what limits the damage either way — is documented under
[Security](https://docs.aeralend.app) in the protocol documentation.

## License

[Business Source License 1.1](./LICENSE), converting to Apache 2.0 on
**2029-09-06**.

Reading, auditing, forking, testing, publishing findings, deploying to a
testnet or a private network, and building interfaces, indexers, keepers and
liquidation bots against an Aera deployment are all expressly permitted. What
is reserved until the change date is offering the Licensed Work to third
parties as a competing hosted product. The exact terms are in the licence; this
paragraph is a summary and the licence is what governs.

`Aera` and the Aera mark are not covered by the licence — see
[TRADEMARKS.md](TRADEMARKS.md).
