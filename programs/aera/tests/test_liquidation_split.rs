//! Gap D: Aera's share of the liquidation bonus, end to end.
//!
//! The one claim this file exists to prove is that **the borrower's penalty does
//! not change**. Aera's share is carved out of the bonus the borrower already
//! pays, never added to it. Everything else here is a consequence of that:
//! conservation, the rounding direction, the small-liquidation behaviour, and
//! the fact that a reserve with no share configured behaves exactly as it did
//! before this feature existed.
//!
//! The destination tests matter for a different reason. Liquidation is the one
//! operation Aera can least afford to make fragile, so the protocol leg is
//! written to be inert when nothing is owed and strictly validated when
//! something is.

mod common;

use aera::state::ReserveConfig;
use anchor_lang::prelude::Pubkey;
use common::*;
use solana_keypair::Keypair;

/// The COOKHOUSE candidate: 12% total bonus, 1.5 points to Aera.
const TOTAL_BONUS_BPS: u16 = 1_200;
const CANDIDATE_SHARE_BPS: u16 = 150;

/// A market where the collateral reserve carries a 12% bonus, a borrower is
/// underwater, and a funded liquidator is standing by.
struct Book {
    env: Env,
    cook: ReserveHandle,
    bcook: ReserveHandle,
    borrower: Keypair,
    obligation: Pubkey,
    liquidator: Keypair,
}

fn collateral_config() -> ReserveConfig {
    ReserveConfig {
        liquidation_bonus_bps: TOTAL_BONUS_BPS,
        ..bcook_config()
    }
}

impl Book {
    /// `collateral_price_thousandths` sets how deep underwater the position
    /// starts.
    fn underwater(collateral_price_thousandths: u64) -> Self {
        let (mut env, cook, _core_bcook) = Env::core(1_000);

        /*
         * A dedicated collateral reserve carrying the 12% bonus, created with it
         * rather than reconfigured into it.
         *
         * `set_params` treats raising a bonus as a loosening and queues it, so a
         * fixture that called it would silently run against the 800 bps default
         * -- and every arithmetic assertion below would be checking the wrong
         * denominator while still passing its conservation checks.
         */
        let bcook = env.add_reserve(DECIMALS, px(1_000), collateral_config());

        let supplier = env.create_user();
        env.fund(&supplier, cook.mint, tokens(200_000));
        env.supply(&supplier, &cook, tokens(200_000));

        let borrower = env.create_user();
        env.fund(&borrower, bcook.mint, tokens(10_000));
        env.fund(&borrower, cook.mint, 0);
        let obligation = env.open_position(&borrower, &bcook, tokens(10_000));
        // 10,000 bCOOK at 1.00, 5% haircut, 55% LTV -> a 5,225 limit.
        env.try_borrow(
            &borrower,
            &cook,
            obligation,
            tokens(5_200),
            &[&cook, &bcook],
        )
        .expect("the initial borrow");

        /*
         * Mark the collateral down, past the liquidation line.
         *
         * `set_price` rather than `move_rate`: it advances the source epoch AND
         * resets the breaker, which is what every other liquidation suite uses.
         * A raw `move_rate` of this size is refused as a 3000 bps jump and
         * leaves the oracle in Emergency holding the OLD price, so the position
         * stays healthy and every assertion below becomes vacuous.
         */
        env.set_price(bcook.mint, px(collateral_price_thousandths));
        let instructions = {
            let mut ixs = env.accrue_all_ixs(&[&cook, &bcook]);
            ixs.push(env.refresh_obligation_ix(obligation));
            ixs
        };
        let admin = env.admin.insecure_clone();
        solana_kite::send_transaction_from_instructions(
            &mut env.svm,
            instructions,
            &[&admin],
            &admin.pubkey(),
        )
        .expect("refresh after the markdown");

        let liquidator = env.create_user();
        env.fund(&liquidator, cook.mint, tokens(100_000));
        env.ensure_share_ata(&liquidator, bcook.share_mint);

        Self {
            env,
            cook,
            bcook,
            borrower,
            obligation,
            liquidator,
        }
    }

    /// Enable Aera's share, waiting out the timelock it is subject to.
    ///
    /// Raising the share is a **loosening** -- zero means Aera takes nothing,
    /// which is the tightest setting -- so turning it on always waits. That is
    /// the intended behaviour and `cfg_02` tests it directly; here it just has
    /// to be got through.
    fn set_share(&mut self, bps: u16) {
        let bcook = self.bcook;
        self.env
            .set_protocol_liquidation_share(&bcook, bps)
            .expect("queue the protocol liquidation share");
        if bps > 0 {
            self.env.warp_seconds(60 * 60 * 24 + 1);
            self.env
                .apply_pending_risk_config(&bcook)
                .expect("apply the queued share");
            // Time moved, so the position has to be re-marked and re-refreshed.
            self.remark();
        }
        assert_eq!(
            self.env
                .read_risk_config(&bcook)
                .map(|c| c.protocol_liquidation_share_bps)
                .unwrap_or(0),
            bps,
            "the share did not take effect"
        );
    }

    /// Re-assert the collateral price and refresh, after time has passed.
    fn remark(&mut self) {
        let (cook, bcook) = (self.cook, self.bcook);
        let obligation = self.obligation;
        let instructions = {
            let mut ixs = self.env.accrue_all_ixs(&[&cook, &bcook]);
            ixs.push(self.env.refresh_obligation_ix(obligation));
            ixs
        };
        let admin = self.env.admin.insecure_clone();
        solana_kite::send_transaction_from_instructions(
            &mut self.env.svm,
            instructions,
            &[&admin],
            &admin.pubkey(),
        )
        .expect("refresh");
    }

    /// Shares held by the obligation, the liquidator and Aera.
    fn holdings(&mut self) -> (u64, u64, u64) {
        let vault = self
            .env
            .obligation_share_vault(&self.bcook, self.obligation);
        let liquidator = share_ata(&self.liquidator.pubkey(), &self.bcook.share_mint);
        let bcook = self.bcook;
        let protocol = self.env.protocol_collateral_dest(&bcook);
        (
            self.env.balance(&vault),
            self.env.balance(&liquidator),
            self.env.balance(&protocol),
        )
    }

    fn liquidate(&mut self, amount: u64) -> Result<(), String> {
        let (liquidator, cook, bcook) = (self.liquidator.insecure_clone(), self.cook, self.bcook);
        let obligation = self.obligation;
        self.env
            .try_liquidate(&liquidator, &cook, &bcook, obligation, amount)
    }
}

// ===========================================================================
// The borrower pays the same either way
// ===========================================================================

#[test]
fn gapd_00_the_borrower_loses_the_same_collateral_at_every_share() {
    /*
     * The claim the whole feature rests on.
     *
     * The same liquidation is run against a fresh market at each share, and the
     * collateral removed from the borrower must be identical every time. If this
     * fails, Aera is charging the borrower for its own revenue.
     */
    let mut penalties = Vec::new();
    for share in [0u16, 25, 50, 100, 150, 200, 250] {
        let mut book = Book::underwater(700);
        book.set_share(share);
        let (before, _, _) = book.holdings();
        book.liquidate(tokens(1_000)).expect("liquidation");
        let (after, _, _) = book.holdings();
        penalties.push((share, before - after));
    }

    let (_, first) = penalties[0];
    for (share, penalty) in &penalties {
        assert_eq!(
            *penalty, first,
            "a {share} bps protocol share changed the borrower's penalty from \
             {first} to {penalty} shares; the share must be carved OUT of the \
             bonus, never added to it. All measurements: {penalties:?}"
        );
    }
}

#[test]
fn gapd_01_the_split_conserves_every_share() {
    // Nothing appears, nothing vanishes, nothing is left unattributed in the
    // obligation's vault.
    for share in [0u16, 1, 150, 250] {
        let mut book = Book::underwater(700);
        book.set_share(share);
        let (vault_before, liquidator_before, protocol_before) = book.holdings();
        book.liquidate(tokens(1_000)).expect("liquidation");
        let (vault_after, liquidator_after, protocol_after) = book.holdings();

        let removed = vault_before - vault_after;
        let received = (liquidator_after - liquidator_before) + (protocol_after - protocol_before);
        assert_eq!(
            removed, received,
            "share {share}: {removed} shares left the borrower but {received} arrived"
        );
    }
}

#[test]
fn gapd_02_the_protocol_receives_its_configured_share_of_the_bonus() {
    /*
     * Not merely "something": the right amount.
     *
     * Aera's entitlement is `share / (BPS + total_bonus)` of the seizure, and
     * the split floors. Checked as a cross-multiplied inequality rather than by
     * recomputing the formula the program uses.
     */
    let mut book = Book::underwater(700);
    book.set_share(CANDIDATE_SHARE_BPS);
    let (vault_before, _, protocol_before) = book.holdings();
    book.liquidate(tokens(1_000)).expect("liquidation");
    let (vault_after, _, protocol_after) = book.holdings();

    let seized = (vault_before - vault_after) as u128;
    let protocol = (protocol_after - protocol_before) as u128;
    assert!(protocol > 0, "the protocol received nothing at 150 bps");

    let denominator = 10_000u128 + TOTAL_BONUS_BPS as u128;
    let share = CANDIDATE_SHARE_BPS as u128;
    assert!(
        protocol * denominator <= seized * share,
        "the protocol took more than its share: {protocol} of {seized}"
    );
    assert!(
        (protocol + 1) * denominator > seized * share,
        "the protocol took less than a floor of its share: {protocol} of {seized}"
    );
}

// ===========================================================================
// Zero share reproduces the old behaviour exactly
// ===========================================================================

#[test]
fn gapd_03_a_reserve_with_no_risk_config_pays_the_protocol_nothing() {
    // Core's state: no `RiskConfig` account exists at all. The handler must read
    // the uncreated PDA as a zero share and take the branch that never touches a
    // fee account.
    let mut book = Book::underwater(700);
    assert!(
        book.env.read_risk_config(&book.bcook).is_none(),
        "the fixture created a RiskConfig it should not have"
    );

    let (vault_before, liquidator_before, protocol_before) = book.holdings();
    book.liquidate(tokens(1_000)).expect("liquidation");
    let (vault_after, liquidator_after, protocol_after) = book.holdings();

    assert_eq!(
        protocol_after, protocol_before,
        "a reserve with no RiskConfig paid the protocol something"
    );
    assert_eq!(
        liquidator_after - liquidator_before,
        vault_before - vault_after,
        "the liquidator did not receive the entire seizure"
    );
}

#[test]
fn gapd_04_an_explicit_zero_share_is_identical_to_no_config() {
    let mut with_config = Book::underwater(700);
    with_config.set_share(0);
    let (vault_before, liquidator_before, _) = with_config.holdings();
    with_config.liquidate(tokens(1_000)).expect("liquidation");
    let (vault_after, liquidator_after, protocol_after) = with_config.holdings();

    assert_eq!(protocol_after, 0);
    assert_eq!(
        liquidator_after - liquidator_before,
        vault_before - vault_after
    );
}

// ===========================================================================
// The destination cannot be redirected
// ===========================================================================

/// Every one of these hands the instruction a protocol destination it must
/// refuse. A liquidator who could name the destination could keep Aera's share.
fn rejected_destination(destination: impl FnOnce(&mut Book) -> Pubkey, what: &str) {
    let mut book = Book::underwater(700);
    book.set_share(CANDIDATE_SHARE_BPS);
    let target = destination(&mut book);

    let (liquidator, cook, bcook) = (book.liquidator.insecure_clone(), book.cook, book.bcook);
    let obligation = book.obligation;
    let result = book.env.try_liquidate_to(
        &liquidator,
        &cook,
        &bcook,
        obligation,
        tokens(1_000),
        target,
    );
    assert!(
        result.is_err(),
        "a protocol destination that was {what} was accepted; the liquidator can \
         redirect Aera's share"
    );
}

#[test]
fn dest_00_the_liquidators_own_account_is_refused() {
    rejected_destination(
        |book| share_ata(&book.liquidator.pubkey(), &book.bcook.share_mint),
        "the liquidator's own share account",
    );
}

#[test]
fn dest_01_the_borrowers_account_is_refused() {
    rejected_destination(
        |book| {
            let borrower = book.borrower.insecure_clone();
            let mint = book.bcook.share_mint;
            book.env.ensure_share_ata(&borrower, mint)
        },
        "the borrower's share account",
    );
}

#[test]
fn dest_02_the_wrong_mint_is_refused() {
    // Owned by the fee destination, but for COOK's share mint rather than the
    // collateral's. Aera's cut is denominated in the asset being seized.
    rejected_destination(
        |book| {
            let owner = book.env.read_global().fee_destination;
            let mint = book.cook.share_mint;
            book.env.ensure_share_ata_for_owner(owner, mint)
        },
        "for the wrong mint",
    );
}

#[test]
fn dest_03_the_obligations_own_vault_is_refused() {
    // Aliasing the source and the destination. The transfer would be a no-op
    // and the borrower's collateral would silently stay put.
    rejected_destination(
        |book| {
            book.env
                .obligation_share_vault(&book.bcook, book.obligation)
        },
        "the obligation's own collateral vault",
    );
}

#[test]
fn dest_04_a_non_token_account_is_refused() {
    rejected_destination(
        |book| book.bcook.reserve,
        "a Reserve account rather than a token account",
    );
}

#[test]
fn dest_05_an_uninitialised_account_is_refused() {
    rejected_destination(|_| Pubkey::new_unique(), "an account that does not exist");
}

#[test]
fn dest_06_a_wrong_risk_config_is_refused() {
    // The share must come from the COLLATERAL reserve's config. Passing the
    // repay reserve's would let a liquidator choose which share applied.
    let mut book = Book::underwater(700);
    book.set_share(CANDIDATE_SHARE_BPS);

    let (liquidator, cook, bcook) = (book.liquidator.insecure_clone(), book.cook, book.bcook);
    let obligation = book.obligation;
    let destination = book.env.protocol_collateral_dest(&bcook);
    let result = book.env.try_liquidate_with_risk_config(
        &liquidator,
        &cook,
        &bcook,
        obligation,
        tokens(1_000),
        destination,
        risk_config_pda(cook.reserve),
    );
    assert!(
        result.is_err(),
        "the repay reserve's RiskConfig was accepted in place of the collateral's"
    );
}

// ===========================================================================
// Small liquidations
// ===========================================================================

#[test]
fn small_00_a_tiny_liquidation_pays_the_protocol_nothing_and_still_settles() {
    /*
     * Aera's share flooring to zero is correct, and must not become a reason to
     * refuse the liquidation. A minimum protocol fee would make the borrower's
     * effective penalty depend on the size of the close, which is exactly the
     * property this design refuses to give up.
     */
    let mut book = Book::underwater(700);
    book.set_share(CANDIDATE_SHARE_BPS);

    let (vault_before, liquidator_before, protocol_before) = book.holdings();
    // One base unit of COOK.
    book.liquidate(1)
        .expect("a one-unit liquidation must settle");
    let (vault_after, liquidator_after, protocol_after) = book.holdings();

    assert_eq!(
        protocol_after, protocol_before,
        "a one-unit liquidation paid the protocol something"
    );
    assert_eq!(
        liquidator_after - liquidator_before,
        vault_before - vault_after,
        "conservation broke on a liquidation too small to split"
    );
}

// ===========================================================================
// Configuration
// ===========================================================================

#[test]
fn cfg_00_a_share_above_the_hard_maximum_is_refused() {
    let mut book = Book::underwater(700);
    let bcook = book.bcook;
    assert!(
        book.env
            .set_protocol_liquidation_share(
                &bcook,
                aera::constants::MAX_PROTOCOL_LIQUIDATION_SHARE_BPS + 1,
            )
            .is_err(),
        "a share above MAX_PROTOCOL_LIQUIDATION_SHARE_BPS was accepted"
    );
    book.env
        .set_protocol_liquidation_share(&bcook, aera::constants::MAX_PROTOCOL_LIQUIDATION_SHARE_BPS)
        .expect("the ceiling itself must be reachable");
}

#[test]
fn cfg_01_a_share_above_the_reserves_own_bonus_is_refused() {
    // A share larger than the bonus would come out of the liquidator's
    // principal rather than out of the bonus.
    let (mut env, _cook, _bcook) = Env::core(1_000);
    let bcook = env.add_reserve(
        DECIMALS,
        px(1_000),
        ReserveConfig {
            liquidation_bonus_bps: 100,
            ..bcook_config()
        },
    );

    assert!(
        env.set_protocol_liquidation_share(&bcook, 150).is_err(),
        "a 150 bps share was accepted against a 100 bps bonus"
    );
    env.set_protocol_liquidation_share(&bcook, 100)
        .expect("a share equal to the bonus is the boundary and must be allowed");
}

#[test]
fn cfg_02_raising_the_share_waits_out_the_timelock() {
    let mut book = Book::underwater(700);
    let bcook = book.bcook;
    let share = |b: &Book| {
        b.env
            .read_risk_config(&bcook)
            .map(|c| c.protocol_liquidation_share_bps)
            .unwrap_or(0)
    };

    /*
     * Turning the share ON is a loosening.
     *
     * Zero means Aera takes nothing, which is the tightest setting the field
     * has, so every increase waits -- including the first one. That differs
     * from `per_wallet_borrow_cap`, where zero means *unlimited* and is
     * therefore the loosest. Two fields in one account meaning opposite things
     * by zero is worth stating plainly rather than discovering.
     */
    book.env
        .set_protocol_liquidation_share(&bcook, 100)
        .expect("queue the first share");
    assert_eq!(share(&book), 0, "enabling the share skipped the timelock");
    book.env.warp_seconds(60 * 60 * 24 + 1);
    book.env
        .apply_pending_risk_config(&bcook)
        .expect("apply the first share");
    assert_eq!(share(&book), 100);

    // 100 -> 50 is a tightening: less for Aera, more for the liquidator.
    book.env
        .set_protocol_liquidation_share(&bcook, 50)
        .expect("lowering");
    assert_eq!(
        share(&book),
        50,
        "lowering Aera's cut should land immediately"
    );

    // 50 -> 200 is a loosening and must queue.
    book.env
        .set_protocol_liquidation_share(&bcook, 200)
        .expect("raising");
    assert_eq!(
        share(&book),
        50,
        "raising Aera's cut took effect without the timelock"
    );

    assert!(
        book.env.apply_pending_risk_config(&bcook).is_err(),
        "the pending raise applied before its eta"
    );
    book.env.warp_seconds(60 * 60 * 24 + 1);
    book.env
        .apply_pending_risk_config(&bcook)
        .expect("apply after the delay");
    assert_eq!(share(&book), 200);
}
