//! The properties that must hold in **every** reachable state.
//!
//! The rest of the suite tests transitions: do this, expect that. This file
//! tests states. Nineteen named invariants are evaluated against the whole
//! protocol -- reserves, obligations, mints, vaults, oracles -- and then
//! evaluated again after every single step of randomised action sequences, so
//! they are checked in states nobody wrote down.
//!
//! That is the point of the randomisation. A hand-written test reaches the
//! states its author thought of; the interesting failures live in the ones they
//! did not. What is random here is the *order and size* of ordinary operations,
//! never the assertions -- every sequence is checked against the same nineteen
//! properties, so a run either strengthens all of them or finds a counterexample
//! to one.
//!
//! ## Reproducibility
//!
//! Every sequence is generated from an explicit `u64` seed by xorshift64*, and
//! every failure prints the seed, the step index and the action that broke the
//! invariant. A failing run is replayable exactly:
//!
//! ```text
//!   INVARIANT VIOLATED  INV-04 borrow index never decreases
//!   seed 0x00000000000004d2  step 37  action Accrue { reserve: COOK }
//! ```
//!
//! Nothing here depends on wall-clock time, thread scheduling or an unseeded
//! RNG. A seed that fails today fails identically tomorrow.
//!
//! ## What they are
//!
//! | # | Invariant |
//! |---|---|
//! | INV-01 | `total_liquidity == available + borrowed − fees` |
//! | INV-02 | the vault holds at least what the program says it holds |
//! | INV-03 | claims never exceed assets by more than integer rounding |
//! | INV-04 | `borrow_index` never decreases |
//! | INV-05 | the share exchange rate never decreases |
//! | INV-06 | `share_mint_supply` mirrors the real mint supply |
//! | INV-07 | accrue is a no-op within one slot |
//! | INV-08 | debt is zero, or the index is at least one |
//! | INV-09 | supplied liquidity never exceeds the supply cap |
//! | INV-10 | borrowed liquidity never exceeds the borrow cap |
//! | INV-11 | borrowed never exceeds what has been supplied |
//! | INV-12 | no obligation ever holds debt with no collateral behind it |
//! | INV-13 | an obligation's recorded collateral matches its share vault |
//! | INV-14 | the oracle rate is exactly what the pool's bytes say it is |
//! | INV-15 | a health of `Healthy` implies a reference has been set |
//! | INV-16 | the reference changes only when an observation is accepted |
//! | INV-17 | no user action leaves a position past its liquidation line |
//! | INV-18 | losing collateral never raises what is owed |
//! | INV-19 | protocol fees never exceed the interest that produced them |
//!
//! Seventeen of the nineteen are properties of a *state*, and those are the ones
//! re-checked after every step of every sequence. INV-07 and the liquidation
//! bound are properties of a *transition* -- they compare two states across a
//! specific operation -- so they have their own named tests below rather than a
//! branch in `check_all`, which is stated here so the "checked after every
//! step" claim is not read as covering more than it does.
//!
//! INV-01, INV-04, INV-07, the liquidation bound and the cap invariant are the
//! five that the invariant register listed as `test_invariants::*` and that were
//! never written. They are written here.

mod common;

use aera::constants::FIXED_POINT_SCALE;
use aera::state::Obligation;
use anchor_lang::AccountDeserialize;
use common::audit::*;
use common::invariants::*;
use common::*;
use solana_keypair::Keypair;

// ===========================================================================
// The invariant set
// ===========================================================================

// ===========================================================================
// The five invariants INVARIANTS.md named and nobody wrote
// ===========================================================================

/// A1 / INV-01, stated directly rather than as part of a sequence.
#[test]
fn a1_accounting_identity_holds() {
    let (mut env, cook, bcook) = Env::core(1_300);
    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(50_000));
    env.supply(&supplier, &cook, tokens(50_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(10_000));
    env.fund(&borrower, cook.mint, tokens(1));
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(2_000),
        &[&cook, &bcook],
    )
    .expect("borrow");

    // A year of interest, so fees are non-zero and the identity has to hold
    // with every term populated.
    env.warp_slots(aera::constants::DEFAULT_SLOTS_PER_YEAR);
    env.accrue(&cook);

    let now = snapshot(&env, &[("COOK", cook), ("bCOOK", bcook)], &[obligation]);
    assert_invariants(&now, None, false, "after a year of accrued interest");

    let r = &now.reserves[0];
    assert!(r.accrued_fees > 0, "the test did not actually accrue fees");
    assert_eq!(
        r.total_liquidity + r.accrued_fees,
        r.available + r.solvency.outstanding_debt,
        "the identity must hold exactly, not approximately"
    );
}

/// A5 / INV-04.
#[test]
fn a5_borrow_index_never_decreases() {
    let (mut env, cook, bcook) = Env::core(1_300);
    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(50_000));
    env.supply(&supplier, &cook, tokens(50_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(10_000));
    env.fund(&borrower, cook.mint, tokens(5_000));
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(2_000),
        &[&cook, &bcook],
    )
    .expect("borrow");

    let mut last = env.read_reserve(&cook).borrow_index;
    for step in 0..40 {
        // Repaying, withdrawing and idling all leave the index alone or raise
        // it. None of them may lower it -- an index that fell would forgive
        // debt somebody already owes.
        env.warp_slots(1_000 + step * 137);
        env.accrue(&cook);
        if step % 7 == 3 {
            env.try_repay(&borrower, &cook, obligation, tokens(10))
                .expect("repay");
        }
        let index = env.read_reserve(&cook).borrow_index;
        assert!(
            index >= last,
            "borrow index fell at step {step}: {last} -> {index}"
        );
        last = index;
    }
    assert!(last > FIXED_POINT_SCALE, "no interest accrued at all");
}

/// A6 / INV-07.
#[test]
fn a6_accrue_twice_in_a_slot_changes_nothing() {
    let (mut env, cook, bcook) = Env::core(1_300);
    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(50_000));
    env.supply(&supplier, &cook, tokens(50_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(10_000));
    env.fund(&borrower, cook.mint, tokens(1));
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(2_000),
        &[&cook, &bcook],
    )
    .expect("borrow");

    env.warp_slots(100_000);
    env.accrue(&cook);
    let after_first = env.read_reserve(&cook);

    /*
     * Same slot, again.
     *
     * The blockhash has to move or the identical transaction is deduplicated
     * rather than executed -- which would make this test pass without ever
     * running a second accrue. Expiring the blockhash does not advance the
     * slot, so the state under test is unchanged.
     */
    env.svm.expire_blockhash();
    env.accrue(&cook);
    let after_second = env.read_reserve(&cook);

    assert_eq!(
        after_first.borrow_index, after_second.borrow_index,
        "a second accrue in the same slot moved the index"
    );
    assert_eq!(
        after_first.accrued_fees, after_second.accrued_fees,
        "a second accrue in the same slot charged fees again"
    );
    assert_eq!(
        after_first.available_liquidity, after_second.available_liquidity,
        "a second accrue in the same slot moved liquidity"
    );
}

/// D5 / INV-13, from the liquidation side.
#[test]
fn d5_liquidation_never_seizes_more_than_deposited() {
    let (mut env, cook, bcook) = Env::core(1_300);
    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(100_000));
    env.supply(&supplier, &cook, tokens(100_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(10_000));
    env.fund(&borrower, cook.mint, tokens(1));
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(6_000),
        &[&cook, &bcook],
    )
    .expect("borrow");

    let deposited = {
        let account = env.svm.get_account(&obligation).unwrap();
        let o = Obligation::try_deserialize(&mut &account.data[..]).unwrap();
        o.deposits
            .iter()
            .find(|d| d.reserve == bcook.reserve)
            .map(|d| d.deposited_shares)
            .unwrap_or(0)
    };
    assert!(deposited > 0, "the borrower posted no collateral");

    // Collapse the collateral so the position is deeply underwater and the
    // close factor opens to 100%. The seize must still stop at what is there.
    env.set_price(bcook.mint, px(400));

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(50_000));
    let _ = env.try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(50_000));

    let account = env.svm.get_account(&obligation).unwrap();
    let after = Obligation::try_deserialize(&mut &account.data[..]).unwrap();
    let remaining = after
        .deposits
        .iter()
        .find(|d| d.reserve == bcook.reserve)
        .map(|d| d.deposited_shares)
        .unwrap_or(0);

    assert!(
        remaining <= deposited,
        "the liquidation seized more than was deposited: {deposited} -> {remaining}"
    );
    let vault = env.balance_or_zero(&obligation_share_vault_for(bcook.reserve, obligation));
    assert_eq!(
        remaining, vault,
        "the obligation's books and its share vault disagree after liquidation"
    );
    assert_solvent(&env, &cook, "after a full-close liquidation");
}

/// F9 / a configuration invariant, checked at the boundary.
#[test]
fn f9_borrow_cap_may_not_exceed_supply_cap() {
    let (env, cook, _bcook) = Env::core(1_300);
    let reserve = env.read_reserve(&cook);

    assert!(
        reserve.config.borrow_cap <= reserve.config.supply_cap || reserve.config.supply_cap == 0,
        "the launch configuration already violates F9: borrow {} supply {}",
        reserve.config.borrow_cap,
        reserve.config.supply_cap
    );

    // Both reserves, not just the one, and bCOOK is the one that matters:
    // its borrow cap is zero because bCOOK is collateral-only.
    for handle in [cook, _bcook] {
        let config = env.read_reserve(&handle).config;
        assert!(
            config.supply_cap == 0 || config.borrow_cap <= config.supply_cap,
            "borrow cap {} exceeds supply cap {}",
            config.borrow_cap,
            config.supply_cap
        );
    }
}

// ===========================================================================
// Randomised sequences
// ===========================================================================

/// xorshift64*, so a failing sequence is reproducible from its seed alone.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next() % bound
        }
    }
}

/// One step of a random sequence.
///
/// Named rather than numbered so a failure report says what happened, not which
/// branch of a match it took.
#[derive(Debug)]
#[allow(dead_code)] // `who` is read only through Debug, which is the point:
                    // a failure report that omits which actor acted is much
                    // harder to replay than one that names them.
enum Action {
    Supply { who: usize, amount: u64 },
    Withdraw { who: usize, shares: u64 },
    DepositCollateral { who: usize, shares: u64 },
    Borrow { who: usize, amount: u64 },
    Repay { who: usize, amount: u64 },
    WithdrawCollateral { who: usize, shares: u64 },
    Wait { slots: u64 },
    Accrue,
    MoveRate { thousandths: u64 },
    RefreshOracle,
}

/// Run one randomised sequence, checking all sixteen after every step.
///
/// Actions are *attempted*, not forced: a refused action is a legitimate
/// outcome -- caps, health checks and the oracle exist to refuse things -- and
/// the invariants must hold either way. What would be a bug is an action that
/// succeeds and leaves the protocol inconsistent.
type Tally = std::collections::BTreeMap<&'static str, usize>;

fn run_sequence(seed: u64, steps: usize) -> Tally {
    let (mut env, cook, bcook) = Env::core(1_300);
    let handles = [("COOK", cook), ("bCOOK", bcook)];

    // A supplier who is not one of the actors, so there is always liquidity to
    // borrow and the sequence is not dominated by empty-reserve refusals. Set
    // up first, because the actors below open positions against it.
    let whale = env.create_user();
    env.fund(&whale, cook.mint, tokens(200_000));
    env.supply(&whale, &cook, tokens(200_000));

    // Three actors, so positions interact rather than each having the reserve
    // to itself.
    let mut actors: Vec<(Keypair, Pubkey)> = Vec::new();
    for _ in 0..3 {
        let user = env.create_user();
        env.fund(&user, cook.mint, tokens(20_000));
        env.fund(&user, bcook.mint, tokens(20_000));

        /*
         * Each actor starts with a real position rather than an empty one.
         *
         * Without this the sequence has to stumble on supply -> deposit ->
         * borrow in that order before anything interesting can happen, and a
         * coverage check showed it almost never did: sixty random steps
         * produced one successful deposit and no borrows at all. Starting from
         * a funded, collateralised position makes the randomness about what
         * happens *to* positions, which is the part worth exploring, and it is
         * also the state a live protocol is actually in.
         */
        let obligation = env.open_position(&user, &bcook, tokens(8_000));
        env.try_borrow(&user, &cook, obligation, tokens(1_000), &[&cook, &bcook])
            .expect("opening position");
        actors.push((user, obligation));
    }
    let obligations: Vec<Pubkey> = actors.iter().map(|(_, o)| *o).collect();

    let mut rng = Rng::new(seed);
    let mut epoch = 1u64;
    // What actually happened, so a sequence in which every action was refused
    // cannot pass for coverage. A fuzz run that exercises nothing asserts
    // nothing, and it looks exactly like a fuzz run that exercises everything.
    let mut succeeded: Tally = Tally::new();
    let mut record = |name: &'static str, ok: bool| {
        if ok {
            *succeeded.entry(name).or_default() += 1;
        }
    };
    let mut before = snapshot(&env, &handles, &obligations);
    assert_invariants(
        &before,
        None,
        false,
        &format!("seed {seed:#018x}  step -1  (initial state)"),
    );

    for step in 0..steps {
        let who = rng.below(actors.len() as u64) as usize;
        let action = match rng.below(10) {
            0 => Action::Supply {
                who,
                amount: tokens(1 + rng.below(500)),
            },
            1 => Action::Withdraw {
                who,
                shares: tokens(1 + rng.below(200)),
            },
            2 => Action::DepositCollateral {
                who,
                shares: tokens(1 + rng.below(300)),
            },
            3 => Action::Borrow {
                who,
                amount: tokens(1 + rng.below(400)),
            },
            4 => Action::Repay {
                who,
                amount: tokens(1 + rng.below(200)),
            },
            5 => Action::WithdrawCollateral {
                who,
                shares: tokens(1 + rng.below(100)),
            },
            6 => Action::Wait {
                slots: 1 + rng.below(400_000),
            },
            7 => Action::Accrue,
            8 => Action::MoveRate {
                // Inside the breaker's per-epoch allowance, so the sequence
                // exercises ordinary movement rather than only emergencies.
                thousandths: 1_250 + rng.below(120),
            },
            _ => Action::RefreshOracle,
        };

        let (user, obligation) = {
            let (u, o) = &actors[who];
            (u.insecure_clone(), *o)
        };

        match &action {
            Action::Supply { amount, .. } => {
                record("supply", env.try_supply(&user, &bcook, *amount).is_ok());
            }
            Action::Withdraw { shares, .. } => {
                record("withdraw", env.try_withdraw(&user, &bcook, *shares).is_ok());
            }
            Action::DepositCollateral { shares, .. } => {
                record(
                    "deposit_collateral",
                    env.try_deposit_collateral(&user, &bcook, obligation, *shares)
                        .is_ok(),
                );
            }
            Action::Borrow { amount, .. } => {
                record(
                    "borrow",
                    env.try_borrow(&user, &cook, obligation, *amount, &[&cook, &bcook])
                        .is_ok(),
                );
            }
            Action::Repay { amount, .. } => {
                record(
                    "repay",
                    env.try_repay(&user, &cook, obligation, *amount).is_ok(),
                );
            }
            Action::WithdrawCollateral { shares, .. } => {
                record(
                    "withdraw_collateral",
                    env.try_withdraw_collateral(
                        &user,
                        &bcook,
                        obligation,
                        *shares,
                        &[&cook, &bcook],
                    )
                    .is_ok(),
                );
            }
            Action::Wait { slots } => env.warp_slots(*slots),
            Action::Accrue => {
                env.accrue(&cook);
                env.accrue(&bcook);
                record("accrue", true);
            }
            Action::MoveRate { thousandths } => {
                epoch += 1;
                env.set_rate(bcook.mint, *thousandths, epoch);
                record("move_rate", true);
            }
            Action::RefreshOracle => {
                record("refresh_oracle", env.try_refresh_oracle(bcook.mint).is_ok());
            }
        }

        // A rate move is not a user action, and INV-17 must not be read as a
        // claim that prices cannot fall.
        let prices_moved = matches!(action, Action::MoveRate { .. } | Action::RefreshOracle);

        let now = snapshot(&env, &handles, &obligations);
        assert_invariants(
            &now,
            Some(&before),
            prices_moved,
            &format!("seed {seed:#018x}  step {step}  action {action:?}"),
        );
        before = now;
    }

    succeeded
}

/// Every money-moving operation must succeed somewhere in the suite.
///
/// A run that refused everything would satisfy all nineteen invariants and prove
/// nothing whatever, and it looks exactly like a run that exercised everything.
/// This is what separates the two.
///
/// Checked across the whole seed set rather than per sequence, because that is
/// the honest granularity: sixty random steps against a borrowed-against
/// position will not always find a moment when withdrawing collateral is
/// permitted, and demanding that of each seed would make the assertion about
/// luck rather than about coverage.
fn assert_exercised(total: &Tally, what: &str) {
    for required in [
        "supply",
        "withdraw",
        "deposit_collateral",
        "withdraw_collateral",
        "borrow",
        "repay",
        "refresh_oracle",
    ] {
        assert!(
            total.get(required).copied().unwrap_or(0) > 0,
            "{what}: no {required} ever succeeded -- these sequences proved \
             nothing about it. successes: {total:?}"
        );
    }
}

/// A fixed set of seeds, so the suite is deterministic run to run.
///
/// Fixed rather than drawn from the clock on purpose. A suite that generates
/// fresh seeds every run is a suite that fails on somebody else's machine and
/// passes on yours, and the failure it found is gone before anyone reads it.
/// New seeds are added deliberately, and a seed that ever fails stays in this
/// list forever as a regression.
const SEEDS: [u64; 8] = [
    0x0000_0000_0000_04d2,
    0x1111_1111_1111_1111,
    0xdead_beef_dead_beef,
    0x0123_4567_89ab_cdef,
    0xffff_ffff_ffff_ffff,
    0x5555_aaaa_5555_aaaa,
    0x0000_0000_0000_0001,
    0x9e37_79b9_7f4a_7c15,
];

#[test]
fn random_sequences_preserve_every_invariant() {
    let mut total = Tally::new();
    for seed in SEEDS {
        for (action, count) in run_sequence(seed, 60) {
            *total.entry(action).or_default() += count;
        }
    }
    assert_exercised(&total, "the seed set");
}

/// One long sequence, because some states are only reachable after a lot of
/// history: a large index, many partial repayments, collateral added and
/// removed repeatedly.
#[test]
fn one_long_sequence_preserves_every_invariant() {
    let total = run_sequence(0x00c0_ffee_00c0_ffee, 300);
    assert_exercised(&total, "the long sequence");
}
