//! v0.1 -> v0.2 migration, against the real v0.1 binary.
//!
//! Every test here stands up a market on `fixtures/aera_v0_1.so` -- the actual
//! program built from the last commit before the guardian oracle was removed --
//! puts real deposits, debt and accrued interest into it, then swaps the
//! program for `fixtures/aera_v0_2.so` exactly as `solana program deploy` would
//! and runs the migration.
//!
//! Constructing v0.1-shaped state with v0.2 structures would prove nothing.
//! `fixtures/MANIFEST.txt` records the hashes of both artifacts actually used.
//!
//! The economic assertion is a whole-state comparison, not a spot check: every
//! quantity is captured before, captured after, and compared field by field.

mod common;

use aera::instructions::admin::init_oracle::OracleConfig;
use aera::oracle::breaker::OracleHealth;
use aera::state::{Global, OracleState, Reserve};
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::system_program;
use anchor_lang::{AccountDeserialize, Discriminator, InstructionData, Space, ToAccountMetas};
use common::v0_1;
use common::*;
use solana_keypair::Keypair;

// ===========================================================================
// Building a real v0.1 market
// ===========================================================================

/// A v0.1 market with both reserves, priced by guardians.
///
/// Deliberately mirrors `Env::core`, but every instruction is executed by the
/// v0.1 program: guardians are registered, prices are published by three of
/// five, and each reserve is bound to a `PriceFeed` PDA.
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

/// Register a guardian set and publish a price, using v0.1's own instructions.
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

/// A reserve created by the v0.1 program.
///
/// `init_reserve` kept the same discriminator and the same account order across
/// versions -- only the name of one field changed -- so the v0.2 accounts
/// struct produces exactly the right metas, provided the v0.1 `PriceFeed` PDA
/// is passed where v0.2 would put the oracle.
fn v1_add_reserve(
    env: &mut Env,
    decimals: u8,
    mantissa: i128,
    config: aera::state::ReserveConfig,
) -> ReserveHandle {
    let admin = env.admin.insecure_clone();
    let mint = solana_kite::create_token_mint(&mut env.svm, &admin, decimals, None).unwrap();

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
            oracle: feed, // v0.1 calls this slot `price_feed`
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
        decimals,
        reserve,
        share_mint,
        liquidity_vault,
        oracle: feed,
    }
}

fn v1_market(bcook_price_thousandths: u64) -> V1Market {
    let mut env = Env::with_program(V0_1_PROGRAM);
    let cook = v1_add_reserve(&mut env, DECIMALS, px(1_000), cook_config());
    let bcook = v1_add_reserve(
        &mut env,
        DECIMALS,
        px(bcook_price_thousandths),
        bcook_config(),
    );
    V1Market { env, cook, bcook }
}

// ===========================================================================
// The economic snapshot
// ===========================================================================

/// Everything that must survive the migration unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Economics {
    // reserve
    available_liquidity: u64,
    share_mint_supply: u64,
    borrowed_principal: u128,
    borrow_index: u128,
    accrued_fees: u64,
    last_update_slot: u64,
    // config
    supply_cap: u64,
    borrow_cap: u64,
    per_wallet_supply_cap: u64,
    loan_to_value_bps: u16,
    liquidation_threshold_bps: u16,
    liquidation_bonus_bps: u16,
    collateral_haircut_bps: u16,
    reserve_factor_bps: u16,
    close_factor_bps: u16,
    slots_per_year: u64,
    // real token balances, read from the chain rather than from our own books
    vault_balance: u64,
    share_mint_real_supply: u64,
    // obligations
    obligations: Vec<ObligationEconomics>,
    // protocol
    fee_destination: Pubkey,
    admin: Pubkey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObligationEconomics {
    key: Pubkey,
    deposits: Vec<(Pubkey, u64)>,
    borrows: Vec<(Pubkey, u128)>,
}

/// Deserialize a reserve written by either version of the program.
///
/// v0.2 appends `bad_debt: u128`, so a v0.1 account is sixteen bytes short and
/// `try_deserialize` refuses it. That refusal is deliberate -- it is what stops
/// v0.2 reading v0.1 state, and `test_half_migrated` depends on it -- but the
/// migration suite has to snapshot the economics of a reserve *before* it is
/// migrated, which means reading exactly the account the program will not.
///
/// Zero-padding is the honest reconstruction: a v0.1 reserve has recognised no
/// losses, so `bad_debt` is zero, which is what the migration itself writes
/// into those bytes when it grows the account.
fn read_reserve_either_era(env: &Env, reserve: Pubkey) -> Reserve {
    let account = env.svm.get_account(&reserve).expect("no such reserve");
    let mut data = account.data.clone();
    let v2_len = 8 + Reserve::INIT_SPACE;
    assert!(
        data.len() == v2_len || data.len() + 16 == v2_len,
        "a reserve of {} bytes is neither v0.1 nor v0.2 ({v2_len})",
        data.len()
    );
    data.resize(v2_len, 0);
    Reserve::try_deserialize(&mut &data[..]).expect("reserve did not deserialize")
}

fn read_economics(env: &Env, handle: &ReserveHandle, obligations: &[Pubkey]) -> Economics {
    let reserve: Reserve = read_reserve_either_era(env, handle.reserve);

    let vault_balance =
        solana_kite::get_token_account_balance(&env.svm, &handle.liquidity_vault).unwrap_or(0);

    // The share mint's own supply, not the reserve's mirror of it. If migration
    // minted or burned a single share this diverges.
    let share_mint_real_supply = {
        let account = env.svm.get_account(&handle.share_mint).unwrap();
        // SPL Token-2022 mint: supply is a u64 at offset 36.
        u64::from_le_bytes(account.data[36..44].try_into().unwrap())
    };

    let obligations = obligations
        .iter()
        .map(|key| {
            let account = env.svm.get_account(key).unwrap();
            let obligation =
                aera::state::Obligation::try_deserialize(&mut &account.data[..]).unwrap();
            ObligationEconomics {
                key: *key,
                deposits: obligation
                    .deposits
                    .iter()
                    .map(|d| (d.reserve, d.deposited_shares))
                    .collect(),
                borrows: obligation
                    .borrows
                    .iter()
                    .map(|b| (b.reserve, b.borrowed_principal))
                    .collect(),
            }
        })
        .collect();

    // `Global` is read as raw bytes: a v0.1 one is a byte short of the v0.2
    // struct, so `try_deserialize` fails on exactly the state under test.
    let global_account = env.svm.get_account(&env.global).unwrap();
    let admin = Pubkey::try_from(&global_account.data[8..40]).unwrap();
    let fee_destination = Pubkey::try_from(&global_account.data[40..72]).unwrap();

    Economics {
        available_liquidity: reserve.available_liquidity,
        share_mint_supply: reserve.share_mint_supply,
        borrowed_principal: reserve.borrowed_principal,
        borrow_index: reserve.borrow_index,
        accrued_fees: reserve.accrued_fees,
        last_update_slot: reserve.last_update_slot,
        supply_cap: reserve.config.supply_cap,
        borrow_cap: reserve.config.borrow_cap,
        per_wallet_supply_cap: reserve.config.per_wallet_supply_cap,
        loan_to_value_bps: reserve.config.loan_to_value_bps,
        liquidation_threshold_bps: reserve.config.liquidation_threshold_bps,
        liquidation_bonus_bps: reserve.config.liquidation_bonus_bps,
        collateral_haircut_bps: reserve.config.collateral_haircut_bps,
        reserve_factor_bps: reserve.config.reserve_factor_bps,
        close_factor_bps: reserve.config.close_factor_bps,
        slots_per_year: reserve.config.slots_per_year,
        vault_balance,
        share_mint_real_supply,
        obligations,
        fee_destination,
        admin,
    }
}

/// Assert nothing economic moved, naming whatever did.
fn assert_economics_preserved(before: &Economics, after: &Economics, label: &str) {
    let diffs = economics_diff(before, after);
    assert!(
        diffs.is_empty(),
        "{}: migration changed economic state:\n{}",
        label,
        diffs.join("\n")
    );
}

/// Every field that differs, named.
///
/// Separate from the assertion so a test can prove the comparison actually
/// detects a change. An equality check that silently compared nothing would
/// make every migration assertion in this file vacuous, and it would look
/// exactly like a passing suite.
fn economics_diff(before: &Economics, after: &Economics) -> Vec<String> {
    let mut diffs: Vec<String> = Vec::new();

    macro_rules! compare {
        ($field:ident) => {
            if before.$field != after.$field {
                diffs.push(format!(
                    "  {:26} {:?}  ->  {:?}",
                    stringify!($field),
                    before.$field,
                    after.$field
                ));
            }
        };
    }

    compare!(available_liquidity);
    compare!(share_mint_supply);
    compare!(borrowed_principal);
    compare!(borrow_index);
    compare!(accrued_fees);
    compare!(last_update_slot);
    compare!(supply_cap);
    compare!(borrow_cap);
    compare!(per_wallet_supply_cap);
    compare!(loan_to_value_bps);
    compare!(liquidation_threshold_bps);
    compare!(liquidation_bonus_bps);
    compare!(collateral_haircut_bps);
    compare!(reserve_factor_bps);
    compare!(close_factor_bps);
    compare!(slots_per_year);
    compare!(vault_balance);
    compare!(share_mint_real_supply);
    compare!(obligations);
    compare!(fee_destination);
    compare!(admin);

    diffs
}

/// The comparison must fail when something moves.
///
/// Without this, every `assert_economics_preserved` in this file could be
/// comparing nothing at all and would still pass. Each mutation below is a
/// quantity a broken migration could plausibly damage, and each must be caught
/// *by name* -- detecting a change but blaming the wrong field would be its own
/// kind of useless.
#[test]
fn the_economic_comparison_catches_a_change() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(10_000));
    let (_borrower, obligation) = seed_borrower(&mut market, tokens(10_000), tokens(2_000));
    let baseline = read_economics(&market.env, &market.cook, &[obligation]);

    assert!(
        economics_diff(&baseline, &baseline).is_empty(),
        "a snapshot must equal itself"
    );

    #[allow(clippy::type_complexity)] // a table of named mutations; the type
    // is the point, not an accident
    let cases: Vec<(&str, Box<dyn Fn(&mut Economics)>)> = vec![
        (
            "available_liquidity",
            Box::new(|e: &mut Economics| e.available_liquidity += 1),
        ),
        (
            "share_mint_supply",
            Box::new(|e: &mut Economics| e.share_mint_supply += 1),
        ),
        (
            "borrowed_principal",
            Box::new(|e: &mut Economics| e.borrowed_principal += 1),
        ),
        (
            "borrow_index",
            Box::new(|e: &mut Economics| e.borrow_index += 1),
        ),
        (
            "accrued_fees",
            Box::new(|e: &mut Economics| e.accrued_fees += 1),
        ),
        (
            "vault_balance",
            Box::new(|e: &mut Economics| e.vault_balance += 1),
        ),
        (
            "share_mint_real_supply",
            Box::new(|e: &mut Economics| e.share_mint_real_supply += 1),
        ),
        (
            "supply_cap",
            Box::new(|e: &mut Economics| e.supply_cap += 1),
        ),
        (
            "loan_to_value_bps",
            Box::new(|e: &mut Economics| e.loan_to_value_bps += 1),
        ),
        (
            "collateral_haircut_bps",
            Box::new(|e: &mut Economics| e.collateral_haircut_bps += 1),
        ),
        (
            "fee_destination",
            Box::new(|e: &mut Economics| e.fee_destination = Pubkey::new_unique()),
        ),
        (
            "admin",
            Box::new(|e: &mut Economics| e.admin = Pubkey::new_unique()),
        ),
        (
            "obligations",
            Box::new(|e: &mut Economics| {
                if let Some(o) = e.obligations.first_mut() {
                    if let Some(b) = o.borrows.first_mut() {
                        b.1 += 1;
                    }
                }
            }),
        ),
    ];

    for (field, mutate) in cases {
        let mut damaged = baseline.clone();
        mutate(&mut damaged);
        let diffs = economics_diff(&baseline, &damaged);
        assert!(
            !diffs.is_empty(),
            "a change to {field} went undetected -- the migration assertions are vacuous"
        );
        assert!(
            diffs.iter().any(|d| d.contains(field)),
            "a change to {field} was detected but misattributed: {diffs:?}"
        );
    }
}

// ===========================================================================
// The migration itself
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

fn cook_oracle_config() -> OracleConfig {
    OracleConfig::unit_of_account()
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

/// Upgrade the program and migrate both reserves in ONE transaction.
///
/// One transaction because a market with one reserve migrated and one not
/// cannot be borrowed against or liquidated until both land. Repay survives it
/// either way -- that is tested separately -- but leaving a market unable to
/// liquidate is not a state to enter deliberately.
fn upgrade_and_migrate(market: &mut V1Market, bcook_rate_thousandths: u64) -> Result<(), String> {
    market.env.upgrade_program(V0_2_PROGRAM);

    // The stake pool the new oracle will read. It must exist before migration,
    // because the reference is bootstrapped from it and there is no other
    // source for a first price.
    let bcook_mint = market.bcook.mint;
    market.env.set_pool(
        bcook_mint,
        px(bcook_rate_thousandths) as u64,
        POOL_SHARES,
        LIVE_WITHDRAWAL_FEE_BPS,
        1,
    );

    let admin = market.env.admin.insecure_clone();
    let cook_ix = migrate_reserve_ix(&market.env, &market.cook, cook_oracle_config());
    let bcook_config = bcook_oracle_config(&market.env, &market.bcook);
    let bcook_ix = migrate_reserve_ix(&market.env, &market.bcook, bcook_config);
    let global_ix = Instruction {
        program_id: aera::id(),
        accounts: aera::accounts::MigrateGlobalToV2 {
            global: market.env.global,
            admin: admin.pubkey(),
            system_program: system_program::id(),
        }
        .to_account_metas(None),
        data: aera::instruction::MigrateGlobalToV2 {}.data(),
    };

    let result = market
        .env
        .send_raw(vec![cook_ix, bcook_ix, global_ix], &[&admin]);

    /*
     * Point the handles at the new oracles.
     *
     * A `ReserveHandle` is the test's own record of an asset's accounts, and it
     * was built before the migration, so it still names the guardian feed. Every
     * post-migration action would pass that address where the oracle belongs and
     * fail with AccountNotInitialized -- which is exactly what a real client
     * that never refreshed its addresses would do.
     */
    if result.is_ok() {
        market.cook.oracle = oracle_pda(market.env.market, market.cook.mint);
        market.bcook.oracle = oracle_pda(market.env.market, market.bcook.mint);
    }

    result
}

// ===========================================================================
// A. Empty market
// ===========================================================================

#[test]
fn a_empty_market_migrates() {
    let mut market = v1_market(1_000);

    // Proof the starting state really is v0.1: a guardian feed exists, with
    // v0.1's discriminator, and the reserve points at it.
    let feed = price_feed_pda(market.env.market, market.bcook.mint);
    let feed_account = market
        .env
        .svm
        .get_account(&feed)
        .expect("v0.1 feed must exist");
    assert_eq!(
        feed_account.data[..8],
        v0_1::PRICE_FEED_DISCRIMINATOR,
        "the fixture did not produce a v0.1 PriceFeed"
    );

    let before = read_economics(&market.env, &market.cook, &[]);
    upgrade_and_migrate(&mut market, 1_300).expect("migration");
    let after = read_economics(&market.env, &market.cook, &[]);

    assert_economics_preserved(&before, &after, "empty market");

    // The feed is gone and the oracle is here.
    assert!(
        market
            .env
            .svm
            .get_account(&feed)
            .map(|a| a.data.len())
            .unwrap_or(0)
            == 0
            || market.env.svm.get_account(&feed).is_none(),
        "the guardian feed was not closed"
    );

    /*
     * A migrated oracle is BOOTSTRAPPING, not HEALTHY, and that is the point.
     *
     * Migration is a moment an operator picks. If one reading of the stake pool
     * at that moment became Aera's permanent anchor, whoever could arrange the
     * pool then would have chosen the anchor, and the movement breaker -- which
     * has no reference yet -- could not have objected. So the reading is
     * checked against the pool's own published previous epoch, used to value
     * everything that already exists, and refused the right to open new risk
     * until a later epoch of the pool confirms it.
     */
    let oracle = market.env.read_oracle(market.bcook.mint);
    assert!(oracle.reference.is_set(), "no reference was bootstrapped");
    assert_eq!(
        OracleHealth::from_u8(oracle.health).unwrap(),
        OracleHealth::Bootstrapping,
        "a single reading at a moment the operator chose must not be trusted \
         enough to permit new borrowing"
    );

    market.env.confirm_bootstrap(market.bcook.mint);
    assert_eq!(
        OracleHealth::from_u8(market.env.read_oracle(market.bcook.mint).health).unwrap(),
        OracleHealth::Healthy,
        "a later epoch agreeing with the anchor must clear the bootstrap"
    );
}

/// The reference must come from the stake pool, not from the guardians.
#[test]
fn a2_the_reference_comes_from_the_pool_not_the_guardians() {
    // Guardians say 1.0. The pool says 1.3. The migration must take 1.3.
    let mut market = v1_market(1_000);
    upgrade_and_migrate(&mut market, 1_300).expect("migration");

    let oracle = market.env.read_oracle(market.bcook.mint);
    assert_eq!(
        oracle.reference.gross_rate,
        aera::constants::FIXED_POINT_SCALE * 13 / 10,
        "the reference did not come from the stake pool"
    );
    assert_eq!(
        oracle.reference.withdrawal_fee_bps, LIVE_WITHDRAWAL_FEE_BPS,
        "the pool's redemption fee was not carried into the reference"
    );
    assert_eq!(
        oracle.reference.effective_rate,
        aera::constants::FIXED_POINT_SCALE * 13 / 10 * 98 / 100,
        "effective must be gross net of the pool's fee"
    );
}

// ===========================================================================
// B-E. Populated markets
// ===========================================================================

fn seed_supplier(market: &mut V1Market, amount: u64) -> Keypair {
    let user = market.env.create_user();
    market.env.fund(&user, market.cook.mint, amount);
    let cook = market.cook;
    market.env.supply(&user, &cook, amount);
    user
}

fn seed_borrower(market: &mut V1Market, collateral: u64, borrow: u64) -> (Keypair, Pubkey) {
    let user = market.env.create_user();
    market.env.fund(&user, market.bcook.mint, collateral);
    market.env.fund(&user, market.cook.mint, tokens(1_000));
    let bcook = market.bcook;
    let cook = market.cook;
    let obligation = market.env.open_position(&user, &bcook, collateral);
    if borrow > 0 {
        market
            .env
            .try_borrow(&user, &cook, obligation, borrow, &[&cook, &bcook])
            .expect("v0.1 borrow");
    }
    (user, obligation)
}

#[test]
fn b_supplier_only_market_migrates() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(10_000));

    let before = read_economics(&market.env, &market.cook, &[]);
    assert!(before.available_liquidity > 0, "the setup supplied nothing");
    assert!(before.share_mint_real_supply > 0, "no aCOOK was minted");

    upgrade_and_migrate(&mut market, 1_300).expect("migration");
    let after = read_economics(&market.env, &market.cook, &[]);
    assert_economics_preserved(&before, &after, "supplier-only");
}

#[test]
fn c_multiple_suppliers_migrate() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(10_000));
    seed_supplier(&mut market, tokens(25_000));
    seed_supplier(&mut market, tokens(1));

    let before = read_economics(&market.env, &market.cook, &[]);
    upgrade_and_migrate(&mut market, 1_300).expect("migration");
    let after = read_economics(&market.env, &market.cook, &[]);
    assert_economics_preserved(&before, &after, "multiple suppliers");
}

#[test]
fn d_active_borrower_migrates() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(50_000));
    let (_borrower, obligation) = seed_borrower(&mut market, tokens(10_000), tokens(2_000));

    let before = read_economics(&market.env, &market.cook, &[obligation]);
    assert!(before.borrowed_principal > 0, "the setup borrowed nothing");

    upgrade_and_migrate(&mut market, 1_300).expect("migration");
    let after = read_economics(&market.env, &market.cook, &[obligation]);
    assert_economics_preserved(&before, &after, "active borrower");
}

#[test]
fn e_multiple_obligations_migrate() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(100_000));
    let (_a, first) = seed_borrower(&mut market, tokens(10_000), tokens(1_000));
    let (_b, second) = seed_borrower(&mut market, tokens(20_000), tokens(3_000));
    let (_c, third) = seed_borrower(&mut market, tokens(5_000), 0);

    let before = read_economics(&market.env, &market.cook, &[first, second, third]);
    assert_eq!(before.obligations.len(), 3);

    upgrade_and_migrate(&mut market, 1_300).expect("migration");
    let after = read_economics(&market.env, &market.cook, &[first, second, third]);
    assert_economics_preserved(&before, &after, "multiple obligations");
}

// ===========================================================================
// F, G. Accrued interest and fees
// ===========================================================================

#[test]
fn f_accrued_interest_survives() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(50_000));
    // 10,000 bCOOK at 1.0, haircut 5%, LTV 55% allows 5,225. Borrow most of it,
    // so utilization is high enough for a year to accrue visible interest.
    let (_borrower, obligation) = seed_borrower(&mut market, tokens(10_000), tokens(5_000));

    // Let a year of interest accrue, then bank it into the index.
    market
        .env
        .warp_slots(aera::constants::DEFAULT_SLOTS_PER_YEAR);
    let cook = market.cook;
    market.env.accrue(&cook);

    let before = read_economics(&market.env, &market.cook, &[obligation]);
    assert!(
        before.borrow_index > aera::constants::FIXED_POINT_SCALE,
        "no interest accrued: index still {}",
        before.borrow_index
    );

    upgrade_and_migrate(&mut market, 1_300).expect("migration");
    let after = read_economics(&market.env, &market.cook, &[obligation]);

    assert_economics_preserved(&before, &after, "accrued interest");
    assert_eq!(
        after.borrow_index, before.borrow_index,
        "the borrow index moved across the migration"
    );
}

#[test]
fn g_accrued_protocol_fees_survive() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(50_000));
    let (_borrower, obligation) = seed_borrower(&mut market, tokens(10_000), tokens(5_000));

    market
        .env
        .warp_slots(aera::constants::DEFAULT_SLOTS_PER_YEAR);
    let cook = market.cook;
    market.env.accrue(&cook);

    let before = read_economics(&market.env, &market.cook, &[obligation]);
    assert!(before.accrued_fees > 0, "no protocol fees accrued");

    upgrade_and_migrate(&mut market, 1_300).expect("migration");
    let after = read_economics(&market.env, &market.cook, &[obligation]);

    assert_eq!(
        after.accrued_fees, before.accrued_fees,
        "protocol fees changed across the migration"
    );
    assert_economics_preserved(&before, &after, "accrued fees");
}

// ===========================================================================
// H, I. Edge states
// ===========================================================================

#[test]
fn h_position_near_liquidation_migrates() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(50_000));
    // Borrow close to the limit: 10,000 bCOOK at 1.0, haircut 5%, LTV 55%
    // allows 5,225.
    let (_borrower, obligation) = seed_borrower(&mut market, tokens(10_000), tokens(5_200));

    let before = read_economics(&market.env, &market.cook, &[obligation]);
    upgrade_and_migrate(&mut market, 1_300).expect("migration");
    let after = read_economics(&market.env, &market.cook, &[obligation]);
    assert_economics_preserved(&before, &after, "near liquidation");
}

#[test]
fn i_market_at_its_caps_migrates() {
    let mut market = v1_market(1_000);
    // Fill the supply cap exactly.
    let user = market.env.create_user();
    market.env.fund(&user, market.cook.mint, HARNESS_SUPPLY_CAP);
    let cook = market.cook;
    market.env.supply(&user, &cook, HARNESS_SUPPLY_CAP);

    let before = read_economics(&market.env, &market.cook, &[]);
    assert_eq!(before.available_liquidity, HARNESS_SUPPLY_CAP);

    upgrade_and_migrate(&mut market, 1_300).expect("migration");
    let after = read_economics(&market.env, &market.cook, &[]);
    assert_economics_preserved(&before, &after, "at caps");
}

// ===========================================================================
// J-P. Failure modes
// ===========================================================================

#[test]
fn j_an_invalid_stake_pool_aborts_the_migration() {
    let mut market = v1_market(1_000);
    market.env.upgrade_program(V0_2_PROGRAM);

    // A pool with zero shares: no rate exists.
    market.env.set_pool(
        market.bcook.mint,
        px(1_300) as u64,
        0,
        LIVE_WITHDRAWAL_FEE_BPS,
        1,
    );

    let admin = market.env.admin.insecure_clone();
    let config = bcook_oracle_config(&market.env, &market.bcook);
    let result = market.env.send_raw(
        vec![migrate_reserve_ix(&market.env, &market.bcook, config)],
        &[&admin],
    );

    assert!(result.is_err(), "migrated against a pool with no shares");

    // And the reserve is untouched: still pointing at the guardian feed.
    let reserve = market.env.read_reserve(&market.bcook);
    assert_eq!(
        reserve.oracle,
        price_feed_pda(market.env.market, market.bcook.mint),
        "a failed migration left the reserve repointed"
    );
}

#[test]
fn k_a_pool_owned_by_the_wrong_program_aborts() {
    let mut market = v1_market(1_000);
    market.env.upgrade_program(V0_2_PROGRAM);

    let pool = market.env.stake_pool_address(market.bcook.mint);
    let data = stake_pool_bytes(market.bcook.mint, px(1_300) as u64, POOL_SHARES, 200, 1);
    market
        .env
        .svm
        .set_account(
            pool,
            solana_account::Account {
                lamports: 1_000_000_000,
                data,
                owner: Pubkey::new_unique(), // not the configured program
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let admin = market.env.admin.insecure_clone();
    let config = bcook_oracle_config(&market.env, &market.bcook);
    let result = market.env.send_raw(
        vec![migrate_reserve_ix(&market.env, &market.bcook, config)],
        &[&admin],
    );

    assert!(result.is_err(), "migrated against a foreign-owned pool");
    assert!(
        result.unwrap_err().contains("OracleOwnerMismatch"),
        "wrong owner must be refused as an owner mismatch"
    );
}

#[test]
fn l_a_pool_for_the_wrong_mint_aborts() {
    let mut market = v1_market(1_000);
    market.env.upgrade_program(V0_2_PROGRAM);

    // Right owner, right address, right shape -- wrong asset.
    let pool = market.env.stake_pool_address(market.bcook.mint);
    let data = stake_pool_bytes(market.cook.mint, px(1_300) as u64, POOL_SHARES, 200, 1);
    market
        .env
        .svm
        .set_account(
            pool,
            solana_account::Account {
                lamports: 1_000_000_000,
                data,
                owner: TEST_STAKE_POOL_PROGRAM,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let admin = market.env.admin.insecure_clone();
    let config = bcook_oracle_config(&market.env, &market.bcook);
    let result = market.env.send_raw(
        vec![migrate_reserve_ix(&market.env, &market.bcook, config)],
        &[&admin],
    );

    assert!(result.is_err(), "migrated against a pool for another mint");
    let message = result.unwrap_err();
    assert!(
        message.contains("OracleMintMismatch"),
        "wrong mint must be refused as a mint mismatch: {message}"
    );
}

#[test]
fn m_an_unauthorized_signer_cannot_migrate() {
    let mut market = v1_market(1_000);
    market.env.upgrade_program(V0_2_PROGRAM);
    market.env.set_pool(
        market.bcook.mint,
        px(1_300) as u64,
        POOL_SHARES,
        LIVE_WITHDRAWAL_FEE_BPS,
        1,
    );

    let stranger = market.env.create_user();
    let config = bcook_oracle_config(&market.env, &market.bcook);

    let mut accounts = aera::accounts::MigrateReserveToV2 {
        global: market.env.global,
        market: market.env.market,
        reserve: market.bcook.reserve,
        legacy_price_feed: price_feed_pda(market.env.market, market.bcook.mint),
        liquidity_mint: market.bcook.mint,
        oracle: oracle_pda(market.env.market, market.bcook.mint),
        admin: stranger.pubkey(),
        system_program: system_program::id(),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(config.source_account, false));

    let result = market.env.send_raw(
        vec![Instruction {
            program_id: aera::id(),
            accounts,
            data: aera::instruction::MigrateReserveToV2 { config }.data(),
        }],
        &[&stranger],
    );

    assert!(result.is_err(), "a stranger migrated the protocol");
    assert!(result.unwrap_err().contains("NotAdmin"));
}

#[test]
fn n_migrating_twice_is_refused() {
    let mut market = v1_market(1_000);
    upgrade_and_migrate(&mut market, 1_300).expect("first migration");

    // Second run: the reserve now points at the oracle PDA, and the guardian
    // feed it would close no longer exists.
    let admin = market.env.admin.insecure_clone();
    let config = bcook_oracle_config(&market.env, &market.bcook);
    let result = market.env.send_raw(
        vec![migrate_reserve_ix(&market.env, &market.bcook, config)],
        &[&admin],
    );

    assert!(result.is_err(), "the migration ran twice");

    // The global stamp likewise refuses a second run.
    let global_ix = Instruction {
        program_id: aera::id(),
        accounts: aera::accounts::MigrateGlobalToV2 {
            global: market.env.global,
            admin: admin.pubkey(),
            system_program: system_program::id(),
        }
        .to_account_metas(None),
        data: aera::instruction::MigrateGlobalToV2 {}.data(),
    };
    let result = market.env.send_raw(vec![global_ix], &[&admin]);
    assert!(result.is_err(), "the global stamp ran twice");
    assert!(result.unwrap_err().contains("AlreadyMigrated"));
}

#[test]
fn o_a_corrupted_legacy_feed_is_refused() {
    let mut market = v1_market(1_000);
    market.env.upgrade_program(V0_2_PROGRAM);
    market.env.set_pool(
        market.bcook.mint,
        px(1_300) as u64,
        POOL_SHARES,
        LIVE_WITHDRAWAL_FEE_BPS,
        1,
    );

    // Overwrite the feed's discriminator: the account at that address is no
    // longer provably a PriceFeed.
    let feed = price_feed_pda(market.env.market, market.bcook.mint);
    let mut account = market.env.svm.get_account(&feed).unwrap();
    account.data[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    market.env.svm.set_account(feed, account).unwrap();

    let admin = market.env.admin.insecure_clone();
    let config = bcook_oracle_config(&market.env, &market.bcook);
    let result = market.env.send_raw(
        vec![migrate_reserve_ix(&market.env, &market.bcook, config)],
        &[&admin],
    );

    assert!(
        result.is_err(),
        "closed an account that was not a PriceFeed"
    );
    assert!(result.unwrap_err().contains("NotMigratable"));
}

/// P. A migration that fails partway leaves nothing behind.
///
/// Both reserves are migrated in one transaction and the second one is made to
/// fail. The first must not survive: Solana rolls the whole transaction back,
/// so the market is entirely v0.1 afterwards, not half of each.
#[test]
fn p_a_failure_halfway_rolls_the_whole_transaction_back() {
    let mut market = v1_market(1_000);
    market.env.upgrade_program(V0_2_PROGRAM);

    // COOK's migration will succeed; bCOOK's will fail, because no pool exists.
    let admin = market.env.admin.insecure_clone();
    let cook_ix = migrate_reserve_ix(&market.env, &market.cook, cook_oracle_config());
    let bcook_config = bcook_oracle_config(&market.env, &market.bcook);
    let bcook_ix = migrate_reserve_ix(&market.env, &market.bcook, bcook_config);

    let result = market.env.send_raw(vec![cook_ix, bcook_ix], &[&admin]);
    assert!(result.is_err(), "the batch should have failed on bCOOK");

    // COOK must be untouched despite its instruction having succeeded.
    let cook_reserve = market.env.read_reserve(&market.cook);
    assert_eq!(
        cook_reserve.oracle,
        price_feed_pda(market.env.market, market.cook.mint),
        "COOK stayed migrated after the batch failed -- not atomic"
    );
    assert!(
        market
            .env
            .svm
            .get_account(&oracle_pda(market.env.market, market.cook.mint))
            .is_none(),
        "a v0.2 oracle survived a rolled-back migration"
    );
}

// ===========================================================================
// Q-T. The protocol still works afterwards
// ===========================================================================

#[test]
fn q_borrowing_works_after_migration() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(50_000));
    let (borrower, obligation) = seed_borrower(&mut market, tokens(10_000), tokens(1_000));

    upgrade_and_migrate(&mut market, 1_300).expect("migration");

    let cook = market.cook;
    let bcook = market.bcook;

    /*
     * Freshly migrated, the market cannot open new risk.
     *
     * This is the half-migrated state made safe: the oracle has a reference and
     * can value every existing position, so supplying, repaying, adding
     * collateral and liquidating all still work -- but the anchor it is valuing
     * against has been seen exactly once, at a moment the operator chose, so it
     * is not yet allowed to justify new debt.
     */
    let frozen = market
        .env
        .try_borrow(&borrower, &cook, obligation, tokens(100), &[&cook, &bcook]);
    let message = frozen.expect_err("a bootstrapping oracle permitted new borrowing");
    assert!(
        message.contains("OracleBorrowFrozen"),
        "borrowing must be refused as a freeze, not by accident: {message}"
    );

    // One epoch later the pool agrees with itself, and the market opens.
    market.env.confirm_bootstrap(bcook.mint);
    market
        .env
        .try_borrow(&borrower, &cook, obligation, tokens(100), &[&cook, &bcook])
        .expect("borrowing must work once the bootstrap is confirmed");
}

/// Repaying must never wait on a bootstrap.
///
/// The freeze above is the dangerous kind of correct: a rule that blocks
/// borrowing is only safe if it leaves the exits open. A borrower who cannot
/// reduce their own risk during a freeze is worse off than one who was never
/// protected by it.
#[test]
fn q2_a_bootstrapping_market_can_still_be_repaid() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(50_000));
    let (borrower, obligation) = seed_borrower(&mut market, tokens(10_000), tokens(2_000));

    upgrade_and_migrate(&mut market, 1_300).expect("migration");
    assert_eq!(
        OracleHealth::from_u8(market.env.read_oracle(market.bcook.mint).health).unwrap(),
        OracleHealth::Bootstrapping,
        "this test is only meaningful while the oracle is bootstrapping"
    );

    let cook = market.cook;
    market
        .env
        .try_repay(&borrower, &cook, obligation, tokens(500))
        .expect("repayment must remain open while the oracle is bootstrapping");
}

#[test]
fn r_repaying_works_after_migration() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(50_000));
    let (borrower, obligation) = seed_borrower(&mut market, tokens(10_000), tokens(2_000));

    upgrade_and_migrate(&mut market, 1_300).expect("migration");

    let cook = market.cook;
    market
        .env
        .try_repay(&borrower, &cook, obligation, tokens(500))
        .expect("repaying must work on the migrated market");
}

/// Repay must also work on a market that is only *half* migrated.
///
/// This is the property that makes a multi-transaction migration survivable: a
/// borrower is never trapped, whatever order the operator lands things in.
#[test]
fn r2_repaying_works_on_a_half_migrated_market() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(50_000));
    let (borrower, obligation) = seed_borrower(&mut market, tokens(10_000), tokens(2_000));

    market.env.upgrade_program(V0_2_PROGRAM);
    // Migrate COOK only. bCOOK still points at its guardian feed.
    let admin = market.env.admin.insecure_clone();
    market
        .env
        .send_raw(
            vec![migrate_reserve_ix(
                &market.env,
                &market.cook,
                cook_oracle_config(),
            )],
            &[&admin],
        )
        .expect("COOK migration");

    let cook = market.cook;
    market
        .env
        .try_repay(&borrower, &cook, obligation, tokens(500))
        .expect("a half-migrated market must never trap a borrower");
}

#[test]
fn s_liquidation_works_after_migration() {
    let mut market = v1_market(1_000);
    seed_supplier(&mut market, tokens(50_000));
    let (_borrower, obligation) = seed_borrower(&mut market, tokens(10_000), tokens(5_200));

    upgrade_and_migrate(&mut market, 1_300).expect("migration");

    // Drive the collateral down until the position is underwater.
    let bcook = market.bcook;
    let cook = market.cook;
    market.env.set_price(bcook.mint, px(700));

    let liquidator = market.env.create_user();
    market.env.fund(&liquidator, cook.mint, tokens(10_000));
    market.env.fund(&liquidator, bcook.mint, 0);

    market
        .env
        .try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(500))
        .expect("liquidation must work on the migrated market");
}

#[test]
fn t_supply_and_withdraw_work_after_migration() {
    let mut market = v1_market(1_000);
    let supplier = seed_supplier(&mut market, tokens(10_000));
    // `seed_supplier` supplies the whole balance, so top it up for the
    // post-migration deposit.
    let cook_mint = market.cook.mint;
    market.env.fund(&supplier, cook_mint, tokens(2_000));

    upgrade_and_migrate(&mut market, 1_300).expect("migration");

    let cook = market.cook;
    market
        .env
        .try_supply(&supplier, &cook, tokens(1_000))
        .expect("supply must work after migration");
    market
        .env
        .try_withdraw(&supplier, &cook, tokens(500))
        .expect("withdraw must work after migration");
}

// ===========================================================================
// Account sizes and the version stamp
// ===========================================================================

#[test]
fn the_reserve_account_does_not_change_size() {
    let mut market = v1_market(1_000);

    let before = market
        .env
        .svm
        .get_account(&market.cook.reserve)
        .unwrap()
        .data
        .len();
    upgrade_and_migrate(&mut market, 1_300).expect("migration");
    let after = market
        .env
        .svm
        .get_account(&market.cook.reserve)
        .unwrap()
        .data
        .len();

    assert_eq!(
        before, after,
        "Reserve was realloc'd; the migration performs none, so this would be corruption"
    );
    assert_eq!(
        after,
        8 + Reserve::INIT_SPACE,
        "Reserve is not the size this program expects"
    );
}

#[test]
fn the_global_grows_by_exactly_one_byte_and_records_the_version() {
    let mut market = v1_market(1_000);

    let before = market
        .env
        .svm
        .get_account(&market.env.global)
        .unwrap()
        .data
        .len();
    assert_eq!(
        before,
        aera::instructions::admin::migrate::global_v1_len(),
        "the v0.1 fixture did not produce a v0.1-sized Global"
    );

    upgrade_and_migrate(&mut market, 1_300).expect("migration");

    let account = market.env.svm.get_account(&market.env.global).unwrap();
    assert_eq!(
        account.data.len(),
        before + 1,
        "Global grew by more than the version byte"
    );
    assert_eq!(account.data.len(), 8 + Global::INIT_SPACE);

    let global = Global::try_deserialize(&mut &account.data[..]).expect("v0.2 Global must decode");
    assert_eq!(global.version, aera::state::GLOBAL_VERSION_V2);

    // And everything else in Global survived.
    assert_eq!(global.admin, market.env.admin.pubkey());
    assert_eq!(global.fee_destination, market.env.fee_wallet.pubkey());
}

#[test]
fn the_oracle_account_is_the_size_the_program_expects() {
    let mut market = v1_market(1_000);
    upgrade_and_migrate(&mut market, 1_300).expect("migration");

    let account = market
        .env
        .svm
        .get_account(&oracle_pda(market.env.market, market.bcook.mint))
        .unwrap();
    assert_eq!(
        account.data.len(),
        OracleState::DISCRIMINATOR.len() + OracleState::INIT_SPACE
    );
}
