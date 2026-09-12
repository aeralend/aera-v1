#![allow(dead_code)]
//! Shared LiteSVM harness for the Aera tests.
//!
//! Builds Aera Core as it is actually configured at launch — a COOK reserve
//! that can be borrowed but not posted as collateral, and a bCOOK reserve that
//! can be posted but not borrowed — and exposes one method per protocol action.
//!
//! Actions that read value bundle the required `accrue` / `refresh_obligation`
//! instructions into the same transaction, exactly as a real client must. The
//! remaining-accounts list for `refresh_obligation` is built by reading the
//! obligation, so tests never hand-maintain it.

use anchor_lang::{
    solana_program::{
        clock::Clock,
        instruction::{AccountMeta, Instruction},
        system_program,
    },
    AccountDeserialize, InstructionData, ToAccountMetas,
};
pub use anchor_spl::token::ID as TOKEN_PROGRAM_ID;
use litesvm::LiteSVM;
use solana_account::Account as SvmAccount;
use solana_keypair::Keypair;
use solana_kite::{
    create_token_mint, create_wallet, get_token_account_balance, mint_tokens_to_token_account,
    send_transaction_from_instructions,
};
use solana_signer::Signer;

use aera::constants::*;
use aera::state::{Global, Obligation, OracleState, Reserve, ReserveConfig, SupplyPosition};

/// Value, solvency and profit assertions used by the audit suite.
pub mod audit;
pub mod damm;
pub mod invariants;
pub mod market;

/// Hand-built v0.1 instructions, for the migration suite.
pub mod v0_1;

pub use anchor_lang::prelude::Pubkey;
/// Re-exported so tests can call `keypair.pubkey()` without importing the trait.
pub use solana_signer::Signer as _AeraSigner;

/// Rates are expressed as `mantissa * 10^-18`, so `px(1_000)` is 1.0.
///
/// `add_reserve` gives the synthetic pool exactly `POOL_SHARES` shares and
/// `mantissa` lamports, so `total_lamports / pool_token_supply` reproduces the
/// intended rate exactly, with no rounding to reason about.
pub const PRICE_EXPONENT: i32 = -18;

/// Share supply of every synthetic pool. Matches PRICE_EXPONENT so a mantissa
/// can be used directly as the lamport figure. The real pool holds ~9.7e16, so
/// this is the same order of magnitude.
pub const POOL_SHARES: u64 = 1_000_000_000_000_000_000;

/// The v0.2 program under test.
///
/// A named fixture, not `target/deploy/aera.so`. That path is whatever was
/// built last, and `cargo test` does not rebuild it -- so tests against it
/// silently exercise a stale binary and report results that look real. Build
/// fixtures with `tools/build-artifacts.sh`; the hashes are in
/// `fixtures/MANIFEST.txt`.
pub const V0_2_PROGRAM: &[u8] = include_bytes!("../../../../fixtures/aera_v0_2.so");

/// The v0.1 program, for the migration suite only.
///
/// Built from the last commit before the guardian oracle was removed, so the
/// migration tests construct v0.1 state with the code that actually shipped it
/// rather than with v0.2 structures pretending.
pub const V0_1_PROGRAM: &[u8] = include_bytes!("../../../../fixtures/aera_v0_1.so");

/// Stands in for the BakeYourStake stake-pool program.
///
/// The program under test never special-cases it: it validates that the source
/// account is owned by whatever program the oracle was configured with, and the
/// harness configures this one. A real deployment configures the real program.
pub const TEST_STAKE_POOL_PROGRAM: Pubkey = Pubkey::new_from_array([7u8; 32]);

/// The synthetic pool's redemption fee, for suites that are not about the fee.
///
/// Zero by default, deliberately. The deployed pool charges 2.00%, and that fee
/// genuinely reduces collateral value -- but the LTV, liquidation, interest and
/// rounding suites are about those mechanisms, and folding a second reduction
/// into every expected number there would obscure what each test is actually
/// asserting.
///
/// The fee is not untested as a result. `test_oracle.rs` drives it directly
/// (`the_withdrawal_fee_is_applied_by_the_oracle_not_the_haircut`,
/// `a_higher_pool_fee_lowers_collateral_value_immediately`,
/// `a_fee_above_the_bound_stops_the_protocol_lending`), and
/// `a_pool_fee_reduces_borrowing_capacity` below proves the whole composition
/// end to end: pool fee -> oracle -> collateral value -> what may be borrowed.
pub const TEST_WITHDRAWAL_FEE_BPS: u16 = 0;

/// The deployed pool's actual fee, for tests that are about it.
pub const LIVE_WITHDRAWAL_FEE_BPS: u16 = 200;

/// The stake-pool program's deploy slot, mirroring the real one on Cookie Chain
/// so the tests exercise the same magnitudes.
pub const TEST_DEPLOY_SLOT: u64 = 5_504_973;

/// The stake-pool program's upgrade authority, standing in for the single
/// wallet key that really holds it.
pub const TEST_UPGRADE_AUTHORITY: Pubkey = Pubkey::new_from_array([9u8; 32]);

/// `BPFLoaderUpgradeab1e11111111111111111111111`.
pub const BPF_LOADER_UPGRADEABLE: Pubkey = anchor_lang::solana_program::bpf_loader_upgradeable::ID;

/// Build a `ProgramData` account body.
///
/// A second implementation of the loader's header, independent of the one in
/// `oracle::deployment` -- if they disagree about where the slot or the
/// authority sits, these tests fail rather than quietly agreeing with a
/// mistake. `authority: None` encodes an immutable program.
pub fn program_data_bytes(slot: u64, authority: Option<Pubkey>) -> Vec<u8> {
    let mut data = vec![0u8; 45];
    // bincode enum discriminant: UpgradeableLoaderState::ProgramData is 3.
    data[0..4].copy_from_slice(&3u32.to_le_bytes());
    data[4..12].copy_from_slice(&slot.to_le_bytes());
    match authority {
        Some(key) => {
            data[12] = 1;
            data[13..45].copy_from_slice(key.as_ref());
        }
        None => data[12] = 0,
    }
    data
}

/// Serialized length of an SPL `StakePool` account.
pub const STAKE_POOL_LEN: usize = 611;

/// Build a stake-pool account body.
///
/// This is deliberately a **second, independent** implementation of the layout
/// -- `oracle::native_bcook`'s unit tests have their own. If the two ever
/// disagree about where a field sits, the integration tests fail, which is the
/// point: a layout the program parses one way and the tests build another way
/// is exactly the bug that would otherwise ship silently.
pub fn stake_pool_bytes(
    pool_mint: Pubkey,
    total_lamports: u64,
    pool_token_supply: u64,
    withdrawal_fee_bps: u16,
    epoch: u64,
) -> Vec<u8> {
    // A pool that completed its previous epoch at the same rate it now reports:
    // the ordinary case, and the one a bootstrap should accept.
    stake_pool_bytes_with_history(
        pool_mint,
        total_lamports,
        pool_token_supply,
        withdrawal_fee_bps,
        epoch,
        Some((total_lamports, pool_token_supply)),
    )
}

/// As [`stake_pool_bytes`], but with explicit control of the two `last_epoch_*`
/// fields the pool publishes about itself.
///
/// `None` writes them as zero, which is what a pool that has never completed an
/// epoch looks like -- the one case where a bootstrap has no history to check
/// itself against.
pub fn stake_pool_bytes_with_history(
    pool_mint: Pubkey,
    total_lamports: u64,
    pool_token_supply: u64,
    withdrawal_fee_bps: u16,
    epoch: u64,
    last_epoch: Option<(u64, u64)>,
) -> Vec<u8> {
    let mut data = vec![0u8; STAKE_POOL_LEN];

    // Fixed prefix, offsets summed from the declaration order.
    let off_pool_mint = 1 + 32 * 3 + 1 + 32 * 2; // 162
    let off_total = off_pool_mint + 32 * 3; // 258
    let off_supply = off_total + 8; // 266
    let off_epoch = off_supply + 8; // 274
    let off_epoch_fee = off_epoch + 8 + 48; // 330
    let off_variable = off_epoch_fee + 16; // 346

    data[0] = 1; // AccountType::StakePool
    data[off_pool_mint..off_pool_mint + 32].copy_from_slice(pool_mint.as_ref());
    data[off_total..off_total + 8].copy_from_slice(&total_lamports.to_le_bytes());
    data[off_supply..off_supply + 8].copy_from_slice(&pool_token_supply.to_le_bytes());
    data[off_epoch..off_epoch + 8].copy_from_slice(&epoch.to_le_bytes());

    let write_fee = |data: &mut Vec<u8>, at: usize, bps: u16| {
        data[at..at + 8].copy_from_slice(&10_000u64.to_le_bytes()); // denominator
        data[at + 8..at + 16].copy_from_slice(&(bps as u64).to_le_bytes()); // numerator
    };
    write_fee(&mut data, off_epoch_fee, 1);

    // Variable-width tail. Every tag is the zero-width variant.
    let mut at = off_variable;
    data[at] = 0; // next_epoch_fee: None
    at += 1;
    data[at] = 0; // preferred_deposit_validator: None
    at += 1;
    data[at] = 0; // preferred_withdraw_validator: None
    at += 1;
    write_fee(&mut data, at, 50); // stake_deposit_fee
    at += 16;
    write_fee(&mut data, at, withdrawal_fee_bps); // stake_withdrawal_fee
    at += 16;
    data[at] = 0; // next_stake_withdrawal_fee: None
    at += 1;
    data[at] = 0; // stake_referral_fee
    at += 1;
    data[at] = 0; // sol_deposit_authority: None
    at += 1;
    write_fee(&mut data, at, 50); // sol_deposit_fee
    at += 16;
    data[at] = 0; // sol_referral_fee
    at += 1;
    data[at] = 0; // sol_withdraw_authority: None
    at += 1;
    write_fee(&mut data, at, withdrawal_fee_bps); // sol_withdrawal_fee
    at += 16;
    data[at] = 0; // next_sol_withdrawal_fee: None
    at += 1;

    // The pool's own account of its previous epoch, in declaration order:
    // supply first, then lamports.
    if let Some((last_lamports, last_supply)) = last_epoch {
        data[at..at + 8].copy_from_slice(&last_supply.to_le_bytes());
        data[at + 8..at + 16].copy_from_slice(&last_lamports.to_le_bytes());
    }
    /*
     * The walk ends well short of 611. That is not a mistake in either
     * implementation: spl-stake-pool allocates the account larger than the
     * struct serialises, leaving room for fields a later version might add, so
     * the real account carries trailing slack after `last_epoch_total_lamports`.
     * What matters is that both walks land on the same byte, which is what the
     * cross-check against `native_bcook` proves.
     */
    assert!(
        at + 16 <= STAKE_POOL_LEN,
        "the last_epoch fields must fit inside the account"
    );

    data
}

/// COOK and bCOOK both use 9 decimals.
pub const DECIMALS: u8 = 9;

/// One COOK in base units.
pub const ONE: u64 = 1_000_000_000;

/// A price expressed in thousandths, so tests stay exact: `px(1_200)` is 1.2.
pub fn px(thousandths: u64) -> i128 {
    (thousandths as i128) * 1_000_000_000_000_000
}

/// `whole` tokens in base units. Named for the unit, not the asset, so it does
/// not collide with the `cook` reserve handle tests bind.
pub fn tokens(whole: u64) -> u64 {
    whole * ONE
}

/// Token-2022, which owns every share mint. The liquidity mints (wCOOK, bCOOK)
/// are legacy SPL Token, so the two live side by side in one transaction.
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    anchor_lang::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

pub const ATA_PROGRAM_ID: Pubkey =
    anchor_lang::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// An associated token address. The token program is part of the seeds, so a
/// share ATA and a liquidity ATA for the same owner live at different addresses.
pub fn ata_for(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

/// Liquidity ATA (legacy SPL Token).
pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    ata_for(owner, mint, &TOKEN_PROGRAM_ID)
}

/// Share ATA (Token-2022).
pub fn share_ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    ata_for(owner, mint, &TOKEN_2022_PROGRAM_ID)
}

/// `CreateIdempotent` on the associated-token-account program.
///
/// Hand-rolled rather than taken from a helper crate because the helper in the
/// test toolkit assumes legacy SPL Token, and half the accounts here are
/// Token-2022.
pub fn create_ata_ix(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata_for(owner, mint, token_program), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        // 1 = CreateIdempotent, so calling it twice is not an error.
        data: vec![1],
    }
}

fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &aera::id()).0
}

/// `["market_oracle", oracle]`.
pub fn market_oracle_pda(oracle: Pubkey) -> Pubkey {
    pda(&[aera::state::MarketOracle::SEED, oracle.as_ref()])
}

/// `["risk_config", reserve]`.
///
/// Always passed to `borrow`, whether or not the account exists. It is not an
/// `Option`, because an optional account can be omitted by the caller and the
/// per-wallet borrow cap would then be evadable by simply leaving it out. When
/// no limits are configured this address holds nothing and the program reads it
/// as unlimited.
pub fn risk_config_pda(reserve: Pubkey) -> Pubkey {
    pda(&[aera::state::RiskConfig::SEED, reserve.as_ref()])
}

/// `TransferChecked` on SPL Token or Token-2022.
///
/// Hand-rolled so the adversarial tests can move tokens *around* the protocol
/// rather than through it — which is precisely what a donation attack does.
/// Instruction 12 is `TransferChecked` in both programs.
pub fn spl_transfer_checked_ix(
    token_program: &Pubkey,
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
    decimals: u8,
) -> Instruction {
    let mut data = vec![12u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);

    Instruction {
        program_id: *token_program,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

/// Map kite's result to a String so tests can assert on the program error name
/// embedded in failed-transaction logs.
fn send(
    svm: &mut LiteSVM,
    instructions: Vec<Instruction>,
    signers: &[&Keypair],
    payer: &Pubkey,
) -> Result<(), String> {
    send_transaction_from_instructions(svm, instructions, signers, payer)
        .map_err(|thrown| format!("{thrown:?}"))
}

/// Handle to one reserve and its PDAs.
#[derive(Clone, Copy)]
pub struct ReserveHandle {
    pub mint: Pubkey,
    pub decimals: u8,
    pub reserve: Pubkey,
    pub share_mint: Pubkey,
    pub liquidity_vault: Pubkey,
    pub oracle: Pubkey,
}

pub struct Env {
    pub svm: LiteSVM,
    pub admin: Keypair,
    pub global: Pubkey,
    pub market: Pubkey,
    /// Five guardians; `publish` posts from the first `quorum` of them.
    pub guardians: Vec<Keypair>,
    /// Where the protocol's whole 15% cut is paid.
    pub fee_wallet: Keypair,

    /// True while a v0.1 program is loaded.
    ///
    /// v0.1 has no `refresh_oracle`, so the prelude every value-reading action
    /// needs is different: accrue only. Sending v0.2's prelude to v0.1 fails
    /// with InstructionFallbackNotFound, which is a confusing way to discover
    /// you are talking to the wrong program.
    pub legacy_v0_1: bool,
}

// ---------------------------------------------------------------------------
// Launch configs
// ---------------------------------------------------------------------------

/// Headroom for the mechanics tests.
///
/// The launch caps (1M / 600k / 250k) are a *deployment* parameter, and
/// `test_caps.rs` exercises them explicitly by setting them. Every other suite
/// is testing interest, rounding, liquidation or account validation, and
/// coupling those to whatever the caps happen to be means lowering a cap
/// silently rewrites what a rounding test exercises -- which is what happened
/// when v0.2 reconciled them down by 20x.
///
/// So the harness gives itself room, and the caps are tested where they belong.
pub const HARNESS_SUPPLY_CAP: u64 = 100_000_000_000_000_000; // 100M
pub const HARNESS_BORROW_CAP: u64 = 60_000_000_000_000_000; // 60M
pub const HARNESS_PER_WALLET_CAP: u64 = 100_000_000_000_000_000; // 100M

/// The COOK reserve exactly as PARAMS.md specifies it: borrowable, never
/// collateral. Caps are the harness's, not the launch values -- see above.
pub fn cook_config() -> ReserveConfig {
    ReserveConfig {
        loan_to_value_bps: 0,
        liquidation_threshold_bps: 0,
        liquidation_bonus_bps: DEFAULT_LIQUIDATION_BONUS_BPS,
        close_factor_bps: DEFAULT_CLOSE_FACTOR_BPS,
        collateral_haircut_bps: 0,
        optimal_utilization_bps: DEFAULT_OPTIMAL_UTILIZATION_BPS,
        min_borrow_rate_bps: DEFAULT_MIN_BORROW_RATE_BPS,
        optimal_borrow_rate_bps: DEFAULT_OPTIMAL_BORROW_RATE_BPS,
        max_borrow_rate_bps: DEFAULT_MAX_BORROW_RATE_BPS,
        reserve_factor_bps: DEFAULT_RESERVE_FACTOR_BPS,
        origination_fee_bps: DEFAULT_ORIGINATION_FEE_BPS,
        supply_cap: HARNESS_SUPPLY_CAP,
        borrow_cap: HARNESS_BORROW_CAP,
        per_wallet_supply_cap: HARNESS_PER_WALLET_CAP,
        borrow_enabled: true,
        collateral_enabled: false,
        isolated: false,
        slots_per_year: DEFAULT_SLOTS_PER_YEAR,
    }
}

/// The bCOOK reserve: collateral only, 55/65/8 with a 5% haircut.
pub fn bcook_config() -> ReserveConfig {
    ReserveConfig {
        loan_to_value_bps: DEFAULT_LTV_BPS,
        liquidation_threshold_bps: DEFAULT_LIQUIDATION_THRESHOLD_BPS,
        liquidation_bonus_bps: DEFAULT_LIQUIDATION_BONUS_BPS,
        close_factor_bps: DEFAULT_CLOSE_FACTOR_BPS,
        collateral_haircut_bps: DEFAULT_COLLATERAL_HAIRCUT_BPS,
        optimal_utilization_bps: DEFAULT_OPTIMAL_UTILIZATION_BPS,
        min_borrow_rate_bps: DEFAULT_MIN_BORROW_RATE_BPS,
        optimal_borrow_rate_bps: DEFAULT_OPTIMAL_BORROW_RATE_BPS,
        max_borrow_rate_bps: DEFAULT_MAX_BORROW_RATE_BPS,
        reserve_factor_bps: DEFAULT_RESERVE_FACTOR_BPS,
        origination_fee_bps: DEFAULT_ORIGINATION_FEE_BPS,
        // A collateral-only reserve still gets a supply cap: it bounds how much
        // bCOOK the protocol will hold at all.
        supply_cap: 0,
        borrow_cap: 0,
        per_wallet_supply_cap: 0,
        borrow_enabled: false,
        collateral_enabled: true,
        isolated: false,
        slots_per_year: DEFAULT_SLOTS_PER_YEAR,
    }
}

impl Env {
    pub fn new() -> Self {
        Self::with_program(V0_2_PROGRAM)
    }

    /// An environment running a named program artifact.
    ///
    /// The migration suite uses this to stand up a market on the real v0.1
    /// binary before upgrading underneath it.
    pub fn with_program(program: &[u8]) -> Self {
        // Content equality, not pointer equality: a `&'static [u8]` const can be
        // materialised at more than one address, so comparing pointers silently
        // reported every program as v0.2. One 646 KB memcmp, once per env.
        let legacy = program == V0_1_PROGRAM;
        let mut svm = LiteSVM::new();
        svm.add_program(aera::id(), program).unwrap();

        let admin = create_wallet(&mut svm, 1_000_000_000_000).unwrap();
        let fee_wallet = create_wallet(&mut svm, 1_000_000_000).unwrap();
        let guardians: Vec<Keypair> = (0..5)
            .map(|_| create_wallet(&mut svm, 1_000_000_000).unwrap())
            .collect();

        let global = pda(&[GLOBAL_SEED]);
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::InitGlobal {
                global,
                admin: admin.pubkey(),
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::InitGlobal {
                fee_destination: fee_wallet.pubkey(),
            }
            .data(),
        };
        send(&mut svm, vec![instruction], &[&admin], &admin.pubkey()).unwrap();

        // The quote currency for Aera Core is COOK itself, so a "value" in this
        // program is a COOK amount. A placeholder mint stands in at market
        // creation; the real COOK mint is created by `add_reserve`.
        let quote_mint = create_token_mint(&mut svm, &admin, DECIMALS, None).unwrap();
        let market_id: u64 = 0;
        let market = pda(&[MARKET_SEED, &market_id.to_le_bytes()]);
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::InitMarket {
                global,
                admin: admin.pubkey(),
                market,
                quote_currency_mint: quote_mint,
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::InitMarket {
                market_id,
                name: "Aera Core".to_string(),
            }
            .data(),
        };
        send(&mut svm, vec![instruction], &[&admin], &admin.pubkey()).unwrap();

        let mut env = Env {
            svm,
            admin,
            global,
            market,
            guardians,
            fee_wallet,
            legacy_v0_1: legacy,
        };

        /*
         * The source program's deployment identity is part of the world, not of
         * any one pool.
         *
         * Every native oracle pins the slot at which the stake-pool program was
         * last deployed, so that account has to exist before anything can be
         * observed -- including in the migration suite, which stands up a v0.1
         * market that never touches `set_pool`. Tests about redeploys and
         * authority transfers overwrite it.
         */
        env.set_program_data(TEST_DEPLOY_SLOT, Some(TEST_UPGRADE_AUTHORITY));
        env
    }

    /// Aera Core as launched: COOK at 1.0, bCOOK at `bcook_price` (thousandths).
    pub fn core(bcook_price_thousandths: u64) -> (Self, ReserveHandle, ReserveHandle) {
        let mut env = Env::new();
        let cook = env.add_reserve(DECIMALS, px(1_000), cook_config());
        let bcook = env.add_reserve(DECIMALS, px(bcook_price_thousandths), bcook_config());

        /*
         * COOK is the unit of account and must be priced at exactly 1, with no
         * redemption fee.
         *
         * `add_reserve` gives every reserve a native stake-pool source, which
         * carries TEST_WITHDRAWAL_FEE_BPS. Leaving COOK on it would price the
         * quote asset at 0.98 -- and because debt is denominated in COOK, that
         * would understate every borrower's debt by 2% and make them look
         * healthier than they are. Wrong in the unsafe direction.
         */
        env.set_unit_oracle(cook.mint);
        env.refresh_oracle(cook.mint);

        (env, cook, bcook)
    }

    /// Replace the program at Aera's address, leaving every account untouched.
    ///
    /// This is what a real `solana program deploy --program-id` does: the code
    /// changes, the state does not. It is the whole reason the migration can be
    /// tested honestly -- v0.1 state, written by v0.1, is still sitting there
    /// when v0.2 starts running.
    pub fn upgrade_program(&mut self, program: &[u8]) {
        self.legacy_v0_1 = program == V0_1_PROGRAM;
        self.svm.add_program(aera::id(), program).unwrap();
    }

    // ----- clock -----

    pub fn clock(&self) -> Clock {
        self.svm.get_sysvar::<Clock>()
    }

    pub fn current_slot(&self) -> u64 {
        self.clock().slot
    }

    pub fn unix_timestamp(&self) -> i64 {
        self.clock().unix_timestamp
    }

    /// Advance `slots` slots and the wall clock consistently at 400ms/slot, so
    /// interest accrual and oracle freshness move together the way they do on a
    /// live cluster.
    pub fn warp_slots(&mut self, slots: u64) {
        let target = self.current_slot() + slots;
        self.svm.warp_to_slot(target);
        let mut clock = self.clock();
        clock.slot = target;
        clock.unix_timestamp += (slots as i64 * 4) / 10;
        self.svm.set_sysvar(&clock);
        self.svm.expire_blockhash();
    }

    /// Advance only the wall clock, leaving the slot alone. Used to age oracle
    /// submissions past `max_age_seconds` without accruing interest.
    pub fn warp_seconds(&mut self, seconds: i64) {
        let mut clock = self.clock();
        clock.unix_timestamp += seconds;
        self.svm.set_sysvar(&clock);
        self.svm.expire_blockhash();
    }

    /// Expire the blockhash without moving the clock, so an otherwise identical
    /// transaction gets a fresh signature instead of `AlreadyProcessed`.
    pub fn bump_blockhash(&mut self) {
        self.svm.expire_blockhash();
    }

    pub fn set_last_restart_slot(&mut self, slot: u64) {
        self.svm
            .set_sysvar(&solana_sysvar::last_restart_slot::LastRestartSlot {
                last_restart_slot: slot,
            });
    }

    // ----- oracle -----

    pub fn oracle_address(&self, mint: Pubkey) -> Pubkey {
        pda(&[ORACLE_SEED, self.market.as_ref(), mint.as_ref()])
    }

    /// The `ProgramData` account for the synthetic stake-pool program.
    ///
    /// Derived exactly as the loader derives it, so the program's own
    /// derivation has something real to agree with.
    pub fn stake_pool_program_data(&self) -> Pubkey {
        Pubkey::find_program_address(&[TEST_STAKE_POOL_PROGRAM.as_ref()], &BPF_LOADER_UPGRADEABLE).0
    }

    /// Write the source program's `ProgramData`, i.e. its deployment identity.
    ///
    /// Tests call this with a different slot to simulate a redeploy, or with a
    /// different authority to simulate a transfer.
    pub fn set_program_data(&mut self, slot: u64, authority: Option<Pubkey>) {
        let key = self.stake_pool_program_data();
        let account = SvmAccount {
            lamports: 1_000_000_000,
            data: program_data_bytes(slot, authority),
            owner: BPF_LOADER_UPGRADEABLE,
            executable: false,
            rent_epoch: 0,
        };
        self.svm.set_account(key, account).unwrap();
    }

    /// The synthetic stake pool standing in for BakeYourStake, one per mint.
    ///
    /// Derived rather than random so a test can reach it without threading a
    /// handle around, and so the same mint always maps to the same pool.
    pub fn stake_pool_address(&self, mint: Pubkey) -> Pubkey {
        pda(&[b"test_stake_pool", mint.as_ref()])
    }

    /// Write a stake-pool account directly into the SVM.
    ///
    /// This is the whole reason the oracle is testable end to end: the rate is
    /// derived from bytes in an account, so a test can put any bytes it likes
    /// there and watch what the protocol does. There is no mock oracle in the
    /// program -- the program always parses a real stake-pool layout -- the
    /// test simply controls what that layout says.
    ///
    /// The byte builder is deliberately a second implementation of the layout,
    /// independent of `native_bcook`'s own test helper. If the two disagree
    /// about where a field sits, these tests fail.
    pub fn set_pool(
        &mut self,
        mint: Pubkey,
        total_lamports: u64,
        pool_token_supply: u64,
        withdrawal_fee_bps: u16,
        epoch: u64,
    ) {
        // Make sure the source program has a deployment identity to pin
        // against. Tests about redeploys overwrite it afterwards.
        self.set_pool_with_history(
            mint,
            total_lamports,
            pool_token_supply,
            withdrawal_fee_bps,
            epoch,
            Some((total_lamports, pool_token_supply)),
        );
    }

    /// As [`Env::set_pool`], with explicit control of the pool's published
    /// previous epoch -- what a bootstrap checks its first reading against.
    #[allow(clippy::too_many_arguments)]
    pub fn set_pool_with_history(
        &mut self,
        mint: Pubkey,
        total_lamports: u64,
        pool_token_supply: u64,
        withdrawal_fee_bps: u16,
        epoch: u64,
        last_epoch: Option<(u64, u64)>,
    ) {
        let address = self.stake_pool_address(mint);
        let data = stake_pool_bytes_with_history(
            mint,
            total_lamports,
            pool_token_supply,
            withdrawal_fee_bps,
            epoch,
            last_epoch,
        );
        let account = SvmAccount {
            lamports: 1_000_000_000,
            data,
            owner: TEST_STAKE_POOL_PROGRAM,
            executable: false,
            rent_epoch: 0,
        };
        self.svm.set_account(address, account).unwrap();
    }

    /// Set a mint's rate outright, for test setup.
    ///
    /// Advances the pool's epoch and then re-anchors the breaker, so the new
    /// rate is simply accepted however far it moved. This is what a test means
    /// when it says "the price is now 0.4" -- it is establishing a world, not
    /// exercising the breaker. Tests that want the breaker's opinion use
    /// `move_rate`, which does not re-anchor.
    pub fn set_price(&mut self, mint: Pubkey, mantissa: i128) {
        let epoch = self.read_oracle(mint).last_source_epoch + 1;
        let lamports = u64::try_from(mantissa).expect("price must be non-negative");
        self.set_pool(mint, lamports, POOL_SHARES, TEST_WITHDRAWAL_FEE_BPS, epoch);
        self.reset_breaker(mint);
    }

    /// Move a mint's rate to `thousandths/1000`, advancing the pool's epoch.
    ///
    /// Advancing the epoch is what gives the breaker its allowance, so a test
    /// that moves a rate without advancing gets one epoch of allowance -- which
    /// is the same thing a real pool updating twice in an epoch would produce.
    pub fn set_rate(&mut self, mint: Pubkey, thousandths: u64, epoch: u64) {
        let lamports = u64::try_from(px(thousandths)).expect("rate must be non-negative");
        self.set_pool(mint, lamports, POOL_SHARES, TEST_WITHDRAWAL_FEE_BPS, epoch);
    }

    /// Confirm a bootstrapping oracle, the way waiting an epoch would.
    ///
    /// A native oracle's first observation is provisional: it prices what
    /// already exists but permits no new borrowing until a *later epoch* of the
    /// source agrees with it. This republishes the pool's current figures under
    /// the next epoch number and cranks, which is exactly what a real operator
    /// gets by waiting. The rate is unchanged, so nothing about the market's
    /// economics moves -- only the oracle's confidence in its own anchor.
    pub fn confirm_bootstrap(&mut self, mint: Pubkey) {
        let oracle = self.read_oracle(mint);
        let (lamports, shares, _) = self.read_pool(mint);
        self.set_pool(
            mint,
            lamports,
            shares,
            oracle.reference.withdrawal_fee_bps,
            oracle.last_source_epoch + 1,
        );
        self.refresh_oracle(mint);
    }

    /// The `(total_lamports, pool_token_supply, last_update_epoch)` a mint's
    /// synthetic pool currently holds.
    pub fn read_pool(&self, mint: Pubkey) -> (u64, u64, u64) {
        let account = self
            .svm
            .get_account(&self.stake_pool_address(mint))
            .expect("no stake pool for this mint");
        let at = |o: usize| u64::from_le_bytes(account.data[o..o + 8].try_into().unwrap());
        (at(258), at(266), at(274))
    }

    /// Move the rate and crank, in one step.
    pub fn move_rate(&mut self, mint: Pubkey, thousandths: u64, epoch: u64) {
        self.set_rate(mint, thousandths, epoch);
        self.refresh_oracle(mint);
    }

    /// Point a mint's oracle at its synthetic pool, with the launch bounds.
    pub fn set_oracle(&mut self, mint: Pubkey) {
        let config = aera::instructions::admin::init_oracle::OracleConfig::native(
            TEST_STAKE_POOL_PROGRAM,
            self.stake_pool_address(mint),
            DEFAULT_MAX_WITHDRAWAL_FEE_BPS,
            DEFAULT_RATE_FLOOR,
            DEFAULT_RATE_CEILING,
            TEST_DEPLOY_SLOT,
            TEST_UPGRADE_AUTHORITY,
        );
        self.set_oracle_with(mint, config);
    }

    /// Configure COOK: one COOK is one COOK, no source read.
    pub fn set_unit_oracle(&mut self, mint: Pubkey) {
        let config = aera::instructions::admin::init_oracle::OracleConfig::unit_of_account();
        self.set_oracle_with(mint, config);
    }

    pub fn set_oracle_with(
        &mut self,
        mint: Pubkey,
        config: aera::instructions::admin::init_oracle::OracleConfig,
    ) {
        self.try_set_oracle_with(mint, config).unwrap();
    }

    pub fn try_set_oracle_with(
        &mut self,
        mint: Pubkey,
        config: aera::instructions::admin::init_oracle::OracleConfig,
    ) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let oracle = self.oracle_address(mint);
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::SetOracle {
                global: self.global,
                admin: admin.pubkey(),
                market: self.market,
                oracle,
                mint,
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::SetOracle { config }.data(),
        };
        send(&mut self.svm, vec![instruction], &[&admin], &admin.pubkey())
    }

    /// Set a reserve's per-wallet borrow cap, creating the config if needed.
    ///
    /// A tightening lands immediately; a loosening is queued behind the global
    /// timelock, exactly as `set_params` behaves for `ReserveConfig`.
    pub fn set_risk_config(
        &mut self,
        handle: &ReserveHandle,
        per_wallet_borrow_cap: u64,
    ) -> Result<(), String> {
        self.try_set_risk_config(handle, per_wallet_borrow_cap, 0)
    }

    /// Aera's share of this reserve's liquidation bonus, in bps of value repaid.
    ///
    /// Carved out of `liquidation_bonus_bps`, never added to it.
    pub fn set_protocol_liquidation_share(
        &mut self,
        handle: &ReserveHandle,
        share_bps: u16,
    ) -> Result<(), String> {
        let cap = self
            .read_risk_config(handle)
            .map(|c| c.per_wallet_borrow_cap)
            .unwrap_or(0);
        self.try_set_risk_config(handle, cap, share_bps)
    }

    pub fn try_set_risk_config(
        &mut self,
        handle: &ReserveHandle,
        per_wallet_borrow_cap: u64,
        protocol_liquidation_share_bps: u16,
    ) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::SetRiskConfig {
                global: self.global,
                admin: admin.pubkey(),
                reserve: handle.reserve,
                risk_config: risk_config_pda(handle.reserve),
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::SetRiskConfig {
                per_wallet_borrow_cap,
                protocol_liquidation_share_bps,
            }
            .data(),
        };
        send(&mut self.svm, vec![instruction], &[&admin], &admin.pubkey())
    }

    /// Rewrite a share mint's name, symbol and URI.
    pub fn try_set_share_metadata(
        &mut self,
        handle: &ReserveHandle,
        signer: &Keypair,
        name: &str,
        symbol: &str,
        uri: &str,
    ) -> Result<(), String> {
        let payer = signer.insecure_clone();
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::SetShareMetadata {
                global: self.global,
                admin: payer.pubkey(),
                market: self.market,
                reserve: handle.reserve,
                share_mint: handle.share_mint,
                share_token_program: spl_token_2022_interface::id(),
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::SetShareMetadata {
                metadata: aera::instructions::ShareMetadata {
                    name: name.to_string(),
                    symbol: symbol.to_string(),
                    uri: uri.to_string(),
                },
            }
            .data(),
        };
        send(&mut self.svm, vec![instruction], &[&payer], &payer.pubkey())
    }

    /// The share mint's metadata as the chain holds it: (name, symbol, uri).
    pub fn share_metadata(&self, handle: &ReserveHandle) -> (String, String, String) {
        let account = self
            .svm
            .get_account(&handle.share_mint)
            .expect("share mint");
        let state = spl_token_2022_interface::extension::StateWithExtensions::<
            spl_token_2022_interface::state::Mint,
        >::unpack(&account.data)
        .expect("mint unpacks");
        let md = spl_token_2022_interface::extension::BaseStateWithExtensions::get_variable_len_extension::<
            spl_token_metadata_interface::state::TokenMetadata,
        >(&state)
        .expect("token metadata");
        (md.name, md.symbol, md.uri)
    }

    /// Promote a queued loosening. Permissionless, like `apply_pending_params`.
    pub fn apply_pending_risk_config(&mut self, handle: &ReserveHandle) -> Result<(), String> {
        let payer = self.admin.insecure_clone();
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::ApplyPendingRiskConfig {
                global: self.global,
                reserve: handle.reserve,
                risk_config: risk_config_pda(handle.reserve),
            }
            .to_account_metas(None),
            data: aera::instruction::ApplyPendingRiskConfig {}.data(),
        };
        send(&mut self.svm, vec![instruction], &[&payer], &payer.pubkey())
    }

    /// The reserve's per-wallet borrow cap, or `None` when no config exists.
    pub fn read_risk_config(&self, handle: &ReserveHandle) -> Option<aera::state::RiskConfig> {
        let account = self.svm.get_account(&risk_config_pda(handle.reserve))?;
        if account.data.is_empty() {
            return None;
        }
        aera::state::RiskConfig::try_deserialize(&mut &account.data[..]).ok()
    }

    // -----------------------------------------------------------------
    // Tier 3 market oracle
    // -----------------------------------------------------------------

    /// The right refresh instruction for whatever kind this mint's oracle is.
    ///
    /// Every write path calls `require_fresh`, which demands a refresh in the
    /// same slot -- so the prelude has to know which instruction that is. A
    /// `MarketTwap` oracle takes `refresh_market_oracle` with its two pools and
    /// four vaults; the other two kinds take `refresh_oracle`. Sending the wrong
    /// one fails with `UnknownOracleSource`, which is an unhelpful way to
    /// discover the reserve is Tier 3.
    ///
    /// The SDK's `preludeIxs` has to make exactly this decision, and reads the
    /// pool addresses out of the same `MarketOracle` account.
    pub fn any_refresh_oracle_ix(&self, mint: Pubkey) -> Instruction {
        let oracle = self.oracle_address(mint);
        let is_market = self
            .svm
            .get_account(&oracle)
            .and_then(|a| OracleState::try_deserialize(&mut &a.data[..]).ok())
            .map(|o| o.source_kind == aera::oracle::OracleSourceKind::MarketTwap as u8)
            .unwrap_or(false);
        if !is_market {
            return self.refresh_oracle_ix(mint);
        }

        let market_oracle = self.read_market_oracle(mint);
        // Only the four addresses are used to build the instruction; the
        // orientation is a property of the pool account itself, which the
        // program re-derives, so any value here is inert.
        let pool = |i: usize| damm::MockPool {
            pool: market_oracle.pools[i].pool,
            collateral_vault: market_oracle.pools[i].collateral_vault,
            quote_vault: market_oracle.pools[i].quote_vault,
            collateral_mint: market_oracle.collateral_mint,
            quote_mint: market_oracle.quote_mint,
            orientation: damm::Orientation::CollateralFirst,
        };
        self.refresh_market_oracle_ix(
            mint,
            &pool(0),
            &pool(1),
            market_oracle.config.amm_program_data,
        )
    }

    /// A reserve whose oracle is `MarketTwap` rather than a stake pool.
    ///
    /// `add_reserve` gives every reserve a native source, because that is what
    /// Core uses. Converting afterwards rather than teaching `add_reserve` a
    /// second mode keeps the Core path untouched -- and `set_oracle` clears the
    /// reference whenever the kind changes, so the reserve starts unanchored
    /// exactly as a freshly configured market oracle would.
    pub fn add_market_reserve(&mut self, decimals: u8, config: ReserveConfig) -> ReserveHandle {
        let handle = self.add_reserve(decimals, px(1_000), config);
        self.set_oracle_with(
            handle.mint,
            aera::instructions::admin::init_oracle::OracleConfig::market_twap(),
        );
        handle
    }

    /// Attach pools and thresholds to a `MarketTwap` oracle.
    #[allow(clippy::too_many_arguments)]
    pub fn init_market_oracle(
        &mut self,
        mint: Pubkey,
        collateral_mint: Pubkey,
        quote_mint: Pubkey,
        collateral_decimals: u8,
        quote_decimals: u8,
        pools: [aera::state::PoolRef; 2],
        config: aera::state::MarketOracleConfig,
    ) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let oracle = self.oracle_address(mint);
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::InitMarketOracle {
                global: self.global,
                admin: admin.pubkey(),
                oracle,
                market_oracle: market_oracle_pda(oracle),
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::InitMarketOracle {
                collateral_mint,
                quote_mint,
                collateral_decimals,
                quote_decimals,
                pools,
                config,
            }
            .data(),
        };
        send(&mut self.svm, vec![instruction], &[&admin], &admin.pubkey())
    }

    /// Build a refresh instruction. Deliberately takes no signer parameter --
    /// there is no authority to hold, and `try_refresh_market_oracle_as` proves
    /// any wallet can send it.
    pub fn refresh_market_oracle_ix(
        &self,
        mint: Pubkey,
        pool_a: &damm::MockPool,
        pool_b: &damm::MockPool,
        amm_program_data: Pubkey,
    ) -> Instruction {
        let oracle = self.oracle_address(mint);
        Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::RefreshMarketOracle {
                oracle,
                market_oracle: market_oracle_pda(oracle),
                pool_a: pool_a.pool,
                pool_a_collateral_vault: pool_a.collateral_vault,
                pool_a_quote_vault: pool_a.quote_vault,
                pool_b: pool_b.pool,
                pool_b_collateral_vault: pool_b.collateral_vault,
                pool_b_quote_vault: pool_b.quote_vault,
                amm_program_data,
            }
            .to_account_metas(None),
            data: aera::instruction::RefreshMarketOracle {}.data(),
        }
    }

    /// Refresh, paid for by whoever is passed.
    ///
    /// `payer` exists only to pay the fee. The instruction has no signer, so
    /// this is how a test shows an arbitrary third party can keep the oracle
    /// fresh -- the property that stops Aera being a liveness monopoly.
    pub fn try_refresh_market_oracle_as(
        &mut self,
        payer: &Keypair,
        mint: Pubkey,
        pool_a: &damm::MockPool,
        pool_b: &damm::MockPool,
        amm_program_data: Pubkey,
    ) -> Result<(), String> {
        let ix = self.refresh_market_oracle_ix(mint, pool_a, pool_b, amm_program_data);
        send(&mut self.svm, vec![ix], &[payer], &payer.pubkey())
    }

    pub fn read_market_oracle(&self, mint: Pubkey) -> aera::state::MarketOracle {
        let oracle = self.oracle_address(mint);
        let account = self
            .svm
            .get_account(&market_oracle_pda(oracle))
            .expect("market oracle account");
        aera::state::MarketOracle::try_deserialize(&mut &account.data[..])
            .expect("market oracle decodes")
    }

    /// The permissionless refresh crank.
    pub fn refresh_oracle(&mut self, mint: Pubkey) {
        self.try_refresh_oracle(mint).unwrap();
    }

    pub fn try_refresh_oracle(&mut self, mint: Pubkey) -> Result<(), String> {
        let payer = self.admin.insecure_clone();
        let instruction = self.refresh_oracle_ix(mint);
        /*
         * Two standalone refreshes are byte-identical -- same payer, same
         * accounts, no arguments -- so with an unchanged blockhash the second
         * is rejected as AlreadyProcessed rather than executed. Expiring the
         * blockhash is what a real client gets for free by simply being a
         * moment later.
         */
        self.svm.expire_blockhash();
        send(&mut self.svm, vec![instruction], &[&payer], &payer.pubkey())
    }

    /// The refresh instruction, for tests that batch it with other actions.
    pub fn refresh_oracle_ix(&self, mint: Pubkey) -> Instruction {
        let oracle = self.oracle_address(mint);
        let kind = self
            .svm
            .get_account(&oracle)
            .and_then(|a| OracleState::try_deserialize(&mut &a.data[..]).ok())
            .map(|o| o.source_kind)
            .unwrap_or(1);

        let mut accounts = aera::accounts::RefreshOracle { oracle }.to_account_metas(None);
        // The source account rides in remaining_accounts, and only when the
        // configured kind actually reads one.
        if kind == 1 {
            accounts.push(AccountMeta::new_readonly(
                self.stake_pool_address(mint),
                false,
            ));
            accounts.push(AccountMeta::new_readonly(
                self.stake_pool_program_data(),
                false,
            ));
        }

        Instruction {
            program_id: aera::id(),
            accounts,
            data: aera::instruction::RefreshOracle {}.data(),
        }
    }

    pub fn read_oracle(&self, mint: Pubkey) -> OracleState {
        let address = self.oracle_address(mint);
        let account = self.svm.get_account(&address).unwrap();
        OracleState::try_deserialize(&mut &account.data[..]).unwrap()
    }

    /// Re-anchor a frozen oracle to whatever the pool currently says.
    pub fn reset_breaker(&mut self, mint: Pubkey) {
        self.try_reset_breaker(mint).unwrap();
    }

    pub fn try_reset_breaker(&mut self, mint: Pubkey) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let oracle = self.oracle_address(mint);
        let mut accounts = aera::accounts::ResetOracleBreaker {
            global: self.global,
            market: self.market,
            oracle,
            admin: admin.pubkey(),
        }
        .to_account_metas(None);
        accounts.push(AccountMeta::new_readonly(
            self.stake_pool_address(mint),
            false,
        ));
        accounts.push(AccountMeta::new_readonly(
            self.stake_pool_program_data(),
            false,
        ));

        let instruction = Instruction {
            program_id: aera::id(),
            accounts,
            data: aera::instruction::ResetOracleBreaker {}.data(),
        };
        self.svm.expire_blockhash();
        send(&mut self.svm, vec![instruction], &[&admin], &admin.pubkey())
    }

    // ----- reserves -----

    pub fn add_reserve(
        &mut self,
        decimals: u8,
        price_mantissa: i128,
        config: ReserveConfig,
    ) -> ReserveHandle {
        let admin = self.admin.insecure_clone();
        let mint = create_token_mint(&mut self.svm, &admin, decimals, None).unwrap();

        /*
         * The oracle must exist before `init_reserve` binds to it, and must
         * have been refreshed at least once before anything can be priced.
         *
         * `price_mantissa` is expressed at PRICE_EXPONENT, i.e. a rate scaled
         * by 1e9. The synthetic pool is given a supply of exactly 1e9 base
         * units so that total_lamports / pool_token_supply reproduces that
         * rate exactly, with no rounding.
         */
        self.set_oracle(mint);
        let lamports = u64::try_from(price_mantissa).expect("price must be non-negative");

        /*
         * Two refreshes, one epoch apart, because one is no longer enough.
         *
         * A native oracle's first observation leaves it BOOTSTRAPPING: accepted
         * for valuing what already exists, but unable to permit new borrowing
         * until a later epoch of the source agrees with it. That is deliberate,
         * and a real launch pays the same cost -- configure, wait an epoch,
         * crank again. Seeding at epoch 0 and confirming at epoch 1 leaves the
         * reserve in exactly the state the old single refresh produced, so
         * every test's epoch arithmetic is unchanged.
         */
        self.set_pool(mint, lamports, POOL_SHARES, TEST_WITHDRAWAL_FEE_BPS, 0);
        self.refresh_oracle(mint);
        self.set_pool(mint, lamports, POOL_SHARES, TEST_WITHDRAWAL_FEE_BPS, 1);
        self.refresh_oracle(mint);

        let reserve = pda(&[RESERVE_SEED, self.market.as_ref(), mint.as_ref()]);
        let share_mint = pda(&[SHARE_MINT_SEED, reserve.as_ref()]);
        let liquidity_vault = pda(&[LIQUIDITY_VAULT_SEED, reserve.as_ref()]);
        let oracle = self.oracle_address(mint);

        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::InitReserve {
                global: self.global,
                admin: admin.pubkey(),
                market: self.market,
                reserve,
                liquidity_mint: mint,
                liquidity_vault,
                share_mint,
                oracle,
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
        send(&mut self.svm, vec![instruction], &[&admin], &admin.pubkey()).unwrap();

        ReserveHandle {
            mint,
            decimals,
            reserve,
            share_mint,
            liquidity_vault,
            oracle,
        }
    }

    pub fn read_reserve(&self, handle: &ReserveHandle) -> Reserve {
        let account = self.svm.get_account(&handle.reserve).unwrap();
        Reserve::try_deserialize(&mut &account.data[..]).unwrap()
    }

    pub fn read_global(&self) -> Global {
        let account = self.svm.get_account(&self.global).unwrap();
        Global::try_deserialize(&mut &account.data[..]).unwrap()
    }

    pub fn read_obligation(&self, obligation: Pubkey) -> Obligation {
        let account = self.svm.get_account(&obligation).unwrap();
        Obligation::try_deserialize(&mut &account.data[..]).unwrap()
    }

    pub fn supply_position_address(&self, handle: &ReserveHandle, owner: Pubkey) -> Pubkey {
        pda(&[
            SUPPLY_POSITION_SEED,
            handle.reserve.as_ref(),
            owner.as_ref(),
        ])
    }

    pub fn read_supply_position(&self, handle: &ReserveHandle, owner: Pubkey) -> SupplyPosition {
        let address = self.supply_position_address(handle, owner);
        let account = self.svm.get_account(&address).unwrap();
        SupplyPosition::try_deserialize(&mut &account.data[..]).unwrap()
    }

    // ----- admin -----

    pub fn try_set_params(
        &mut self,
        handle: &ReserveHandle,
        config: ReserveConfig,
    ) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::SetParams {
                global: self.global,
                admin: admin.pubkey(),
                reserve: handle.reserve,
                market: self.market,
            }
            .to_account_metas(None),
            data: aera::instruction::SetParams { config }.data(),
        };
        send(&mut self.svm, vec![instruction], &[&admin], &admin.pubkey())
    }

    pub fn try_apply_pending(&mut self, handle: &ReserveHandle) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::ApplyPendingParams {
                reserve: handle.reserve,
            }
            .to_account_metas(None),
            data: aera::instruction::ApplyPendingParams {}.data(),
        };
        send(&mut self.svm, vec![instruction], &[&admin], &admin.pubkey())
    }

    fn pause_ix(&self, data: Vec<u8>) -> Instruction {
        Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::PauseControl {
                global: self.global,
                admin: self.admin.pubkey(),
            }
            .to_account_metas(None),
            data,
        }
    }

    pub fn pause_borrow(&mut self) {
        self.try_pause_borrow().unwrap();
    }

    pub fn pause_all(&mut self) {
        self.try_pause_all().unwrap();
    }

    pub fn unpause(&mut self) {
        self.try_unpause().unwrap();
    }

    /*
     * Fallible variants, for the fuzz grid.
     *
     * A randomised sequence pauses and resumes at arbitrary moments, and
     * pausing an already-paused protocol is a legitimate refusal rather than a
     * test failure. The blockhash is expired first because two identical pause
     * transactions are deduplicated as AlreadyProcessed, which would make a
     * repeated pause look like it succeeded when it never ran.
     */
    pub fn try_pause_borrow(&mut self) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let ix = self.pause_ix(aera::instruction::PauseBorrow {}.data());
        self.svm.expire_blockhash();
        send(&mut self.svm, vec![ix], &[&admin], &admin.pubkey())
    }

    pub fn try_pause_all(&mut self) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let ix = self.pause_ix(aera::instruction::PauseAll {}.data());
        self.svm.expire_blockhash();
        send(&mut self.svm, vec![ix], &[&admin], &admin.pubkey())
    }

    pub fn try_unpause(&mut self) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let ix = self.pause_ix(aera::instruction::Unpause {}.data());
        self.svm.expire_blockhash();
        send(&mut self.svm, vec![ix], &[&admin], &admin.pubkey())
    }

    // ----- users -----

    /// A bare token mint, with no reserve behind it.
    ///
    /// For tests that exercise an oracle on its own, where standing up a whole
    /// reserve would only add setup that the assertions do not depend on.
    pub fn create_mint(&mut self, decimals: u8) -> Pubkey {
        let admin = self.admin.insecure_clone();
        create_token_mint(&mut self.svm, &admin, decimals, None).unwrap()
    }

    pub fn create_user(&mut self) -> Keypair {
        create_wallet(&mut self.svm, 1_000_000_000_000).unwrap()
    }

    pub fn fund(&mut self, user: &Keypair, mint: Pubkey, amount: u64) -> Pubkey {
        let admin = self.admin.insecure_clone();
        let token_account = self.ensure_ata(user, mint);
        if amount > 0 {
            mint_tokens_to_token_account(&mut self.svm, &mint, &token_account, amount, &admin)
                .unwrap();
        }
        token_account
    }

    pub fn balance(&self, token_account: &Pubkey) -> u64 {
        get_token_account_balance(&self.svm, token_account).unwrap()
    }

    /// The user's associated token account for `mint` under `token_program`,
    /// created if absent.
    ///
    /// Program-aware because share mints are Token-2022 and liquidity mints are
    /// legacy SPL: the token program is part of the ATA seeds, so the two live
    /// at different addresses and are created by different programs.
    pub fn ensure_ata_for(
        &mut self,
        user: &Keypair,
        mint: Pubkey,
        token_program: Pubkey,
    ) -> Pubkey {
        let address = ata_for(&user.pubkey(), &mint, &token_program);
        if self
            .svm
            .get_account(&address)
            .map(|a| a.data.len())
            .unwrap_or(0)
            == 0
        {
            let ix = create_ata_ix(&user.pubkey(), &user.pubkey(), &mint, &token_program);
            send(&mut self.svm, vec![ix], &[user], &user.pubkey()).unwrap();
        }
        address
    }

    /// Liquidity ATA (legacy SPL Token).
    pub fn ensure_ata(&mut self, user: &Keypair, mint: Pubkey) -> Pubkey {
        self.ensure_ata_for(user, mint, TOKEN_PROGRAM_ID)
    }

    /// Share ATA (Token-2022).
    pub fn ensure_share_ata(&mut self, user: &Keypair, mint: Pubkey) -> Pubkey {
        self.ensure_ata_for(user, mint, TOKEN_2022_PROGRAM_ID)
    }

    /// A share ATA for an owner who is not a signer, paid for by the admin.
    ///
    /// The protocol's fee destination is an authority, not a wallet the tests
    /// hold a key for, so its token accounts have to be created this way.
    pub fn ensure_share_ata_for_owner(&mut self, owner: Pubkey, mint: Pubkey) -> Pubkey {
        let address = ata_for(&owner, &mint, &TOKEN_2022_PROGRAM_ID);
        if self
            .svm
            .get_account(&address)
            .map(|a| a.data.len())
            .unwrap_or(0)
            == 0
        {
            let payer = self.admin.insecure_clone();
            let ix = create_ata_ix(&payer.pubkey(), &owner, &mint, &TOKEN_2022_PROGRAM_ID);
            send(&mut self.svm, vec![ix], &[&payer], &payer.pubkey()).unwrap();
        }
        address
    }

    // ----- bare instruction builders -----
    //
    // The `try_*` helpers bundle the accrue/refresh prelude the protocol
    // requires. These return the action alone, so a test can deliberately omit
    // the prelude and prove the staleness guards actually fire.

    pub fn supply_ix(&self, user: &Keypair, handle: &ReserveHandle, amount: u64) -> Instruction {
        Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::Supply {
                global: self.global,
                reserve: handle.reserve,
                liquidity_mint: handle.mint,
                liquidity_vault: handle.liquidity_vault,
                share_mint: handle.share_mint,
                user_liquidity: ata(&user.pubkey(), &handle.mint),
                user_share: share_ata(&user.pubkey(), &handle.share_mint),
                supply_position: self.supply_position_address(handle, user.pubkey()),
                owner: user.pubkey(),
                liquidity_token_program: TOKEN_PROGRAM_ID,
                share_token_program: TOKEN_2022_PROGRAM_ID,
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::Supply {
                liquidity_amount: amount,
            }
            .data(),
        }
    }

    pub fn borrow_ix(
        &self,
        user: &Keypair,
        handle: &ReserveHandle,
        obligation: Pubkey,
        amount: u64,
    ) -> Instruction {
        Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::Borrow {
                global: self.global,
                obligation,
                owner: user.pubkey(),
                reserve: handle.reserve,
                oracle: handle.oracle,
                liquidity_mint: handle.mint,
                liquidity_vault: handle.liquidity_vault,
                user_liquidity: ata(&user.pubkey(), &handle.mint),
                liquidity_token_program: TOKEN_PROGRAM_ID,
                risk_config: risk_config_pda(handle.reserve),
            }
            .to_account_metas(None),
            data: aera::instruction::Borrow {
                liquidity_amount: amount,
            }
            .data(),
        }
    }

    // ----- raw token moves, for adversarial tests -----

    /// A plain SPL transfer straight into an account, bypassing the protocol.
    ///
    /// This is how a donation attack is staged: the vault's token balance moves
    /// and nothing the protocol reads does.
    pub fn transfer_tokens(&mut self, from: &Keypair, mint: Pubkey, to: Pubkey, amount: u64) {
        let source = ata(&from.pubkey(), &mint);
        let ix = spl_transfer_checked_ix(
            &TOKEN_PROGRAM_ID,
            &source,
            &mint,
            &to,
            &from.pubkey(),
            amount,
            DECIMALS,
        );
        send(&mut self.svm, vec![ix], &[from], &from.pubkey()).unwrap();
    }

    /// The same, for a Token-2022 share token.
    pub fn transfer_shares(&mut self, from: &Keypair, mint: Pubkey, to: Pubkey, amount: u64) {
        let source = share_ata(&from.pubkey(), &mint);
        let ix = spl_transfer_checked_ix(
            &TOKEN_2022_PROGRAM_ID,
            &source,
            &mint,
            &to,
            &from.pubkey(),
            amount,
            DECIMALS,
        );
        send(&mut self.svm, vec![ix], &[from], &from.pubkey()).unwrap();
    }

    // ----- deliberately malformed calls -----

    /// Supply with no `accrue` in front of it.
    pub fn try_supply_without_accrue(
        &mut self,
        user: &Keypair,
        handle: &ReserveHandle,
        amount: u64,
    ) -> Result<(), String> {
        // The share ATA must exist first, or the call fails on
        // `AccountNotInitialized` before it ever reaches the staleness check —
        // which would make this test pass for the wrong reason.
        self.ensure_share_ata(user, handle.share_mint);
        // Move a slot on so the reserve is definitely not accrued this slot.
        self.warp_slots(1);
        let ix = self.supply_ix(user, handle, amount);
        send(&mut self.svm, vec![ix], &[user], &user.pubkey())
    }

    /// Borrow with the accrue prelude but no `refresh_obligation`.
    pub fn try_borrow_without_refresh(
        &mut self,
        user: &Keypair,
        handle: &ReserveHandle,
        obligation: Pubkey,
        amount: u64,
        accrue: &[&ReserveHandle],
    ) -> Result<(), String> {
        let mut instructions = self.accrue_all_ixs(accrue);
        instructions.push(self.borrow_ix(user, handle, obligation, amount));
        send(&mut self.svm, instructions, &[user], &user.pubkey())
    }

    /// `refresh_obligation` with a hand-supplied pair list instead of the one
    /// derived from the obligation.
    pub fn try_refresh_with_pairs(
        &mut self,
        obligation: Pubkey,
        pairs: &[&ReserveHandle],
    ) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let mut metas = aera::accounts::RefreshObligation { obligation }.to_account_metas(None);
        for handle in pairs {
            metas.push(AccountMeta::new_readonly(handle.reserve, false));
            metas.push(AccountMeta::new_readonly(handle.oracle, false));
        }
        let refresh = Instruction {
            program_id: aera::id(),
            accounts: metas,
            data: aera::instruction::RefreshObligation {}.data(),
        };

        // Everything named must still be accrued this slot, or the failure would
        // be `ReserveStale` rather than the account-list check under test.
        let mut instructions = self.accrue_all_ixs(pairs);
        instructions.push(refresh);
        send(&mut self.svm, instructions, &[&admin], &admin.pubkey())
    }

    /// `collect_fees` aimed at a wallet that is not the fee destination.
    pub fn try_collect_fees_to(
        &mut self,
        handle: &ReserveHandle,
        destination: &Keypair,
    ) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::CollectFees {
                global: self.global,
                reserve: handle.reserve,
                liquidity_mint: handle.mint,
                liquidity_vault: handle.liquidity_vault,
                fee_token: ata(&destination.pubkey(), &handle.mint),
                liquidity_token_program: TOKEN_PROGRAM_ID,
            }
            .to_account_metas(None),
            data: aera::instruction::CollectFees {}.data(),
        };
        let accrue = self.accrue_ix(handle);
        send(
            &mut self.svm,
            vec![accrue, instruction],
            &[&admin],
            &admin.pubkey(),
        )
    }

    // ----- instruction builders -----

    pub fn accrue_ix(&self, handle: &ReserveHandle) -> Instruction {
        Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::Accrue {
                reserve: handle.reserve,
            }
            .to_account_metas(None),
            data: aera::instruction::Accrue {}.data(),
        }
    }

    /// Send `accrue` on its own, for tests that advance time and then inspect
    /// the reserve without touching an obligation.
    pub fn accrue(&mut self, handle: &ReserveHandle) {
        let admin = self.admin.insecure_clone();
        let ix = self.accrue_ix(handle);
        /*
         * Two standalone accrues are byte-identical -- same payer, same
         * account, no arguments -- so with an unchanged blockhash the second is
         * deduplicated as AlreadyProcessed rather than executed. A test that
         * means to run accrue twice would silently run it once.
         *
         * Expiring the blockhash is what a real client gets for free by being a
         * moment later, and it does not advance the slot, so a test asserting
         * that accrue is a no-op within one slot still tests that.
         */
        self.svm.expire_blockhash();
        send(&mut self.svm, vec![ix], &[&admin], &admin.pubkey()).unwrap();
    }

    /// A reserve's `oracle`, read at its offset rather than by deserializing.
    ///
    /// v0.2 appends `bad_debt` to `Reserve`, so a v0.1 account is sixteen bytes
    /// short and `try_deserialize` refuses it -- which is the point of
    /// appending the field, and which the migration suite depends on. But these
    /// tests also have to *build instructions against* v0.1 reserves, and
    /// `oracle` occupies the same bytes `price_feed` occupied in v0.1: same
    /// offset, same width, same type. Reading it directly works in both eras.
    ///
    /// The offset is derived from the declaration order rather than written as
    /// a number, so adding a field before it breaks this rather than silently
    /// moving it.
    pub fn reserve_oracle(&self, reserve: Pubkey) -> Pubkey {
        const OFF_ORACLE: usize = 8      // discriminator
            + 32                          // market
            + 32                          // liquidity_mint
            + 32                          // liquidity_vault
            + 32; // share_mint
        let account = self.svm.get_account(&reserve).expect("no such reserve");
        let bytes: [u8; 32] = account.data[OFF_ORACLE..OFF_ORACLE + 32]
            .try_into()
            .expect("reserve too short to hold an oracle");
        Pubkey::from(bytes)
    }

    /// Build `refresh_obligation` with the right remaining accounts by reading
    /// the obligation, so tests never hand-maintain the list.
    pub fn refresh_obligation_ix(&self, obligation: Pubkey) -> Instruction {
        let state = self.read_obligation(obligation);
        let mut metas = aera::accounts::RefreshObligation { obligation }.to_account_metas(None);

        let push_pair = |metas: &mut Vec<AccountMeta>, reserve_key: Pubkey| {
            metas.push(AccountMeta::new_readonly(reserve_key, false));
            metas.push(AccountMeta::new_readonly(
                self.reserve_oracle(reserve_key),
                false,
            ));
        };

        for deposit in state.deposits.iter() {
            push_pair(&mut metas, deposit.reserve);
        }
        for borrow in state.borrows.iter() {
            push_pair(&mut metas, borrow.reserve);
        }

        Instruction {
            program_id: aera::id(),
            accounts: metas,
            data: aera::instruction::RefreshObligation {}.data(),
        }
    }

    /// The prelude every value-reading action needs, in the order a real client
    /// must send it: refresh each oracle, then accrue each reserve.
    ///
    /// v0.2 added the first half. `refresh_obligation` now requires every
    /// oracle it reads to have been refreshed in this same transaction, exactly
    /// as it already required every reserve to have been accrued in it — so a
    /// price, like an interest index, can never be one an attacker had a slot
    /// to arrange around.
    pub fn accrue_all_ixs(&self, handles: &[&ReserveHandle]) -> Vec<Instruction> {
        // v0.1 has no oracle to refresh: its prices are published, not derived.
        // Sending v0.2's prelude to it fails with InstructionFallbackNotFound,
        // which is a confusing way to learn you are on the wrong program.
        if self.legacy_v0_1 {
            return self.accrue_only_ixs(handles);
        }
        let mut instructions: Vec<Instruction> = handles
            .iter()
            .map(|h| self.any_refresh_oracle_ix(h.mint))
            .collect();
        instructions.extend(handles.iter().map(|h| self.accrue_ix(h)));
        instructions
    }

    /// The same prelude without the oracle refresh.
    ///
    /// For tests that need to prove an un-refreshed oracle actually blocks an
    /// action, rather than assuming it.
    pub fn accrue_only_ixs(&self, handles: &[&ReserveHandle]) -> Vec<Instruction> {
        handles.iter().map(|h| self.accrue_ix(h)).collect()
    }

    // ----- actions -----

    pub fn try_supply(
        &mut self,
        user: &Keypair,
        handle: &ReserveHandle,
        amount: u64,
    ) -> Result<Pubkey, String> {
        let user_liquidity = ata(&user.pubkey(), &handle.mint);
        let user_share = self.ensure_share_ata(user, handle.share_mint);

        let supply = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::Supply {
                global: self.global,
                reserve: handle.reserve,
                liquidity_mint: handle.mint,
                liquidity_vault: handle.liquidity_vault,
                share_mint: handle.share_mint,
                user_liquidity,
                user_share,
                supply_position: self.supply_position_address(handle, user.pubkey()),
                owner: user.pubkey(),
                liquidity_token_program: TOKEN_PROGRAM_ID,
                share_token_program: TOKEN_2022_PROGRAM_ID,
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::Supply {
                liquidity_amount: amount,
            }
            .data(),
        };
        let accrue = self.accrue_ix(handle);
        send(&mut self.svm, vec![accrue, supply], &[user], &user.pubkey())?;
        Ok(user_share)
    }

    pub fn supply(&mut self, user: &Keypair, handle: &ReserveHandle, amount: u64) -> Pubkey {
        self.try_supply(user, handle, amount).unwrap()
    }

    pub fn try_withdraw(
        &mut self,
        user: &Keypair,
        handle: &ReserveHandle,
        share_amount: u64,
    ) -> Result<(), String> {
        let withdraw = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::Withdraw {
                global: self.global,
                reserve: handle.reserve,
                liquidity_mint: handle.mint,
                liquidity_vault: handle.liquidity_vault,
                share_mint: handle.share_mint,
                user_liquidity: ata(&user.pubkey(), &handle.mint),
                user_share: share_ata(&user.pubkey(), &handle.share_mint),
                supply_position: self.supply_position_address(handle, user.pubkey()),
                owner: user.pubkey(),
                liquidity_token_program: TOKEN_PROGRAM_ID,
                share_token_program: TOKEN_2022_PROGRAM_ID,
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::Withdraw { share_amount }.data(),
        };
        let accrue = self.accrue_ix(handle);
        send(
            &mut self.svm,
            vec![accrue, withdraw],
            &[user],
            &user.pubkey(),
        )
    }

    pub fn init_obligation(&mut self, user: &Keypair) -> Pubkey {
        let obligation = pda(&[
            OBLIGATION_SEED,
            self.market.as_ref(),
            user.pubkey().as_ref(),
        ]);
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::InitObligation {
                market: self.market,
                obligation,
                owner: user.pubkey(),
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::InitObligation {}.data(),
        };
        send(&mut self.svm, vec![instruction], &[user], &user.pubkey()).unwrap();
        obligation
    }

    /// `init_obligation`, returning the error instead of unwrapping.
    ///
    /// Exists so a test can assert that a second obligation for the same wallet
    /// in the same market is refused -- the property the per-wallet borrow cap
    /// rests on, since a wallet that could hold two obligations could split its
    /// debt across them and never trip the cap.
    pub fn try_open_obligation(&mut self, user: &Keypair) -> Result<Pubkey, String> {
        let obligation = pda(&[
            OBLIGATION_SEED,
            self.market.as_ref(),
            user.pubkey().as_ref(),
        ]);
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::InitObligation {
                market: self.market,
                obligation,
                owner: user.pubkey(),
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::InitObligation {}.data(),
        };
        send(&mut self.svm, vec![instruction], &[user], &user.pubkey()).map(|()| obligation)
    }

    pub fn obligation_share_vault(&self, handle: &ReserveHandle, obligation: Pubkey) -> Pubkey {
        pda(&[
            OBLIGATION_SHARE_VAULT_SEED,
            handle.reserve.as_ref(),
            obligation.as_ref(),
        ])
    }

    pub fn try_deposit_collateral(
        &mut self,
        user: &Keypair,
        handle: &ReserveHandle,
        obligation: Pubkey,
        share_amount: u64,
    ) -> Result<(), String> {
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::DepositCollateral {
                global: self.global,
                obligation,
                owner: user.pubkey(),
                reserve: handle.reserve,
                share_mint: handle.share_mint,
                obligation_share_vault: self.obligation_share_vault(handle, obligation),
                user_share: share_ata(&user.pubkey(), &handle.share_mint),
                share_token_program: TOKEN_2022_PROGRAM_ID,
                system_program: system_program::id(),
            }
            .to_account_metas(None),
            data: aera::instruction::DepositCollateral { share_amount }.data(),
        };
        send(&mut self.svm, vec![instruction], &[user], &user.pubkey())
    }

    pub fn try_withdraw_collateral(
        &mut self,
        user: &Keypair,
        handle: &ReserveHandle,
        obligation: Pubkey,
        share_amount: u64,
        accrue: &[&ReserveHandle],
    ) -> Result<(), String> {
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::WithdrawCollateral {
                global: self.global,
                obligation,
                owner: user.pubkey(),
                reserve: handle.reserve,
                oracle: handle.oracle,
                share_mint: handle.share_mint,
                obligation_share_vault: self.obligation_share_vault(handle, obligation),
                user_share: share_ata(&user.pubkey(), &handle.share_mint),
                share_token_program: TOKEN_2022_PROGRAM_ID,
            }
            .to_account_metas(None),
            data: aera::instruction::WithdrawCollateral { share_amount }.data(),
        };
        let mut instructions = self.accrue_all_ixs(accrue);
        instructions.push(self.refresh_obligation_ix(obligation));
        instructions.push(instruction);
        send(&mut self.svm, instructions, &[user], &user.pubkey())
    }

    pub fn try_borrow(
        &mut self,
        user: &Keypair,
        handle: &ReserveHandle,
        obligation: Pubkey,
        amount: u64,
        accrue: &[&ReserveHandle],
    ) -> Result<(), String> {
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::Borrow {
                global: self.global,
                obligation,
                owner: user.pubkey(),
                reserve: handle.reserve,
                oracle: handle.oracle,
                liquidity_mint: handle.mint,
                liquidity_vault: handle.liquidity_vault,
                user_liquidity: ata(&user.pubkey(), &handle.mint),
                liquidity_token_program: TOKEN_PROGRAM_ID,
                risk_config: risk_config_pda(handle.reserve),
            }
            .to_account_metas(None),
            data: aera::instruction::Borrow {
                liquidity_amount: amount,
            }
            .data(),
        };
        let mut instructions = self.accrue_all_ixs(accrue);
        instructions.push(self.refresh_obligation_ix(obligation));
        instructions.push(instruction);
        send(&mut self.svm, instructions, &[user], &user.pubkey())
    }

    pub fn try_repay(
        &mut self,
        user: &Keypair,
        handle: &ReserveHandle,
        obligation: Pubkey,
        amount: u64,
    ) -> Result<(), String> {
        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::Repay {
                obligation,
                reserve: handle.reserve,
                liquidity_mint: handle.mint,
                liquidity_vault: handle.liquidity_vault,
                user_liquidity: ata(&user.pubkey(), &handle.mint),
                repayer: user.pubkey(),
                liquidity_token_program: TOKEN_PROGRAM_ID,
            }
            .to_account_metas(None),
            data: aera::instruction::Repay {
                liquidity_amount: amount,
            }
            .data(),
        };
        let accrue = self.accrue_ix(handle);
        send(
            &mut self.svm,
            vec![accrue, instruction],
            &[user],
            &user.pubkey(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn try_liquidate(
        &mut self,
        liquidator: &Keypair,
        repay: &ReserveHandle,
        collateral: &ReserveHandle,
        obligation: Pubkey,
        amount: u64,
    ) -> Result<(), String> {
        let destination = self.protocol_collateral_dest(collateral);
        self.try_liquidate_to(
            liquidator,
            repay,
            collateral,
            obligation,
            amount,
            destination,
        )
    }

    /// Aera's token account for a collateral's share mint: the ATA of
    /// `global.fee_destination`. Created on demand, as an operator would.
    pub fn protocol_collateral_dest(&mut self, collateral: &ReserveHandle) -> Pubkey {
        // An operator does exactly this once per collateral asset, before
        // enabling the share. The destination is an authority, so it holds one
        // account per collateral and needs no further configuration.
        let owner = self.read_global().fee_destination;
        self.ensure_share_ata_for_owner(owner, collateral.share_mint)
    }

    /// Liquidate naming an explicit protocol destination, so a test can pass a
    /// wrong one.
    #[allow(clippy::too_many_arguments)]
    pub fn try_liquidate_to(
        &mut self,
        liquidator: &Keypair,
        repay: &ReserveHandle,
        collateral: &ReserveHandle,
        obligation: Pubkey,
        amount: u64,
        protocol_collateral_dest: Pubkey,
    ) -> Result<(), String> {
        let risk_config = risk_config_pda(collateral.reserve);
        self.try_liquidate_with_risk_config(
            liquidator,
            repay,
            collateral,
            obligation,
            amount,
            protocol_collateral_dest,
            risk_config,
        )
    }

    /// Liquidate naming both the protocol destination and the risk config, so a
    /// test can substitute either.
    #[allow(clippy::too_many_arguments)]
    pub fn try_liquidate_with_risk_config(
        &mut self,
        liquidator: &Keypair,
        repay: &ReserveHandle,
        collateral: &ReserveHandle,
        obligation: Pubkey,
        amount: u64,
        protocol_collateral_dest: Pubkey,
        collateral_risk_config: Pubkey,
    ) -> Result<(), String> {
        let liquidator_collateral_dest = self.ensure_share_ata(liquidator, collateral.share_mint);

        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::Liquidate {
                global: self.global,
                obligation,
                liquidator: liquidator.pubkey(),
                repay_reserve: repay.reserve,
                collateral_reserve: collateral.reserve,
                repay_oracle: repay.oracle,
                collateral_oracle: collateral.oracle,
                repay_liquidity_mint: repay.mint,
                collateral_share_mint: collateral.share_mint,
                repay_liquidity_vault: repay.liquidity_vault,
                obligation_collateral_vault: self.obligation_share_vault(collateral, obligation),
                liquidator_repay_source: ata(&liquidator.pubkey(), &repay.mint),
                liquidator_collateral_dest,
                collateral_risk_config,
                protocol_collateral_dest,
                liquidity_token_program: TOKEN_PROGRAM_ID,
                share_token_program: TOKEN_2022_PROGRAM_ID,
            }
            .to_account_metas(None),
            data: aera::instruction::Liquidate {
                liquidity_amount: amount,
            }
            .data(),
        };
        let mut instructions = self.accrue_all_ixs(&[repay, collateral]);
        instructions.push(self.refresh_obligation_ix(obligation));
        instructions.push(instruction);
        send(
            &mut self.svm,
            instructions,
            &[liquidator],
            &liquidator.pubkey(),
        )
    }

    pub fn try_collect_fees(&mut self, handle: &ReserveHandle) -> Result<(), String> {
        let admin = self.admin.insecure_clone();
        let fee_token = ata(&self.fee_wallet.pubkey(), &handle.mint);

        let instruction = Instruction {
            program_id: aera::id(),
            accounts: aera::accounts::CollectFees {
                global: self.global,
                reserve: handle.reserve,
                liquidity_mint: handle.mint,
                liquidity_vault: handle.liquidity_vault,
                fee_token,
                liquidity_token_program: TOKEN_PROGRAM_ID,
            }
            .to_account_metas(None),
            data: aera::instruction::CollectFees {}.data(),
        };
        let accrue = self.accrue_ix(handle);
        send(
            &mut self.svm,
            vec![accrue, instruction],
            &[&admin],
            &admin.pubkey(),
        )
    }

    /// Post collateral end-to-end: supply bCOOK, then lock the shares.
    pub fn open_position(&mut self, user: &Keypair, bcook: &ReserveHandle, amount: u64) -> Pubkey {
        let obligation = self.init_obligation(user);
        let shares = self.supply(user, bcook, amount);
        let share_balance = self.balance(&shares);
        self.try_deposit_collateral(user, bcook, obligation, share_balance)
            .unwrap();
        obligation
    }
}

/// Assert a failed transaction mentions a specific Anchor error variant.
///
/// Generic over the success type so it accepts every `try_*` helper, including
/// the ones that hand back an address on success.
pub fn assert_error<T>(result: Result<T, String>, expected: &str) {
    match result {
        Ok(_) => panic!("expected {expected}, but the transaction succeeded"),
        Err(message) => assert!(
            message.contains(expected),
            "expected {expected}, got: {message}"
        ),
    }
}
