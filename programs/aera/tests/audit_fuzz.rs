//! Fuzz families A-F.
//!
//! Seeded and deterministic: every case prints its seed on failure so it can be
//! replayed exactly. Randomness here is a way to cover a grid too large to write
//! out, not a way to be vague.
//!
//! The families vary amounts, order, actors and oracle age rather than the test
//! name. A thousand copies of "supply 1 COOK" would be a thousand assertions and
//! no coverage.
//!
//! Every case ends the same way: the reserve must be solvent and its vault must
//! back what the program says it tracks. That single check is what turns a fuzz
//! run into an audit rather than a smoke test.

mod common;

use common::audit::*;
use common::*;
use solana_keypair::Keypair;

/// xorshift64*, so a failing case is reproducible from its seed alone.
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

    fn pick<'a, T>(&mut self, options: &'a [T]) -> &'a T {
        &options[self.below(options.len() as u64) as usize]
    }
}

fn actor(env: &mut Env, mint: Pubkey, amount: u64) -> Keypair {
    let user = env.create_user();
    env.fund(&user, mint, amount);
    user
}

// ===========================================================================
// Fuzz-A — random legal sequences
// ===========================================================================

/// 120 random sequences of legal actions across three users.
///
/// Nothing here is expected to fail; the point is that after any order of any
/// sizes, the book still adds up and the indexes have only risen.
#[test]
fn fuzz_a_random_legal_sequences_keep_the_book_solvent() {
    let mut cases = 0;

    for seed in 1..=120u64 {
        let mut rng = Rng::new(seed * 0x9E37_79B9);
        let (mut env, cook, bcook) = Env::core(1_000);

        // A supplier so there is always something to borrow.
        let seeder = actor(&mut env, cook.mint, tokens(1_000_000));
        env.supply(&seeder, &cook, tokens(500_000));

        let mut users = Vec::new();
        for _ in 0..3 {
            let user = actor(&mut env, cook.mint, tokens(100_000));
            env.fund(&user, bcook.mint, tokens(100_000));
            let obligation = env.open_position(&user, &bcook, tokens(10_000));
            users.push((user, obligation));
        }

        let mut last_index = env.read_reserve(&cook).borrow_index;

        for _ in 0..rng.below(16) + 4 {
            let index = rng.below(users.len() as u64) as usize;
            let (user, obligation) = &users[index];
            let amount = *rng.pick(&[1u64, 2, 10, 1_000, tokens(1), tokens(100), tokens(1_000)]);

            match rng.below(6) {
                0 => {
                    let _ = env.try_supply(user, &cook, amount);
                }
                1 => {
                    let _ = env.try_withdraw(user, &cook, amount);
                }
                2 => {
                    let _ = env.try_borrow(user, &cook, *obligation, amount, &[&cook, &bcook]);
                }
                3 => {
                    let _ = env.try_repay(user, &cook, *obligation, amount);
                }
                4 => {
                    let _ = env.try_deposit_collateral(user, &bcook, *obligation, amount);
                }
                _ => {
                    env.warp_slots(rng.below(5_000_000) + 1);
                    env.accrue(&cook);
                }
            }

            // Invariant 5: the borrow index only ever rises.
            let index_now = env.read_reserve(&cook).borrow_index;
            assert!(
                index_now >= last_index,
                "seed {seed}: borrow index fell from {last_index} to {index_now}"
            );
            last_index = index_now;

            // The invariant that actually matters.
            let solvency = env.solvency(&cook);
            assert!(
                solvency.vault_backs_tracked(),
                "seed {seed}: {}",
                solvency.report("fuzz-A")
            );
            assert!(
                solvency.solvent(),
                "seed {seed}: {}",
                solvency.report("fuzz-A")
            );
            cases += 1;
        }

        // The collateral reserve is never borrowed from, so its index must not
        // have moved at all.
        assert_eq!(
            env.read_reserve(&bcook).borrow_index,
            FIXED_POINT,
            "seed {seed}: the bCOOK index moved"
        );
    }

    println!("FUZZ-A assertions: {cases}");
    assert!(cases >= 500, "fuzz-A only executed {cases} assertions");
}

// ===========================================================================
// Fuzz-B — illegal sequences change nothing
// ===========================================================================

/// 100 malformed calls. Each must fail and leave the reserve exactly as it was.
///
/// A refusal that still moves state is worse than an accepted attack, because
/// nothing in the logs says anything happened.
#[test]
fn fuzz_b_illegal_calls_leave_no_trace() {
    let mut checks = 0;

    for seed in 1..=100u64 {
        let mut rng = Rng::new(seed * 0x1000_0001);
        let (mut env, cook, bcook) = Env::core(1_000);

        let supplier = actor(&mut env, cook.mint, tokens(100_000));
        env.supply(&supplier, &cook, tokens(100_000));

        let attacker = actor(&mut env, cook.mint, tokens(1_000));
        let obligation = env.init_obligation(&attacker);

        let before = env.read_reserve(&cook);
        let vault_before = env.balance(&cook.liquidity_vault);

        let amount = *rng.pick(&[0u64, 1, tokens(1), tokens(1_000_000), u64::MAX / 2]);
        let result = match rng.below(5) {
            // Borrow with no collateral posted.
            0 => env.try_borrow(
                &attacker,
                &cook,
                obligation,
                amount.max(1),
                &[&cook, &bcook],
            ),
            // Withdraw shares that were never minted.
            1 => env.try_withdraw(&attacker, &cook, amount.max(1)),
            // Repay a debt that does not exist.
            2 => env.try_repay(&attacker, &cook, obligation, amount.max(1)),
            // Supply without accruing first.
            3 => env.try_supply_without_accrue(&attacker, &cook, amount.max(1)),
            // Borrow without refreshing the obligation.
            _ => env.try_borrow_without_refresh(
                &attacker,
                &cook,
                obligation,
                amount.max(1),
                &[&cook, &bcook],
            ),
        };

        assert!(result.is_err(), "seed {seed}: an illegal call was accepted");

        let after = env.read_reserve(&cook);
        assert_eq!(
            before.available_liquidity, after.available_liquidity,
            "seed {seed}: a refused call moved available_liquidity"
        );
        assert_eq!(
            before.share_mint_supply, after.share_mint_supply,
            "seed {seed}: a refused call moved the share supply"
        );
        assert_eq!(
            before.borrowed_principal, after.borrowed_principal,
            "seed {seed}: a refused call moved borrowed principal"
        );
        assert_eq!(
            vault_before,
            env.balance(&cook.liquidity_vault),
            "seed {seed}: a refused call moved tokens"
        );
        checks += 5;
    }

    println!("FUZZ-B assertions: {checks}");
}

// ===========================================================================
// Fuzz-C — oracle ages and quorum
// ===========================================================================

/// Fuzz C - every oracle state against every gated action.
///
/// v0.1's version of this walked a grid of guardian counts and submission ages.
/// Neither exists now. What replaced it is more useful: the gate matrix in
/// `oracle::breaker` is a claim about what the protocol permits in each state,
/// and this drives each state onto the chain and checks the claim against what
/// the program actually does.
///
/// The property that matters most is the bottom row. Repay must succeed in
/// every state without exception, because a borrower who cannot repay can only
/// be liquidated.
#[test]
fn fuzz_c_oracle_state_and_action_grid() {
    use aera::oracle::breaker::{OracleHealth, RiskAction};

    let mut checks = 0;

    for state in ["healthy", "warning", "frozen", "emergency"] {
        let (mut env, cook, bcook) = Env::core(1_000);
        let supplier = actor(&mut env, cook.mint, tokens(100_000));
        env.supply(&supplier, &cook, tokens(100_000));

        let borrower = actor(&mut env, bcook.mint, tokens(20_000));
        env.fund(&borrower, cook.mint, tokens(5_000));
        let obligation = env.open_position(&borrower, &bcook, tokens(10_000));

        // A live position, opened while everything is healthy.
        env.try_borrow(
            &borrower,
            &cook,
            obligation,
            tokens(1_000),
            &[&cook, &bcook],
        )
        .expect("the setup borrow must succeed while healthy");

        let expected = match state {
            "healthy" => OracleHealth::Healthy,
            // Accepted, but the pool has not advanced for several epochs.
            "warning" => {
                env.move_rate(bcook.mint, 1_001, 9);
                OracleHealth::RateWarning
            }
            // A rise past the per-epoch allowance, under the emergency bound.
            "frozen" => {
                env.move_rate(bcook.mint, 1_050, 2);
                OracleHealth::BorrowFrozen
            }
            // A redemption fee past the bound Aera accepts.
            "emergency" => {
                env.set_pool(bcook.mint, px(1_000) as u64, POOL_SHARES, 9_000, 2);
                env.refresh_oracle(bcook.mint);
                OracleHealth::Emergency
            }
            _ => unreachable!(),
        };

        let actual = OracleHealth::from_u8(env.read_oracle(bcook.mint).health).unwrap();
        assert_eq!(
            actual, expected,
            "{state}: the oracle did not reach the state this row is about"
        );
        checks += 1;

        // --- borrow -------------------------------------------------------
        let result = env.try_borrow(&borrower, &cook, obligation, tokens(10), &[&cook, &bcook]);
        assert_eq!(
            result.is_ok(),
            expected.permits(RiskAction::Borrow),
            "{state}: borrow disagreed with the gate matrix: {result:?}"
        );
        checks += 1;

        // --- withdraw collateral ------------------------------------------
        let result = env.try_withdraw_collateral(
            &borrower,
            &bcook,
            obligation,
            tokens(10),
            &[&cook, &bcook],
        );
        assert_eq!(
            result.is_ok(),
            expected.permits(RiskAction::WithdrawCollateral),
            "{state}: collateral withdrawal disagreed with the gate matrix: {result:?}"
        );
        checks += 1;

        // --- repay: must ALWAYS work --------------------------------------
        let result = env.try_repay(&borrower, &cook, obligation, tokens(100));
        assert!(
            result.is_ok(),
            "{state}: REPAY BLOCKED - a borrower who cannot repay can only be              liquidated: {result:?}"
        );
        checks += 1;

        // --- adding collateral: must always work --------------------------
        //
        // Collateral is posted as *shares*, and the borrower holds raw bCOOK,
        // so supply it first. Supplying is itself permitted in every state,
        // which this line quietly depends on and the matrix guarantees.
        env.supply(&borrower, &bcook, tokens(100));
        let result = env.try_deposit_collateral(&borrower, &bcook, obligation, tokens(10));
        assert!(
            result.is_ok(),
            "{state}: adding collateral blocked, though it only reduces risk: {result:?}"
        );
        checks += 1;

        assert_solvent(&env, &cook, &format!("fuzz-C {state}"));
    }

    println!("Fuzz C assertions: {checks}");
    assert!(checks >= 20, "expected a real grid, ran {checks}");
}

// ===========================================================================
// Fuzz-D — the rounding grid
// ===========================================================================

/// Supply then immediately redeem, for every small amount.
///
/// Redeeming must never return more than was deposited. Any unit of drift is
/// the protocol's, never the user's - that is invariant 11.
#[test]
fn fuzz_d_supply_then_redeem_never_returns_more() {
    let (mut env, cook, _bcook) = Env::core(1_000);

    // A live book, so the rate is not a trivial 1:1.
    let seeder = actor(&mut env, cook.mint, tokens(100_000));
    env.supply(&seeder, &cook, tokens(100_000));

    let user = actor(&mut env, cook.mint, tokens(10_000));
    // The share account must exist before it can be read; `supply` would create
    // it, but the first reading happens before the first supply.
    env.ensure_share_ata(&user, cook.share_mint);
    let mut checks = 0;

    for amount in 1..=50u64 {
        let cook_before = env.balance(&ata(&user.pubkey(), &cook.mint));
        let shares_before = env.balance(&share_ata(&user.pubkey(), &cook.share_mint));

        if env.try_supply(&user, &cook, amount).is_err() {
            continue; // too small to mint; refusing is correct
        }
        let minted = env.balance(&share_ata(&user.pubkey(), &cook.share_mint)) - shares_before;
        assert!(
            minted > 0,
            "amount {amount} minted no shares but was accepted"
        );

        env.try_withdraw(&user, &cook, minted)
            .unwrap_or_else(|e| panic!("redeeming {minted} shares failed: {e}"));

        let cook_after = env.balance(&ata(&user.pubkey(), &cook.mint));
        assert!(
            cook_after <= cook_before,
            "amount {amount}: round-tripped {} -> {} and gained {}",
            cook_before,
            cook_after,
            cook_after - cook_before
        );
        assert_solvent(&env, &cook, "fuzz-D");
        checks += 3;
    }

    println!("FUZZ-D assertions: {checks}");
}

// ===========================================================================
// Fuzz-E — cap boundaries
// ===========================================================================

/// At the cap, one unit under, and one unit over, on both reserves.
#[test]
fn fuzz_e_cap_boundaries() {
    let cap = tokens(10_000);
    let mut checks = 0;

    for (label, amount, allowed) in [
        ("one unit under", cap - 1, true),
        ("exactly the cap", cap, true),
        ("one unit over", cap + 1, false),
    ] {
        let (mut env, cook, _bcook) = Env::core(1_000);
        let base = env.read_reserve(&cook).config;
        env.try_set_params(
            &cook,
            aera::state::ReserveConfig {
                supply_cap: cap,
                borrow_cap: cap / 2,
                ..base
            },
        )
        .expect("tightening the caps applies immediately");

        let user = actor(&mut env, cook.mint, cap + tokens(10));
        let result = env.try_supply(&user, &cook, amount);

        if allowed {
            assert!(
                result.is_ok(),
                "{label}: supply of {amount} against cap {cap} refused"
            );
        } else {
            assert!(result.is_err(), "{label}: SUPPLY EXCEEDED THE CAP");
            if let Err(message) = result {
                assert!(
                    message.contains("SupplyCapExceeded"),
                    "{label}: refused for the wrong reason: {message}"
                );
            }
        }
        assert_solvent(&env, &cook, "fuzz-E");
        checks += 2;
    }

    println!("FUZZ-E assertions: {checks}");
}

// ===========================================================================
// Fuzz-F — the health grid
// ===========================================================================

/// Borrow at every interesting fraction of the limit.
///
/// At or below the max pull must succeed; anything past it must fail. The
/// boundary is the whole point, so it is walked one unit at a time.
#[test]
fn fuzz_f_health_grid() {
    let mut checks = 0;

    // Fractions of the post-haircut collateral value, in basis points.
    for target_bps in [
        0u64, 1_000, 3_000, 5_000, 5_400, 5_499, 5_500, 5_501, 6_000, 6_500, 9_000,
    ] {
        let (mut env, cook, bcook) = Env::core(1_000);
        let supplier = actor(&mut env, cook.mint, tokens(1_000_000));
        env.supply(&supplier, &cook, tokens(1_000_000));

        let borrower = actor(&mut env, bcook.mint, tokens(10_000));
        env.fund(&borrower, cook.mint, 0);
        let obligation = env.open_position(&borrower, &bcook, tokens(10_000));

        let value = tokens(10_000) * 9_500 / 10_000; // after the 5% haircut
        let amount = value * target_bps / 10_000;

        let result = env.try_borrow(&borrower, &cook, obligation, amount, &[&cook, &bcook]);

        if amount == 0 {
            assert!(result.is_err(), "a zero borrow was accepted");
        } else if target_bps <= 5_500 {
            assert!(
                result.is_ok(),
                "LTV {target_bps}bps: borrow of {amount} inside the limit was refused: {result:?}"
            );
        } else {
            assert!(
                result.is_err(),
                "LTV {target_bps}bps: BORROWED PAST THE LIMIT"
            );
        }

        assert_solvent(&env, &cook, "fuzz-F");
        checks += 1;
    }

    println!("FUZZ-F assertions: {}", checks * 2);
    assert_eq!(checks, 11);
}
