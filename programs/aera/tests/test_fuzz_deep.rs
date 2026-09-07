//! Fifty thousand state transitions, checked against every invariant.
//!
//! `test_invariants.rs` states each of the nineteen and runs a few hundred
//! steps. This runs fifty seeds of a thousand operations each and evaluates all
//! seventeen state invariants after every one, which is 850,000 individual
//! assertions over 50,000 reachable states.
//!
//! ## What the extra scale buys
//!
//! Not more of the same. Long sequences reach states short ones cannot:
//!
//! ```text
//!   a borrow index far from 1.0     after enough accrual, the fixed-point
//!                                   arithmetic is operating on numbers no
//!                                   hand-written test would pick
//!   deeply layered positions        collateral added and removed dozens of
//!                                   times, each leaving its own rounding
//!   liquidation mid-sequence        a seizure landing on a position that was
//!                                   built by fifty prior operations
//!   config changes under load       caps cut while the book is near them
//!   pause during an incident        the gate matrix exercised in combination
//!                                   rather than one state at a time
//! ```
//!
//! ## Determinism
//!
//! Every sequence comes from a fixed seed through xorshift64*. A failure prints
//! the seed, the step, the action and the invariant, and re-running that seed
//! reproduces it exactly. Nothing here reads the clock, the environment or an
//! unseeded RNG.
//!
//! Seeds are listed rather than generated. A suite that drew fresh seeds each
//! run would fail on somebody else's machine and pass on yours, and the failure
//! it found would be gone before anyone read the report.
//!
//! ## Runtime
//!
//! Roughly seven minutes at ~125 operations per second. That is the cost of the
//! coverage; `test_invariants.rs` remains the fast suite for ordinary work.

mod common;

use aera::constants::DEFAULT_MAX_WITHDRAWAL_FEE_BPS;
use aera::oracle::breaker::OracleHealth;
use common::invariants::*;
use common::*;
use solana_keypair::Keypair;

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

/// One step. Named rather than numbered so a failure says what happened.
#[derive(Debug)]
#[allow(dead_code)] // read through Debug, which is the point of the fields
enum Action {
    Supply { who: usize, amount: u64 },
    Withdraw { who: usize, shares: u64 },
    DepositCollateral { who: usize, shares: u64 },
    WithdrawCollateral { who: usize, shares: u64 },
    Borrow { who: usize, amount: u64 },
    Repay { who: usize, amount: u64 },
    Liquidate { target: usize, amount: u64 },
    Accrue,
    Wait { slots: u64 },
    MoveRate { thousandths: u64, epoch: u64 },
    MoveFee { bps: u16, epoch: u64 },
    RefreshOracle,
    PauseBorrow,
    PauseAll,
    Resume,
    TightenCaps { supply: u64, borrow: u64 },
    TightenLtv { bps: u16 },
    CollectFees,
}

type Tally = std::collections::BTreeMap<&'static str, usize>;

/// Run one sequence, checking every state invariant after every step.
fn run(seed: u64, steps: usize) -> Tally {
    let (mut env, cook, bcook) = Env::core(1_300);
    let handles = [("COOK", cook), ("bCOOK", bcook)];

    // A supplier outside the actor set, so there is always something to borrow.
    let whale = env.create_user();
    env.fund(&whale, cook.mint, tokens(500_000));
    env.supply(&whale, &cook, tokens(400_000));

    // Actors start with real positions. An empty book makes the first hundred
    // steps a search for a legal opening move rather than an exploration.
    let mut actors: Vec<(Keypair, Pubkey)> = Vec::new();
    for _ in 0..4 {
        let user = env.create_user();
        env.fund(&user, cook.mint, tokens(30_000));
        env.fund(&user, bcook.mint, tokens(30_000));
        let obligation = env.open_position(&user, &bcook, tokens(8_000));
        env.supply(&user, &bcook, tokens(2_000));
        /*
         * Opened close to the line, not comfortably above it.
         *
         * 8,000 bCOOK at 1.30, less the 2% redemption fee and the 5% haircut,
         * is 9,682 COOK of collateral value; at 55% LTV that permits about
         * 5,325. Borrowing 4,800 leaves a health factor near 1.3, so an
         * ordinary run of accepted declines can push it under and the grid
         * actually exercises liquidation.
         *
         * The first version borrowed 1,000 against the same collateral -- a
         * health factor of 4.8, which no decline the breaker accepts can ever
         * reach. Fifty thousand operations produced not one liquidation, and
         * the coverage assertion caught it.
         */
        env.try_borrow(&user, &cook, obligation, tokens(4_800), &[&cook, &bcook])
            .expect("opening borrow");
        actors.push((user, obligation));
    }
    let obligations: Vec<Pubkey> = actors.iter().map(|(_, o)| *o).collect();

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(200_000));

    /*
     * The fee destination needs a token account to be paid into.
     *
     * `collect_fees` transfers to `Global::fee_destination`'s associated token
     * account, and `Env::new` creates the wallet but not the ATA. Without this
     * every collect_fees in the grid fails on a missing account rather than on
     * anything to do with fees, which is what fifty thousand operations
     * reported before the coverage check named it.
     */
    let fee_wallet = env.fee_wallet.insecure_clone();
    env.fund(&fee_wallet, cook.mint, 1);

    let mut rng = Rng::new(seed);
    // Half the seeds walk the collateral rate down, half up. Fixed per seed, so
    // a failure is still reproducible from the seed alone.
    let drift_down = seed.is_multiple_of(2);
    /*
     * One seed in four runs a collapse profile.
     *
     * The mixed profile cannot reach a liquidation, and three attempts at
     * tuning its probabilities did not change that. The reason is structural:
     * repayment is permitted in every oracle state and every pause state, while
     * borrowing needs a healthy oracle and an unpaused protocol, so repayment
     * succeeds roughly twenty times as often. Debt drains, and a position with
     * no debt cannot be liquidated however far the collateral falls.
     *
     * That asymmetry is correct protocol behaviour -- exits stay open, entries
     * do not -- so the fix is not to distort it. A collapse profile instead
     * models the case liquidation exists for: collateral falling at the fastest
     * pace the breaker accepts, with borrowers who are not repaying. Under it
     * positions do go underwater, and the grid exercises seizure against
     * positions built by hundreds of prior operations rather than by a fixture.
     */
    let collapse = seed.is_multiple_of(4);
    let mut epoch = 1u64;
    let mut rate = 1_300u64;
    let mut fee_bps = TEST_WITHDRAWAL_FEE_BPS;
    let mut succeeded = Tally::new();
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

        let action = match rng.below(18) {
            0 => Action::Supply {
                who,
                amount: tokens(1 + rng.below(400)),
            },
            1 => Action::Withdraw {
                who,
                shares: tokens(1 + rng.below(200)),
            },
            2 => Action::DepositCollateral {
                who,
                shares: tokens(1 + rng.below(300)),
            },
            3 => Action::WithdrawCollateral {
                who,
                shares: tokens(1 + rng.below(100)),
            },
            4 => Action::Borrow {
                who,
                amount: tokens(1 + rng.below(500)),
            },
            // Under a collapse, nobody is repaying. That is the scenario.
            5 if !collapse => Action::Repay {
                who,
                amount: tokens(1 + rng.below(300)),
            },
            5 => Action::Liquidate {
                target: who,
                amount: tokens(1 + rng.below(3_000)),
            },
            6 => Action::Liquidate {
                target: who,
                amount: tokens(1 + rng.below(2_000)),
            },
            7 | 8 => Action::Accrue,
            9 => Action::Wait {
                slots: 1 + rng.below(600_000),
            },
            10 | 11 => {
                epoch += 1;
                /*
                 * Movement inside the breaker's allowance most of the time, and
                 * occasionally past it.
                 *
                 * A grid that never trips the breaker would never exercise the
                 * frozen states; one that always trips it would never exercise
                 * the accepted path. Roughly one step in eight is a shock.
                 */
                rate = match if collapse { 7 } else { rng.below(8) } {
                    // A jump past the absolute emergency bound.
                    0 => 1_000 + rng.below(2_500),
                    /*
                     * The middle band: past the per-epoch allowance, inside the
                     * emergency bound. This is what produces BORROW_FROZEN, and
                     * the first version of this generator had no such case --
                     * every move was either inside the allowance or far past
                     * the emergency bound, so the grid reached Healthy,
                     * RateWarning and Emergency and never the state between
                     * them. `the_grid_reaches_every_oracle_state` caught it.
                     */
                    1 | 2 => {
                        let magnitude = 300 + rng.below(600); // 3% to 9%
                        if rng.below(2) == 0 {
                            (rate * (10_000 + magnitude) / 10_000).max(1)
                        } else {
                            (rate * (10_000 - magnitude) / 10_000).max(1)
                        }
                    }
                    /*
                     * Ordinary movement, inside the allowance, with a drift.
                     *
                     * The drift is per-seed and persistent. Without it the walk
                     * is a martingale: it oscillates around 1.30 and positions
                     * opened near the liquidation line never actually cross it,
                     * so fifty thousand operations produced not one successful
                     * liquidation. A trend is also what a real staking receipt
                     * does -- it accrues steadily, and it collapses steadily
                     * when something is wrong.
                     *
                     * Half the seeds trend down and half trend up, so the grid
                     * explores both a market falling into liquidations and one
                     * where collateral is appreciating.
                     */
                    _ => {
                        if collapse {
                            // The steepest fall the breaker accepts, every
                            // epoch. Ceiling, so a rounded step never exceeds
                            // the 1% allowance and trips into a frozen
                            // reference that stops tracking reality.
                            (rate * 99).div_ceil(100).max(1)
                        } else if drift_down {
                            (rate * (10_000 - 20 - rng.below(70)) / 10_000).max(1)
                        } else {
                            (rate * (10_000 + 10 + rng.below(170)) / 10_000).max(1)
                        }
                    }
                };
                Action::MoveRate {
                    thousandths: rate,
                    epoch,
                }
            }
            12 => {
                epoch += 1;
                // Inside the bound most of the time; past it sometimes, which
                // must freeze rather than reprice.
                fee_bps = if rng.below(6) == 0 {
                    (DEFAULT_MAX_WITHDRAWAL_FEE_BPS + 1 + rng.below(2_000) as u16).min(9_000)
                } else {
                    rng.below(DEFAULT_MAX_WITHDRAWAL_FEE_BPS as u64) as u16
                };
                Action::MoveFee {
                    bps: fee_bps,
                    epoch,
                }
            }
            13 => Action::RefreshOracle,
            /*
             * Weighted toward resuming.
             *
             * An even split leaves the protocol paused about two thirds of the
             * time, which suppresses every other action and makes the grid
             * mostly an exploration of the paused state. Pausing is worth
             * exercising; living there is not.
             */
            14 if !collapse => match rng.below(4) {
                0 => Action::PauseBorrow,
                1 => Action::PauseAll,
                _ => Action::Resume,
            },
            14 => Action::Accrue,
            /*
             * Config changes are rare, and bounded away from the floor.
             *
             * They only ever tighten -- a loosening queues behind the timelock
             * and would leave the sequence asserting against a config that has
             * not applied -- so they ratchet. An earlier version made them two
             * actions in eighteen with no floor, and after a few hundred steps
             * the LTV had been walked down to 10% and the caps to their minimum:
             * borrowing became impossible, positions were repaid away, and
             * fifty thousand operations produced 33 successful borrows and no
             * liquidations at all. Rare and floored keeps the market alive.
             */
            15 => match rng.below(6) {
                0 => Action::TightenCaps {
                    supply: tokens(300_000 + rng.below(700_000)),
                    borrow: tokens(200_000 + rng.below(400_000)),
                },
                1 => Action::TightenLtv {
                    bps: 4_000 + rng.below(1_500) as u16,
                },
                _ => Action::Accrue,
            },
            16 => Action::CollectFees,
            _ => Action::Liquidate {
                target: rng.below(4) as usize,
                amount: tokens(1 + rng.below(3_000)),
            },
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
            Action::Liquidate { target, amount } => {
                let victim = actors[*target].1;
                let keeper = liquidator.insecure_clone();
                record(
                    "liquidate",
                    env.try_liquidate(&keeper, &cook, &bcook, victim, *amount)
                        .is_ok(),
                );
            }
            Action::Accrue => {
                env.accrue(&cook);
                env.accrue(&bcook);
                record("accrue", true);
            }
            Action::Wait { slots } => env.warp_slots(*slots),
            Action::MoveRate { thousandths, epoch } => {
                env.set_pool(
                    bcook.mint,
                    px(*thousandths) as u64,
                    POOL_SHARES,
                    fee_bps,
                    *epoch,
                );
                record("move_rate", true);
            }
            Action::MoveFee { bps, epoch } => {
                env.set_pool(bcook.mint, px(rate) as u64, POOL_SHARES, *bps, *epoch);
                record("move_fee", true);
            }
            Action::RefreshOracle => {
                record("refresh_oracle", env.try_refresh_oracle(bcook.mint).is_ok());
            }
            Action::PauseBorrow => record("pause_borrow", env.try_pause_borrow().is_ok()),
            Action::PauseAll => record("pause_all", env.try_pause_all().is_ok()),
            Action::Resume => record("resume", env.try_unpause().is_ok()),
            Action::TightenCaps { supply, borrow } => {
                let mut config = env.read_reserve(&cook).config;
                // Only ever downward: raising a cap queues behind the timelock
                // and would leave the sequence asserting against a config that
                // has not applied.
                config.supply_cap = config.supply_cap.min(*supply);
                config.borrow_cap = config.borrow_cap.min(*borrow).min(config.supply_cap);
                record("tighten_caps", env.try_set_params(&cook, config).is_ok());
            }
            Action::TightenLtv { bps } => {
                let mut config = env.read_reserve(&bcook).config;
                config.loan_to_value_bps = config.loan_to_value_bps.min(*bps);
                record("tighten_ltv", env.try_set_params(&bcook, config).is_ok());
            }
            Action::CollectFees => {
                record("collect_fees", env.try_collect_fees(&cook).is_ok());
            }
        }

        let prices_moved = matches!(
            action,
            Action::MoveRate { .. } | Action::MoveFee { .. } | Action::RefreshOracle
        );

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

/// Fifty seeds, fixed in source.
///
/// Drawn once from a shuffled range and then written down. New seeds are added
/// deliberately; a seed that ever fails stays here forever as a regression.
const SEEDS: [u64; 50] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_04d2,
    0x0000_0000_dead_beef,
    0x0123_4567_89ab_cdef,
    0x1111_1111_1111_1111,
    0x2222_2222_2222_2222,
    0x3141_5926_5358_9793,
    0x4444_4444_4444_4444,
    0x5555_aaaa_5555_aaaa,
    0x6666_6666_6666_6666,
    0x7777_7777_7777_7777,
    0x8888_8888_8888_8888,
    0x9999_9999_9999_9999,
    0x9e37_79b9_7f4a_7c15,
    0xaaaa_5555_aaaa_5555,
    0xbbbb_bbbb_bbbb_bbbb,
    0xcafe_babe_cafe_babe,
    0xdead_beef_dead_beef,
    0xeeee_eeee_eeee_eeee,
    0xffff_ffff_ffff_ffff,
    0x00c0_ffee_00c0_ffee,
    0x0f0f_0f0f_0f0f_0f0f,
    0x1234_1234_1234_1234,
    0x1b0b_1b0b_1b0b_1b0b,
    0x2718_2818_2845_9045,
    0x3b9a_ca00_3b9a_ca00,
    0x4d2d_4d2d_4d2d_4d2d,
    0x5eed_5eed_5eed_5eed,
    0x6c62_6c62_6c62_6c62,
    0x7fff_ffff_ffff_ffff,
    0x8000_0000_0000_0000,
    0x90a1_b2c3_d4e5_f607,
    0xa5a5_a5a5_a5a5_a5a5,
    0xb00b_b00b_b00b_b00b,
    0xc001_c001_c001_c001,
    0xd00d_d00d_d00d_d00d,
    0xe1e2_e3e4_e5e6_e7e8,
    0xf00d_f00d_f00d_f00d,
    0x0102_0304_0506_0708,
    0x1a2b_3c4d_5e6f_7081,
    0x2b7e_1516_28ae_d2a6,
    0x3c6e_f372_fe94_f82b,
    0x4a64_998a_2c1b_7c9d,
    0x5be0_cd19_137e_2179,
    0x6a09_e667_f3bc_c908,
    0x7b21_1a1c_0d0e_0f10,
    0x8fe2_c4d2_00d5_1a3f,
    0x9b05_688c_2b3e_6c1f,
    0xab1f_ea23_9d18_c4b7,
    0xbb67_ae85_84ca_a73b,
];

/// The whole grid: fifty seeds, a thousand operations each.
///
/// Every state invariant is evaluated after every step, so this is 50,000
/// transitions and 850,000 assertions. Roughly seven minutes.
#[test]
fn fifty_thousand_transitions_preserve_every_invariant() {
    let mut total = Tally::new();
    for seed in SEEDS {
        for (action, count) in run(seed, 1_000) {
            *total.entry(action).or_default() += count;
        }
    }

    /*
     * Every operation had to actually succeed somewhere.
     *
     * A run that refused everything satisfies all nineteen invariants and
     * proves nothing whatever, and it looks exactly like a run that exercised
     * everything. An earlier version of the shorter suite did precisely that --
     * sixty steps with no successful borrow -- until this check was added.
     */
    for required in [
        "supply",
        "withdraw",
        "deposit_collateral",
        "withdraw_collateral",
        "borrow",
        "repay",
        "liquidate",
        "refresh_oracle",
        "pause_borrow",
        "resume",
        "tighten_caps",
        "tighten_ltv",
        "collect_fees",
    ] {
        assert!(
            total.get(required).copied().unwrap_or(0) > 0,
            "no {required} ever succeeded across 50,000 operations -- the grid \
             proved nothing about it. successes: {total:?}"
        );
    }

    let operations: usize = total.values().sum();
    println!(
        "50 seeds x 1,000 operations = 50,000 transitions; {operations} succeeded\n  {total:?}"
    );
}

/// The oracle reaches every state during the grid.
///
/// A fuzz run that never froze the breaker would say nothing about the frozen
/// paths, and a shock every eighth rate move is a guess about how often that
/// happens rather than a guarantee. This checks it.
#[test]
fn the_grid_reaches_every_oracle_state() {
    let mut seen = std::collections::BTreeSet::new();
    let (mut env, _cook, bcook) = Env::core(1_300);

    let mut rng = Rng::new(0xfeed_face_feed_face);
    let mut epoch = 1u64;
    let mut rate = 1_300u64;

    for _ in 0..400 {
        epoch += 1;
        rate = match rng.below(8) {
            0 => 1_000 + rng.below(2_500),
            1 | 2 => {
                let magnitude = 300 + rng.below(600);
                if rng.below(2) == 0 {
                    (rate * (10_000 + magnitude) / 10_000).max(1)
                } else {
                    (rate * (10_000 - magnitude) / 10_000).max(1)
                }
            }
            _ => (rate * (10_000 + rng.below(180) - 90) / 10_000).max(1),
        };
        env.set_rate(bcook.mint, rate, epoch);
        let _ = env.try_refresh_oracle(bcook.mint);
        seen.insert(OracleHealth::from_u8(env.read_oracle(bcook.mint).health).unwrap() as u8);
    }

    for state in [
        OracleHealth::Healthy,
        OracleHealth::BorrowFrozen,
        OracleHealth::Emergency,
    ] {
        assert!(
            seen.contains(&(state as u8)),
            "the grid never reached {state:?}; states seen: {seen:?}"
        );
    }
}
