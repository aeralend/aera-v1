//! What a market can and cannot do while the migration is only part done.
//!
//! The migration is not one transaction. `Global` is stamped by one
//! instruction, each reserve is repointed by another, and between any two of
//! them the protocol is in a state no version of the code was designed for:
//! v0.1 accounts and v0.2 logic, side by side.
//!
//! An operator can also be interrupted -- a failed transaction, a dropped
//! connection, a second thought -- and leave the protocol there for hours.
//! Every state below is therefore reachable in practice, and the question for
//! each is not "does it work" but "can anyone lose money in it".
//!
//! The rule this suite holds the protocol to:
//!
//! > In every partially migrated state, no action that increases anyone's risk
//! > may succeed, and every action that reduces it must.
//!
//! Where that already follows from something structural, the test says so and
//! pins the structure, because a safety property that holds by accident stops
//! holding the moment somebody edits the thing it was an accident of.

mod common;

use aera::instructions::admin::init_oracle::OracleConfig;
use aera::state::{Global, OracleState};
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::system_program;
use anchor_lang::{AccountDeserialize, Discriminator, InstructionData, Space, ToAccountMetas};
use common::v0_1;
use common::*;
use solana_keypair::Keypair;

// ===========================================================================
// A v0.1 market, as `test_migration.rs` builds one
// ===========================================================================

struct V1Market {
    env: Env,
    cook: ReserveHandle,
    bcook: ReserveHandle,
}

fn price_feed_pda(market: Pubkey, mint: Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"price_feed", market.as_ref(), mint.as_ref()],
        &aera::id(),
    )
    .0
}

fn oracle_pda(market: Pubkey, mint: Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"oracle", market.as_ref(), mint.as_ref()], &aera::id()).0
}

fn v1_set_up_feed(env: &mut Env, mint: Pubkey, mantissa: i128) {
    let admin = env.admin.insecure_clone();
    let feed = price_feed_pda(env.market, mint);
    let guardians: Vec<Pubkey> = env.guardians.iter().map(|g| g.pubkey()).collect();

    env.send_raw(
        vec![v0_1::set_oracle_ix(
            aera::id(),
            env.global,
            env.market,
            mint,
            feed,
            admin.pubkey(),
            &guardians,
            3,
            120,
            2_500,
        )],
        &[&admin],
    )
    .expect("v0.1 set_oracle");

    for index in 0..3 {
        let guardian = env.guardians[index].insecure_clone();
        env.send_raw(
            vec![v0_1::publish_price_ix(
                aera::id(),
                feed,
                guardian.pubkey(),
                mantissa,
                -18,
            )],
            &[&guardian],
        )
        .expect("v0.1 publish_price");
    }
}

fn v1_add_reserve(
    env: &mut Env,
    mantissa: i128,
    config: aera::state::ReserveConfig,
) -> ReserveHandle {
    let admin = env.admin.insecure_clone();
    let mint = solana_kite::create_token_mint(&mut env.svm, &admin, DECIMALS, None).unwrap();

    v1_set_up_feed(env, mint, mantissa);

    let reserve = Pubkey::find_program_address(
        &[b"reserve", env.market.as_ref(), mint.as_ref()],
        &aera::id(),
    )
    .0;
    let share_mint =
        Pubkey::find_program_address(&[b"share_mint", reserve.as_ref()], &aera::id()).0;
    let liquidity_vault =
        Pubkey::find_program_address(&[b"liquidity_vault", reserve.as_ref()], &aera::id()).0;
    let feed = price_feed_pda(env.market, mint);

    let instruction = Instruction {
        program_id: aera::id(),
        accounts: aera::accounts::InitReserve {
            global: env.global,
            admin: admin.pubkey(),
            market: env.market,
            reserve,
            liquidity_mint: mint,
            liquidity_vault,
            share_mint,
            oracle: feed,
            liquidity_token_program: TOKEN_PROGRAM_ID,
            share_token_program: TOKEN_2022_PROGRAM_ID,
            system_program: system_program::id(),
        }
        .to_account_metas(None),
        data: aera::instruction::InitReserve {
            config,
            metadata: aera::instructions::admin::init_reserve::ShareMetadata {
                name: "Aera Share".to_string(),
                symbol: "aSHARE".to_string(),
                uri: "https://aera.io/acook.json".to_string(),
            },
        }
        .data(),
    };
    env.send_raw(vec![instruction], &[&admin])
        .expect("v0.1 init_reserve");

    ReserveHandle {
        mint,
        decimals: DECIMALS,
        reserve,
        share_mint,
        liquidity_vault,
        oracle: feed,
    }
}

/// A v0.1 market with a supplier and a borrower already in it.
fn live_v1_market() -> (V1Market, Keypair, Pubkey) {
    let mut env = Env::with_program(V0_1_PROGRAM);
    let cook = v1_add_reserve(&mut env, px(1_000), cook_config());
    let bcook = v1_add_reserve(&mut env, px(1_300), bcook_config());
    let mut market = V1Market { env, cook, bcook };

    let supplier = market.env.create_user();
    market.env.fund(&supplier, market.cook.mint, tokens(50_000));
    let cook_handle = market.cook;
    market.env.supply(&supplier, &cook_handle, tokens(50_000));

    let borrower = market.env.create_user();
    market
        .env
        .fund(&borrower, market.bcook.mint, tokens(11_000));
    market.env.fund(&borrower, market.cook.mint, tokens(1_000));
    let bcook_handle = market.bcook;
    let obligation = market
        .env
        .open_position(&borrower, &bcook_handle, tokens(10_000));
    market.env.supply(&borrower, &bcook_handle, tokens(1_000));
    market
        .env
        .try_borrow(
            &borrower,
            &cook_handle,
            obligation,
            tokens(2_000),
            &[&cook_handle, &bcook_handle],
        )
        .expect("v0.1 borrow");

    (market, borrower, obligation)
}

// ===========================================================================
// The migration steps, individually
// ===========================================================================

fn migrate_reserve_ix(env: &Env, handle: &ReserveHandle, config: OracleConfig) -> Instruction {
    let mut accounts = aera::accounts::MigrateReserveToV2 {
        global: env.global,
        market: env.market,
        reserve: handle.reserve,
        legacy_price_feed: price_feed_pda(env.market, handle.mint),
        liquidity_mint: handle.mint,
        oracle: oracle_pda(env.market, handle.mint),
        admin: env.admin.pubkey(),
        system_program: system_program::id(),
    }
    .to_account_metas(None);

    if config.source_kind == 1 {
        accounts.push(AccountMeta::new_readonly(config.source_account, false));
        accounts.push(AccountMeta::new_readonly(
            env.stake_pool_program_data(),
            false,
        ));
    }

    Instruction {
        program_id: aera::id(),
        accounts,
        data: aera::instruction::MigrateReserveToV2 { config }.data(),
    }
}

fn migrate_global_ix(env: &Env) -> Instruction {
    Instruction {
        program_id: aera::id(),
        accounts: aera::accounts::MigrateGlobalToV2 {
            global: env.global,
            admin: env.admin.pubkey(),
            system_program: system_program::id(),
        }
        .to_account_metas(None),
        data: aera::instruction::MigrateGlobalToV2 {}.data(),
    }
}

fn bcook_oracle_config(env: &Env, bcook: &ReserveHandle) -> OracleConfig {
    OracleConfig::native(
        TEST_STAKE_POOL_PROGRAM,
        env.stake_pool_address(bcook.mint),
        aera::constants::DEFAULT_MAX_WITHDRAWAL_FEE_BPS,
        aera::constants::DEFAULT_RATE_FLOOR,
        aera::constants::DEFAULT_RATE_CEILING,
        TEST_DEPLOY_SLOT,
        TEST_UPGRADE_AUTHORITY,
    )
}

/// Swap in v0.2 and place the stake pool the bCOOK oracle will read.
fn upgrade(market: &mut V1Market) {
    market.env.upgrade_program(V0_2_PROGRAM);
    let mint = market.bcook.mint;
    market.env.set_pool(
        mint,
        px(1_300) as u64,
        POOL_SHARES,
        LIVE_WITHDRAWAL_FEE_BPS,
        1,
    );
}

fn migrate_cook(market: &mut V1Market) {
    let admin = market.env.admin.insecure_clone();
    let ix = migrate_reserve_ix(&market.env, &market.cook, OracleConfig::unit_of_account());
    market
        .env
        .send_raw(vec![ix], &[&admin])
        .expect("COOK migration");
    // The reserve now points at a v0.2 oracle, so the handle must too -- a
    // client that kept using the old feed address is exactly what the next
    // instruction would refuse.
    market.cook.oracle = oracle_pda(market.env.market, market.cook.mint);
}

fn migrate_bcook(market: &mut V1Market) {
    let admin = market.env.admin.insecure_clone();
    let config = bcook_oracle_config(&market.env, &market.bcook);
    let ix = migrate_reserve_ix(&market.env, &market.bcook, config);
    market
        .env
        .send_raw(vec![ix], &[&admin])
        .expect("bCOOK migration");
    market.bcook.oracle = oracle_pda(market.env.market, market.bcook.mint);
}

fn migrate_global(market: &mut V1Market) {
    let admin = market.env.admin.insecure_clone();
    let ix = migrate_global_ix(&market.env);
    market
        .env
        .send_raw(vec![ix], &[&admin])
        .expect("Global migration");
}

/// Try to borrow. Used for its verdict, not its effect.
fn try_borrow(market: &mut V1Market, borrower: &Keypair, obligation: Pubkey) -> Result<(), String> {
    let cook = market.cook;
    let bcook = market.bcook;
    market
        .env
        .try_borrow(borrower, &cook, obligation, tokens(100), &[&cook, &bcook])
}

// ===========================================================================
// The states, one at a time
// ===========================================================================

/// Nothing migrated: v0.2 code, entirely v0.1 accounts.
///
/// The first state the protocol is in after `solana program deploy` returns,
/// and the one it stays in for as long as the operator takes to send the next
/// transaction.
#[test]
fn half_00_nothing_migrated_cannot_borrow() {
    let (mut market, borrower, obligation) = live_v1_market();
    upgrade(&mut market);

    let result = try_borrow(&mut market, &borrower, obligation);
    assert!(
        result.is_err(),
        "v0.2 opened new debt against entirely v0.1 state"
    );
}

/// Nothing migrated: the borrower can still get out.
#[test]
fn half_01_nothing_migrated_can_still_repay() {
    let (mut market, borrower, obligation) = live_v1_market();
    upgrade(&mut market);

    let cook = market.cook;
    market
        .env
        .try_repay(&borrower, &cook, obligation, tokens(500))
        .expect("an unmigrated market must not trap a borrower");
}

/// Global stamped, reserves not: still no new debt.
///
/// The tempting order, because `migrate_global` is the instruction with no
/// arguments to get wrong. It must not be the one that unlocks the market.
#[test]
fn half_02_global_only_cannot_borrow() {
    let (mut market, borrower, obligation) = live_v1_market();
    upgrade(&mut market);
    migrate_global(&mut market);

    assert_eq!(
        market.env.read_global().version,
        2,
        "the fixture did not actually stamp the version"
    );
    let result = try_borrow(&mut market, &borrower, obligation);
    assert!(
        result.is_err(),
        "stamping a version byte was enough to open the market"
    );
}

/// The debt reserve migrated, the collateral reserve not.
///
/// The most dangerous shape on paper: the asset being borrowed has a working
/// v0.2 oracle, and the asset backing it is still priced by a guardian feed
/// that v0.2 has no code to read.
#[test]
fn half_03_debt_migrated_collateral_not_cannot_borrow() {
    let (mut market, borrower, obligation) = live_v1_market();
    upgrade(&mut market);
    migrate_global(&mut market);
    migrate_cook(&mut market);

    let result = try_borrow(&mut market, &borrower, obligation);
    assert!(
        result.is_err(),
        "borrowed against collateral v0.2 cannot price"
    );
}

/// The collateral reserve migrated, the debt reserve not.
#[test]
fn half_04_collateral_migrated_debt_not_cannot_borrow() {
    let (mut market, borrower, obligation) = live_v1_market();
    upgrade(&mut market);
    migrate_global(&mut market);
    migrate_bcook(&mut market);

    let result = try_borrow(&mut market, &borrower, obligation);
    assert!(result.is_err(), "borrowed a liability v0.2 cannot price");
}

/// Collateral cannot leave a half-migrated market either.
///
/// Withdrawing collateral is the other action that increases risk, and it is
/// the one an attacker would prefer: it takes value out rather than putting
/// debt in.
#[test]
fn half_05_collateral_cannot_be_withdrawn_half_migrated() {
    let (mut market, borrower, obligation) = live_v1_market();
    upgrade(&mut market);
    migrate_global(&mut market);
    migrate_cook(&mut market);

    let cook = market.cook;
    let bcook = market.bcook;
    let result = market.env.try_withdraw_collateral(
        &borrower,
        &bcook,
        obligation,
        tokens(1),
        &[&cook, &bcook],
    );
    assert!(
        result.is_err(),
        "collateral left a market whose collateral price v0.2 cannot read"
    );
}

/// Every partial state leaves repayment open.
///
/// Run across all four orderings, because the freeze is only defensible if the
/// exit is open in each one -- and "we tested the one we expected" is how the
/// other three get missed.
#[test]
fn half_06_every_partial_state_can_be_repaid() {
    type Step = fn(&mut V1Market);
    let orders: [(&str, &[Step]); 4] = [
        ("nothing", &[]),
        ("global only", &[migrate_global]),
        ("global, then COOK", &[migrate_global, migrate_cook]),
        ("global, then bCOOK", &[migrate_global, migrate_bcook]),
    ];

    for (name, steps) in orders {
        let (mut market, borrower, obligation) = live_v1_market();
        upgrade(&mut market);
        for step in steps {
            step(&mut market);
        }

        let cook = market.cook;
        market
            .env
            .try_repay(&borrower, &cook, obligation, tokens(100))
            .unwrap_or_else(|error| panic!("repayment blocked at [{name}]: {error}"));
    }
}

/// Reserves may be migrated before `Global`, and the result is the same.
///
/// Order independence matters because an operator under pressure will not
/// necessarily follow the runbook, and a migration that is only safe in one
/// order is a migration with an undocumented precondition.
#[test]
fn half_07_the_migration_is_order_independent() {
    let (mut market, borrower, obligation) = live_v1_market();
    upgrade(&mut market);

    // Reserves first, Global last -- the opposite of the documented order.
    migrate_cook(&mut market);
    migrate_bcook(&mut market);
    migrate_global(&mut market);
    market.env.confirm_bootstrap(market.bcook.mint);

    let global = market.env.read_global();
    assert_eq!(global.version, 2);

    try_borrow(&mut market, &borrower, obligation)
        .expect("a fully migrated market must work whatever order it got there in");
}

/// A migrated reserve points at an `OracleState`, and the legacy feed is gone.
#[test]
fn half_08_migration_repoints_the_reserve_and_closes_the_feed() {
    let (mut market, _borrower, _obligation) = live_v1_market();
    upgrade(&mut market);
    migrate_cook(&mut market);

    assert_eq!(
        market.env.reserve_oracle(market.cook.reserve),
        oracle_pda(market.env.market, market.cook.mint),
        "the reserve still points at its guardian feed"
    );

    let feed = price_feed_pda(market.env.market, market.cook.mint);
    let closed = market
        .env
        .svm
        .get_account(&feed)
        .map(|a| a.data.is_empty())
        .unwrap_or(true);
    assert!(closed, "the guardian feed outlived the migration");

    // And the untouched reserve still points at its own feed, so the migration
    // is genuinely per-reserve rather than protocol-wide.
    // Read by offset, because this reserve has not been migrated and is
    // therefore still sixteen bytes short of the v0.2 struct.
    assert_eq!(
        market.env.reserve_oracle(market.bcook.reserve),
        price_feed_pda(market.env.market, market.bcook.mint),
        "an unmigrated reserve was repointed by somebody else's migration"
    );
}

/// A v0.1 `PriceFeed` can never be read as a v0.2 `OracleState`.
///
/// This is the structural fact the states above rest on, so it is pinned
/// directly rather than inferred from the fact that borrowing failed. Anchor
/// prefixes every account with eight bytes of `sha256("account:<Name>")`, and
/// the two names differ -- so an unmigrated reserve's oracle slot cannot be
/// deserialised into an oracle at all, whatever the bytes behind it say.
///
/// If this ever stopped being true, every "cannot borrow" test above would
/// start passing for the wrong reason.
#[test]
fn half_09_a_guardian_feed_cannot_be_read_as_an_oracle() {
    let (market, _borrower, _obligation) = live_v1_market();

    let feed = price_feed_pda(market.env.market, market.bcook.mint);
    let account = market
        .env
        .svm
        .get_account(&feed)
        .expect("the v0.1 feed should exist");

    assert_ne!(
        &account.data[..8],
        OracleState::DISCRIMINATOR,
        "a v0.1 PriceFeed and a v0.2 OracleState share a discriminator"
    );
    assert!(
        OracleState::try_deserialize(&mut &account.data[..]).is_err(),
        "a guardian feed deserialised as an oracle"
    );
}

/// `Global` cannot be migrated twice, so the version cannot be walked back.
#[test]
fn half_10_global_cannot_be_migrated_twice() {
    let (mut market, _borrower, _obligation) = live_v1_market();
    upgrade(&mut market);
    migrate_global(&mut market);

    let admin = market.env.admin.insecure_clone();
    let result = market
        .env
        .send_raw(vec![migrate_global_ix(&market.env)], &[&admin]);
    assert!(
        result.is_err(),
        "Global was migrated twice; a second realloc would corrupt it"
    );
}

/// A v0.1 `Global` is one byte short, and v0.2 refuses to read it.
///
/// Recorded because several tests above depend on it. The `version` byte was
/// appended last precisely so that a v0.1 account is exactly one byte short of
/// the v0.2 struct, which makes every typed read of an unmigrated `Global`
/// fail rather than succeed with a garbage tail.
#[test]
fn half_11_an_unmigrated_global_is_one_byte_short() {
    let (market, _borrower, _obligation) = live_v1_market();

    let account = market.env.svm.get_account(&market.env.global).unwrap();
    assert_eq!(
        account.data.len(),
        aera::state::GLOBAL_V1_LEN,
        "the v0.1 Global is not the size the migration assumes"
    );
    assert_eq!(
        account.data.len() + 1,
        8 + Global::INIT_SPACE,
        "v0.2 must be exactly one byte longer, or the realloc is wrong"
    );
    assert!(
        Global::try_deserialize(&mut &account.data[..]).is_err(),
        "a v0.1 Global deserialised as v0.2 -- the version gate is not a gate"
    );
}

/// Supplying stays open in every partial state.
///
/// A supplier adding liquidity cannot make anybody's position worse, and
/// stopping them would strand a market that an operator is halfway through
/// fixing. It also has to keep working, because a market that cannot take
/// deposits during a migration is a market with an outage.
#[test]
fn half_12_supplying_stays_open_while_half_migrated() {
    let (mut market, _borrower, _obligation) = live_v1_market();
    upgrade(&mut market);
    migrate_global(&mut market);
    migrate_cook(&mut market);

    let supplier = market.env.create_user();
    market.env.fund(&supplier, market.cook.mint, tokens(100));
    let cook = market.cook;
    market
        .env
        .try_supply(&supplier, &cook, tokens(100))
        .expect("supplying must survive a half-migrated market");
}

/// The `Global` gate works on its own, with every oracle already migrated.
///
/// Every "cannot borrow" test above has two reasons to fail: the reserve still
/// points at a guardian feed *and* `Global` is a byte short. A test with two
/// reasons proves neither, so this one removes the oracle reason entirely --
/// both reserves fully migrated, only the version byte outstanding -- and shows
/// the size gate stops the protocol by itself.
#[test]
fn half_13_the_global_gate_holds_with_both_oracles_migrated() {
    let (mut market, borrower, obligation) = live_v1_market();
    upgrade(&mut market);
    migrate_cook(&mut market);
    migrate_bcook(&mut market);
    market.env.confirm_bootstrap(market.bcook.mint);

    // Both oracles are healthy. Only Global is still v0.1.
    assert!(
        Global::try_deserialize(
            &mut &market.env.svm.get_account(&market.env.global).unwrap().data[..]
        )
        .is_err(),
        "the fixture migrated Global after all"
    );

    let result = try_borrow(&mut market, &borrower, obligation);
    assert!(
        result.is_err(),
        "a v0.1 Global was read by v0.2 -- the size gate is not doing the work"
    );

    let supplier = market.env.create_user();
    market.env.fund(&supplier, market.cook.mint, tokens(100));
    let cook = market.cook;
    assert!(
        market
            .env
            .try_supply(&supplier, &cook, tokens(100))
            .is_err(),
        "supplying reached a v0.1 Global"
    );

    // And stamping the version is the only thing left between here and a
    // working market. The blockhash has to move first: this borrow is
    // byte-identical to the one refused above, so an unchanged blockhash would
    // have it deduplicated rather than executed.
    migrate_global(&mut market);
    market.env.svm.expire_blockhash();
    try_borrow(&mut market, &borrower, obligation)
        .expect("stamping the version must complete the migration");
}

/// The oracle gate works on its own, with `Global` already migrated.
///
/// The other half of the isolation: `Global` is v0.2, so nothing about its size
/// can be doing the work, and the only thing still v0.1 is the collateral
/// reserve's price source.
#[test]
fn half_14_the_oracle_gate_holds_with_global_migrated() {
    let (mut market, borrower, obligation) = live_v1_market();
    upgrade(&mut market);
    migrate_global(&mut market);
    migrate_cook(&mut market);

    assert_eq!(market.env.read_global().version, 2);

    let result = try_borrow(&mut market, &borrower, obligation);
    assert!(
        result.is_err(),
        "a guardian-priced collateral reserve backed new debt under v0.2"
    );

    // A supplier is unaffected: supplying COOK needs no collateral price, and
    // stranding depositors mid-migration would be its own harm.
    let supplier = market.env.create_user();
    market.env.fund(&supplier, market.cook.mint, tokens(100));
    let cook = market.cook;
    market
        .env
        .try_supply(&supplier, &cook, tokens(100))
        .expect("an unrelated unmigrated reserve must not strand suppliers");
}

/// Liquidation is blocked while either side of it is unmigrated.
///
/// The one thing genuinely lost in a partial state, and the reason the runbook
/// says to migrate every reserve in one transaction. It is the right refusal --
/// a liquidation priced off a guardian feed v0.2 has no code to read would be
/// worse -- but a market that cannot close bad positions accumulates them, so
/// this is a cost of stopping halfway rather than a feature of it.
#[test]
fn half_15_liquidation_is_blocked_until_both_sides_are_migrated() {
    let (mut market, _borrower, _obligation) = live_v1_market();
    upgrade(&mut market);
    migrate_global(&mut market);
    migrate_cook(&mut market);

    // The collateral oracle is still a guardian feed, so `Liquidate` cannot
    // even deserialise its accounts.
    let feed = price_feed_pda(market.env.market, market.bcook.mint);
    let account = market.env.svm.get_account(&feed).unwrap();
    assert!(
        OracleState::try_deserialize(&mut &account.data[..]).is_err(),
        "the collateral side is not actually unmigrated"
    );

    migrate_bcook(&mut market);
    market.env.confirm_bootstrap(market.bcook.mint);
    let oracle = market
        .env
        .svm
        .get_account(&oracle_pda(market.env.market, market.bcook.mint))
        .unwrap();
    assert!(
        OracleState::try_deserialize(&mut &oracle.data[..]).is_ok(),
        "finishing the migration must give liquidation an oracle to read"
    );
}

/// A rate incident arriving the moment the migration lands.
///
/// The worst timing available to an attacker who is watching for the upgrade:
/// let the migration establish its anchor, then move the pool hard in the very
/// next block, before anyone has cranked twice. The bootstrap and the breaker
/// are both new at that instant -- the oracle has exactly one observation and
/// no reference to judge the second against.
///
/// What must hold is that the incident finds a market that cannot open new
/// risk anyway, and that the migrated economic state survives it untouched.
#[test]
fn half_16_a_rate_incident_immediately_after_migration_opens_nothing() {
    let (mut market, borrower, obligation) = live_v1_market();
    upgrade(&mut market);
    migrate_global(&mut market);
    migrate_cook(&mut market);
    migrate_bcook(&mut market);

    let anchor = market
        .env
        .read_oracle(market.bcook.mint)
        .reference
        .gross_rate;

    // The incident: the pool triples in the block after the migration.
    let bcook_mint = market.bcook.mint;
    market.env.set_pool(
        bcook_mint,
        px(3_900) as u64,
        POOL_SHARES,
        LIVE_WITHDRAWAL_FEE_BPS,
        2,
    );
    let _ = market.env.try_refresh_oracle(bcook_mint);

    assert_eq!(
        market.env.read_oracle(bcook_mint).reference.gross_rate,
        anchor,
        "a rate incident in the block after migration became the reference"
    );
    assert!(
        try_borrow(&mut market, &borrower, obligation).is_err(),
        "an incident immediately after migration financed a loan"
    );

    // And the borrower can still get out, which is the only thing that has to
    // work while everything else is frozen.
    let cook = market.cook;
    market
        .env
        .try_repay(&borrower, &cook, obligation, tokens(100))
        .expect("repayment must survive an incident during a migration");
}
