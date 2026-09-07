//! Create or reconfigure one asset's oracle.
//!
//! v0.1's version of this installed a guardian set: five publisher keys, a
//! quorum, a freshness window. There are no publishers now, so what an admin
//! configures is entirely different — *where* the rate is derived from and
//! *what bounds* it must stay inside. Never the rate.
//!
//! Nothing an admin can pass here produces a number. `source_account` names an
//! account whose contents the admin does not control; the bounds can only ever
//! make Aera more willing to refuse a rate, never more willing to invent one.

use anchor_lang::prelude::*;

use crate::constants::{
    DEFAULT_EMERGENCY_DEVIATION_BPS, DEFAULT_MAX_DOWN_BPS_PER_EPOCH, DEFAULT_MAX_EPOCH_ALLOWANCE,
    DEFAULT_MAX_UP_BPS_PER_EPOCH, ORACLE_SEED,
};
use crate::errors::AeraError;
use crate::oracle::breaker::{BreakerConfig, OracleHealth, Reference};
use crate::state::{Global, Market, OracleState};

/// Everything an admin may choose about an oracle.
///
/// A struct rather than a long argument list so adding a bound later is a
/// serialization change in one place, and so the timelock logic in `set_params`
/// can compare an old and a new configuration field by field.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug)]
pub struct OracleConfig {
    /// [`crate::oracle::OracleSourceKind`] as `u8`.
    pub source_kind: u8,
    /// Program that must own `source_account`. Zero for `UnitOfAccount`.
    pub source_program: Pubkey,
    /// The account the rate is derived from. Zero for `UnitOfAccount`.
    pub source_account: Pubkey,
    /// Redemption fee past which observations are refused.
    pub max_withdrawal_fee_bps: u16,
    pub rate_floor: u128,
    pub rate_ceiling: u128,

    /// The source program's `ProgramData` deploy slot at the moment this
    /// configuration was authorised.
    ///
    /// An admin choosing this is not choosing a price: they are recording which
    /// deployment of somebody else's program they reviewed. If that program is
    /// redeployed the slot moves, every observation is refused, and borrowing
    /// freezes until a human looks at the new code and re-authorises it.
    pub expected_deploy_slot: u64,

    /// The source program's upgrade authority at that moment, or all zeros if
    /// it was already immutable.
    pub expected_upgrade_authority: Pubkey,

    pub breaker: BreakerConfig,
}

impl OracleConfig {
    /// The bCOOK configuration, from measured values. Used by the deployment
    /// scripts and by every test that wants a realistic oracle.
    #[allow(clippy::too_many_arguments)]
    pub fn native(
        source_program: Pubkey,
        source_account: Pubkey,
        max_withdrawal_fee_bps: u16,
        rate_floor: u128,
        rate_ceiling: u128,
        expected_deploy_slot: u64,
        expected_upgrade_authority: Pubkey,
    ) -> Self {
        Self {
            source_kind: crate::oracle::OracleSourceKind::NativeExchangeRate as u8,
            source_program,
            source_account,
            max_withdrawal_fee_bps,
            rate_floor,
            rate_ceiling,
            expected_deploy_slot,
            expected_upgrade_authority,
            breaker: BreakerConfig {
                max_up_bps_per_epoch: DEFAULT_MAX_UP_BPS_PER_EPOCH,
                max_down_bps_per_epoch: DEFAULT_MAX_DOWN_BPS_PER_EPOCH,
                emergency_deviation_bps: DEFAULT_EMERGENCY_DEVIATION_BPS,
                max_epoch_allowance: DEFAULT_MAX_EPOCH_ALLOWANCE,
            },
        }
    }

    /// The COOK configuration: one COOK is one COOK.
    pub fn unit_of_account() -> Self {
        Self {
            source_kind: crate::oracle::OracleSourceKind::UnitOfAccount as u8,
            source_program: Pubkey::default(),
            source_account: Pubkey::default(),
            max_withdrawal_fee_bps: 0,
            rate_floor: crate::constants::FIXED_POINT_SCALE,
            rate_ceiling: crate::constants::FIXED_POINT_SCALE,
            // Reads no program, so there is no deployment to pin.
            expected_deploy_slot: 0,
            expected_upgrade_authority: Pubkey::default(),
            breaker: BreakerConfig {
                // A constant cannot move, so the bounds are formalities. They
                // are still validated, and `validate` requires the emergency
                // bound to be at least the per-epoch allowance.
                max_up_bps_per_epoch: 0,
                max_down_bps_per_epoch: 0,
                emergency_deviation_bps: 1,
                max_epoch_allowance: 1,
            },
        }
    }

    /// A Tier 3 market-priced oracle.
    ///
    /// The rate band and the epoch breaker below are **not** what governs this
    /// kind. A market oracle's movement rules live in `MarketOracleConfig`
    /// (`max_rise_bps_per_window`, the cross-pool and depth degradations), which
    /// is asymmetric because a market price legitimately falls without limit and
    /// may only rise as fast as the configured bound. The fields here exist
    /// because `OracleState` is one shape for all three kinds; they are given
    /// permissive-but-valid values rather than values that would silently
    /// half-apply an epoch model this source does not use.
    ///
    /// In particular the band is deliberately *not* enforced against the
    /// accepted market price. A genuine crash below a floor would freeze the
    /// oracle, and a frozen oracle keeps valuing collateral at the older, higher
    /// price -- exactly backwards during a crash.
    pub fn market_twap() -> Self {
        Self {
            source_kind: crate::oracle::OracleSourceKind::MarketTwap as u8,
            // The AMM and its pools are named in `MarketOracle`, not here. See
            // `OracleState::validate_config`, which requires both to be zero.
            source_program: Pubkey::default(),
            source_account: Pubkey::default(),
            // No redemption path, so no redemption fee.
            max_withdrawal_fee_bps: 0,
            rate_floor: 1,
            rate_ceiling: u128::MAX,
            // The AMM deployment is pinned in `MarketOracleConfig`, checked on
            // every refresh, so pinning it twice would let the two disagree.
            expected_deploy_slot: 0,
            expected_upgrade_authority: Pubkey::default(),
            breaker: BreakerConfig {
                max_up_bps_per_epoch: 0,
                max_down_bps_per_epoch: 0,
                emergency_deviation_bps: 1,
                max_epoch_allowance: 1,
            },
        }
    }
}

pub fn handle_set_oracle(context: Context<SetOracle>, config: OracleConfig) -> Result<()> {
    OracleState::validate_config(
        config.source_kind,
        config.source_program,
        config.source_account,
        config.max_withdrawal_fee_bps,
        config.rate_floor,
        config.rate_ceiling,
        &config.breaker,
    )?;

    let oracle = &mut context.accounts.oracle;
    let market = context.accounts.market.key();
    let mint = context.accounts.mint.key();

    /*
     * Reconfiguring clears the reference.
     *
     * A reference is a statement about a particular source. Carrying one across
     * a change of source would let a new source inherit the credibility of the
     * old one -- and, worse, would let the breaker measure a completely
     * unrelated rate against it and conclude everything was fine. The oracle
     * starts unanchored and the next refresh establishes a new reference, which
     * the absolute floor and ceiling still bound.
     */
    let source_changed = oracle.source_account != config.source_account
        || oracle.source_program != config.source_program
        || oracle.source_kind != config.source_kind
        // A new deployment pin is a change of source even when the ids match.
        // The account is the same; the code that writes it is not, so the old
        // reference is a statement about what those bytes used to mean. Letting
        // it survive would let a re-authorised deployment inherit the trust
        // earned by the one it replaced, which is exactly the inheritance the
        // pin exists to prevent.
        || oracle.expected_deploy_slot != config.expected_deploy_slot
        || oracle.expected_upgrade_authority != config.expected_upgrade_authority;

    oracle.market = market;
    oracle.mint = mint;
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

    if source_changed {
        oracle.reference = Reference::default();
        oracle.last_moved_bps = 0;
        oracle.last_source_epoch = 0;
    }

    /*
     * A freshly configured oracle is frozen for risk-increasing actions until a
     * refresh has actually read the source.
     *
     * `last_refresh_slot` is left at zero, so `require_fresh` refuses anyway;
     * the health is set explicitly as well so the state is honest to a reader
     * rather than merely unusable.
     */
    oracle.health = OracleHealth::BorrowFrozen as u8;
    oracle.last_refresh_slot = 0;

    Ok(())
}

#[derive(Accounts)]
pub struct SetOracle<'info> {
    #[account(
        constraint = global.admin == admin.key() @ AeraError::NotAdmin,
    )]
    pub global: Box<Account<'info, Global>>,

    #[account(
        constraint = market.global == global.key() @ AeraError::GlobalMismatch,
    )]
    pub market: Box<Account<'info, Market>>,

    /// CHECK: the asset being priced. Only its key is used, as a PDA seed and
    /// as the mint the source must issue; it is never deserialized here.
    pub mint: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = admin,
        space = OracleState::DISCRIMINATOR.len() + OracleState::INIT_SPACE,
        seeds = [ORACLE_SEED, market.key().as_ref(), mint.key().as_ref()],
        bump,
    )]
    pub oracle: Box<Account<'info, OracleState>>,

    #[account(mut)]
    pub admin: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[cfg(test)]
mod encoding_tests {
    use super::*;
    use anchor_lang::{AnchorDeserialize, AnchorSerialize};

    /// The config must survive a Borsh round trip byte for byte.
    ///
    /// If the client and the program disagree about this encoding, every
    /// `set_oracle` fails with InstructionDidNotDeserialize and nothing else
    /// in the protocol can be configured.
    #[test]
    fn oracle_config_round_trips() {
        let original = OracleConfig::native(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            500,
            crate::constants::FIXED_POINT_SCALE,
            10 * crate::constants::FIXED_POINT_SCALE,
            5_504_973,
            Pubkey::new_unique(),
        );

        let mut bytes = Vec::new();
        original.serialize(&mut bytes).expect("serialize");
        println!("OracleConfig serializes to {} bytes", bytes.len());

        let decoded = OracleConfig::deserialize(&mut bytes.as_slice()).expect("deserialize");
        assert_eq!(decoded.source_kind, original.source_kind);
        assert_eq!(decoded.rate_floor, original.rate_floor);
        assert_eq!(decoded.rate_ceiling, original.rate_ceiling);
        assert_eq!(
            decoded.breaker.max_up_bps_per_epoch,
            original.breaker.max_up_bps_per_epoch
        );
    }
}
