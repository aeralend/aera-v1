//! Supply cap, borrow cap, and the per-wallet cap.
//!
//! The caps exist because COOK's real float is ~441M and the largest private
//! holder is ~12.8M: without them one wallet could be most of the pool.

mod common;

use common::*;

/// Lower one or more caps, leaving the others where they are.
///
/// `None` means "leave unchanged". Passing 0 would *not* mean that: 0 is the
/// encoding for "no cap", which is a loosening, and a loosening queues behind
/// the timelock instead of applying — so a helper that zeroed the caps it was
/// not interested in would silently apply nothing.
fn set_caps(
    env: &mut Env,
    handle: &ReserveHandle,
    supply_cap: Option<u64>,
    borrow_cap: Option<u64>,
    per_wallet: Option<u64>,
) {
    let mut config = env.read_reserve(handle).config;
    if let Some(value) = supply_cap {
        config.supply_cap = value;
    }
    if let Some(value) = borrow_cap {
        config.borrow_cap = value;
    }
    if let Some(value) = per_wallet {
        config.per_wallet_supply_cap = value;
    }
    env.try_set_params(handle, config).unwrap();

    // Cutting caps is a tightening, so it must already be live with no
    // timelock. test_admin.rs asserts the converse for raises.
    let live = env.read_reserve(handle).config;
    assert_eq!(
        live.supply_cap, config.supply_cap,
        "cap cut must be instant"
    );
    assert_eq!(
        live.borrow_cap, config.borrow_cap,
        "cap cut must be instant"
    );
    assert_eq!(
        live.per_wallet_supply_cap, config.per_wallet_supply_cap,
        "cap cut must be instant"
    );
}

#[test]
fn supply_cap_is_enforced() {
    let (mut env, cook, _) = Env::core(1_000);
    set_caps(
        &mut env,
        &cook,
        Some(tokens(1_000)),
        Some(tokens(1_000)),
        None,
    );

    let alice = env.create_user();
    env.fund(&alice, cook.mint, tokens(5_000));

    // Right up to the cap is fine.
    env.supply(&alice, &cook, tokens(1_000));
    assert_eq!(env.read_reserve(&cook).available_liquidity, tokens(1_000));

    // One base unit past it is not.
    assert_error(env.try_supply(&alice, &cook, 1), "SupplyCapExceeded");
}

/// The cap counts everything the pool has claim to, so lending liquidity out
/// does not quietly free headroom for more deposits.
#[test]
fn borrowing_does_not_free_supply_cap_headroom() {
    let (mut env, cook, bcook) = Env::core(1_000);
    set_caps(
        &mut env,
        &cook,
        Some(tokens(1_000)),
        Some(tokens(1_000)),
        None,
    );

    let alice = env.create_user();
    env.fund(&alice, cook.mint, tokens(5_000));
    env.supply(&alice, &cook, tokens(1_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(5_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(5_000));
    env.try_borrow(&borrower, &cook, obligation, tokens(500), &[&cook, &bcook])
        .unwrap();

    /*
     * 500 COOK is out on loan, but the pool still has claim to 1,000 — which is
     * the property under test: borrowing does not free supply-cap headroom.
     *
     * Available is the draw less the origination fee, because the fee stays in
     * the vault as protocol revenue rather than being paid out. `gross_liquidity`
     * — what the cap is measured against — is unchanged either way.
     */
    let reserve = env.read_reserve(&cook);
    let fee = tokens(500) * u64::from(reserve.config.origination_fee_bps) / 10_000;
    assert_eq!(reserve.available_liquidity, tokens(500) + fee);
    /*
     * Gross grows by the fee, and that is the conservative direction.
     *
     * `gross_liquidity` is available + borrowed, and the fee sits in available
     * while the borrower owes the full draw — so it counts once in each. The
     * supply cap is measured against gross, so protocol revenue consumes cap
     * headroom rather than creating it. Suppliers' own claim is unaffected:
     * `total_liquidity` subtracts the fee and is exactly what was supplied.
     */
    assert_eq!(
        reserve.gross_liquidity().unwrap(),
        tokens(1_000) as u128 + fee as u128
    );
    assert_eq!(
        reserve.total_liquidity().unwrap(),
        tokens(1_000) as u128,
        "suppliers' claim is exactly what was supplied; the fee is not theirs",
    );
    assert_error(env.try_supply(&alice, &cook, 1), "SupplyCapExceeded");
}

#[test]
fn borrow_cap_is_enforced() {
    let (mut env, cook, bcook) = Env::core(1_000);
    set_caps(&mut env, &cook, None, Some(tokens(300)), None);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(5_000));
    env.supply(&supplier, &cook, tokens(5_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(10_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));

    // Collateral would allow far more, but the cap does not.
    assert_error(
        env.try_borrow(&borrower, &cook, obligation, tokens(301), &[&cook, &bcook]),
        "BorrowCapExceeded",
    );

    env.try_borrow(&borrower, &cook, obligation, tokens(300), &[&cook, &bcook])
        .unwrap();

    // And no more after that.
    assert_error(
        env.try_borrow(&borrower, &cook, obligation, 1, &[&cook, &bcook]),
        "BorrowCapExceeded",
    );
}

#[test]
fn per_wallet_cap_is_enforced() {
    let (mut env, cook, _) = Env::core(1_000);
    set_caps(&mut env, &cook, None, None, Some(tokens(100)));

    let alice = env.create_user();
    env.fund(&alice, cook.mint, tokens(1_000));

    env.supply(&alice, &cook, tokens(100));
    assert_eq!(
        env.read_supply_position(&cook, alice.pubkey())
            .supplied_liquidity,
        tokens(100)
    );

    assert_error(env.try_supply(&alice, &cook, 1), "PerWalletCapExceeded");
}

/// The cap is per wallet, not per pool: a second wallet has its own headroom.
/// This is a concentration brake on the honest path, not a proof of identity,
/// and PARAMS.md says so.
#[test]
fn per_wallet_cap_is_per_wallet() {
    let (mut env, cook, _) = Env::core(1_000);
    set_caps(&mut env, &cook, None, None, Some(tokens(100)));

    let alice = env.create_user();
    env.fund(&alice, cook.mint, tokens(1_000));
    env.supply(&alice, &cook, tokens(100));

    let bob = env.create_user();
    env.fund(&bob, cook.mint, tokens(1_000));
    env.supply(&bob, &cook, tokens(100));

    assert_eq!(env.read_reserve(&cook).available_liquidity, tokens(200));
}

/// Withdrawing frees the wallet's headroom again.
#[test]
fn withdrawing_frees_per_wallet_headroom() {
    let (mut env, cook, _) = Env::core(1_000);
    set_caps(&mut env, &cook, None, None, Some(tokens(100)));

    let alice = env.create_user();
    env.fund(&alice, cook.mint, tokens(1_000));
    let shares = env.supply(&alice, &cook, tokens(100));
    assert_error(env.try_supply(&alice, &cook, 1), "PerWalletCapExceeded");

    let held = env.balance(&shares);
    env.try_withdraw(&alice, &cook, held / 2).unwrap();
    assert_eq!(
        env.read_supply_position(&cook, alice.pubkey())
            .supplied_liquidity,
        tokens(50)
    );

    // Half the cap is available again.
    env.supply(&alice, &cook, tokens(50));
}

/// A cap of zero means "no cap", which is how the collateral-only bCOOK reserve
/// is configured.
#[test]
fn zero_means_no_cap() {
    let (mut env, _, bcook) = Env::core(1_000);
    let config = env.read_reserve(&bcook).config;
    assert_eq!(config.supply_cap, 0);
    assert_eq!(config.per_wallet_supply_cap, 0);

    let whale = env.create_user();
    env.fund(&whale, bcook.mint, tokens(50_000_000));
    env.supply(&whale, &bcook, tokens(50_000_000));
    assert_eq!(
        env.read_reserve(&bcook).available_liquidity,
        tokens(50_000_000)
    );
}
