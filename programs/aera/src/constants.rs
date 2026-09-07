//! Every tunable in the protocol, in one place.
//!
//! These are plain `pub const`s rather than Anchor `#[constant]`s: `#[constant]`
//! only re-exports a value into the IDL, and anchor's idl-build mis-evaluates a
//! u128 literal this large as i32. None of these need to be in the IDL.

// ---------------------------------------------------------------------------
// Fixed-point
// ---------------------------------------------------------------------------

/// Fixed-point scale for every ratio in the program: interest rates, the
/// cumulative borrow index, the share exchange rate, and obligation values. A
/// ratio `r` is stored as the integer `r * FIXED_POINT_SCALE`.
///
/// All money math is integer-only (no floats, no fixed-point crates). 10^18
/// keeps a single slot's interest — a tiny fraction of the index — from
/// truncating to zero, while u128's ~3.4e38 ceiling leaves headroom for the
/// index to grow and for intermediate products before the final narrowing cast.
pub const FIXED_POINT_SCALE: u128 = 1_000_000_000_000_000_000;

/// log10(FIXED_POINT_SCALE). Folds the price exponent and the fixed-point scale
/// into one power of ten so price conversions never form a needless 10^18
/// intermediate that would overflow for high-priced assets.
pub const FIXED_POINT_SCALE_DECIMALS: i32 = 18;

/// Denominator for every basis-point config value. 100% == 10_000 bps.
pub const BPS_DENOMINATOR: u128 = 10_000;

// ---------------------------------------------------------------------------
// Aera Core launch parameters (docs/PARAMS.md is generated from these)
// ---------------------------------------------------------------------------

/// Loan-to-value: the fraction of (haircut) collateral value a borrower may draw.
pub const DEFAULT_LTV_BPS: u16 = 5_500; // 55%

/// Above this fraction of collateral value, the obligation is liquidatable.
pub const DEFAULT_LIQUIDATION_THRESHOLD_BPS: u16 = 6_500; // 65%

/// Extra collateral a liquidator receives, as a fraction of the value repaid.
pub const DEFAULT_LIQUIDATION_BONUS_BPS: u16 = 800; // 8%

/// Fraction of a borrow one liquidation may close while HF is in [0.95, 1).
pub const DEFAULT_CLOSE_FACTOR_BPS: u16 = 5_000; // 50%

/// Below this health factor a liquidator may close the whole position at once.
/// A position this deep is close to insolvent; capping the close factor there
/// just forces more transactions to reach the same end state.
pub const FULL_CLOSE_HEALTH_FACTOR_BPS: u128 = 9_500; // HF < 0.95

/// Extra discount applied to bCOOK collateral value before LTV and LT. bCOOK is
/// an LST whose redemption is not instant, so its mark is haircut on top of the
/// ordinary risk parameters.
pub const DEFAULT_COLLATERAL_HAIRCUT_BPS: u16 = 500; // 5%

/// Utilization at which the borrow rate reaches `DEFAULT_OPTIMAL_BORROW_RATE_BPS`.
pub const DEFAULT_OPTIMAL_UTILIZATION_BPS: u16 = 6_000; // kink u* = 60%

/// Borrow APR at 0% utilization ("base").
pub const DEFAULT_MIN_BORROW_RATE_BPS: u16 = 200; // 2%

/// Borrow APR at the kink: base 2% + slope1 8%.
pub const DEFAULT_OPTIMAL_BORROW_RATE_BPS: u16 = 1_000; // 10%

/// Borrow APR at 100% utilization: base 2% + slope1 8% + slope2 80%.
pub const DEFAULT_MAX_BORROW_RATE_BPS: u16 = 9_000; // 90%

/// Share of accrued borrow interest kept by the protocol. The rest lifts the
/// supplier exchange rate.
///
/// All of it accrues to the single `Global::fee_destination`. There is no
/// second beneficiary and no split.
pub const DEFAULT_RESERVE_FACTOR_BPS: u16 = 1_500; // 15%

// Caps, in base units of a 9-decimal COOK. These are ceilings, not targets: the
// protocol works correctly with 1 COOK in the vault.

// These were 20,000,000 / 12,000,000 / 1,000,000 here while the deployed
// reserve enforced 1,000,000 / 600,000 / 250,000, because `init_reserve` takes
// its config from the SDK and the SDK carried different numbers. The Rust
// defaults were therefore never the live values, and three sources of truth
// disagreed. They are aligned on what the chain enforces; this is a
// reconciliation, not a parameter change.

/// Maximum COOK the reserve will hold.
pub const DEFAULT_SUPPLY_CAP: u64 = 1_000_000_000_000_000; // 1,000,000 COOK

/// Maximum COOK that may be borrowed out of the reserve. 60% of the supply cap,
/// which is the borrow-to-supply ratio the kink is set for.
pub const DEFAULT_BORROW_CAP: u64 = 600_000_000_000_000; // 600,000 COOK

/// Maximum COOK a single wallet may supply. Real float is ~441M COOK and the
/// largest private holder is ~12.8M, so without this one holder could fill the
/// entire book in a single transaction. At 250,000 it takes four wallets and no
/// single one is more than a quarter of the pool.
pub const DEFAULT_PER_WALLET_SUPPLY_CAP: u64 = 250_000_000_000_000; // 250,000 COOK

/// Slots in a year on Cookie Chain, from measurement rather than assumption.
///
/// `getRecentPerformanceSamples(30)` gave a mean slot of **445.2 ms** over
/// 4,043 slots, not the 400 ms a Solana default would suggest.
///
/// **Measured 2026-08-31 and frozen for launch.** `getBlockTime` deltas across
/// five baselines on two independent Cookie RPCs, which agreed to 0.02 ms at the
/// baseline both could serve -- the ledger reads the same from every honest
/// node, whereas `getRecentPerformanceSamples` is one node's recent view:
///
/// ```text
///   baseline      span      mean slot      slots/year
///   50k slots     0.26 d    456.865 ms     69,074,266
///   200k slots    1.05 d    454.866 ms     69,377,862
///   500k slots    2.63 d    454.812 ms     69,385,999
///   1M slots      5.27 d    455.280 ms     69,314,686
///   2M slots     10.54 d    455.291 ms     69,313,098   <- frozen
/// ```
///
/// The ten-day baseline is the value taken: long enough that diurnal variation
/// and skipped slots average out, recent enough to describe the chain as it is.
/// Everything from one day outward agrees to within 0.1%.
///
/// The direction of an error here matters. This is the divisor turning an APR
/// into a per-slot rate, so a value that is too *high* makes each slot's
/// interest too small and a value too *low* overcharges. Neither shows on any
/// screen: the UI quotes the configured APR and the chain accrues something
/// else.
///
/// Both previous values were wrong in opposite directions. The program carried
/// 70,881,876 (2.26% high, borrowers undercharged) while `sdk/params.ts` carried
/// 67,609,680 (2.46% low, borrowers overcharged) -- and because `init_reserve`
/// takes its configuration from the caller, the SDK's figure is what the live
/// reserves were actually created with.
///
/// It lives in `ReserveConfig`, not here, precisely because it drifts. This is
/// only the deployment default; existing reserves keep whatever they were
/// created with until `set_params` changes it, which waits out the timelock
/// because it moves the economics of every open position.
pub const DEFAULT_SLOTS_PER_YEAR: u64 = 69_313_098; // 365.25 * 24 * 3600 / 0.455291

// ---------------------------------------------------------------------------
// Oracle
// ---------------------------------------------------------------------------

// v0.2 has no guardians, no quorum, no freshness window and no publisher.
// `DEFAULT_MAX_PRICE_AGE_SECONDS`, `MAX_GUARDIANS` and
// `DEFAULT_CIRCUIT_BREAKER_BPS` are gone with them. The price is derived from
// the stake pool's own accounting on every refresh, so there is nothing to go
// stale in the sense v0.1 meant.

/// The stake-pool program that issues bCOOK, and must own the account the rate
/// is derived from.
///
/// Verified against Cookie Chain by `scripts/discover-bcook-oracle.ts`. It is a
/// deployment default only: the value that binds is the one stored in
/// `OracleState`, so a different market could price a different pool.
pub const BCOOK_STAKE_POOL_PROGRAM: &str = "GZgs5uREPp6BvDt8eysmhavQPAHBAtjePgV4zfhgd9pH";

/// The pool account itself.
pub const BCOOK_STAKE_POOL: &str = "GxbNKNYdtNXQkhDkpHdLDAMX64GxaECgANqdfp6cUGH4";

/// Redemption fee above which an observation is refused and borrowing freezes.
///
/// The deployed pool charges 2.00%. This bound is what stops its operator
/// consuming Aera's risk margin by raising their own fee: past it, Aera stops
/// lending rather than quietly absorbing the difference.
pub const DEFAULT_MAX_WITHDRAWAL_FEE_BPS: u16 = 500; // 5%

/// Absolute floor on the gross rate.
///
/// Deliberately far below parity, and it took a test failure to get here.
///
/// The first version of this was exactly 1.0, on the reasoning that a staking
/// receipt cannot be worth less than the asset it wraps. That is true of a
/// *healthy* pool -- the ratio only accumulates -- but not of a slashed one,
/// and a floor at parity would have refused every rate below 1.0 outright. The
/// consequence is the wrong way round: a slashing event would freeze the oracle
/// at its last pre-slash reference, so liquidations would price collateral
/// above what it was worth and under-seize, exactly when the protocol most
/// needs them to work.
///
/// The floor's job is to catch a field that has been redefined or corrupted,
/// not a loss. 0.1 is far below any recoverable stake-pool state and far above
/// the near-zero a misread would produce.
pub const DEFAULT_RATE_FLOOR: u128 = FIXED_POINT_SCALE / 10; // 0.1

/// Guards a redefined or corrupted field. The observed rate is ~1.30.
pub const DEFAULT_RATE_CEILING: u128 = 10 * FIXED_POINT_SCALE; // 10.0

// Circuit-breaker defaults, sized from measurement rather than intuition. The
// rate moved +0.21% over the epoch preceding the v0.2 work, and Cookie epochs
// are 53.4 hours, so movement is measured per elapsed pool epoch.

/// Maximum upward move per elapsed pool epoch. About 10x the observed step.
pub const DEFAULT_MAX_UP_BPS_PER_EPOCH: u16 = 200; // 2%

/// Maximum downward move per elapsed pool epoch. Tighter than upward: a stake
/// pool's rate does not fall as a matter of course, so a fall is an incident
/// signal rather than a slower version of yield.
pub const DEFAULT_MAX_DOWN_BPS_PER_EPOCH: u16 = 100; // 1%

/// Absolute move from the reference, either direction, that goes straight to
/// EMERGENCY however many epochs have elapsed.
pub const DEFAULT_EMERGENCY_DEVIATION_BPS: u16 = 1_000; // 10%

/// Ceiling on accumulated per-epoch allowance, so a reference left unrefreshed
/// for a long time cannot permit an unbounded jump.
pub const DEFAULT_MAX_EPOCH_ALLOWANCE: u8 = 10;

// ---------------------------------------------------------------------------
// Admin hard limits — the program refuses these regardless of who signs
// ---------------------------------------------------------------------------

/// No admin may set LTV above this.
pub const MAX_ADMIN_LTV_BPS: u16 = 7_500; // 75%

/// No admin may set a liquidation bonus above this.
pub const MAX_ADMIN_LIQUIDATION_BONUS_BPS: u16 = 1_500; // 15%

/// No admin may set a reserve factor above this.
pub const MAX_ADMIN_RESERVE_FACTOR_BPS: u16 = 3_000; // 30%

/// No admin may set the collateral haircut below this.
///
/// The haircut is Aera's own risk margin and is deliberately kept separate from
/// the source's redemption fee, which is applied inside the oracle before this
/// ever sees the rate. The floor exists so that margin cannot be tuned away
/// entirely: at zero, Aera would lend against the full redeemable value of an
/// asset whose price it does not control and whose issuer can be upgraded.
pub const MIN_ADMIN_COLLATERAL_HAIRCUT_BPS: u16 = 100; // 1%

/// No admin may set an origination fee above this.
///
/// The fee ships **off** (`DEFAULT_ORIGINATION_FEE_BPS`). The capability exists
/// so turning it on later is a parameter change rather than a program upgrade,
/// and the ceiling is deliberately low: an origination fee is charged up front
/// on every draw, so a large one is a much sharper instrument than the interest
/// rate and should not be reachable by a captured admin.
pub const MAX_ADMIN_ORIGINATION_FEE_BPS: u16 = 50; // 0.50%

/// Origination fee at launch: 15 bps.
///
/// Aera V1's economic policy is that the protocol earns from **credit
/// activity** — debt originated, debt outstanding, and liquidation — and
/// charges nothing for routine account management. Supplying, withdrawing,
/// repaying, depositing collateral, withdrawing collateral and redeeming are
/// all zero, and there is no spread hidden in any of them.
///
/// 15 bps is the smallest of the three, and the only one a borrower pays up
/// front. On a 10,000 COOK draw it is 15 COOK: the borrower owes 10,000 and
/// receives 9,985, with the fee retained in the vault and recognised
/// immediately as protocol revenue. Suppliers' claim on the pool is unchanged
/// by it — see `borrow.rs`, where `available_liquidity` falls by the amount
/// paid out and `accrued_fees` rises by the fee, so `total_liquidity()` nets to
/// where it was.
///
/// The ceiling above stays at 50 bps for the reason recorded there: an up-front
/// fee is a sharper instrument than the interest rate, and a captured admin
/// should not be able to reach far with it.
///
/// **This is the launch constant, not a deployed value.** A market that is
/// already live keeps whatever it was configured with until a separate,
/// explicitly approved `set_params` transaction changes it.
pub const DEFAULT_ORIGINATION_FEE_BPS: u16 = 15;

/// No admin may take more than this share of the liquidation bonus.
///
/// This is a ceiling on how much of the **existing** bonus Aera may keep. It
/// does not add to the borrower's penalty — see `risk::split_seized_shares`.
/// Every basis point here is a basis point the liquidator does not receive, so
/// the binding constraint is not revenue but whether liquidation still happens.
///
/// ## Where 300 comes from
///
/// `tools/liquidation-economics.ts`, run against the two COOKHOUSE pools as the
/// Tier 3 collector measured them (3,924,540 COOK combined, 88 bps apart).
///
/// A liquidator who routes across both pools has enormous headroom: even a
/// 150,000 COOK position closed in full leaves a healthy margin at a 470 bps
/// share. That is the wrong case to size against, because it assumes the deep
/// pool is always available. Sizing against the **thin pool alone**:
///
/// ```text
///   debt    close    highest share leaving >=3% liquidator margin
///   ------  -------  ---------------------------------------------
///    25,000    100%    595 bps
///    50,000    100%    260 bps
///    75,000    100%    negative at any share, including zero
/// ```
///
/// Produced by `tools/liquidation-economics.ts` from the observation log. An
/// earlier version of this comment quoted 640 and 300 for the first two rows,
/// written before that tool existed; both were optimistic, and the second was
/// the one the ceiling had been justified against.
///
/// The per-wallet borrow cap for COOKHOUSE is 25,000, so the realistic worst
/// case is the first row, against which 250 bps carries better than 2x headroom.
/// The ceiling is set by the second row instead -- twice the cap, which is the
/// room needed for interest accruing past it and for the cap being raised later
/// -- and sits just under its 260 bps rather than above it.
///
/// Past that, no protocol share is safe under pessimistic routing, including a
/// share of zero. That is a statement about position size, which the wallet cap
/// governs, not about this parameter: Aera taking nothing would not make a
/// 75,000 thin-pool liquidation profitable.
///
/// The COOKHOUSE candidate is 150 bps, comfortably below this.
///
/// This is a **code-level** ceiling chosen from one book at one moment, and the
/// book is the thing most likely to change. It is not a recommendation to go
/// anywhere near it.
pub const MAX_PROTOCOL_LIQUIDATION_SHARE_BPS: u16 = 250; // 2.5 percentage points

/// Delay before a *loosening* parameter change may be applied. Tightening
/// changes and pauses take effect immediately; see `state::reserve::PendingConfig`.
pub const DEFAULT_PARAM_TIMELOCK_SECONDS: i64 = 24 * 60 * 60; // 24h

/// Upper bound on the configurable timelock, so it cannot be set absurdly long
/// (which would be its own kind of governance capture).
pub const MAX_PARAM_TIMELOCK_SECONDS: i64 = 48 * 60 * 60; // 48h

// ---------------------------------------------------------------------------
// Sizing
// ---------------------------------------------------------------------------

// Share-token metadata bounds. These cap the mint's rent, which the admin pays
// at `init_reserve`, and keep a mistyped URI from allocating an absurd account.
pub const MAX_SHARE_NAME: usize = 32;
pub const MAX_SHARE_SYMBOL: usize = 10;
pub const MAX_SHARE_URI: usize = 200;

/// Maximum distinct reserves an obligation may use as collateral, and
/// separately as borrows. Bounds account size and the compute cost of
/// `refresh_obligation`, which iterates every entry.
pub const MAX_OBLIGATION_RESERVES: usize = 4;

// ---------------------------------------------------------------------------
// PDA seeds
// ---------------------------------------------------------------------------

pub const GLOBAL_SEED: &[u8] = b"global";
pub const MARKET_SEED: &[u8] = b"market";
pub const RESERVE_SEED: &[u8] = b"reserve";
pub const LIQUIDITY_VAULT_SEED: &[u8] = b"liquidity_vault";
pub const SHARE_MINT_SEED: &[u8] = b"share_mint";
pub const OBLIGATION_SEED: &[u8] = b"obligation";
pub const OBLIGATION_SHARE_VAULT_SEED: &[u8] = b"obligation_share_vault";
/// v0.1's guardian feed. Retained only so the migration can find and close it.
pub const PRICE_FEED_SEED: &[u8] = b"price_feed";

/// v0.2's oracle state. A new seed rather than a reuse, so a half-migrated
/// deployment can never read guardian bytes as breaker configuration.
pub const ORACLE_SEED: &[u8] = b"oracle";
pub const SUPPLY_POSITION_SEED: &[u8] = b"supply_position";
