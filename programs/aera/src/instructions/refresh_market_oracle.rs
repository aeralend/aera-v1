//! Derive a market price from two AMM pools and append it to the history.
//!
//! # Permissionless, on purpose
//!
//! Anyone may call this. That is not a concession, it is the point: the caller
//! supplies no number. They hand over pool accounts, and the program reads the
//! reserves out of those accounts itself. There is nothing to lie about.
//!
//! What a caller does choose is *when* an observation is taken, and that is the
//! entire residual trust. Three things bound it:
//!
//! - `min_spacing_seconds` — a caller cannot fill the window from one short
//!   manipulation, because observations closer together than this are refused.
//! - the TWAP is **time-weighted**, so thirty samples during a thirty-second
//!   pump carry thirty seconds of weight, not thirty samples' worth.
//! - `min_span_seconds` — a market cannot open on history that does not span
//!   enough real time, however many rows it contains.
//!
//! Making it permissionless also removes Aera as a liveness monopoly. If our
//! keeper stops, anyone can keep the oracle fresh. Under a privileged keeper,
//! our outage would freeze the market.
//!
//! # The verification chain
//!
//! Every link is checked against the previous one, so no caller-supplied account
//! is trusted on its own:
//!
//! ```text
//!   config.amm_program        pins which program may own a pool
//!        │
//!   pool.owner == that        a substitute pool from another AMM is refused
//!        │
//!   pool.data[168..232]       the pool names its own two mints
//!        │
//!   == (collateral, quote)    in either order; the two pools disagree on
//!        │                    orientation and both are correct
//!   pool.data[232..296]       the pool names its own two vaults
//!        │
//!   == passed vault accounts  a substitute vault is refused
//!        │
//!   vault.mint == pool.mint   and the vault agrees about what it holds
//!        │
//!   price = reserves          derived, never supplied
//! ```

use anchor_lang::prelude::*;
use anchor_spl::token_interface::TokenAccount;

use crate::errors::AeraError;
use crate::math::{mul_div_floor, ten_pow};
use crate::oracle::market_breaker::{evaluate_market, Degradation};
use crate::oracle::OracleSourceKind;
use crate::state::{MarketOracle, Observation, OracleState};

use crate::constants::{BPS_DENOMINATOR, FIXED_POINT_SCALE};

/// Byte offsets into a Meteora DAMM v2 pool account.
///
/// There is no published IDL for this program on Cookie Chain, so the layout was
/// established empirically: every 8-aligned offset in twenty pool accounts was
/// resolved on chain, and exactly four held accounts of the right shapes at the
/// same offsets in all of them. The two 82-byte accounts are the mints; the two
/// 165-byte accounts are the vaults.
///
/// Verified against both COOKHOUSE pools, including that each vault's own `mint`
/// field agrees with the mint the pool names at the corresponding offset.
///
/// This is a decoder for four fields, not an implementation of the AMM. It reads
/// identity, never arithmetic — the reserves come from the vault token accounts,
/// whose layout is the SPL standard and not Meteora's.
mod damm_v2 {
    /// Pool accounts are exactly this long. A different length is a different
    /// account, or a different version of this one.
    pub const POOL_LEN: usize = 1112;
    pub const MINT_A: usize = 168;
    pub const MINT_B: usize = 200;
    pub const VAULT_A: usize = 232;
    pub const VAULT_B: usize = 264;
}

/// One pool's contribution, after verification.
struct PoolReading {
    /// Quote units per whole collateral token, 1e18-scaled.
    price: u128,
    /// Quote-side reserve, in quote base units.
    quote_depth: u64,
}

pub fn handle_refresh_market_oracle(context: Context<RefreshMarketOracle>) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    require!(
        context.accounts.oracle.kind()? == OracleSourceKind::MarketTwap,
        AeraError::UnknownOracleSource
    );

    let market_oracle = &context.accounts.market_oracle;
    let config = market_oracle.config;

    /*
     * The AMM is upgradeable by a key Aera does not control.
     *
     * Aera cannot prevent that. What it can do is refuse to keep pricing
     * against an implementation it has never seen: the decoder above depends on
     * a byte layout, and a redeploy may change it. Detection, not prevention --
     * the same guarantee, and the same limit, as the stake-pool pin on Core.
     */
    {
        let info = &context.accounts.amm_program_data;
        require_keys_eq!(
            info.key(),
            config.amm_program_data,
            AeraError::UnexpectedAmmProgram
        );
        let data = info.try_borrow_data()?;
        // UpgradeableLoaderState::ProgramData: tag(4) + slot(8) + ...
        require!(data.len() >= 12, AeraError::AmmDeploymentChanged);
        let deployed = u64::from_le_bytes(
            data[4..12]
                .try_into()
                .map_err(|_| AeraError::AmmDeploymentChanged)?,
        );
        require!(
            deployed == config.expected_deploy_slot,
            AeraError::AmmDeploymentChanged
        );
    }

    let a = read_pool(
        &context.accounts.pool_a,
        &context.accounts.pool_a_collateral_vault,
        &context.accounts.pool_a_quote_vault,
        market_oracle,
        0,
    )?;
    let b = read_pool(
        &context.accounts.pool_b,
        &context.accounts.pool_b_collateral_vault,
        &context.accounts.pool_b_quote_vault,
        market_oracle,
        1,
    )?;

    /*
     * Depth gates each pool individually -- a deep pool does not excuse a thin
     * one, since an attacker only has to move the thin one.
     *
     * But note this is a *degradation*, not a refusal. Thin books mean new
     * borrowing must stop; they are never a reason to keep valuing collateral
     * at an older, higher price. The distinction is the whole of §11: separate
     * "may risk increase?" from "what is the most conservative known value?".
     */
    let shallow =
        a.quote_depth < config.min_pool_quote_depth || b.quote_depth < config.min_pool_quote_depth;

    /*
     * Cross-pool deviation, symmetric about the midpoint.
     *
     *     |Pa - Pb| / ((Pa + Pb) / 2)
     *
     * Computed as `2 * |Pa - Pb| * BPS / (Pa + Pb)` so it stays in integers.
     * Symmetric matters: dividing by either price alone would make the same
     * disagreement measure differently depending on which pool was named first.
     */
    let deviation_bps = {
        let sum = a
            .price
            .checked_add(b.price)
            .ok_or(AeraError::MathOverflow)?;
        require!(sum > 0, AeraError::InvalidOraclePrice);
        let diff = a.price.abs_diff(b.price);
        mul_div_floor(
            diff.checked_mul(2).ok_or(AeraError::MathOverflow)?,
            BPS_DENOMINATOR,
            sum,
        )?
    };

    /*
     * Disagreement means one of them is being pushed and we cannot tell which.
     *
     * Also a degradation rather than a refusal. Refusing outright would mean
     * that when BOTH pools report a price below the accepted one -- a genuine
     * crash, during which they will naturally diverge -- Aera would keep the old
     * higher valuation precisely when it most needs to mark down. The breaker
     * takes the lower reading and freezes new borrowing instead.
     */
    let pools_disagree = deviation_bps > config.max_cross_pool_deviation_bps as u128;

    /*
     * Combine by taking the MINIMUM, not a depth-weighted mean.
     *
     * Both were modelled. For collateral valuation the minimum is strictly
     * better: to move a mean an attacker must move one pool, and the cost is
     * roughly halved by the other pool diluting it. To move the minimum they
     * must move BOTH pools, because the lower one always wins.
     *
     * Against the measured book that is the difference between manipulating
     * ~3.1M of depth and manipulating ~4.0M -- and, more importantly, the
     * attacker cannot pick the cheaper pool. Depth-weighting actively rewards
     * pushing the thin pool when the deep one anchors the average.
     *
     * The cost is a small persistent understatement of collateral whenever the
     * pools differ, which is the direction that fails safe.
     */
    let price = a.price.min(b.price);
    let quote_depth = a.quote_depth.saturating_add(b.quote_depth);

    let market_oracle = &mut context.accounts.market_oracle;

    /*
     * Spacing. This is the observation-spam defence.
     *
     * Without it, a caller who manipulates the pools for thirty seconds can
     * call refresh every slot and fill the entire ring buffer from that window.
     * The TWAP is time-weighted, so those rows would carry only thirty seconds
     * of weight -- but they would also evict every older observation, and the
     * buffer would then contain nothing but the manipulation.
     *
     * It governs the HISTORY, not the freshness stamp, and the difference is
     * the difference between a usable market and an unusable one.
     *
     * `OracleState::require_fresh` demands `last_refresh_slot == current_slot`,
     * so every borrow, withdrawal and liquidation must carry a refresh in the
     * same transaction. An earlier version failed the whole instruction when
     * the previous observation was under a minute old -- which meant a Tier 3
     * market was open for one slot a minute and shut for the other 149, and
     * liquidation, which cannot choose its moment, was the thing that broke.
     *
     * So a refresh inside the spacing window is accepted: it re-prices against
     * the current clock and restates freshness, but appends nothing. See the
     * clamp further down for why it also may not raise the reference.
     */
    let previous_timestamp = market_oracle.latest().map(|o| o.unix_timestamp);
    if let Some(previous) = previous_timestamp {
        // The Clock is monotonic within a chain, but a validator disagreeing
        // about time must not corrupt the ordering the TWAP depends on.
        require!(now >= previous, AeraError::ObservationTooSoon);
    }
    let recorded = match previous_timestamp {
        Some(previous) => now.saturating_sub(previous) >= config.min_spacing_seconds as i64,
        None => true,
    };

    if recorded {
        let observation = Observation {
            price,
            slot: clock.slot,
            unix_timestamp: now,
            quote_depth,
        };

        let index = market_oracle.next_index as usize % crate::state::OBSERVATION_CAPACITY;
        if market_oracle.observations.len() < crate::state::OBSERVATION_CAPACITY {
            market_oracle.observations.push(observation);
        } else {
            market_oracle.observations[index] = observation;
        }
        market_oracle.next_index = ((index + 1) % crate::state::OBSERVATION_CAPACITY) as u8;
    }

    let twap = market_oracle.twap(now)?;

    /*
     * Judge the new average against the accepted reference.
     *
     * Falls apply at any size, immediately. Rises are admitted only as far as
     * the configured per-window bound. Degraded information can lower the price
     * and can never raise it.
     */
    /*
     * The gap this refresh just closed -- NOT the age of the newest
     * observation, which is `now` and therefore always zero.
     *
     * An earlier version measured `now - latest()` after pushing, so
     * `age_seconds` was zero on every call and the whole staleness ladder was
     * dead code: an oracle nobody had cranked for a day still reported Healthy
     * the instant somebody did.
     *
     * The gap is the right quantity anyway. `OracleState::require_fresh`
     * forces a refresh in the same slot as any action, so the health written
     * here is the health the action sees; what the caller needs to know is not
     * "is this reading recent" (it always is) but "how much of the history
     * behind it is missing".
     */
    let age_seconds = previous_timestamp
        .map(|previous| now.saturating_sub(previous))
        .unwrap_or(0);
    let bootstrapped = market_oracle.is_bootstrapped();
    let degradation = Degradation {
        pools_disagree,
        shallow,
    };

    let oracle_state = &context.accounts.oracle;
    let current = if oracle_state.reference.is_set() {
        Some(oracle_state.reference.effective_rate)
    } else {
        None
    };

    let verdict = evaluate_market(
        current,
        twap,
        bootstrapped,
        age_seconds,
        degradation,
        &config,
    );

    // The breaker already applies the bootstrap floor; this is just a name.
    let health = verdict.health;

    /*
     * A refresh that recorded nothing may lower the reference but never raise
     * it.
     *
     * Without this the rate limit is worthless. `max_rise_bps_per_window` caps
     * one *step*, and the cost of a manipulation is the cost of holding it long
     * enough to take many steps -- which is only true while steps are rationed
     * by `min_spacing_seconds`. If a no-op refresh could also advance the
     * reference, an attacker would take a step every slot instead of every
     * minute, converging on the manipulated price in seconds.
     *
     * Falls stay unconditional. Applying a lower price sooner is always the
     * conservative direction, and it is what makes a refresh bundled with a
     * liquidation useful rather than merely permitted.
     */
    let accepted_price = match (recorded, current) {
        (false, Some(current)) => verdict.accepted_price.min(current),
        _ => verdict.accepted_price,
    };

    {
        let oracle = &mut context.accounts.oracle;
        oracle.last_refresh_slot = clock.slot;
        oracle.last_moved_bps = u64::try_from(verdict.moved_bps).unwrap_or(u64::MAX);
        oracle.health = health as u8;
        oracle.reference = crate::oracle::breaker::Reference {
            gross_rate: accepted_price,
            // A market price has no withdrawal fee between gross and effective;
            // the two are the same number and the field stays 0 rather than
            // implying a fee that does not exist.
            effective_rate: accepted_price,
            withdrawal_fee_bps: 0,
            source_epoch: 0,
            slot: clock.slot,
            unix_timestamp: now,
        };
    }

    let reference = accepted_price;

    emit!(MarketObservationRecorded {
        oracle: context.accounts.oracle.key(),
        price,
        reference,
        pool_a_price: a.price,
        pool_b_price: b.price,
        deviation_bps: deviation_bps as u16,
        quote_depth,
        slot: clock.slot,
        health: health as u8,
        rise_capped: verdict.rise_capped,
        pools_disagree,
        shallow,
        recorded,
    });

    Ok(())
}

/// Verify one pool and derive its spot price.
fn read_pool<'info>(
    pool: &UncheckedAccount<'info>,
    collateral_vault: &InterfaceAccount<'info, TokenAccount>,
    quote_vault: &InterfaceAccount<'info, TokenAccount>,
    market_oracle: &MarketOracle,
    which: usize,
) -> Result<PoolReading> {
    let expected = market_oracle.pools[which];
    require_keys_eq!(pool.key(), expected.pool, AeraError::UnexpectedPoolPair);

    // Owned by the AMM this oracle was configured against, and nothing else.
    require_keys_eq!(
        *pool.to_account_info().owner,
        market_oracle.config.amm_program,
        AeraError::UnexpectedAmmProgram
    );

    let data = pool.try_borrow_data()?;
    // A different length is a different account layout, and the offsets below
    // would read arbitrary bytes as pubkeys. Fail closed.
    require!(
        data.len() == damm_v2::POOL_LEN,
        AeraError::UnexpectedPoolPair
    );

    let key_at = |offset: usize| -> Pubkey {
        Pubkey::try_from(&data[offset..offset + 32]).unwrap_or_default()
    };
    let mint_a = key_at(damm_v2::MINT_A);
    let mint_b = key_at(damm_v2::MINT_B);
    let vault_a = key_at(damm_v2::VAULT_A);
    let vault_b = key_at(damm_v2::VAULT_B);

    /*
     * Orientation is not consistent between pools.
     *
     * The two COOKHOUSE pools disagree about which side is A: one has the
     * collateral as token B, the other as token A. Both are correct, and an
     * implementation that assumed either would read the price upside down for
     * half the market -- so the pair is matched, not positioned.
     */
    let collateral = market_oracle.collateral_mint;
    let quote = market_oracle.quote_mint;
    let (expected_collateral_vault, expected_quote_vault) =
        if mint_a == collateral && mint_b == quote {
            (vault_a, vault_b)
        } else if mint_b == collateral && mint_a == quote {
            (vault_b, vault_a)
        } else {
            return err!(AeraError::UnexpectedPoolPair);
        };

    // The vaults the caller passed must be the ones this pool names...
    require_keys_eq!(
        collateral_vault.key(),
        expected_collateral_vault,
        AeraError::UnexpectedPoolPair
    );
    require_keys_eq!(
        quote_vault.key(),
        expected_quote_vault,
        AeraError::UnexpectedPoolPair
    );
    // ...and they must be pinned in the oracle's own configuration too, so a
    // pool that changed its vaults cannot quietly redirect the reading.
    require_keys_eq!(
        collateral_vault.key(),
        expected.collateral_vault,
        AeraError::UnexpectedPoolPair
    );
    require_keys_eq!(
        quote_vault.key(),
        expected.quote_vault,
        AeraError::UnexpectedPoolPair
    );
    // ...and they must agree about what they hold.
    require_keys_eq!(
        collateral_vault.mint,
        collateral,
        AeraError::UnexpectedPoolPair
    );
    require_keys_eq!(quote_vault.mint, quote, AeraError::UnexpectedPoolPair);

    let collateral_reserve = collateral_vault.amount;
    let quote_reserve = quote_vault.amount;
    require!(
        collateral_reserve > 0 && quote_reserve > 0,
        AeraError::InvalidOraclePrice
    );

    /*
     * Price, normalised across a decimal mismatch.
     *
     * COOKHOUSE is 6 decimals and COOK is 9, so raw reserves differ by 1000x
     * before any price is involved. The result is quote units per WHOLE
     * collateral token, 1e18-scaled:
     *
     *     price = quote_reserve * 10^collateral_decimals * SCALE
     *             ------------------------------------------------
     *             collateral_reserve * 10^quote_decimals
     *
     * Integer throughout. Rounding down understates collateral, which is the
     * direction that fails safe.
     *
     * The two powers of ten are cancelled into a single factor BEFORE scaling,
     * and this is not a tidiness point -- computing it literally overflows.
     * `quote_reserve * 10^6 * 1e18` at the measured COOKHOUSE book (~2M COOK a
     * side, so ~2e15 base units) is 2e39, against a u128 ceiling of 3.4e38.
     * Every refresh would have failed on a real pool while passing on a small
     * synthetic one. Cancelling first leaves `quote_reserve * 1e18`, which is
     * at most 1.8e37 for any u64 reserve and cannot overflow.
     */
    let (collateral_decimals, quote_decimals) = (
        market_oracle.collateral_decimals as u32,
        market_oracle.quote_decimals as u32,
    );

    let mut numerator = (quote_reserve as u128)
        .checked_mul(FIXED_POINT_SCALE)
        .ok_or(AeraError::MathOverflow)?;
    let mut denominator = collateral_reserve as u128;

    // Only the difference in decimals matters, so apply it to whichever side
    // keeps both factors small.
    if collateral_decimals >= quote_decimals {
        numerator = numerator
            .checked_mul(ten_pow(collateral_decimals - quote_decimals)?)
            .ok_or(AeraError::MathOverflow)?;
    } else {
        denominator = denominator
            .checked_mul(ten_pow(quote_decimals - collateral_decimals)?)
            .ok_or(AeraError::MathOverflow)?;
    }

    let price = mul_div_floor(numerator, 1, denominator)?;

    Ok(PoolReading {
        price,
        quote_depth: quote_reserve,
    })
}

#[derive(Accounts)]
pub struct RefreshMarketOracle<'info> {
    #[account(
        mut,
        constraint = oracle.key() == market_oracle.oracle @ AeraError::OracleAccountMismatch,
    )]
    pub oracle: Box<Account<'info, OracleState>>,

    #[account(
        mut,
        seeds = [MarketOracle::SEED, oracle.key().as_ref()],
        bump = market_oracle.bump,
    )]
    pub market_oracle: Box<Account<'info, MarketOracle>>,

    /// CHECK: owner, length and contents are all verified in `read_pool`.
    pub pool_a: UncheckedAccount<'info>,
    pub pool_a_collateral_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub pool_a_quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    /// CHECK: as above.
    pub pool_b: UncheckedAccount<'info>,
    pub pool_b_collateral_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    pub pool_b_quote_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    /// CHECK: address and deploy slot are checked against the pinned config.
    pub amm_program_data: UncheckedAccount<'info>,
    /*
     * No signer.
     *
     * Deliberate. Nothing here is authorised -- the price is derived from
     * accounts the program verifies, so there is no privilege to hold. Adding a
     * signer would make Aera a liveness monopoly for no security gain.
     */
}

#[event]
pub struct MarketObservationRecorded {
    pub oracle: Pubkey,
    /// The combined price recorded, which is the lower of the two pools.
    pub price: u128,
    /// The time-weighted reference after this observation.
    pub reference: u128,
    pub pool_a_price: u128,
    pub pool_b_price: u128,
    pub deviation_bps: u16,
    pub quote_depth: u64,
    pub slot: u64,
    /// The health the oracle is in after this observation.
    pub health: u8,
    /// A rise was admitted only as far as the configured bound.
    pub rise_capped: bool,
    pub pools_disagree: bool,
    pub shallow: bool,
    /// Whether this refresh appended to the observation history, or only
    /// re-priced and restated freshness because it fell inside the spacing
    /// window. A keeper uses this to tell a useful crank from a no-op.
    pub recorded: bool,
}
