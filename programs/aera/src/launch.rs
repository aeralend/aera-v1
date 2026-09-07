//! The launch configuration, in one place, derived from the constants.
//!
//! Every value Aera Core deploys with lives here as a function of
//! [`crate::constants`]. Nothing in this module invents a number.
//!
//! ## Why this exists
//!
//! The same figures were previously written out in four places -- the program's
//! constants, the SDK's `params.ts`, the deployment scripts and the keeper
//! configuration -- and they had already drifted. The SDK carried
//! `slots_per_year = 67_609_680` while the program carried `70_881_876`, a 4.8%
//! disagreement in the divisor that turns an APR into a per-slot rate. Because
//! `init_reserve` takes its configuration from whoever calls it, the SDK's copy
//! was what a deployment would actually have written on chain.
//!
//! So there is now one source, it is this file, and
//! `tests/test_launch_config.rs` emits it to `config/aera.launch.json` and
//! fails if the committed file disagrees. Everything outside the program --
//! SDK, deployment, keepers, preflight -- reads that JSON.
//!
//! ## The program is still the final authority
//!
//! A JSON file is a convenience for tooling, not a rule. `set_params` validates
//! every field against the hard maxima in `constants`, so a wrong number here
//! is refused on chain rather than silently applied. This file makes the
//! *intended* configuration checkable; the program makes the *actual* one safe.

use crate::constants::*;
use crate::state::ReserveConfig;

/// The interest curve and fee shared by both reserves.
///
/// Identical on purpose: the curve describes how Aera prices utilisation, and
/// there is no reason for two reserves in one market to disagree about that.
fn shared() -> ReserveConfig {
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
        supply_cap: 0,
        borrow_cap: 0,
        per_wallet_supply_cap: 0,
        borrow_enabled: false,
        collateral_enabled: false,
        isolated: false,
        slots_per_year: DEFAULT_SLOTS_PER_YEAR,
    }
}

/// COOK: borrowable, never collateral, capped.
///
/// `collateral_enabled: false` is what makes "aCOOK is not accepted as
/// collateral" a program rule rather than a UI convention.
///
/// The caps are deliberately small. The code is unaudited, and 1,000,000 COOK
/// bounds what can be lost while the book is young. 600,000 keeps the 60%
/// borrow-to-supply ratio the interest kink is set for, and 250,000 stops one
/// wallet being the entire book -- which a per-wallet cap equal to the supply
/// cap would allow. Cutting a cap is a tightening and lands immediately;
/// raising one waits out the timelock, so starting low costs nothing but a day.
pub fn cook_reserve_config() -> ReserveConfig {
    ReserveConfig {
        supply_cap: DEFAULT_SUPPLY_CAP,
        borrow_cap: DEFAULT_BORROW_CAP,
        per_wallet_supply_cap: DEFAULT_PER_WALLET_SUPPLY_CAP,
        borrow_enabled: true,
        ..shared()
    }
}

/// bCOOK: collateral only, 55/65/8 with a 5% haircut on top of the pool's fee.
///
/// `borrow_enabled: false` means nothing is ever drawn from this reserve, so
/// its index never moves and its share token stays 1:1 with bCOOK.
///
/// Uncapped because the cap that matters is on the borrowable side: collateral
/// nobody can borrow against is not a risk to the protocol, and capping it
/// would only stop people protecting positions they already hold.
pub fn bcook_reserve_config() -> ReserveConfig {
    ReserveConfig {
        loan_to_value_bps: DEFAULT_LTV_BPS,
        liquidation_threshold_bps: DEFAULT_LIQUIDATION_THRESHOLD_BPS,
        collateral_haircut_bps: DEFAULT_COLLATERAL_HAIRCUT_BPS,
        collateral_enabled: true,
        ..shared()
    }
}

/// Identities the deployment must match, as measured on Cookie Chain.
///
/// Not configuration in the sense of "an operator may choose these". They are
/// facts about the network, recorded so a deployment against the wrong chain,
/// the wrong pool or a redeployed stake-pool program fails a check instead of
/// succeeding quietly. `tools/preflight.sh` verifies every one against the
/// live chain before a launch is allowed to proceed.
pub mod chain {
    /// Cookie Chain's genesis hash.
    pub const GENESIS: &str = "9wDaBRDgArEUpvhHxGguNkwozsZh4UpGZB9o2EoEcBB2";

    /// BakeYourStake's stake-pool program.
    pub const STAKE_POOL_PROGRAM: &str = "GZgs5uREPp6BvDt8eysmhavQPAHBAtjePgV4zfhgd9pH";

    /// The `ProgramData` account for it, a PDA of the program id under the
    /// upgradeable loader.
    pub const STAKE_POOL_PROGRAM_DATA: &str = "6Dsx1cdbzEsNaV4BKzJhEvTpuSGf4CcVH3UCt45mERTF";

    /// The deploy slot Aera's oracle is pinned to. A redeploy moves this and
    /// every observation is refused until a human re-authorises it.
    pub const STAKE_POOL_DEPLOY_SLOT: u64 = 5_504_973;

    /// The upgrade authority at the moment of pinning: a single wallet key,
    /// system-owned with zero bytes of data. See `docs/KNOWN_RISKS.md`.
    pub const STAKE_POOL_UPGRADE_AUTHORITY: &str = "GSPUoahS7jSQUEAEkjaejsN9vo2w4B2NYHZ9oJSMm45p";

    /// The bCOOK stake pool Aera reads its collateral rate from.
    pub const STAKE_POOL: &str = "GxbNKNYdtNXQkhDkpHdLDAMX64GxaECgANqdfp6cUGH4";

    /// bCOOK's mint, which the pool names as its `pool_mint`.
    pub const BCOOK_MINT: &str = "EkPafx58mgwkEnGwo62jXhXDAdJ37Z8G8MFBRPsr9uhz";

    /// Wrapped COOK. COOK itself is the native token; this is the SPL mint the
    /// reserve holds, because a vault cannot hold lamports.
    pub const WCOOK_MINT: &str = "So11111111111111111111111111111111111111112";

    /// Both mints use nine decimals.
    pub const DECIMALS: u8 = 9;
}

/// What the chain's slot time was measured at, and when it was frozen.
///
/// Measured 2026-08-31 across five baselines on two independent Cookie RPCs,
/// which agreed to 0.02 ms at the baseline both could serve. The ten-day figure
/// is the one adopted into [`crate::constants::DEFAULT_SLOTS_PER_YEAR`], so a
/// fresh deployment now accrues interest at the rate it quotes.
///
/// | baseline | span | mean slot | slots/year |
/// |---|---|---|---|
/// | 50k | 0.26 d | 456.865 ms | 69,074,266 |
/// | 200k | 1.05 d | 454.866 ms | 69,377,862 |
/// | 500k | 2.63 d | 454.812 ms | 69,385,999 |
/// | 1M | 5.27 d | 455.280 ms | 69,314,686 |
/// | **2M** | **10.54 d** | **455.291 ms** | **69,313,098** |
pub mod measured {
    /// 2026-08-31, 2,000,000-slot baseline, cross-checked on two endpoints.
    pub const SLOT_MS_MEASURED: f64 = 455.291;
    pub const SLOTS_PER_YEAR_MEASURED: u64 = 69_313_098;
    pub const MEASURED_ON: &str = "2026-08-31";

    /// Zero: the deployed default is now the measurement.
    ///
    /// This was 220 bps before the value was frozen. It is kept as a field
    /// rather than deleted because Cookie's slot time drifts -- the same
    /// measurement a year from now will not give the same answer, and a
    /// non-zero value here is the signal to re-measure and decide again.
    ///
    /// It says nothing about *existing* reserves, which keep the
    /// `slots_per_year` they were created with until `set_params` changes it.
    pub const DEPLOYED_DRIFT_BPS: i64 = 0;
}
