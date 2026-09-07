//! The nineteen invariants, and the machinery to evaluate them.
//!
//! Lives in `common` rather than in one test file because three suites need it:
//! `test_invariants` states each invariant on its own, `test_fuzz_deep` checks
//! all of them after every step of fifty thousand random operations, and
//! `test_launch_scenario` checks them after every stage of a full launch.
//!
//! Seventeen are properties of a *state* and are what `check_all` evaluates.
//! INV-07 (accrue is a no-op within a slot) and the liquidation bound are
//! properties of a *transition* and have their own named tests, which is stated
//! here so "checked after every step" is not read as covering more than it does.
//!
//! | # | Invariant |
//! |---|---|
//! | INV-01 | `total_liquidity == available + borrowed − fees` |
//! | INV-02 | the vault holds at least what the program says it holds |
//! | INV-03 | claims never exceed assets by more than integer rounding |
//! | INV-04 | `borrow_index` never decreases |
//! | INV-05 | the share exchange rate never decreases |
//! | INV-06 | `share_mint_supply` mirrors the real mint supply |
//! | INV-07 | accrue is a no-op within one slot *(transition)* |
//! | INV-08 | debt is zero, or the index is at least one |
//! | INV-09 | supplied liquidity never *grows* past the supply cap |
//! | INV-10 | borrowed principal never *grows* past the borrow cap |
//! | INV-11 | borrowed never exceeds what has been supplied |
//! | INV-12 | no obligation ever holds debt with no collateral behind it |
//! | INV-13 | an obligation's recorded collateral matches its share vault |
//! | INV-14 | the oracle rate is exactly what the pool's bytes say it is |
//! | INV-15 | a health of `Healthy` implies a reference has been set |
//! | INV-16 | the reference changes only when an observation is accepted |
//! | INV-17 | no user action leaves a position past its liquidation line |
//! | INV-18 | losing collateral never raises what is owed |
//! | INV-19 | protocol fees never exceed the interest **and origination** that produced them |

#![allow(dead_code)]

use aera::constants::FIXED_POINT_SCALE;
use aera::oracle::breaker::OracleHealth;
use aera::state::{Obligation, OracleState};
use anchor_lang::AccountDeserialize;

use super::audit::*;
use super::*;

/// One invariant's verdict, carrying enough to reproduce the failure.
pub struct Violation {
    pub id: &'static str,
    pub detail: String,
}

/// Everything needed to evaluate them at one instant.
///
/// Read once and passed around, so a check cannot see a different state from
/// the one its neighbour saw.
pub struct Snapshot {
    pub reserves: Vec<ReserveSnapshot>,
    pub obligations: Vec<ObligationSnapshot>,
}

pub struct ReserveSnapshot {
    pub label: &'static str,
    pub solvency: Solvency,
    pub available: u128,
    pub borrowed_principal: u128,
    pub borrow_index: u128,
    pub accrued_fees: u128,
    pub share_supply: u128,
    pub real_share_supply: u128,
    pub supply_cap: u64,
    pub borrow_cap: u64,
    pub total_liquidity: u128,
    pub oracle: OracleState,
    pub pool_gross_rate: Option<u128>,
    pub pool_fee_bps: Option<u16>,
    /// The reserve's own origination fee, for INV-19's allowance.
    pub origination_fee_bps: u16,
}

pub struct ObligationSnapshot {
    pub key: Pubkey,
    pub deposits: Vec<(Pubkey, u64)>,
    pub vault_balances: Vec<(Pubkey, u64)>,
    /// Total debt principal across every borrow entry.
    pub debt: u128,
    /// Total collateral shares across every deposit entry.
    pub collateral_shares: u128,
    /// The obligation's own record of what it owes and what backs it, after
    /// the last refresh.
    pub borrowed_value: u128,
    pub unhealthy_borrow_value: u128,
    pub stale: bool,
}

/// Everything the invariants are evaluated against, read in one pass.
pub fn snapshot(
    env: &Env,
    reserves: &[(&'static str, ReserveHandle)],
    obligations: &[Pubkey],
) -> Snapshot {
    let reserves = reserves
        .iter()
        .map(|(label, handle)| {
            let reserve = env.read_reserve(handle);
            let oracle = env.read_oracle(handle.mint);

            // The pool's own bytes, so INV-14 compares the oracle against the
            // source rather than against itself.
            let (pool_gross_rate, pool_fee_bps) = match oracle.source_kind {
                1 => {
                    let (lamports, shares, epoch) = env.read_pool(handle.mint);
                    /*
                     * Only meaningful when the oracle has actually seen this
                     * epoch of the pool.
                     *
                     * The reference is a record of the last *accepted*
                     * observation, so comparing it against bytes written since
                     * then would only measure how long ago somebody cranked --
                     * which is the breaker's business, not this invariant's.
                     * The guard is the *reference's* own epoch, not the
                     * oracle's last-seen epoch. A refused observation still
                     * records that it was seen, so last_source_epoch advances
                     * on readings the breaker threw away -- and comparing
                     * against those would fail the invariant for doing exactly
                     * what it is supposed to do. The reference's epoch is the
                     * one it claims to have come from, so that is the epoch
                     * whose bytes have to agree with it.
                     */
                    if shares == 0 || oracle.reference.source_epoch != epoch {
                        (None, None)
                    } else {
                        let gross = (lamports as u128) * FIXED_POINT_SCALE / (shares as u128);
                        (Some(gross), Some(oracle.reference.withdrawal_fee_bps))
                    }
                }
                _ => (None, None),
            };

            ReserveSnapshot {
                label,
                solvency: env.solvency(handle),
                available: reserve.available_liquidity as u128,
                borrowed_principal: reserve.borrowed_principal,
                borrow_index: reserve.borrow_index,
                accrued_fees: reserve.accrued_fees as u128,
                origination_fee_bps: reserve.config.origination_fee_bps,
                share_supply: reserve.share_mint_supply as u128,
                real_share_supply: real_mint_supply(env, &handle.share_mint) as u128,
                supply_cap: reserve.config.supply_cap,
                borrow_cap: reserve.config.borrow_cap,
                total_liquidity: reserve
                    .total_liquidity()
                    .expect("total_liquidity overflowed"),
                oracle,
                pool_gross_rate,
                pool_fee_bps,
            }
        })
        .collect();

    let obligations = obligations
        .iter()
        .filter_map(|key| {
            let account = env.svm.get_account(key)?;
            let obligation = Obligation::try_deserialize(&mut &account.data[..]).ok()?;
            let deposits: Vec<(Pubkey, u64)> = obligation
                .deposits
                .iter()
                .filter(|d| d.deposited_shares > 0)
                .map(|d| (d.reserve, d.deposited_shares))
                .collect();
            let vault_balances = deposits
                .iter()
                .map(|(reserve, _)| {
                    (
                        *reserve,
                        env.balance_or_zero(&obligation_share_vault_for(*reserve, *key)),
                    )
                })
                .collect();
            let debt: u128 = obligation
                .borrows
                .iter()
                .map(|b| b.borrowed_principal)
                .sum();
            let collateral_shares: u128 = obligation
                .deposits
                .iter()
                .map(|d| d.deposited_shares as u128)
                .sum();
            Some(ObligationSnapshot {
                key: *key,
                deposits,
                vault_balances,
                debt,
                collateral_shares,
                borrowed_value: obligation.borrowed_value,
                unhealthy_borrow_value: obligation.unhealthy_borrow_value,
                stale: obligation.stale,
            })
        })
        .collect();

    Snapshot {
        reserves,
        obligations,
    }
}

/// A share mint's real supply, read from the mint rather than from the
/// reserve's mirror of it.
///
/// The whole point of INV-06 is that the two can disagree, so it must not be
/// read through the program's own bookkeeping.
pub fn real_mint_supply(env: &Env, mint: &Pubkey) -> u64 {
    let account = env.svm.get_account(mint).expect("no share mint");
    // SPL Token-2022 mint: supply is a u64 at offset 36.
    u64::from_le_bytes(account.data[36..44].try_into().unwrap())
}

/// The share vault a reserve holds an obligation's collateral in.
pub fn obligation_share_vault_for(reserve: Pubkey, obligation: Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            aera::constants::OBLIGATION_SHARE_VAULT_SEED,
            reserve.as_ref(),
            obligation.as_ref(),
        ],
        &aera::id(),
    )
    .0
}

/// Evaluate all sixteen, returning every violation rather than the first.
///
/// Returning all of them matters: one broken invariant usually breaks several,
/// and the *set* that broke together says more about the cause than the one
/// that happened to be checked first.
pub fn check_all(now: &Snapshot, before: Option<&Snapshot>, prices_moved: bool) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut fail = |id: &'static str, detail: String| out.push(Violation { id, detail });

    for (index, r) in now.reserves.iter().enumerate() {
        let label = r.label;

        // INV-01. The identity the whole accounting rests on.
        let expected = r.available + r.solvency.outstanding_debt - r.accrued_fees.min(r.available);
        if r.total_liquidity + r.accrued_fees != r.available + r.solvency.outstanding_debt {
            fail(
                "INV-01 accounting identity",
                format!(
                    "{label}: total_liquidity {} + fees {} != available {} + debt {} (expected ~{expected})",
                    r.total_liquidity, r.accrued_fees, r.available, r.solvency.outstanding_debt
                ),
            );
        }

        // INV-02. Tokens the program believes it has must actually be there.
        if !r.solvency.vault_backs_tracked() {
            fail("INV-02 vault backs the books", r.solvency.report(label));
        }

        // INV-03. Claims must not outrun assets by more than rounding.
        if !r.solvency.solvent() {
            fail("INV-03 claims <= assets", r.solvency.report(label));
        }

        // INV-06. The mirror must match the mint.
        if r.share_supply != r.real_share_supply {
            fail(
                "INV-06 share supply mirrors the mint",
                format!(
                    "{label}: tracked {} real {}",
                    r.share_supply, r.real_share_supply
                ),
            );
        }

        // INV-08. An index below one would forgive debt.
        if r.borrowed_principal > 0 && r.borrow_index < FIXED_POINT_SCALE {
            fail(
                "INV-08 index at least one while in debt",
                format!("{label}: index {}", r.borrow_index),
            );
        }

        /*
         * INV-09 / INV-10. Caps, with zero meaning unlimited.
         *
         * Stated as "usage never *grows* past a cap", not "usage never exceeds
         * a cap". The difference is an operator cutting a cap below what is
         * already supplied, which is a legitimate and important action -- it is
         * how an incident is contained -- and which the program allows because
         * a tightening blocks new supply rather than forcing existing suppliers
         * out. The stronger statement fires on that, and the deep fuzz grid
         * found it on the second step of the first seed.
         *
         * What must never happen is the protocol accepting *more* supply or
         * *more* debt while already over a cap.
         */
        if r.supply_cap > 0 && r.total_liquidity > r.supply_cap as u128 {
            /*
             * Interest accrual is not supplying.
             *
             * `total_liquidity` rises every slot as borrowers accrue, so a
             * reserve past its cap drifts further past it with nobody
             * depositing. Shares are minted only by `supply` and burned only by
             * `withdraw`, so the share supply is the quantity that moves when a
             * user actually adds liquidity -- which is what the cap governs.
             */
            let grew = before
                .map(|b| r.share_supply > b.reserves[index].share_supply)
                .unwrap_or(false);
            if grew {
                fail(
                    "INV-09 supply cap",
                    format!(
                        "{label}: liquidity grew to {} while already past the cap {}",
                        r.total_liquidity, r.supply_cap
                    ),
                );
            }
        }
        if r.borrow_cap > 0 && r.solvency.outstanding_debt > r.borrow_cap as u128 {
            let grew = before
                .map(|b| {
                    /*
                     * Interest accrual is not borrowing.
                     *
                     * Debt rises every slot without anybody opening a position,
                     * and a reserve sitting at its cap will drift past it purely
                     * through accrual. What this invariant is about is the
                     * protocol handing out *new* debt, so compare the scaled
                     * principal, which only moves on borrow and repay.
                     */
                    r.borrowed_principal > b.reserves[index].borrowed_principal
                })
                .unwrap_or(false);
            if grew {
                fail(
                    "INV-10 borrow cap",
                    format!(
                        "{label}: principal grew to {} while debt {} is already past the cap {}",
                        r.borrowed_principal, r.solvency.outstanding_debt, r.borrow_cap
                    ),
                );
            }
        }

        // INV-11. You cannot lend out more than was put in.
        if r.solvency.outstanding_debt > r.total_liquidity {
            fail(
                "INV-11 debt <= supplied",
                format!(
                    "{label}: debt {} > liquidity {}",
                    r.solvency.outstanding_debt, r.total_liquidity
                ),
            );
        }

        // INV-14. The oracle's rate is the pool's, not its own opinion.
        if let (Some(gross), Some(fee_bps)) = (r.pool_gross_rate, r.pool_fee_bps) {
            if r.oracle.reference.is_set() && r.oracle.reference.gross_rate != gross {
                fail(
                    "INV-14 rate is derived from the source",
                    format!(
                        "{label}: reference {} but the pool says {} (fee {} bps)",
                        r.oracle.reference.gross_rate, gross, fee_bps
                    ),
                );
            }
        }

        // INV-15. Healthy is a claim about a reference that exists.
        let health = OracleHealth::from_u8(r.oracle.health).expect("health byte out of range");
        if matches!(health, OracleHealth::Healthy | OracleHealth::RateWarning)
            && !r.oracle.reference.is_set()
        {
            fail(
                "INV-15 healthy implies anchored",
                format!("{label}: health {health:?} with no reference"),
            );
        }

        if let Some(before) = before {
            let was = &before.reserves[index];

            /*
             * INV-19. Protocol fees cannot exceed the revenue that produced
             * them.
             *
             * Fees come from two places, and this used to know about one. A
             * share of accrued interest, and — since Core's origination fee
             * became non-zero — a one-off cut of newly drawn debt. With only
             * the interest term, every borrow tripped this: `accrued_fees` rose
             * by exactly the origination fee while the index had not moved, so
             * the check reported fees appearing from nowhere.
             *
             * The origination allowance is bounded by the debt that was
             * ACTUALLY drawn in this step, so it is zero on every step that is
             * not a borrow. A fee appearing during a repay, a supply or an
             * accrue still fails, which is the case that would mean fees were
             * being taken out of principal — i.e. out of the suppliers.
             */
            let fees_taken = r.accrued_fees.saturating_sub(was.accrued_fees);
            /*
             * Interest is derived from the index, not from the change in
             * outstanding debt.
             *
             * The first version of this check compared fees against the net
             * debt movement and fired immediately: a repayment both accrues
             * interest and reduces principal, so the net change can be zero
             * while real interest was charged. The index isolates accrual from
             * repayment, because nothing but interest moves it.
             */
            let interest = was.borrowed_principal * r.borrow_index.saturating_sub(was.borrow_index)
                / FIXED_POINT_SCALE;

            /*
             * New debt drawn this step, at the current index, and the fee that
             * may have been charged on it.
             *
             * Scaled principal only grows when somebody borrows; a repayment
             * lowers it and a pure accrual leaves it alone. So this term is
             * zero except on a borrow, and the check stays as strict as it was
             * everywhere else.
             */
            let new_debt = r.borrowed_principal.saturating_sub(was.borrowed_principal)
                * r.borrow_index
                / FIXED_POINT_SCALE;
            let origination = new_debt * r.origination_fee_bps as u128 / 10_000;

            if fees_taken > interest + origination + 1 {
                fail(
                    "INV-19 fees never exceed the revenue that made them",
                    format!(
                        "{label}: fees rose by {fees_taken}, against {interest} of interest \
                         (index {} -> {} on principal {}) and {origination} of origination \
                         (on {new_debt} newly drawn at {} bps)",
                        was.borrow_index,
                        r.borrow_index,
                        was.borrowed_principal,
                        r.origination_fee_bps
                    ),
                );
            }

            // INV-04. A falling index would forgive debt already owed.
            if r.borrow_index < was.borrow_index {
                fail(
                    "INV-04 borrow index never decreases",
                    format!("{label}: {} -> {}", was.borrow_index, r.borrow_index),
                );
            }

            // INV-05. A falling share rate would pay one supplier out of
            // another's claim.
            if r.solvency.share_supply > 0
                && was.solvency.share_supply > 0
                && r.solvency.share_rate < was.solvency.share_rate
            {
                fail(
                    "INV-05 share rate never decreases",
                    format!(
                        "{label}: {} -> {}",
                        was.solvency.share_rate, r.solvency.share_rate
                    ),
                );
            }

            // INV-16. The reference moves only on an accepted observation, and
            // an accepted observation is one the epoch advanced for.
            let reference_moved = r.oracle.reference.gross_rate != was.oracle.reference.gross_rate;
            if reference_moved && r.oracle.last_source_epoch < was.oracle.last_source_epoch {
                fail(
                    "INV-16 reference only moves forward",
                    format!(
                        "{label}: reference changed while the source epoch went {} -> {}",
                        was.oracle.last_source_epoch, r.oracle.last_source_epoch
                    ),
                );
            }
        }
    }

    for o in &now.obligations {
        /*
         * INV-12. Debt without collateral is unrecoverable.
         *
         * Not a restatement of the health check: an obligation *can* legally be
         * unhealthy, and liquidation is how that gets resolved. What can never
         * happen is debt with nothing behind it, because a liquidator has no
         * reason to touch it and the loss lands on the suppliers. Every path
         * that removes collateral -- withdraw and seizure alike -- has to leave
         * this true or the protocol has manufactured bad debt.
         */
        if o.debt > 0 && o.collateral_shares == 0 {
            fail(
                "INV-12 no debt without collateral",
                format!("obligation {}: debt {} with no collateral", o.key, o.debt),
            );
        }
    }

    if let Some(before) = before {
        for (index, o) in now.obligations.iter().enumerate() {
            let was = &before.obligations[index];

            /*
             * INV-17. A user action must not leave a position underwater.
             *
             * Only when no price moved. A falling collateral rate legitimately
             * pushes positions past the liquidation line -- that is what
             * liquidation is for -- so checking this after a rate change would
             * be asserting that markets cannot move. What must never happen is
             * a *borrow* or *withdraw* that lands the borrower there, which is
             * precisely the check the health rule exists to perform.
             */
            if !prices_moved && !o.stale && o.borrowed_value > o.unhealthy_borrow_value {
                fail(
                    "INV-17 no user action leaves a position underwater",
                    format!(
                        "obligation {}: borrowed {} > unhealthy limit {} with no price move",
                        o.key, o.borrowed_value, o.unhealthy_borrow_value
                    ),
                );
            }

            /*
             * INV-18. Losing collateral must not raise what is owed.
             *
             * The shape a liquidation bug takes: seize the collateral and leave
             * the debt where it was, or worse, higher. A step that reduced a
             * position's collateral while its principal did not fall is either
             * a seizure that forgot to repay or a withdrawal that should have
             * been refused.
             */
            if o.collateral_shares < was.collateral_shares && o.debt > was.debt {
                fail(
                    "INV-18 losing collateral never raises debt",
                    format!(
                        "obligation {}: collateral {} -> {} while debt {} -> {}",
                        o.key, was.collateral_shares, o.collateral_shares, was.debt, o.debt
                    ),
                );
            }
        }
    }

    // INV-13. An obligation's books must match the tokens held for it.
    for o in &now.obligations {
        for ((reserve, recorded), (_, actual)) in o.deposits.iter().zip(o.vault_balances.iter()) {
            if recorded != actual {
                fail(
                    "INV-13 collateral matches its vault",
                    format!(
                        "obligation {} reserve {reserve}: recorded {recorded} vault {actual}",
                        o.key
                    ),
                );
            }
        }
    }

    out
}

/// Assert the sixteen hold, with enough context to replay the failure.
pub fn assert_invariants(
    now: &Snapshot,
    before: Option<&Snapshot>,
    prices_moved: bool,
    context: &str,
) {
    let violations = check_all(now, before, prices_moved);
    if violations.is_empty() {
        return;
    }
    let mut message = format!(
        "\nINVARIANT VIOLATED  ({} of 16)\n{context}\n",
        violations.len()
    );
    for v in &violations {
        message.push_str(&format!("  {}\n    {}\n", v.id, v.detail));
    }
    panic!("{message}");
}
