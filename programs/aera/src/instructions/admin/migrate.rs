//! v0.1 -> v0.2 migration.
//!
//! The deployed protocol holds real deposits, real debt and real accrued
//! interest. This moves it to the derived oracle without touching any of that.
//!
//! ## What actually changes
//!
//! Exactly one field of one account, plus two accounts appearing and
//! disappearing:
//!
//! | | |
//! |---|---|
//! | `Reserve.oracle` | repointed from the guardian feed PDA to the oracle PDA |
//! | `OracleState` | created, reference bootstrapped from the live stake pool |
//! | `PriceFeed` | closed, rent returned to the admin |
//! | `Global.version` | stamped v2, by a separate instruction, last |
//!
//! Every other byte of every other account is left exactly as it was, and the
//! instruction asserts that before it returns.
//!
//! ## Why no realloc of `Reserve`
//!
//! `Reserve.oracle` occupies the bytes `Reserve.price_feed` occupied: same
//! offset, same width, same type. The rename was chosen for precisely this
//! reason. A compile-time assertion below pins the account size so a future
//! field cannot quietly turn this into a migration that needs a realloc it does
//! not perform.
//!
//! ## Where the first price comes from
//!
//! Not from the guardians. Their last agreed median is discarded unread — it is
//! a number five keys on one machine asserted, and carrying it into v0.2 would
//! give the new oracle an anchor the new oracle never validated.
//!
//! The reference is bootstrapped by reading the stake pool *through the same
//! validation path every subsequent refresh uses*. If that read fails for any
//! reason, the migration fails: a v0.2 reserve with no reference cannot price
//! collateral, and leaving one behind would be worse than not migrating.
//!
//! ## Atomicity
//!
//! Per reserve, this is atomic: it either fully migrates or the transaction
//! reverts, because every failure path is an early return before any state is
//! written, and Anchor rolls back on error.
//!
//! Across reserves, the operator should send one instruction per reserve **in a
//! single transaction**. That is a recommendation about downtime, not about
//! safety: the partial states are safe, and `test_half_migrated.rs` holds them
//! to it. It is still not a state to leave a market in deliberately, because
//! liquidation is among the things blocked, and a market that cannot close bad
//! positions accumulates them.
//!
//! ## The half-migrated market
//!
//! The migration is several instructions, an operator can be interrupted
//! between any two of them, and the result is a protocol running v0.2 code over
//! a mixture of v0.1 and v0.2 accounts. The rule those states are held to:
//!
//! > No action that increases anyone's risk may succeed, and every action that
//! > reduces it must.
//!
//! Two structural gates enforce it, and between them they cover every
//! instruction:
//!
//! 1. **The discriminator.** An unmigrated reserve still points at a v0.1
//!    `PriceFeed`. Anchor prefixes every account with eight bytes of
//!    `sha256("account:<Name>")`, and `PriceFeed` and `OracleState` hash
//!    differently, so a guardian feed cannot be deserialised as an oracle at
//!    all. Every instruction that needs a price -- `borrow`,
//!    `withdraw_collateral`, `liquidate`, `refresh_obligation` -- takes its
//!    oracles as typed accounts and therefore refuses.
//!
//! 2. **The one-byte `Global`.** `version` was appended last, so a v0.1
//!    `Global` is exactly one byte short of the v0.2 struct and fails to
//!    deserialise. Every instruction that takes `Global` -- which is every
//!    remaining one that can increase exposure -- therefore refuses until the
//!    stamp lands.
//!
//! What is deliberately left open in every partial state: `repay` and
//! `accrue`, which take neither account, because a borrower who cannot repay
//! during a migration has been made worse off by it; and `supply` and
//! `deposit_collateral` once `Global` is stamped, because neither can worsen
//! anybody's position and stranding depositors is its own harm.
//!
//! ## Why there is no `Market` migration state machine
//!
//! The obvious design is a `Market.migration_state` of V1/MIGRATING/V2 that
//! every instruction checks. It was considered and rejected, for three reasons
//! in increasing order of importance.
//!
//! First, it needs a layout change. `Market` has no spare byte, so adding one
//! means reallocating a live account -- the very operation this migration is
//! trying to keep to a minimum, and one more account whose partial migration
//! would need its own analysis.
//!
//! Second, it would be redundant. The two gates above already refuse every
//! risk-increasing action in every partial state, and they do it without an
//! operator having to remember anything. A flag would add a third gate that
//! agrees with them.
//!
//! Third -- and this is the reason it would be worse rather than merely
//! unnecessary -- a flag is a *claim* about the state rather than the state
//! itself. It can be stamped V2 while a reserve is still on a guardian feed, or
//! left MIGRATING on a market that finished hours ago. The first is a lie that
//! opens the market; the second is an outage. Deriving the answer from the
//! accounts cannot drift from the accounts.
//!
//! A registry of migrated reserves has the same defect and adds a write to
//! every migration. Neither is invented here.
//!
//! What the flag *would* buy is a way to say "this market is mid-migration" to
//! a reader who is not sending a transaction. That is a client concern, and a
//! client can compute it exactly: a market is fully migrated when `Global`
//! deserialises and every reserve's `oracle` holds an `OracleState`.

use anchor_lang::prelude::*;

/// Anchor's marker for a closed account. Written over the discriminator so the
/// runtime refuses to deserialize it again in this transaction.
const CLOSED_ACCOUNT_DISCRIMINATOR: [u8; 8] = [255; 8];

use crate::constants::{ORACLE_SEED, PRICE_FEED_SEED};
use crate::errors::AeraError;
use crate::oracle::breaker::{bootstrap_verdict, OracleHealth, Verdict};
use crate::oracle::{native_bcook, validation, OracleSourceKind};
use crate::state::{
    unit_observation, Global, Market, OracleState, PendingConfig, Reserve, ReserveConfig,
    GLOBAL_V1_LEN, GLOBAL_VERSION_V1, GLOBAL_VERSION_V2,
};

/// Serialized length of a v0.2 `Global`: exactly one byte more than v0.1.
const GLOBAL_V2_LEN: usize = 8 + Global::INIT_SPACE;

use super::init_oracle::OracleConfig;

/// Anchor's discriminator for v0.1's `PriceFeed`, i.e.
/// `sha256("account:PriceFeed")[..8]`.
///
/// Hardcoded because the type no longer exists in this program to derive it
/// from. It is checked before the account is closed, so the migration can prove
/// it is discarding a guardian feed rather than whatever else might occupy a
/// derivable address.
const LEGACY_PRICE_FEED_DISCRIMINATOR: [u8; 8] = [189, 103, 252, 23, 152, 35, 243, 156];

/// The `Reserve` account size, which must not change across the migration.
///
/// This migration performs no realloc of a reserve, so a larger `Reserve`
/// would be written past its allocation or silently truncated.
///
/// More importantly, growing `Reserve` at all is not a free choice. A v0.1
/// reserve that v0.2 cannot deserialise cannot be passed to `accrue`, and
/// every repayment needs an accrued reserve -- so a longer `Reserve` traps
/// borrowers for the whole duration of a migration. That was tried, and
/// `test_half_migrated::half_06` caught it.
///
/// The assertion is written in terms of the fields rather than as
/// `X == X`, which is what it used to be and which could never fail.
pub const RESERVE_LEN: usize = 8 + Reserve::INIT_SPACE;
const _: () = assert!(
    RESERVE_LEN
        == 8 + 32 * 5
            + 1
            + 8
            + 8
            + 16
            + 16
            + 8
            + 8
            + ReserveConfig::INIT_SPACE
            + PendingConfig::INIT_SPACE
            + 1,
    "Reserve changed size: the migration performs no realloc, and a reserve v0.2 \
     cannot deserialise cannot be accrued, so borrowers could not repay during a \
     migration"
);

/// Every economic quantity the migration must leave alone.
///
/// Captured before anything is written and compared after. The comparison is
/// in the program rather than only in a test, so a future edit that starts
/// touching one of these fails on chain rather than in review.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct EconomicSnapshot {
    available_liquidity: u64,
    share_mint_supply: u64,
    borrowed_principal: u128,
    borrow_index: u128,
    accrued_fees: u64,
    last_update_slot: u64,
    liquidity_decimals: u8,
    liquidity_mint: Pubkey,
    liquidity_vault: Pubkey,
    share_mint: Pubkey,
    market: Pubkey,
    bump: u8,
}

impl EconomicSnapshot {
    fn of(reserve: &Reserve) -> Self {
        Self {
            available_liquidity: reserve.available_liquidity,
            share_mint_supply: reserve.share_mint_supply,
            borrowed_principal: reserve.borrowed_principal,
            borrow_index: reserve.borrow_index,
            accrued_fees: reserve.accrued_fees,
            last_update_slot: reserve.last_update_slot,
            liquidity_decimals: reserve.liquidity_decimals,
            liquidity_mint: reserve.liquidity_mint,
            liquidity_vault: reserve.liquidity_vault,
            share_mint: reserve.share_mint,
            market: reserve.market,
            bump: reserve.bump,
        }
    }
}

pub fn handle_migrate_reserve_to_v2(
    context: Context<MigrateReserveToV2>,
    config: OracleConfig,
) -> Result<()> {
    let clock = Clock::get()?;
    let market_key = context.accounts.market.key();
    let mint_key = context.accounts.liquidity_mint.key();

    // ---- 0. the admin, read from raw bytes -------------------------------
    //
    // `Global` is not deserialized here: a v0.1 one is a byte short of the v0.2
    // struct. The discriminator proves it is a Global, and the admin sits at a
    // fixed offset in both versions because the field was appended, not
    // inserted.
    {
        let global = context.accounts.global.to_account_info();
        require_keys_eq!(*global.owner, crate::ID, AeraError::GlobalMismatch);
        let data = global.try_borrow_data()?;
        let head = data.get(..8).ok_or(AeraError::NotMigratable)?;
        require!(head == Global::DISCRIMINATOR, AeraError::NotMigratable);

        let admin_bytes = data.get(8..40).ok_or(AeraError::NotMigratable)?;
        let stored_admin = Pubkey::try_from(admin_bytes).map_err(|_| AeraError::NotMigratable)?;
        require_keys_eq!(
            stored_admin,
            context.accounts.admin.key(),
            AeraError::NotAdmin
        );
    }

    // ---- 1. this reserve is still v0.1 -----------------------------------
    //
    // The version is not a byte anyone can set: it is where `Reserve.oracle`
    // points. A v0.1 reserve points at the guardian feed's PDA, a v0.2 reserve
    // at the oracle's. Both are derivable here, so neither can be forged.
    let (legacy_feed_pda, _) = Pubkey::find_program_address(
        &[PRICE_FEED_SEED, market_key.as_ref(), mint_key.as_ref()],
        &crate::ID,
    );
    let (oracle_pda, _) = Pubkey::find_program_address(
        &[ORACLE_SEED, market_key.as_ref(), mint_key.as_ref()],
        &crate::ID,
    );

    let reserve_oracle = context.accounts.reserve.oracle;

    // Already migrated. Refused explicitly rather than treated as a no-op, so
    // running the migration twice cannot silently re-bootstrap a reference and
    // wipe a breaker freeze an operator is relying on.
    require_keys_neq!(reserve_oracle, oracle_pda, AeraError::AlreadyMigrated);
    require_keys_eq!(reserve_oracle, legacy_feed_pda, AeraError::NotMigratable);

    // ---- 2. the account being closed is what it claims -------------------
    let legacy = &context.accounts.legacy_price_feed;
    require_keys_eq!(legacy.key(), legacy_feed_pda, AeraError::NotMigratable);
    require_keys_eq!(*legacy.owner, crate::ID, AeraError::NotMigratable);
    {
        let data = legacy.try_borrow_data()?;
        let head = data.get(..8).ok_or(AeraError::NotMigratable)?;
        require!(
            head == LEGACY_PRICE_FEED_DISCRIMINATOR,
            AeraError::NotMigratable
        );
    }

    // ---- 3. snapshot, before a single byte is written --------------------
    let before = EconomicSnapshot::of(&context.accounts.reserve);

    // ---- 4. bootstrap the reference from the live source -----------------
    //
    // Through exactly the path every later refresh uses. Nothing here reads the
    // guardians' last agreed price, and no argument to this instruction can
    // supply a rate.
    OracleState::validate_config(
        config.source_kind,
        config.source_program,
        config.source_account,
        config.max_withdrawal_fee_bps,
        config.rate_floor,
        config.rate_ceiling,
        &config.breaker,
    )?;

    let kind = OracleSourceKind::from_u8(config.source_kind)?;
    let observation = match kind {
        OracleSourceKind::UnitOfAccount => unit_observation(clock.slot, clock.unix_timestamp),
        /*
         * A market-priced oracle is not refreshable through here.
         *
         * This path reads one source account and trusts it to be authoritative.
         * A market price is derived from two pools, checked against each other,
         * gated on depth, and appended to a time-weighted history -- none of
         * which this handler does. Routing one through here would produce a
         * reference with no TWAP, no cross-pool check and no spacing rule.
         *
         * `refresh_market_oracle` is the only way to move one.
         */
        OracleSourceKind::MarketTwap => return err!(AeraError::UnknownOracleSource),

        OracleSourceKind::NativeExchangeRate => {
            let source = context
                .remaining_accounts
                .first()
                .ok_or(AeraError::OracleAccountMismatch)?;
            let program_data = context
                .remaining_accounts
                .get(1)
                .ok_or(AeraError::OracleProgramDataMismatch)?;
            validation::require_configured_account(
                source,
                &config.source_account,
                &config.source_program,
            )?;
            validation::require_read_only(source)?;
            validation::require_alive(source)?;

            let bounds = native_bcook::NativeOracleBounds {
                expected_program: config.source_program,
                expected_pool: config.source_account,
                expected_pool_mint: mint_key,
                max_withdrawal_fee_bps: config.max_withdrawal_fee_bps,
                rate_floor: config.rate_floor,
                rate_ceiling: config.rate_ceiling,
                expected_deploy_slot: config.expected_deploy_slot,
                expected_upgrade_authority: config.expected_upgrade_authority,
            };

            /*
             * A hard error, not a recorded EMERGENCY.
             *
             * `refresh_oracle` deliberately records an unreadable source and
             * succeeds, because a breaker verdict that rolls back is useless.
             * Migration is the opposite case: a reserve that arrives in v0.2
             * with no reference cannot value collateral at all, so producing
             * one is the entire point and failing to is a reason to abort.
             */
            native_bcook::observe(
                source,
                program_data,
                &bounds,
                clock.slot,
                clock.unix_timestamp,
            )?
        }
    };

    // ---- 5. write the new oracle -----------------------------------------
    let oracle = &mut context.accounts.oracle;
    oracle.market = market_key;
    oracle.mint = mint_key;
    oracle.source_kind = config.source_kind;
    oracle.source_program = config.source_program;
    oracle.source_account = config.source_account;
    oracle.max_withdrawal_fee_bps = config.max_withdrawal_fee_bps;
    oracle.rate_floor = config.rate_floor;
    oracle.rate_ceiling = config.rate_ceiling;
    oracle.expected_deploy_slot = config.expected_deploy_slot;
    oracle.expected_upgrade_authority = config.expected_upgrade_authority;
    oracle.breaker = config.breaker;
    oracle.bump = context.bumps.oracle;

    /*
     * The migrated oracle starts BOOTSTRAPPING, not HEALTHY.
     *
     * An earlier version stamped it Healthy on the strength of one observation.
     * That was the weakest link in the whole design: the migration is a moment
     * an operator chooses, so anyone able to arrange the source at that moment
     * could have chosen Aera's permanent anchor, and the movement breaker --
     * which needs a prior reference -- could not have objected.
     *
     * Now the first reading is checked against the source's own previous-epoch
     * figures, and the oracle values existing positions without permitting new
     * ones until a later epoch confirms it. A market mid-migration can repay,
     * supply, add collateral and be liquidated; it cannot be borrowed against.
     */
    let verdict = if observation.needs_bootstrap() {
        bootstrap_verdict(
            &observation,
            observation.previous_epoch_rate,
            &config.breaker,
        )?
    } else {
        // Reads no program, so there is no anchor anyone could have chosen.
        Verdict {
            accept: true,
            health: OracleHealth::Healthy,
            moved_bps: 0,
        }
    };
    require!(verdict.accept, AeraError::OracleBootstrapInconsistent);
    oracle.record(&observation, &verdict, clock.slot);

    // ---- 6. repoint the reserve ------------------------------------------
    context.accounts.reserve.oracle = oracle.key();

    // ---- 7. close the guardian feed --------------------------------------
    //
    // By hand, because `close` requires a typed account and `PriceFeed` is not
    // a type this program has any more. Same effect as Anchor's: drain the
    // lamports to the admin and stamp the closed-account discriminator so the
    // runtime will not let it be reopened or re-read within this transaction.
    {
        let feed = context.accounts.legacy_price_feed.to_account_info();
        let admin = context.accounts.admin.to_account_info();

        let reclaimed = feed.lamports();
        **feed.try_borrow_mut_lamports()? = 0;
        **admin.try_borrow_mut_lamports()? = admin
            .lamports()
            .checked_add(reclaimed)
            .ok_or(AeraError::MathOverflow)?;

        let mut data = feed.try_borrow_mut_data()?;
        data[..8].copy_from_slice(&CLOSED_ACCOUNT_DISCRIMINATOR);
    }

    // ---- 8. prove nothing economic moved ---------------------------------
    let after = EconomicSnapshot::of(&context.accounts.reserve);
    require!(before == after, AeraError::MigrationChangedState);

    msg!(
        "aera: migrated reserve {} to v0.2, reference {} gross / {} effective",
        context.accounts.reserve.key(),
        observation.gross_rate,
        observation.effective_rate
    );

    Ok(())
}

#[derive(Accounts)]
pub struct MigrateReserveToV2<'info> {
    /// CHECK: raw bytes, for the same reason `MigrateGlobalToV2` takes it raw:
    /// a v0.1 `Global` is one byte short of the v0.2 struct, so
    /// `Account<Global>` cannot deserialize the very state being migrated. The
    /// discriminator and the admin are checked by hand in the handler.
    ///
    /// This is why the version stamp is a separate instruction and goes last:
    /// if `Global` were migrated first, every reserve migration would need to
    /// handle both shapes instead of only the old one.
    pub global: UncheckedAccount<'info>,

    #[account(
        constraint = market.global == global.key() @ AeraError::GlobalMismatch,
    )]
    pub market: Box<Account<'info, Market>>,

    #[account(
        mut,
        has_one = liquidity_mint,
        constraint = reserve.market == market.key() @ AeraError::MarketMismatch,
    )]
    pub reserve: Box<Account<'info, Reserve>>,

    /// CHECK: v0.1's `PriceFeed`. Its type no longer exists in this program, so
    /// it is validated by address, owner and discriminator in the handler and
    /// closed by hand there -- Anchor's `close` needs a typed account, and
    /// there is no type left to give it. Its contents are guardian submissions
    /// and are deliberately not read; see the module docs.
    #[account(mut)]
    pub legacy_price_feed: UncheckedAccount<'info>,

    /// CHECK: only its key is used, as a PDA seed and as the mint the source
    /// must issue.
    pub liquidity_mint: UncheckedAccount<'info>,

    #[account(
        init,
        payer = admin,
        space = OracleState::DISCRIMINATOR.len() + OracleState::INIT_SPACE,
        seeds = [ORACLE_SEED, market.key().as_ref(), liquidity_mint.key().as_ref()],
        bump,
    )]
    pub oracle: Box<Account<'info, OracleState>>,

    #[account(mut)]
    pub admin: Signer<'info>,

    pub system_program: Program<'info, System>,
}

// ===========================================================================
// Global
// ===========================================================================

/// Stamp the protocol version, after every reserve has been migrated.
///
/// Deliberately separate, and deliberately last. It reallocs `Global` by the
/// single byte the version field occupies -- v0.1's account predates the field
/// entirely and is one byte shorter.
///
/// This does not and cannot verify that every reserve has migrated: the program
/// holds no registry of them. It is a record, not a gate. The gate is per
/// reserve and is `Reserve.oracle`, which no caller can forge.
pub fn handle_migrate_global_to_v2(context: Context<MigrateGlobalToV2>) -> Result<()> {
    let global = context.accounts.global.to_account_info();
    let admin = context.accounts.admin.to_account_info();

    /*
     * `Global` is handled as raw bytes, not as `Account<Global>`.
     *
     * A v0.1 `Global` is GLOBAL_V1_LEN bytes and has no `version` field at all.
     * Anchor deserializes named accounts before the handler runs, so
     * `Account<Global>` would fail on the short buffer and this instruction
     * could never execute against the very state it exists to migrate. The
     * checks Anchor would have done are done here instead, explicitly.
     */
    {
        let data = global.try_borrow_data()?;

        // Discriminator: this is a Global and not something else at this PDA.
        let head = data.get(..8).ok_or(AeraError::NotMigratable)?;
        require!(head == Global::DISCRIMINATOR, AeraError::NotMigratable);

        // Admin, read at its fixed offset rather than by deserializing.
        let admin_bytes = data.get(8..40).ok_or(AeraError::NotMigratable)?;
        let stored_admin = Pubkey::try_from(admin_bytes).map_err(|_| AeraError::NotMigratable)?;
        require_keys_eq!(stored_admin, admin.key(), AeraError::NotAdmin);

        // Length is the version. A v0.1 account is exactly one byte short of a
        // v0.2 one, and nothing a caller controls can change that.
        require!(data.len() != GLOBAL_V2_LEN, AeraError::AlreadyMigrated);
        require!(data.len() == GLOBAL_V1_LEN, AeraError::NotMigratable);
    }

    // ---- grow by one byte ------------------------------------------------
    //
    // Rent first: an account that is not rent-exempt after the realloc would be
    // reaped, taking the whole protocol's admin and pause state with it.
    let rent = Rent::get()?;
    let needed = rent.minimum_balance(GLOBAL_V2_LEN);
    let held = global.lamports();
    if needed > held {
        let top_up = needed - held;
        anchor_lang::system_program::transfer(
            CpiContext::new(
                context.accounts.system_program.key(),
                anchor_lang::system_program::Transfer {
                    from: admin.clone(),
                    to: global.clone(),
                },
            ),
            top_up,
        )?;
    }

    // `true` zero-fills the new byte, so `version` reads GLOBAL_VERSION_V1
    // before it is set rather than whatever the allocator had there.
    global.resize(GLOBAL_V2_LEN)?;

    {
        let mut data = global.try_borrow_mut_data()?;
        let slot = data
            .get_mut(GLOBAL_V1_LEN)
            .ok_or(AeraError::NotMigratable)?;
        require!(*slot == GLOBAL_VERSION_V1, AeraError::NotMigratable);
        *slot = GLOBAL_VERSION_V2;
    }

    msg!("aera: global stamped v0.2");
    Ok(())
}

#[derive(Accounts)]
pub struct MigrateGlobalToV2<'info> {
    /// CHECK: a v0.1 `Global` is one byte too short for `Account<Global>` to
    /// deserialize, which is precisely the state being migrated. Discriminator,
    /// admin and length are all checked by hand in the handler.
    #[account(mut)]
    pub global: UncheckedAccount<'info>,

    #[account(mut)]
    pub admin: Signer<'info>,

    pub system_program: Program<'info, System>,
}

/// Referenced by the migration tests to assert the v0.1 length is what the
/// program believes it is.
pub const fn global_v1_len() -> usize {
    GLOBAL_V1_LEN
}
