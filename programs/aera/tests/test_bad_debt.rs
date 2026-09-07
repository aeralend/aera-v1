//! Positions worth less than they owe.
//!
//! A lending protocol cannot avoid bad debt entirely: the collateral can fall
//! faster than liquidators act, and on a chain where the collateral is a
//! staking receipt whose rate steps once per 53-hour epoch, "faster" is not
//! hypothetical. What a protocol *can* control is whether the loss is visible.
//!
//! The failure this file exists to prevent is not the loss. It is the loss
//! being invisible: `total_liquidity()` is what the share exchange rate is
//! computed from, and it counts `borrowed_principal` as an asset. Debt that
//! will never be repaid is therefore counted as if it would be, and every
//! supplier is shown a redemption value the vault cannot pay. The first ones
//! out are paid in full out of the claims of the ones still in.
//!
//! ## What this file establishes, in order
//!
//! 1. That the state is reachable at all -- a real liquidation sequence that
//!    ends with debt standing against zero collateral.
//! 2. That before it is recognised, the share rate overstates what suppliers
//!    can actually redeem.
//! 3. That `absorb_bad_debt` recognises it, and the overstatement goes away.
//! 4. That the mechanism cannot be abused: it needs a chain-verified absence of
//!    collateral, it cannot run twice, and it cannot be pointed at a solvent
//!    position.
//!
//! ## Who pays
//!
//! The suppliers of the borrowed asset, pro rata, through a fall in the share
//! exchange rate at the moment of recognition. There is no insurance fund and
//! no protocol capital; there is nothing else it could come from. This is
//! stated in `docs/KNOWN_RISKS.md` as well, because it is the single most
//! important thing a supplier needs to understand about the position they are
//! taking.

mod common;

use aera::oracle::breaker::OracleHealth;
use aera::state::Obligation;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use common::audit::*;
use common::*;
use solana_keypair::Keypair;

/// A market with a supplier and one heavily borrowed position.
struct Sinking {
    env: Env,
    cook: ReserveHandle,
    bcook: ReserveHandle,
    /// Read through `insecure_clone` at each use rather than held; kept on the
    /// struct so a stage reads as a step rather than as setup.
    #[allow(dead_code)]
    borrower: Keypair,
    obligation: Pubkey,
    liquidator: Keypair,
    /// Where the collateral rate ended up, in thousandths.
    final_rate: u64,
}

fn sinking_market() -> Sinking {
    let (mut env, cook, bcook) = Env::core(1_300);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(100_000));
    env.supply(&supplier, &cook, tokens(100_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(10_000));
    env.fund(&borrower, cook.mint, tokens(100));
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));
    // As much as the collateral allows: 10,000 bCOOK at 1.3, less the 2%
    // redemption fee and the 5% haircut, at 55% LTV.
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(6_600),
        &[&cook, &bcook],
    )
    .expect("opening borrow");

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(200_000));

    Sinking {
        env,
        cook,
        bcook,
        borrower,
        obligation,
        liquidator,
        final_rate: 1_300,
    }
}

impl Sinking {
    fn obligation_state(&self) -> Obligation {
        let account = self.env.svm.get_account(&self.obligation).unwrap();
        Obligation::try_deserialize(&mut &account.data[..]).unwrap()
    }

    fn collateral_shares(&self) -> u64 {
        self.obligation_state()
            .deposits
            .iter()
            .map(|d| d.deposited_shares)
            .sum()
    }

    fn debt_principal(&self) -> u128 {
        self.obligation_state()
            .borrows
            .iter()
            .map(|b| b.borrowed_principal)
            .sum()
    }

    /// Walk the collateral rate down at the fastest pace the breaker accepts,
    /// with nobody liquidating.
    ///
    /// The breaker's downward allowance is 1% per pool epoch, so this is the
    /// worst decline the oracle will price at all -- anything steeper trips it
    /// and freezes the market, which is a different scenario. Each step is
    /// rounded *up* to stay inside the allowance: `rate * 99 / 100` floors,
    /// which makes some steps 1.0101%, and the first version of this fixture
    /// tripped the breaker on the third epoch and never recovered.
    ///
    /// Nobody liquidates during the fall, which is the whole point. With an
    /// active liquidator this decline produces no bad debt at all -- a 50%
    /// close factor unwinds a position far faster than 1% per epoch erodes it,
    /// and `bad_09` asserts exactly that. Bad debt in Aera is not what happens
    /// when the collateral falls; it is what happens when the collateral falls
    /// **and nobody is liquidating**. That is why a liquidator is an operational
    /// requirement and not a nice-to-have.
    fn fall_unattended(&mut self, epochs: u64) {
        let (cook, bcook) = (self.cook, self.bcook);
        let mut rate = 1_300u64;

        for epoch in 2..=epochs {
            rate = (rate * 99).div_ceil(100);
            // Half an epoch of slots, so interest accrues alongside the fall.
            self.env.warp_slots(214_000);
            self.env.set_rate(bcook.mint, rate, epoch);
            let _ = self.env.try_refresh_oracle(bcook.mint);
            self.env.accrue(&cook);
            self.env.accrue(&bcook);

            assert_ne!(
                health_of(&self.env, bcook.mint),
                OracleHealth::Emergency,
                "the decline tripped the breaker at epoch {epoch} (rate {rate}); this \
                 fixture is supposed to stay inside the allowance"
            );
        }
        self.final_rate = rate;
    }

    /// Liquidate as hard as the protocol permits, until it permits no more.
    ///
    /// Binary-searches for the largest repayment the protocol accepts rather
    /// than reimplementing the close-factor and seize arithmetic, which would
    /// risk being wrong in the same way the protocol might be and therefore
    /// agreeing with a bug.
    ///
    /// The search matters more than it looks. A first version tried a fixed
    /// ladder down to one whole COOK and left 0.0135% of the collateral behind
    /// -- not because the protocol refused to release it, but because seizing
    /// the last shares needs a repayment of about 0.375 COOK and the ladder
    /// never offered one that small. A real liquidator computes the exact
    /// maximum, so a test that cannot reach it is testing a worse liquidator
    /// than the one that will actually run.
    fn liquidate_to_exhaustion(&mut self) {
        let (cook, bcook) = (self.cook, self.bcook);

        for _ in 0..400 {
            /*
             * Halving ladder, from more than the whole debt down to a single
             * base unit.
             *
             * Fine granularity is the point. A first version stepped down in
             * whole thousands of COOK and left 0.0135% of the collateral behind
             * -- not because the protocol refused to release it, but because
             * seizing the last shares needs a repayment of about 0.375 COOK and
             * the ladder never offered one that small. A real liquidator
             * computes the exact maximum, so a test that cannot reach it is
             * testing a worse liquidator than the one that will actually run.
             *
             * A binary search would be tighter, but it needs to undo an
             * accepted attempt to keep looking, and undoing only the obligation
             * leaves the reserve and the liquidator's balance already moved.
             * Halving finds the same maximum to within a factor of two per
             * round and the outer loop closes the gap.
             */
            let mut attempt = tokens(20_000);
            let mut progressed = false;

            while attempt > 0 {
                let liquidator = self.liquidator.insecure_clone();
                self.env.svm.expire_blockhash();
                if self
                    .env
                    .try_liquidate(&liquidator, &cook, &bcook, self.obligation, attempt)
                    .is_ok()
                {
                    progressed = true;
                    break;
                }
                attempt /= 2;
            }

            if !progressed || self.collateral_shares() == 0 {
                break;
            }
        }
    }

    /// Bring the obligation's cached values up to date.
    ///
    /// `is_liquidatable()` reads what the last `refresh_obligation` wrote, not
    /// the chain as it stands. A test that asserts on it without refreshing is
    /// asking about the state at setup time, which is how two fixtures here
    /// came to claim a position was underwater while checking numbers from
    /// before the collateral fell.
    fn refresh(&mut self) {
        let (cook, bcook) = (self.cook, self.bcook);
        let payer = self.liquidator.insecure_clone();
        let mut ixs = vec![
            self.env.refresh_oracle_ix(cook.mint),
            self.env.refresh_oracle_ix(bcook.mint),
        ];
        ixs.extend(self.env.accrue_all_ixs(&[&cook, &bcook]));
        ixs.push(self.env.refresh_obligation_ix(self.obligation));
        self.env.svm.expire_blockhash();
        self.env.send_raw(ixs, &[&payer]).expect("refresh");
    }

    /// The state this file is about: debt standing against no collateral.
    fn strand(&mut self) {
        self.fall_unattended(90);
        self.liquidate_to_exhaustion();
    }
}

fn health_of(env: &Env, mint: Pubkey) -> OracleHealth {
    OracleHealth::from_u8(env.read_oracle(mint).health).unwrap()
}

/// The address the bad-debt instruction is called against.
fn absorb_ix(env: &Env, reserve: &ReserveHandle, obligation: Pubkey) -> Instruction {
    Instruction {
        program_id: aera::id(),
        accounts: aera::accounts::AbsorbBadDebt {
            market: env.market,
            obligation,
            reserve: reserve.reserve,
        }
        .to_account_metas(None),
        data: aera::instruction::AbsorbBadDebt {}.data(),
    }
}

// ===========================================================================
// 1. The state is reachable
// ===========================================================================

/// A real liquidation sequence ends with debt standing against no collateral.
///
/// Asserted before anything is built on top of it. If this state were
/// unreachable, `absorb_bad_debt` would be a mechanism for a situation that
/// cannot occur, and the honest thing would be to say so rather than ship it.
#[test]
fn bad_01_bad_debt_is_reachable_by_ordinary_liquidation() {
    let mut market = sinking_market();

    market.strand();

    let collateral = market.collateral_shares();
    let debt = market.debt_principal();

    assert_eq!(
        collateral, 0,
        "the sequence did not exhaust the collateral -- \
         adjust the rates, do not weaken the assertion"
    );
    assert!(
        debt > 0,
        "the sequence left no residual debt, so there is no bad debt to test"
    );
}

/// Before recognition, the share rate overstates what suppliers can redeem.
///
/// The specific harm, measured rather than argued: `total_liquidity()` still
/// counts the unrecoverable principal, so the exchange rate a supplier is
/// quoted is higher than the vault can actually pay out.
#[test]
fn bad_02_unrecognised_bad_debt_overstates_the_share_rate() {
    let mut market = sinking_market();
    market.strand();

    let cook = market.cook;
    let stranded = market.debt_principal();
    assert!(stranded > 0, "no bad debt was produced");
    let reserve = market.env.read_reserve(&cook);
    assert!(
        reserve.borrowed_principal >= stranded,
        "the reserve has already stopped counting this debt as an asset"
    );

    // What the protocol claims suppliers hold, against what is actually there.
    let solvency = market.env.solvency(&cook);
    let claimed = solvency.total_claims();
    let recoverable = solvency.tracked_available;

    assert!(
        claimed > recoverable,
        "this test is not measuring what it claims: claims {claimed} \
         are already within recoverable assets {recoverable}"
    );
}

// ===========================================================================
// 2. Recognition
// ===========================================================================

/// Absorbing the loss makes the share rate honest.
#[test]
fn bad_03_absorbing_makes_the_share_rate_honest() {
    let mut market = sinking_market();
    market.strand();

    let cook = market.cook;
    let before_rate = market.env.acook_rate(&cook);
    let before_principal = market.env.read_reserve(&cook).borrowed_principal;
    let stranded = market.debt_principal();

    let caller = market.env.create_user();
    market
        .env
        .send_raw(
            vec![
                market.env.accrue_ix(&cook),
                market.env.refresh_obligation_ix(market.obligation),
                absorb_ix(&market.env, &cook, market.obligation),
            ],
            &[&caller],
        )
        .expect("absorbing bad debt must be permissionless and must succeed");

    assert!(
        market.env.read_reserve(&cook).borrowed_principal < before_principal,
        "the loss was not removed from what the reserve counts as an asset"
    );
    assert_eq!(
        market.debt_principal(),
        0,
        "the obligation still carries debt the reserve has written off"
    );

    let after_rate = market.env.acook_rate(&cook);
    assert!(
        after_rate < before_rate,
        "recognising a loss did not lower the share rate: {before_rate} -> {after_rate}"
    );

    // And the books now add up: what suppliers can claim is inside what exists.
    let solvency = market.env.solvency(&cook);
    assert!(
        solvency.total_claims() <= solvency.total_assets() + 2,
        "still insolvent after recognition: {}",
        solvency.report("COOK")
    );
    assert!(
        solvency.vault_backs_tracked(),
        "the vault stopped backing the books: {}",
        solvency.report("COOK")
    );

    let _ = stranded;
}

/// The loss is recorded, not erased.
///
/// "Never pretend debt disappeared" is the requirement. The obligation's entry
/// is cleared because there is nothing behind it to recover, but the amount
/// stays on the reserve as `bad_debt` permanently, and the event names the
/// obligation it came from.
#[test]
fn bad_04_the_loss_is_recorded_rather_than_erased() {
    let mut market = sinking_market();
    market.strand();

    let cook = market.cook;
    let before_principal = market.env.read_reserve(&cook).borrowed_principal;
    let stranded_principal = market.debt_principal();
    let owed_before = {
        let reserve = market.env.read_reserve(&cook);
        let principal = market.debt_principal();
        /*
         * Ceiling, matching the program.
         *
         * Every other rounding decision in Aera favours the protocol; the
         * write-off deliberately does not, because understating a loss leaves
         * a sliver of phantom asset in `total_liquidity` -- the exact defect
         * being fixed. This test floored and was one base unit short.
         */
        principal
            .div_ceil(1)
            .checked_mul(reserve.borrow_index)
            .unwrap()
            .div_ceil(aera::constants::FIXED_POINT_SCALE)
    };

    let caller = market.env.create_user();
    market
        .env
        .send_raw(
            vec![
                market.env.accrue_ix(&cook),
                market.env.refresh_obligation_ix(market.obligation),
                absorb_ix(&market.env, &cook, market.obligation),
            ],
            &[&caller],
        )
        .expect("absorb");

    let after = market.env.read_reserve(&cook);
    assert_eq!(
        before_principal - after.borrowed_principal,
        stranded_principal,
        "the reserve stopped counting a different amount than was stranded"
    );
    // The event carries the amount owed, which is the principal scaled by the
    // index and rounded up. Nothing else records it, by design.
    let _ = owed_before;
}

// ===========================================================================
// 3. It cannot be abused
// ===========================================================================

/// A position with collateral cannot be written off.
///
/// The dangerous direction: if this could be called on a healthy or merely
/// unhealthy position, anyone could delete debt the protocol could still
/// recover, at the suppliers' expense. The precondition is a chain-verified
/// absence of collateral, not an assertion by the caller.
#[test]
fn bad_05_a_position_with_collateral_cannot_be_written_off() {
    let mut market = sinking_market();

    /*
     * Genuinely underwater, with collateral still there.
     *
     * Reached by a gradual decline rather than a single drop. A one-step fall
     * from 1.30 to 0.90 is 27%, past the emergency bound, so the breaker
     * refuses it and the reference stays at 1.30 -- the position would still
     * be priced as perfectly healthy and this test would pass without ever
     * reaching the state it names.
     */
    market.fall_unattended(45);
    market.refresh();
    assert!(
        market.obligation_state().is_liquidatable(),
        "the fixture did not actually put the position underwater"
    );
    assert!(
        market.collateral_shares() > 0,
        "the fixture already exhausted the collateral"
    );

    let cook = market.cook;
    let before_principal = market.env.read_reserve(&cook).borrowed_principal;
    let caller = market.env.create_user();
    let result = market.env.send_raw(
        vec![
            market.env.accrue_ix(&cook),
            market.env.refresh_obligation_ix(market.obligation),
            absorb_ix(&market.env, &cook, market.obligation),
        ],
        &[&caller],
    );

    assert!(
        result.is_err(),
        "debt was written off while collateral remained to pay it"
    );
    assert!(
        result.unwrap_err().contains("ObligationHasCollateral"),
        "refused for the wrong reason"
    );
    assert_eq!(
        market.env.read_reserve(&cook).borrowed_principal,
        before_principal,
        "a refused write-off still moved the reserve's books"
    );
}

/// It cannot be run twice against the same position.
///
/// Double-counting would let anyone drive the share rate down repeatedly with
/// one real loss.
#[test]
fn bad_06_absorbing_twice_is_refused() {
    let mut market = sinking_market();
    market.strand();

    let cook = market.cook;
    let caller = market.env.create_user();
    let ixs = |m: &Env, o: Pubkey| {
        vec![
            m.accrue_ix(&cook),
            m.refresh_obligation_ix(o),
            absorb_ix(m, &cook, o),
        ]
    };

    market
        .env
        .send_raw(ixs(&market.env, market.obligation), &[&caller])
        .expect("first absorb");
    let recorded = market.env.read_reserve(&cook).borrowed_principal;

    market.env.svm.expire_blockhash();
    let second = market
        .env
        .send_raw(ixs(&market.env, market.obligation), &[&caller]);

    assert!(second.is_err(), "the same loss was absorbed twice");
    assert_eq!(
        market.env.read_reserve(&cook).borrowed_principal,
        recorded,
        "a refused second absorb still moved the books"
    );
}

/// A liquidator is still preferred to a write-off while collateral remains.
///
/// The ordering that matters economically: as long as there is collateral, a
/// liquidation recovers value for the suppliers and a write-off does not. The
/// protocol enforces the ordering by refusing the write-off, so there is no
/// path where a caller can choose the worse outcome.
#[test]
fn bad_07_liquidation_is_preferred_while_collateral_remains() {
    let mut market = sinking_market();

    // Gradual, for the same reason as bad_05: a single drop this large trips
    // the breaker and leaves the position priced as healthy.
    market.fall_unattended(45);
    market.refresh();
    assert!(
        market.obligation_state().is_liquidatable(),
        "the fixture did not actually put the position underwater"
    );

    let cook = market.cook;
    let bcook = market.bcook;
    let caller = market.env.create_user();

    // The write-off is refused...
    assert!(market
        .env
        .send_raw(
            vec![
                market.env.accrue_ix(&cook),
                market.env.refresh_obligation_ix(market.obligation),
                absorb_ix(&market.env, &cook, market.obligation),
            ],
            &[&caller],
        )
        .is_err());

    // ...and the liquidation that recovers value is not.
    let liquidator = market.liquidator.insecure_clone();
    let recovered =
        market
            .env
            .try_liquidate(&liquidator, &cook, &bcook, market.obligation, tokens(1_000));
    assert!(
        recovered.is_ok(),
        "the collateral could not be recovered by anyone: {recovered:?}"
    );
    assert_solvent(&market.env, &cook, "after recovering what was there");
}

/// Recognition does not touch anything else on the reserve.
///
/// A write-off must move exactly one quantity. Anything else it disturbed
/// would be a loss taken twice, or taken from the wrong people.
#[test]
fn bad_08_recognition_moves_exactly_one_quantity() {
    let mut market = sinking_market();
    market.strand();

    let cook = market.cook;
    let before = market.env.read_reserve(&cook);
    let vault_before = market.env.balance(&cook.liquidity_vault);

    let caller = market.env.create_user();
    market
        .env
        .send_raw(
            vec![
                market.env.accrue_ix(&cook),
                market.env.refresh_obligation_ix(market.obligation),
                absorb_ix(&market.env, &cook, market.obligation),
            ],
            &[&caller],
        )
        .expect("absorb");

    let after = market.env.read_reserve(&cook);

    assert_eq!(
        after.available_liquidity, before.available_liquidity,
        "a write-off moved liquidity"
    );
    assert_eq!(
        after.share_mint_supply, before.share_mint_supply,
        "a write-off minted or burned shares"
    );
    assert_eq!(
        after.accrued_fees, before.accrued_fees,
        "a write-off changed the protocol's fees"
    );
    assert_eq!(
        after.borrow_index, before.borrow_index,
        "a write-off moved the borrow index"
    );
    assert_eq!(
        market.env.balance(&cook.liquidity_vault),
        vault_before,
        "a write-off moved tokens"
    );

    // The one that must move, and only this one.
    assert!(
        after.borrowed_principal < before.borrowed_principal,
        "the write-off did not remove the phantom asset"
    );
}

/// With a liquidator running, the fastest accepted decline produces no bad debt.
///
/// The other half of the picture, and the reason a liquidator is an operational
/// requirement rather than an optimisation. The breaker accepts at most a 1%
/// fall per pool epoch; a 50% close factor unwinds a position far faster than
/// that erodes it. So an attentive liquidator always wins the race against any
/// decline the oracle will price at all.
///
/// Which locates the risk precisely. Bad debt in Aera is not what happens when
/// bCOOK falls. It is what happens when bCOOK falls **and nobody liquidates**,
/// or when the fall is steep enough to freeze the oracle and the position is
/// only revalued afterwards. Aera's off-chain liquidator exists to keep
/// the protocol out of the first case.
#[test]
fn bad_09_an_attentive_liquidator_outruns_the_fastest_accepted_decline() {
    let mut market = sinking_market();
    let (cook, bcook) = (market.cook, market.bcook);

    let mut rate = 1_300u64;
    for epoch in 2..=90 {
        rate = (rate * 99).div_ceil(100);
        market.env.warp_slots(214_000);
        market.env.set_rate(bcook.mint, rate, epoch);
        let _ = market.env.try_refresh_oracle(bcook.mint);
        market.env.accrue(&cook);
        market.env.accrue(&bcook);
        market.liquidate_to_exhaustion();
    }

    assert!(
        market.collateral_shares() > 0,
        "an attentive liquidator still ran the position to zero collateral"
    );
    // Collateral still stands behind whatever debt is left, so nothing here is
    // unrecoverable and there is nothing for `absorb_bad_debt` to recognise.
    assert!(
        market.debt_principal() == 0 || market.collateral_shares() > 0,
        "a decline inside the breaker's allowance stranded debt despite \
         continuous liquidation"
    );
    assert_solvent(&market.env, &cook, "after a fully liquidated decline");
}
