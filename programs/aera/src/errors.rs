use anchor_lang::prelude::*;

#[error_code]
pub enum AeraError {
    // --- math / input ---
    #[msg("Arithmetic operation overflowed")]
    MathOverflow,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Deposit is too small to mint any share tokens")]
    DepositTooSmall,

    // --- config ---
    #[msg("Reserve config has an invalid value")]
    InvalidConfig,
    #[msg("Loan-to-value above the protocol hard maximum of 75%")]
    LtvAboveHardMax,
    #[msg("Liquidation bonus above the protocol hard maximum of 15%")]
    BonusAboveHardMax,
    #[msg("Reserve factor above the protocol hard maximum of 30%")]
    ReserveFactorAboveHardMax,
    #[msg("Origination fee above the protocol hard maximum of 50 bps")]
    OriginationFeeAboveHardMax,
    #[msg("Collateral haircut below the protocol hard minimum of 1%")]
    HaircutBelowHardMin,
    #[msg("Timelock outside the permitted 0..48h range")]
    InvalidTimelock,

    // --- timelock ---
    #[msg("No parameter change is pending for this reserve")]
    NoPendingConfig,
    #[msg("Pending parameter change is still inside its timelock")]
    TimelockNotElapsed,

    // --- staleness ---
    #[msg("Reserve must be accrued in this same transaction before use")]
    ReserveStale,
    #[msg("Obligation must be refreshed in this same transaction before use")]
    ObligationStale,

    // --- oracle ---
    //
    // v0.2 replaced the guardian feed with a derived exchange rate. The
    // guardian variants are gone; nothing publishes a price any more.
    #[msg("Price source reported a non-positive price")]
    InvalidOraclePrice,
    #[msg("Circuit breaker is tripped: new borrowing is refused")]
    CircuitBreakerTripped,

    // Source account validation. Each is a distinct refusal so a failure names
    // which check caught it rather than collapsing into one opaque code.
    #[msg("Oracle source account is not the one this reserve is configured against")]
    OracleAccountMismatch,
    #[msg("Oracle source account is not owned by the configured price program")]
    OracleOwnerMismatch,
    #[msg("Oracle source account is malformed, the wrong length, or the wrong type")]
    OracleAccountMalformed,
    #[msg("Oracle source issues a different mint than this reserve prices")]
    OracleMintMismatch,
    #[msg("Oracle source reports no shares outstanding, so no rate exists")]
    OracleZeroSupply,
    #[msg("Oracle source reports no backing assets, so no rate exists")]
    OracleZeroBacking,
    #[msg("Oracle source redemption fee is above the bound this reserve accepts")]
    OracleWithdrawalFeeTooHigh,
    #[msg("Derived exchange rate is below the absolute floor for this asset")]
    OracleRateBelowFloor,
    #[msg("Derived exchange rate is above the absolute ceiling for this asset")]
    OracleRateAboveCeiling,
    #[msg("Oracle source kind is not one this program implements")]
    UnknownOracleSource,

    // Circuit breaker / oracle state machine.
    #[msg("Oracle must be refreshed in this same transaction before use")]
    OracleStale,
    #[msg("Oracle is frozen for risk-increasing actions; repayment remains open")]
    OracleBorrowFrozen,
    #[msg("Oracle is in an emergency state; only risk-reducing actions are permitted")]
    OracleEmergency,
    #[msg("Oracle configuration is invalid")]
    InvalidOracleConfig,

    // Deployment pinning: the price source's *program*, not its account.
    #[msg("Oracle source ProgramData account is not the one this program derives to")]
    OracleProgramDataMismatch,
    #[msg("Oracle source program has been redeployed since it was pinned")]
    OracleProgramUpgraded,
    #[msg("Oracle source program's upgrade authority has changed")]
    OracleAuthorityChanged,
    #[msg("Bootstrap rate is inconsistent with the source's own previous epoch")]
    OracleBootstrapInconsistent,
    #[msg("Oracle is still bootstrapping; new borrowing is not yet permitted")]
    OracleBootstrapping,

    // --- pause ---
    #[msg("Borrowing is paused")]
    BorrowPaused,
    #[msg("The protocol is paused")]
    ProtocolPaused,
    #[msg("This reserve does not allow borrowing")]
    BorrowNotEnabled,
    #[msg("This reserve may not be used as collateral")]
    CollateralNotEnabled,

    // --- caps ---
    #[msg("Deposit would exceed the reserve supply cap")]
    SupplyCapExceeded,
    #[msg("Borrow would exceed the reserve borrow cap")]
    BorrowCapExceeded,
    #[msg("Deposit would exceed this wallet's supply cap")]
    PerWalletCapExceeded,
    #[msg("This wallet's debt in this reserve would exceed the per-wallet borrow cap")]
    PerWalletBorrowCapExceeded,
    #[msg("Pool account is not owned by the configured AMM program")]
    UnexpectedAmmProgram,
    #[msg("Pool does not hold the expected collateral/quote pair")]
    UnexpectedPoolPair,
    #[msg("Pool quote-side depth is below the configured minimum")]
    InsufficientPoolDepth,
    #[msg("The two pools disagree by more than the configured deviation")]
    PoolsDisagree,
    #[msg("An observation was submitted too soon after the previous one")]
    ObservationTooSoon,
    #[msg("The AMM program has been redeployed since this oracle was configured")]
    AmmDeploymentChanged,

    // --- health ---
    #[msg("Borrow would exceed the obligation's allowed borrow value")]
    BorrowTooLarge,
    #[msg("Withdraw would leave the obligation undercollateralized")]
    WithdrawTooLarge,
    #[msg("Obligation is healthy and cannot be liquidated")]
    ObligationHealthy,

    #[msg("Obligation still holds collateral; liquidate it before writing anything off")]
    ObligationHasCollateral,

    #[msg("Obligation has no debt in this reserve to write off")]
    NoBadDebt,
    #[msg("Reserve does not have enough available liquidity")]
    InsufficientReserveLiquidity,
    #[msg("Repay amount would seize more collateral than the obligation holds")]
    LiquidationTooLarge,

    // --- structure ---
    #[msg("Obligation already uses the maximum number of reserves")]
    TooManyReserves,
    #[msg("Reserve is not part of this obligation")]
    ReserveNotFound,
    #[msg("A refresh account did not match the obligation's stored reserves")]
    InvalidObligationAccount,
    #[msg("Reserve belongs to a different market than the obligation")]
    MarketMismatch,
    #[msg("Account does not belong to this protocol instance")]
    GlobalMismatch,
    #[msg("Signer is not the protocol admin")]
    NotAdmin,
    #[msg("Fee destination account does not match the configured destination")]
    WrongFeeDestination,
    #[msg("No protocol fees are available to collect")]
    NothingToCollect,

    // --- migration ---
    #[msg("This account has already been migrated to v0.2")]
    AlreadyMigrated,
    #[msg("This account is not in a migratable v0.1 state")]
    NotMigratable,
    #[msg("Migration altered economic state and was rolled back")]
    MigrationChangedState,
}
