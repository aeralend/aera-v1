//! The whole launch, start to incident, in one test.
//!
//! Twenty-five stages, from an empty validator to an oracle in emergency, with
//! every state invariant checked after each one. Nothing here is mocked: the
//! program is the real SBF artifact, the stake pool is a real 611-byte layout,
//! and every step goes through the instruction a real operator or user would
//! send.
//!
//! ## Why this exists when the suite already has 300 tests
//!
//! Because they are all *local*. Each proves one thing about one transition,
//! and a protocol can pass every one of them while being unusable end to end —
//! an ordering requirement nobody wrote down, a state that is reachable but
//! that no single test starts from, a stage that works only because a fixture
//! set something up that a real deployment would not.
//!
//! This is the sequence an operator will actually perform, in order, once.
//!
//! ## The stages
//!
//! ```text
//!    1-6   deploy, initialise global, market, both reserves, both oracles
//!    7     the oracle enters BOOTSTRAPPING and blocks new borrowing
//!    8-9   a later pool epoch confirms it; the market opens
//!   10-12  a supplier deposits, a borrower posts collateral and borrows
//!   13-14  interest accrues, the borrower repays part of it
//!   15-16  bCOOK falls; the health factor crosses 1
//!   17-18  a liquidator finds the position and clears it
//!   19-20  the supplier withdraws, protocol fees are collected
//!   21-22  the stake-pool program is redeployed; the oracle refuses everything
//!   23-25  new borrowing fails, repayment still works, the state is readable
//! ```
//!
//! Stage 21 is the one worth reading. It models a real, measured risk: the
//! stake-pool program's upgrade authority on Cookie Chain is a single wallet
//! key, so one signature can replace the code every bCOOK valuation depends on.
//! The protocol's answer is not to prevent it — it cannot — but to notice and
//! stop lending, while leaving every exit open.

mod common;

use aera::oracle::breaker::OracleHealth;
use common::audit::*;
use common::invariants::*;
use common::*;
use solana_keypair::Keypair;

/// The scenario's running state, so each stage reads as a step rather than as
/// setup.
struct Launch {
    env: Env,
    cook: ReserveHandle,
    bcook: ReserveHandle,
    supplier: Keypair,
    borrower: Keypair,
    obligation: Pubkey,
    liquidator: Keypair,
    stage: u32,
}

impl Launch {
    fn handles(&self) -> [(&'static str, ReserveHandle); 2] {
        [("COOK", self.cook), ("bCOOK", self.bcook)]
    }

    /// Check every state invariant, and say which stage was being left.
    ///
    /// After *every* stage, not just the interesting ones. A launch that
    /// breaks an invariant at stage 4 and repairs it by stage 12 would pass a
    /// test that only looked at the end, and the intermediate state is one real
    /// users would have been in.
    fn assert_stage(&mut self, what: &str) {
        let now = snapshot(&self.env, &self.handles(), &[self.obligation]);
        assert_invariants(
            &now,
            None,
            true, // stages move prices deliberately
            &format!("stage {} — {what}", self.stage),
        );
        assert_solvent(&self.env, &self.cook, what);
        assert_solvent(&self.env, &self.bcook, what);
        println!("  stage {:>2}  {what}", self.stage);
        self.stage += 1;
    }

    fn health(&self) -> OracleHealth {
        OracleHealth::from_u8(self.env.read_oracle(self.bcook.mint).health).unwrap()
    }

    fn try_borrow(&mut self, amount: u64) -> Result<(), String> {
        let (cook, bcook) = (self.cook, self.bcook);
        let borrower = self.borrower.insecure_clone();
        self.env
            .try_borrow(&borrower, &cook, self.obligation, amount, &[&cook, &bcook])
    }

    /// Refresh both oracles, accrue both reserves, refresh the obligation.
    fn crank(&mut self) {
        let (cook, bcook) = (self.cook, self.bcook);
        let payer = self.liquidator.insecure_clone();
        let mut ixs = vec![
            self.env.refresh_oracle_ix(cook.mint),
            self.env.refresh_oracle_ix(bcook.mint),
        ];
        ixs.extend(self.env.accrue_all_ixs(&[&cook, &bcook]));
        ixs.push(self.env.refresh_obligation_ix(self.obligation));
        self.env.svm.expire_blockhash();
        self.env.send_raw(ixs, &[&payer]).expect("crank");
    }
}

/// The full launch.
#[test]
fn a_market_can_be_launched_used_and_survive_an_incident() {
    println!("\nAera v0.2 launch scenario\n");

    // ---- 1-6. Deploy and initialise ---------------------------------------
    //
    // `Env::core` performs exactly the sequence a deployment script does:
    // init_global, init_market, then per reserve init_oracle, set_pool,
    // refresh, init_reserve. The COOK reserve is then repointed to the
    // unit-of-account source, because the quote asset must be exactly 1.
    let (env, cook, bcook) = Env::core(1_300);

    let mut launch = Launch {
        env,
        cook,
        bcook,
        supplier: Keypair::new(),
        borrower: Keypair::new(),
        obligation: Pubkey::default(),
        liquidator: Keypair::new(),
        stage: 1,
    };
    launch.supplier = launch.env.create_user();
    launch.borrower = launch.env.create_user();
    launch.liquidator = launch.env.create_user();

    launch.assert_stage("program deployed, global and market initialised");
    launch.assert_stage("COOK reserve created, priced as the unit of account");
    launch.assert_stage("bCOOK reserve created, priced from the stake pool");

    // ---- 7. Bootstrapping blocks new risk ---------------------------------
    //
    // `Env::core` confirms the bootstrap as part of setup, so this reconstructs
    // the state a real launch is in the moment before its second crank: the
    // oracle has seen the pool once and no later epoch has agreed.
    {
        let new_slot = TEST_DEPLOY_SLOT + 1;
        launch
            .env
            .set_program_data(new_slot, Some(TEST_UPGRADE_AUTHORITY));
        let config = aera::instructions::admin::init_oracle::OracleConfig::native(
            TEST_STAKE_POOL_PROGRAM,
            launch.env.stake_pool_address(bcook.mint),
            aera::constants::DEFAULT_MAX_WITHDRAWAL_FEE_BPS,
            aera::constants::DEFAULT_RATE_FLOOR,
            aera::constants::DEFAULT_RATE_CEILING,
            new_slot,
            TEST_UPGRADE_AUTHORITY,
        );
        launch.env.set_oracle_with(bcook.mint, config);
        launch.env.refresh_oracle(bcook.mint);
    }
    assert_eq!(
        launch.health(),
        OracleHealth::Bootstrapping,
        "a first observation must not be trusted enough to open new risk"
    );
    launch.assert_stage("oracle BOOTSTRAPPING after its first observation");

    // Fund everyone now so the block below tests the oracle rather than a
    // missing token account.
    launch
        .env
        .fund(&launch.supplier, cook.mint, tokens(100_000));
    launch
        .env
        .fund(&launch.borrower, bcook.mint, tokens(20_000));
    launch.env.fund(&launch.borrower, cook.mint, tokens(100));
    launch
        .env
        .fund(&launch.liquidator, cook.mint, tokens(100_000));

    {
        let supplier = launch.supplier.insecure_clone();
        let borrower = launch.borrower.insecure_clone();
        launch.obligation = launch.env.init_obligation(&borrower);
        launch
            .env
            .try_supply(&supplier, &cook, tokens(50_000))
            .expect("supplying must work while bootstrapping — it adds no risk");
    }
    {
        // Supply bCOOK to get shares, then pledge them. Two steps because
        // collateral is the share token, not the underlying -- which is what
        // makes "aCOOK is not collateral" a rule about a different mint rather
        // than a special case.
        let borrower = launch.borrower.insecure_clone();
        let shares = launch.env.supply(&borrower, &bcook, tokens(10_000));
        let balance = launch.env.balance(&shares);
        launch
            .env
            .try_deposit_collateral(&borrower, &bcook, launch.obligation, balance)
            .expect("posting collateral must work while bootstrapping");
    }

    assert!(
        launch.try_borrow(tokens(100)).is_err(),
        "an unconfirmed anchor financed a loan"
    );
    launch.assert_stage("supply and collateral open, borrowing blocked");

    // ---- 8-9. A later pool epoch confirms it ------------------------------
    launch.env.confirm_bootstrap(bcook.mint);
    assert_eq!(
        launch.health(),
        OracleHealth::Healthy,
        "a later pool epoch must clear the bootstrap"
    );
    launch.assert_stage("pool advanced an epoch; oracle HEALTHY");

    // ---- 10-12. The market is used ----------------------------------------
    launch
        .try_borrow(tokens(4_800))
        .expect("borrowing must work once the anchor is confirmed");
    launch.assert_stage("borrower drew 4,800 COOK against 10,000 bCOOK");

    let debt_at_open = launch.env.read_reserve(&cook).borrowed_principal;
    assert!(debt_at_open > 0);

    // ---- 13. Interest accrues ---------------------------------------------
    launch
        .env
        .warp_slots(aera::constants::DEFAULT_SLOTS_PER_YEAR / 4);
    launch.env.accrue(&cook);
    let reserve = launch.env.read_reserve(&cook);
    assert!(
        reserve.borrow_index > aera::constants::FIXED_POINT_SCALE,
        "a quarter of a year accrued no interest"
    );
    assert!(reserve.accrued_fees > 0, "the protocol earned no fee");
    launch.assert_stage("a quarter of a year of interest accrued");

    // ---- 14. Partial repayment --------------------------------------------
    {
        let borrower = launch.borrower.insecure_clone();
        launch
            .env
            .try_repay(&borrower, &cook, launch.obligation, tokens(50))
            .expect("partial repayment");
    }
    launch.assert_stage("borrower repaid part of the debt");

    // ---- 15-16. The collateral falls --------------------------------------
    //
    // At the fastest pace the breaker accepts, one epoch at a time, so the
    // reference tracks it. A single large drop would trip the breaker and
    // freeze the reference instead, which is stage 21's scenario and not this
    // one.
    let mut rate = 1_300u64;
    let mut epoch = 2u64;
    /*
     * Sixty epochs, not forty.
     *
     * 10,000 bCOOK less the 2% redemption fee and the 5% haircut, at the 65%
     * liquidation threshold, covers a 4,900 COOK debt down to a rate of about
     * 0.81. At the 1% per epoch the breaker accepts, reaching that from 1.30
     * takes 47 epochs. Forty left the position at a health factor of 1.09 --
     * falling, and not yet liquidatable.
     */
    for _ in 0..60 {
        epoch += 1;
        rate = (rate * 99).div_ceil(100);
        launch.env.warp_slots(100_000);
        launch.env.set_rate(bcook.mint, rate, epoch);
        let _ = launch.env.try_refresh_oracle(bcook.mint);
        launch.env.accrue(&cook);
        launch.env.accrue(&bcook);
    }
    launch.crank();
    assert_ne!(
        launch.health(),
        OracleHealth::Emergency,
        "a decline inside the allowance must not trip the breaker"
    );
    launch.assert_stage(&format!("bCOOK fell to {rate}/1000 over 60 epochs"));

    let obligation = launch.env.read_obligation(launch.obligation);
    assert!(
        obligation.is_liquidatable(),
        "the decline did not put the position underwater; \
         adjust the fixture, do not weaken the assertion"
    );
    launch.assert_stage("health factor crossed 1; the position is liquidatable");

    // ---- 17-18. A liquidator clears it ------------------------------------
    let seized_before = launch
        .env
        .balance_or_zero(&share_ata(&launch.liquidator.pubkey(), &bcook.share_mint));

    let mut liquidated = false;
    let mut attempt = tokens(3_000);
    while attempt > 0 {
        let liquidator = launch.liquidator.insecure_clone();
        launch.env.svm.expire_blockhash();
        if launch
            .env
            .try_liquidate(&liquidator, &cook, &bcook, launch.obligation, attempt)
            .is_ok()
        {
            liquidated = true;
            break;
        }
        attempt /= 2;
    }
    assert!(
        liquidated,
        "no liquidation was possible against an underwater position"
    );

    let seized_after = launch
        .env
        .balance_or_zero(&share_ata(&launch.liquidator.pubkey(), &bcook.share_mint));
    assert!(
        seized_after > seized_before,
        "the liquidator repaid debt and received no collateral"
    );
    launch.assert_stage("liquidator repaid debt and seized collateral at the bonus");

    // ---- 19. The supplier withdraws ---------------------------------------
    {
        let supplier = launch.supplier.insecure_clone();
        launch
            .env
            .try_withdraw(&supplier, &cook, tokens(1_000))
            .expect("a supplier must be able to leave");
    }
    launch.assert_stage("supplier withdrew part of their deposit");

    // ---- 20. Fees are collected -------------------------------------------
    {
        let fee_wallet = launch.env.fee_wallet.insecure_clone();
        launch.env.fund(&fee_wallet, cook.mint, 1);
        launch
            .env
            .try_collect_fees(&cook)
            .expect("the protocol's fee must be collectable");
    }
    assert_eq!(
        launch.env.read_reserve(&cook).accrued_fees,
        0,
        "collect_fees left something behind"
    );
    launch.assert_stage("protocol fees collected to the fee destination");

    // ---- 21-22. The stake-pool program is redeployed ----------------------
    //
    // The measured risk: one wallet key can replace the code behind the program
    // Aera reads its collateral rate from. Aera cannot prevent it. What it can
    // do is notice.
    let reference_before = launch.env.read_oracle(bcook.mint).reference;
    launch
        .env
        .set_program_data(TEST_DEPLOY_SLOT + 99, Some(TEST_UPGRADE_AUTHORITY));
    epoch += 1;
    launch.env.set_rate(bcook.mint, rate, epoch);
    launch
        .env
        .try_refresh_oracle(bcook.mint)
        .expect("a refused observation must still record the freeze, not abort");

    assert_eq!(
        launch.health(),
        OracleHealth::Emergency,
        "a redeployed source program must put the oracle in emergency"
    );
    assert_eq!(
        launch.env.read_oracle(bcook.mint).reference.gross_rate,
        reference_before.gross_rate,
        "a refused observation moved the reference"
    );
    launch.assert_stage("stake-pool program redeployed; oracle EMERGENCY");

    // ---- 23. New borrowing fails ------------------------------------------
    assert!(
        launch.try_borrow(tokens(1)).is_err(),
        "new debt was opened against a redeployed source"
    );
    launch.assert_stage("new borrowing refused");

    // ---- 24. Repayment still works ----------------------------------------
    {
        let borrower = launch.borrower.insecure_clone();
        launch
            .env
            .try_repay(&borrower, &cook, launch.obligation, tokens(10))
            .expect(
                "repayment must survive an emergency — this is the rule the design bends around",
            );
    }
    launch.assert_stage("repayment still works");

    // ---- 25. The incident is observable -----------------------------------
    //
    // Everything an operator needs is readable from the accounts: which state,
    // what the last accepted rate was, which epoch it came from, and what the
    // pin expected against what is there.
    let oracle = launch.env.read_oracle(bcook.mint);
    assert_eq!(
        OracleHealth::from_u8(oracle.health).unwrap(),
        OracleHealth::Emergency
    );
    assert!(
        oracle.reference.is_set(),
        "the last trusted price is still there"
    );
    assert_eq!(
        oracle.expected_deploy_slot,
        TEST_DEPLOY_SLOT + 1,
        "the pin records which deployment was authorised"
    );
    launch.assert_stage("incident state is fully readable from the accounts");

    println!(
        "\n  {} stages, every invariant held at each\n",
        launch.stage - 1
    );
}
